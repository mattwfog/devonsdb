//! The four tools and their handlers (`docs/MCP.md` §3).

use std::collections::BTreeMap;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use devondb::text::parser::{Parsed, parse};
use devondb::text::printer::{print_plan, print_statement};
use devondb::{
    Database, NodeTableSummary, Plan, RelTableSummary, SchemaSummary, StatementEnvelope, fold,
};
use devondb_nl::{Compiled, CompiledStatement, DeterministicCompiler, IntentCompiler, NoParse};
use devondb_types::value::Value;
use serde_json::{Value as JsonValue, json};

use super::render::{self, DEFAULT_LIMIT, MAX_LIMIT, Window};
use crate::api::append_statement_hints;

const DAY_SECONDS: i64 = 86_400;

/// One tool call's result: text for the model, flagged when it is a failure.
pub struct Outcome {
    pub text: String,
    pub is_error: bool,
}

/// Row counts per table, keyed by table name; a failed count carries its message.
pub type Counts = BTreeMap<String, Result<u64, String>>;

/// Tool definitions — terse on purpose (`docs/MCP.md` §1 law 8).
pub fn definitions() -> JsonValue {
    json!([
        {
            "name": "schema",
            "description": "Schema tree: node tables (columns, types, row counts), relationship tables, interfaces, classes, pinned plans. Call first.",
            "inputSchema": {"type": "object", "properties": {
                "counts": {"type": "boolean", "description": "include row counts (default true)"}
            }}
        },
        {
            "name": "ask",
            "description": "Ask the database a plain-language question. Returns the deterministic plan it compiled and the rows; if the question cannot be grounded, names the unrecognized words and nearest phrasings.",
            "inputSchema": {"type": "object", "properties": {
                "question": {"type": "string"},
                "limit": {"type": "integer", "description": "rows to return (default 50, max 500)"},
                "offset": {"type": "integer", "description": "rows to skip"}
            }, "required": ["question"]}
        },
        {
            "name": "query",
            "description": "Run plan text (the canonical language shown by ask/explain) or a pinned plan by name. Read-only.",
            "inputSchema": {"type": "object", "properties": {
                "text": {"type": "string", "description": "plan text"},
                "pin": {"type": "string", "description": "pinned plan name"},
                "limit": {"type": "integer", "description": "rows to return (default 50, max 500)"},
                "offset": {"type": "integer", "description": "rows to skip"}
            }}
        },
        {
            "name": "explain",
            "description": "Canonical plan text for plan-language input, without running it. Parse errors carry positions.",
            "inputSchema": {"type": "object", "properties": {
                "text": {"type": "string"}
            }, "required": ["text"]}
        }
    ])
}

/// Routes one call; unknown tools are tool-level errors, never protocol errors.
pub fn call(database: &Mutex<Database>, name: &str, args: &JsonValue) -> Outcome {
    match name {
        "schema" => schema(database, args),
        "ask" => ask(database, args),
        "query" => query(database, args),
        "explain" => explain(args),
        other => fail(format!(
            "unknown tool `{other}`; tools: schema, ask, query, explain"
        )),
    }
}

fn ok(text: String) -> Outcome {
    Outcome {
        text,
        is_error: false,
    }
}

fn fail(text: impl Into<String>) -> Outcome {
    Outcome {
        text: text.into(),
        is_error: true,
    }
}

fn lock(database: &Mutex<Database>) -> MutexGuard<'_, Database> {
    database.lock().unwrap_or_else(PoisonError::into_inner)
}

fn schema(database: &Mutex<Database>, args: &JsonValue) -> Outcome {
    let with_counts = match bool_arg(args, "counts") {
        Ok(value) => value.unwrap_or(true),
        Err(message) => return fail(message),
    };
    let mut database = lock(database);
    let summary = database.schema_summary();
    let counts = with_counts.then(|| table_counts(&mut database, &summary));
    ok(render::schema_tree(&summary, counts.as_ref()))
}

