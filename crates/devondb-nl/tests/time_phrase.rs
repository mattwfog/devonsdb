use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use devondb::text::parser::{Parsed, parse};
use devondb::text::printer::print_plan;
use devondb::{ColumnSummary, Database, NodeTableSummary, SchemaSummary};
use devondb_nl::{
    Compiled, DeterministicCompiler, IntentCompiler, NL_VERSION, NoParse, Ungrounded,
};

const REFERENCE_DATE: i64 = 1_787_097_600; // 2026-08-19T00:00:00Z
const DAY_SECONDS: i64 = 86_400;
static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

fn column(name: &str, ty: &str, primary_key: bool) -> ColumnSummary {
    ColumnSummary {
        name: name.to_owned(),
        ty: ty.to_owned(),
        primary_key,
    }
}

fn event_schema() -> SchemaSummary {
    SchemaSummary {
        node_tables: vec![NodeTableSummary {
            name: "Event".to_owned(),
            columns: vec![
                column("id", "Int64", true),
                column("title", "String", false),
                column("occurred", "Int64", false),
                column("priority", "Int64", false),
                column("note", "String", false),
            ],
        }],
        rel_tables: Vec::new(),
        classes: None,
        pins: Vec::new(),
    }
}

fn compile_time_phrase(question: &str, schema: &SchemaSummary) -> devondb::Plan {
    match DeterministicCompiler.compile_at(question, schema, REFERENCE_DATE) {
        Compiled::Plan(plan) => plan,
        Compiled::NoParse(report) => panic!("time phrase refused: {question:?}: {report:?}"),
    }
}

fn refusal_at(question: &str, schema: &SchemaSummary, reference_date: i64) -> NoParse {
    match DeterministicCompiler.compile_at(question, schema, reference_date) {
        Compiled::NoParse(report) => report,
        Compiled::Plan(plan) => panic!("refused time phrase compiled: {question:?}: {plan:?}"),
    }
}

struct TestDatabase {
    database: Option<Database>,
    directory: PathBuf,
}

impl TestDatabase {
    fn new() -> Self {
        let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let directory =
            std::env::temp_dir().join(format!("devondb-nl-time-{}-{sequence}", std::process::id()));
        fs::create_dir(&directory).unwrap();
        let mut database = Database::create(directory.join("time.devondb"), 4096).unwrap();
        for statement in [
            "create node table Event (id Int64 primary key, title String, occurred Int64, priority Int64, note String)",
            "insert into Event values (1, \"Old\", 1784419200, 0, \"old\"), (2, \"Week start\", 1786492800, 1, \"week\"), (3, \"Yesterday start\", 1787011200, 2, \"yesterday\"), (4, \"Yesterday end\", 1787097599, 1, \"yesterday\"), (5, \"Today start\", 1787097600, 3, \"today\"), (6, \"Today end\", 1787183999, 4, \"today\"), (7, \"Tomorrow\", 1787184000, 5, \"future\")",
        ] {
            let Parsed::Statement(envelope) = parse(statement).unwrap() else {
                panic!("fixture input parsed as a query: {statement}");
            };
            database.execute(&envelope.stmt).unwrap();
        }
        Self {
            database: Some(database),
            directory,
        }
    }

    fn database(&mut self) -> &mut Database {
        self.database.as_mut().unwrap()
    }
}

impl Drop for TestDatabase {
    fn drop(&mut self) {
        drop(self.database.take());
        let _ = fs::remove_dir_all(&self.directory);
    }
}