fn table_counts(database: &mut Database, summary: &SchemaSummary) -> Counts {
    let mut counts = Counts::new();
    for table in &summary.node_tables {
        if let Some(text) = node_count_text(table) {
            counts.insert(table.name.clone(), count_rows(database, &text));
        }
    }
    for rel in &summary.rel_tables {
        if let Some(text) = rel_count_text(summary, rel) {
            counts.insert(rel.name.clone(), count_rows(database, &text));
        }
    }
    counts
}

fn primary_key(table: &NodeTableSummary) -> Option<&str> {
    table
        .columns
        .iter()
        .find(|column| column.primary_key)
        .map(|column| column.name.as_str())
}

fn node_count_text(table: &NodeTableSummary) -> Option<String> {
    let key = primary_key(table)?;
    Some(format!(
        "nodes({}) as n | aggregate count(n.{key}) as `rows`",
        table.name
    ))
}

fn rel_count_text(summary: &SchemaSummary, rel: &RelTableSummary) -> Option<String> {
    let target = summary
        .node_tables
        .iter()
        .find(|table| fold(&table.name) == fold(&rel.to))?;
    let key = primary_key(target)?;
    Some(format!(
        "nodes({}) as a | expand {} out as b | aggregate count(b.{key}) as `rows`",
        rel.from, rel.name
    ))
}

fn count_rows(database: &mut Database, text: &str) -> Result<u64, String> {
    let plan = match parse(text).map_err(|error| error.to_string())? {
        Parsed::Query(plan) => plan,
        Parsed::Statement(_) => return Err("count compiled to a statement".to_owned()),
    };
    let result = database.run(&plan).map_err(|error| error.to_string())?;
    match result.rows.first().and_then(|row| row.first()) {
        Some(Value::Int64(count)) => u64::try_from(*count).map_err(|error| error.to_string()),
        other => Err(format!("count returned {other:?}")),
    }
}

fn ask(database: &Mutex<Database>, args: &JsonValue) -> Outcome {
    let question = match str_arg(args, "question") {
        Ok(Some(question)) => question,
        Ok(None) => return fail("`question` is required"),
        Err(message) => return fail(message),
    };
    let window = match window_arg(args) {
        Ok(window) => window,
        Err(message) => return fail(message),
    };
    // Row-backed compilation, never the schema-only `compile`: row-backed
    // grounding resolves a value to its STORED spelling (`docs/NL.md`
    // § entity value slots) — the same lesson `api.rs` and the CLI record.
    // Orchestration boundary — the compiler stays deterministic; NL.md §16.
    let reference_date = utc_reference_date();
    let mut database = lock(database);
    match DeterministicCompiler.compile_at_with_database(question, &mut database, reference_date) {
        Compiled::Plan(plan) => run_plan(&mut database, &plan, window, true),
        Compiled::NoParse(report) => refuse(&database, question, report),
    }
}

fn refuse(database: &Database, question: &str, mut report: NoParse) -> Outcome {
    match DeterministicCompiler.compile_statement(question, &database.schema_summary()) {
        CompiledStatement::Statement(statement) => fail(read_only(&StatementEnvelope {
            v: 0,
            stmt: statement,
        })),
        CompiledStatement::NoParse(statement_report) => {
            append_statement_hints(&mut report, &statement_report);
            // A refusal is an answer, not a failure: the model reads what
            // did not ground and rephrases.
            ok(render::refusal(&report))
        }
    }
}

fn run_plan(database: &mut Database, plan: &Plan, window: Window, with_plan: bool) -> Outcome {
    let canonical = match print_plan(plan) {
        Ok(canonical) => canonical,
        Err(error) => return fail(error.to_string()),
    };
    let result = match database.run(plan) {
        Ok(result) => result,
        Err(error) => return fail(error.to_string()),
    };
    let table = render::result_table(&result, window);
    ok(if with_plan {
        format!("plan: {canonical}\n\n{table}")
    } else {
        table
    })
}

fn query(database: &Mutex<Database>, args: &JsonValue) -> Outcome {
    let window = match window_arg(args) {
        Ok(window) => window,
        Err(message) => return fail(message),
    };
    let (text, pin) = match (str_arg(args, "text"), str_arg(args, "pin")) {
        (Ok(text), Ok(pin)) => (text, pin),
        (Err(message), _) | (_, Err(message)) => return fail(message),
    };
    let mut database = lock(database);
    match (text, pin) {
        (Some(text), None) => query_text(&mut database, text, window),
        (None, Some(pin)) => match database.run_pin(pin) {
            Ok(result) => ok(render::result_table(&result, window)),
            Err(error) => fail(error.to_string()),
        },
        _ => fail("pass exactly one of `text` or `pin`"),
    }
}

fn query_text(database: &mut Database, text: &str, window: Window) -> Outcome {
    match parse(text) {
        Ok(Parsed::Query(plan)) => run_plan(database, &plan, window, false),
        Ok(Parsed::Statement(statement)) => fail(read_only(&statement)),
        Err(error) => fail(error.to_string()),
    }
}

fn explain(args: &JsonValue) -> Outcome {
    let text = match str_arg(args, "text") {
        Ok(Some(text)) => text,
        Ok(None) => return fail("`text` is required"),
        Err(message) => return fail(message),
    };
    match parse(text) {
        Ok(Parsed::Query(plan)) => match print_plan(&plan) {
            Ok(canonical) => ok(canonical),
            Err(error) => fail(error.to_string()),
        },
        Ok(Parsed::Statement(statement)) => fail(read_only(&statement)),
        Err(error) => fail(error.to_string()),
    }
}

fn read_only(statement: &StatementEnvelope) -> String {
    match print_statement(statement) {
        Ok(canonical) => format!("read-only server: `{canonical}` is a mutation (docs/MCP.md §2)"),
        Err(error) => format!("read-only server: mutation refused ({error})"),
    }
}

fn utc_reference_date() -> i64 {
    let epoch_secs = match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(duration) => duration.as_secs() as i64,
        Err(error) => -(error.duration().as_secs() as i64),
    };
    epoch_secs - epoch_secs.rem_euclid(DAY_SECONDS)
}

fn str_arg<'a>(args: &'a JsonValue, key: &str) -> Result<Option<&'a str>, String> {
    match args.get(key) {
        None | Some(JsonValue::Null) => Ok(None),
        Some(JsonValue::String(text)) => Ok(Some(text)),
        Some(other) => Err(format!("`{key}` must be a string, got {other}")),
    }
}

fn bool_arg(args: &JsonValue, key: &str) -> Result<Option<bool>, String> {
    match args.get(key) {
        None | Some(JsonValue::Null) => Ok(None),
        Some(JsonValue::Bool(flag)) => Ok(Some(*flag)),
        Some(other) => Err(format!("`{key}` must be a boolean, got {other}")),
    }
}

fn usize_arg(args: &JsonValue, key: &str) -> Result<Option<usize>, String> {
    match args.get(key) {
        None | Some(JsonValue::Null) => Ok(None),
        Some(JsonValue::Number(number)) => number
            .as_u64()
            .and_then(|value| usize::try_from(value).ok())
            .map(Some)
            .ok_or_else(|| format!("`{key}` must be a non-negative integer, got {number}")),
        Some(other) => Err(format!("`{key}` must be an integer, got {other}")),
    }
}

/// `limit` clamps into `1..=MAX_LIMIT` (law 2); `offset` is unbounded.
fn window_arg(args: &JsonValue) -> Result<Window, String> {
    let limit = usize_arg(args, "limit")?
        .unwrap_or(DEFAULT_LIMIT)
        .clamp(1, MAX_LIMIT);
    let offset = usize_arg(args, "offset")?.unwrap_or(0);
    Ok(Window { limit, offset })
}