#[test]
fn time_phrase_golden_corpus_pins_text_and_executes_boundary_rows() {
    assert_eq!(NL_VERSION, 10);
    let mut fixture = TestDatabase::new();
    let database = fixture.database();
    let schema = database.schema_summary();
    for (question, expected, expected_ids) in [
        (
            "events with occurred since last week",
            "nodes(Event) as event | filter event.occurred >= 1786492800",
            &[2, 3, 4, 5, 6, 7][..],
        ),
        (
            "events occurred in the last 30 days",
            "nodes(Event) as event | filter event.occurred >= 1784505600 and event.occurred < 1787097600",
            &[2, 3, 4],
        ),
        (
            "events occurred yesterday",
            "nodes(Event) as event | filter event.occurred >= 1787011200 and event.occurred < 1787097600",
            &[3, 4],
        ),
        (
            "events occurred today",
            "nodes(Event) as event | filter event.occurred >= 1787097600 and event.occurred < 1787184000",
            &[5, 6],
        ),
        (
            "events occurred since 3 days ago",
            "nodes(Event) as event | filter event.occurred >= 1786838400",
            &[3, 4, 5, 6, 7],
        ),
        (
            "events with priority over 1 and occurred yesterday",
            "nodes(Event) as event | filter event.priority > 1 and event.occurred >= 1787011200 and event.occurred < 1787097600",
            &[3],
        ),
    ] {
        let plan = compile_time_phrase(question, &schema);
        assert_eq!(
            print_plan(&plan).unwrap(),
            expected,
            "question: {question:?}"
        );
        let result = database.run(&plan).unwrap();
        let ids = result
            .rows
            .iter()
            .map(|row| row[0].to_string().parse::<i64>().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(ids, expected_ids, "question: {question:?}");
    }
}

#[test]
fn time_phrase_compilation_is_deterministic_for_a_fixed_reference_date() {
    let schema = event_schema();
    for question in [
        "events occurred since last week",
        "events occurred in the last 30 days",
        "events occurred yesterday",
        "events occurred today",
        "events occurred since 3 days ago",
    ] {
        let first = DeterministicCompiler.compile_at(question, &schema, REFERENCE_DATE);
        let second = DeterministicCompiler.compile_at(question, &schema, REFERENCE_DATE);
        assert_eq!(first, second, "question: {question:?}");
        let (Compiled::Plan(first), Compiled::Plan(second)) = (first, second) else {
            panic!("golden time phrase refused: {question:?}");
        };
        assert_eq!(first.to_json().unwrap(), second.to_json().unwrap());
    }
}

#[test]
fn time_phrase_refusals_cover_column_type_vocabulary_and_dateless_compile() {
    let schema = event_schema();

    let unnamed = refusal_at("events since last week", &schema, REFERENCE_DATE);
    assert!(unnamed.unrecognized.iter().any(|item| {
        item.suggestion
            .as_deref()
            .is_some_and(|text| text.contains("named Int64 column"))
    }));

    let non_integer = refusal_at("events with note today", &schema, REFERENCE_DATE);
    assert_eq!(
        non_integer.unrecognized,
        [Ungrounded {
            token: "note".to_owned(),
            suggestion: Some("time column `note` must be Int64, found String".to_owned()),
        }]
    );

    let fortnight = refusal_at(
        "events with occurred since last fortnight",
        &schema,
        REFERENCE_DATE,
    );
    assert!(
        fortnight
            .unrecognized
            .iter()
            .any(|item| item.token == "fortnight")
    );

    let Compiled::NoParse(dateless) =
        DeterministicCompiler.compile("events with occurred today", &schema)
    else {
        panic!("dateless time phrase compiled");
    };
    assert!(dateless.unrecognized.iter().any(|item| {
        item.suggestion
            .as_deref()
            .is_some_and(|text| text.contains("reference date"))
    }));
}

#[test]
fn time_phrase_checked_arithmetic_refuses_invalid_or_overflowing_int64_bounds() {
    let schema = event_schema();
    let invalid_midnight = refusal_at("events occurred today", &schema, REFERENCE_DATE + 1);
    assert!(invalid_midnight.unrecognized.iter().any(|item| {
        item.suggestion
            .as_deref()
            .is_some_and(|text| text.contains("UTC midnight"))
    }));

    let min_midnight = i64::MIN + (DAY_SECONDS - i64::MIN.rem_euclid(DAY_SECONDS));
    let underflow = refusal_at("events occurred since last week", &schema, min_midnight);
    assert!(underflow.unrecognized.iter().any(|item| {
        item.suggestion
            .as_deref()
            .is_some_and(|text| text.contains("Int64 epoch-second bounds"))
    }));

    let max_midnight = i64::MAX - i64::MAX.rem_euclid(DAY_SECONDS);
    let overflow = refusal_at("events occurred today", &schema, max_midnight);
    assert!(overflow.unrecognized.iter().any(|item| {
        item.suggestion
            .as_deref()
            .is_some_and(|text| text.contains("Int64 epoch-second bounds"))
    }));

    let huge_count = refusal_at(
        "events occurred in the last 106751991167301 days",
        &schema,
        REFERENCE_DATE,
    );
    assert!(huge_count.unrecognized.iter().any(|item| {
        item.suggestion
            .as_deref()
            .is_some_and(|text| text.contains("Int64 epoch-second bounds"))
    }));
}

struct ExistingCompiler;

impl IntentCompiler for ExistingCompiler {
    fn compile(&self, _question: &str, _schema: &SchemaSummary) -> Compiled {
        Compiled::NoParse(NoParse::default())
    }
}

#[test]
fn time_phrase_compile_at_has_a_source_compatible_default() {
    assert_eq!(
        ExistingCompiler.compile_at("events occurred today", &event_schema(), REFERENCE_DATE),
        Compiled::NoParse(NoParse::default())
    );
}
