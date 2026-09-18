//! In-process MCP tests (`docs/MCP.md` §7): hand-written JSON-RPC lines over
//! a seeded, read-only database; exact rendered text asserted.

use std::env;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use devondb::text::parser::{Parsed, parse};
use devondb::{Database, Options};
use devondb_server::mcp::{self, BODY_BYTES, Mcp};
use serde_json::{Value, json};

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

const CANONICAL: &str = "nodes(Person) as person | filter person.name = \"ada\" | expand Knows out as other | project other.name";

struct TestDirectory {
    path: PathBuf,
}

impl TestDirectory {
    fn new(label: &str) -> Self {
        let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        let path = env::temp_dir().join(format!(
            "devondb-server-mcp-{label}-{timestamp}-{sequence}-{}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("create test directory");
        Self { path }
    }

    fn db_path(&self) -> PathBuf {
        self.path.join("db.devondb")
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn execute(database: &mut Database, text: &str) {
    match parse(text).unwrap_or_else(|error| panic!("parse `{text}`: {error}")) {
        Parsed::Statement(statement) => database
            .execute(&statement.stmt)
            .unwrap_or_else(|error| panic!("execute `{text}`: {error}")),
        Parsed::Query(_) => panic!("seed text is a query: {text}"),
    }
}

const SEED: &[&str] = &[
    "create node table Person (id Int64 primary key, name String, age Int64)",
    "create rel table Knows from Person to Person",
    "insert into Person values (1, \"ada\", 36), (2, \"Grace\", 85), (3, \"Linus\", null)",
    "insert rel into Knows values (1 -> 2), (1 -> 3)",
];

fn seeded(label: &str, extra: &[&str]) -> (TestDirectory, Mcp) {
    let directory = TestDirectory::new(label);
    let mut database = Database::create(directory.db_path(), 4096).expect("create database");
    for text in SEED.iter().chain(extra) {
        execute(&mut database, text);
    }
    database.checkpoint().expect("checkpoint");
    drop(database);
    // The production open policy (`docs/MCP.md` §2), on a plain file.
    let database = mcp::open(&directory.db_path(), Options::default()).expect("open");
    (directory, Mcp::new(database))
}

#[test]
fn open_policy_serves_activated_files_as_followers() {
    let directory = TestDirectory::new("follower");
    let mut database = Database::create(directory.db_path(), 4096).expect("create database");
    for text in SEED {
        execute(&mut database, text);
    }
    database.checkpoint().expect("checkpoint");
    drop(database);
    Database::activate_multiprocess(directory.db_path()).expect("activate");
    let follower = Database::open_read_only(directory.db_path()).expect("follower open works");
    drop(follower);
    let mcp = Mcp::new(mcp::open(&directory.db_path(), Options::default()).expect("open"));
    let (text, is_error) = call(
        &mcp,
        "query",
        json!({"text": "nodes(Person) as p | project p.id"}),
    );
    assert!(!is_error, "{text}");
    assert_eq!(text, "p.id:Int64\n1\n2\n3\n(3 rows)");
}

fn request(id: u64, method: &str, params: Value) -> String {
    json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}).to_string()
}

fn reply(mcp: &Mcp, line: &str) -> Value {
    let reply = mcp.handle_line(line).expect("request gets a reply");
    serde_json::from_str(&reply).expect("reply is JSON")
}

/// Calls a tool and returns `(text, isError)`.
fn call(mcp: &Mcp, name: &str, arguments: Value) -> (String, bool) {
    let reply = reply(
        mcp,
        &request(
            7,
            "tools/call",
            json!({"name": name, "arguments": arguments}),
        ),
    );
    assert_eq!(reply["id"], 7);
    let result = &reply["result"];
    let content = result["content"].as_array().expect("content array");
    assert_eq!(content.len(), 1, "one text block: {result}");
    assert_eq!(content[0]["type"], "text");
    (
        content[0]["text"].as_str().expect("text").to_owned(),
        result["isError"].as_bool().expect("isError"),
    )
}

#[test]
fn initialize_echoes_protocol_version_and_names_the_server() {
    let (_directory, mcp) = seeded("init", &[]);
    let reply = reply(
        &mcp,
        &request(1, "initialize", json!({"protocolVersion": "2025-03-26"})),
    );
    assert_eq!(reply["jsonrpc"], "2.0");
    assert_eq!(reply["id"], 1);
    assert_eq!(reply["result"]["protocolVersion"], "2025-03-26");
    assert_eq!(reply["result"]["serverInfo"]["name"], "devondb");
    assert_eq!(reply["result"]["capabilities"], json!({"tools": {}}));
}

#[test]
fn notifications_blank_lines_and_parse_errors() {
    let (_directory, mcp) = seeded("silent", &[]);
    assert_eq!(mcp.handle_line(""), None);
    assert_eq!(mcp.handle_line("   \n"), None);
    assert_eq!(
        mcp.handle_line(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#),
        None
    );
    let error = reply(&mcp, "{not json");
    assert_eq!(error["error"]["code"], -32700);
    let unknown = reply(&mcp, &request(2, "resources/read", json!({})));
    assert_eq!(unknown["error"]["code"], -32601);
    let ping = reply(&mcp, &request(3, "ping", json!({})));
    assert_eq!(ping["result"], json!({}));
}

#[test]
fn tools_list_is_exactly_four_terse_tools() {
    let (_directory, mcp) = seeded("list", &[]);
    let reply = reply(&mcp, &request(1, "tools/list", json!({})));
    let tools = reply["result"]["tools"].as_array().expect("tools");
    let names: Vec<&str> = tools
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["schema", "ask", "query", "explain"]);
    for tool in tools {
        let description = tool["description"].as_str().unwrap();
        assert!(
            description.len() < 240,
            "description bloat ({} chars): {description}",
            description.len()
        );
        assert_eq!(tool["inputSchema"]["type"], "object");
    }
}

#[test]
fn schema_renders_the_tree_with_counts() {
    let (_directory, mcp) = seeded("schema", &[]);
    let (text, is_error) = call(&mcp, "schema", json!({}));
    assert!(!is_error, "{text}");
    // ONTOLOGY.md:39-42: an unannotated node table still gets its derived class.
    assert_eq!(
        text,
        "nodes\n  Person (id:Int64 pk, name:String, age:Int64)  3 rows\nrels\n  Knows: Person -> Person  2 rows\nclasses\n  Person \"Person\"/\"people\" label=name\n"
    );
    let (without, _) = call(&mcp, "schema", json!({"counts": false}));
    assert_eq!(
        without,
        "nodes\n  Person (id:Int64 pk, name:String, age:Int64)\nrels\n  Knows: Person -> Person\nclasses\n  Person \"Person\"/\"people\" label=name\n"
    );
}

#[test]
fn schema_renders_interfaces_classes_and_pins() {
    let (_directory, mcp) = seeded(
        "ontology",
        &[
            "create interface Nameable (name String)",
            "create class for Person (plural \"people\", summary (name, age), implements (Nameable))",
            "create class for Knows (verb \"knows\", inverse \"is known by\")",
        ],
    );
    let (text, is_error) = call(&mcp, "schema", json!({"counts": false}));
    assert!(!is_error, "{text}");
    assert_eq!(
        text,
        "nodes\n  Person (id:Int64 pk, name:String, age:Int64)\nrels\n  Knows: Person -> Person\ninterfaces\n  Nameable (name:String) <- Person\nclasses\n  Person \"Person\"/\"people\" label=name summary=(name, age) implements Nameable\n  Knows verb \"knows\" inverse \"is known by\"\n"
    );
}

#[test]
fn ask_returns_the_plan_line_and_bare_typed_rows() {
    let (_directory, mcp) = seeded("ask", &[]);
    let (text, is_error) = call(&mcp, "ask", json!({"question": "who does ada know"}));
    assert!(!is_error, "{text}");
    assert_eq!(
        text,
        format!("plan: {CANONICAL}\n\nother.name:String\nGrace\nLinus\n(2 rows)")
    );
}

#[test]
fn ask_refusal_is_compact_and_not_an_error() {
    let (_directory, mcp) = seeded("noparse", &[]);
    let (text, is_error) = call(
        &mcp,
        "ask",
        json!({"question": "which persn has the most widgets"}),
    );
    assert!(!is_error, "a refusal is an answer: {text}");
    assert!(text.starts_with("no parse\n"), "{text}");
    assert!(text.contains("unrecognized: "), "{text}");
    assert!(!text.contains('{'), "no JSON in a refusal: {text}");
}

#[test]
fn ask_requires_a_question() {
    let (_directory, mcp) = seeded("noq", &[]);
    let (text, is_error) = call(&mcp, "ask", json!({}));
    assert!(is_error);
    assert_eq!(text, "`question` is required");
    let (text, is_error) = call(&mcp, "ask", json!({"question": 5}));
    assert!(is_error);
    assert_eq!(text, "`question` must be a string, got 5");
}

#[test]
fn query_windows_rows_and_states_scale() {
    let (_directory, mcp) = seeded("window", &[]);
    let text = "nodes(Person) as p | project p.name";
    let (all, _) = call(&mcp, "query", json!({"text": text}));
    assert_eq!(all, "p.name:String\nada\nGrace\nLinus\n(3 rows)");
    let (page, _) = call(
        &mcp,
        "query",
        json!({"text": text, "limit": 1, "offset": 1}),
    );
    assert_eq!(
        page,
        "p.name:String\nGrace\nrows 2-2 of 3 (more: offset=2, or narrow with a filter or aggregate)"
    );
    let (last, _) = call(
        &mcp,
        "query",
        json!({"text": text, "limit": 5, "offset": 2}),
    );
    assert_eq!(last, "p.name:String\nLinus\nrows 3-3 of 3");
    let (past, _) = call(&mcp, "query", json!({"text": text, "offset": 9}));
    assert_eq!(
        past,
        "p.name:String\nrows none of 3 (offset 9 is past the end)"
    );
    let (empty, _) = call(
        &mcp,
        "query",
        json!({"text": "nodes(Person) as p | filter p.age > 1000 | project p.name"}),
    );
    assert_eq!(empty, "p.name\n(0 rows)");
}

#[test]
fn query_nulls_are_bare_and_types_come_from_the_first_non_null() {
    let (_directory, mcp) = seeded("nulls", &[]);
    let (text, _) = call(
        &mcp,
        "query",
        json!({"text": "nodes(Person) as p | project p.name, p.age"}),
    );
    assert_eq!(
        text,
        "p.name:String\tp.age:Int64\nada\t36\nGrace\t85\nLinus\tnull\n(3 rows)"
    );
}

#[test]
fn vectors_are_never_printed() {
    let (_directory, mcp) = seeded(
        "vector",
        &[
            "create node table Doc (id Int64 primary key, embedding Vector(4))",
            "insert into Doc values (1, [0.25, 0.5, 0.75, 1.0])",
        ],
    );
    let (text, is_error) = call(
        &mcp,
        "query",
        json!({"text": "nodes(Doc) as d | project d.id, d.embedding"}),
    );
    assert!(!is_error, "{text}");
    assert_eq!(
        text,
        "d.id:Int64\td.embedding:Vector(4)\n1\t<vector 4>\n(1 rows)"
    );
    assert!(!text.contains("0.25"), "embedding numbers leaked: {text}");
}

#[test]
fn scalar_v2_cells_are_bare_with_types_in_the_header() {
    let (_directory, mcp) = seeded(
        "scalars",
        &[
            "create node table Sale (id Int64 primary key, amount Decimal(10,2), at Timestamp, blob Bytes, doc Json)",
            "insert into Sale values (1, decimal(\"12.50\"), timestamp(\"2026-08-30T00:00:00Z\"), bytes(\"deadbeef\"), json(\"{\\\"k\\\":1}\"))",
        ],
    );
    let (text, is_error) = call(
        &mcp,
        "query",
        json!({"text": "nodes(Sale) as s | project s.amount, s.at, s.blob, s.doc"}),
    );
    assert!(!is_error, "{text}");
    assert_eq!(
        text,
        "s.amount:Decimal\ts.at:Timestamp\ts.blob:Bytes\ts.doc:Json\n12.50\t2026-08-30T00:00:00Z\t0xdeadbeef\t{\"k\":1}\n(1 rows)"
    );
}

#[test]
fn body_cap_ends_the_body_and_the_footer_says_so() {
    let wide = "x".repeat(150);
    let mut tuples = Vec::new();
    for id in 0..200 {
        tuples.push(format!("({id}, \"{wide}\")"));
    }
    let insert = format!("insert into Wide values {}", tuples.join(", "));
    let (_directory, mcp) = seeded(
        "cap",
        &[
            "create node table Wide (id Int64 primary key, text String)",
            insert.as_str(),
        ],
    );
    let (text, is_error) = call(
        &mcp,
        "query",
        json!({"text": "nodes(Wide) as w | project w.text", "limit": 500}),
    );
    assert!(!is_error, "{text}");
    assert!(text.len() <= BODY_BYTES + 160, "body {} bytes", text.len());
    let footer = text.lines().last().unwrap();
    assert!(footer.starts_with("rows 1-"), "{footer}");
    assert!(footer.contains("of 200 (body cap hit;"), "{footer}");
    let shown = text.lines().count() - 2;
    assert!(shown < 200 && shown > 10, "shown {shown}");
}

#[test]
fn long_cells_are_clipped() {
    let long = "y".repeat(400);
    let insert = format!("insert into Long values (1, \"{long}\")");
    let (_directory, mcp) = seeded(
        "clip",
        &[
            "create node table Long (id Int64 primary key, text String)",
            insert.as_str(),
        ],
    );
    let (text, _) = call(
        &mcp,
        "query",
        json!({"text": "nodes(Long) as l | project l.text"}),
    );
    let cell = text.lines().nth(1).unwrap();
    assert_eq!(cell, format!("{}…(+240)", "y".repeat(160)));
}

#[test]
fn mutations_are_refused_everywhere() {
    let (_directory, mcp) = seeded("readonly", &[]);
    let statement = "insert into Person values (9, \"Eve\", 1)";
    let (text, is_error) = call(&mcp, "query", json!({"text": statement}));
    assert!(is_error);
    assert!(
        text.starts_with("read-only server: `insert into Person"),
        "{text}"
    );
    let (text, is_error) = call(&mcp, "explain", json!({"text": statement}));
    assert!(is_error);
    assert!(
        text.starts_with("read-only server: `insert into Person"),
        "{text}"
    );
    let (text, is_error) = call(&mcp, "ask", json!({"question": "delete Person 2"}));
    assert!(
        text.starts_with("read-only server:") || text.starts_with("no parse\n"),
        "{text}"
    );
    assert_eq!(is_error, text.starts_with("read-only server:"));
    let (rows, _) = call(
        &mcp,
        "query",
        json!({"text": "nodes(Person) as p | project p.id"}),
    );
    assert_eq!(rows, "p.id:Int64\n1\n2\n3\n(3 rows)");
}

#[test]
fn query_by_pin_and_argument_errors() {
    let directory = TestDirectory::new("pin");
    let mut database = Database::create(directory.db_path(), 4096).expect("create database");
    for text in SEED {
        execute(&mut database, text);
    }
    let Parsed::Query(plan) = parse("nodes(Person) as p | project p.name | limit 1").unwrap()
    else {
        panic!("query")
    };
    database
        .pin("first_person", "first person", &plan)
        .expect("pin");
    database.checkpoint().expect("checkpoint");
    drop(database);
    let mcp = Mcp::new(mcp::open(&directory.db_path(), Options::default()).expect("open"));
    let (schema, _) = call(&mcp, "schema", json!({"counts": false}));
    assert!(
        schema.ends_with("pins\n  first_person: nodes(Person) as p | project p.name | limit 1\n"),
        "{schema}"
    );
    let (text, is_error) = call(&mcp, "query", json!({"pin": "first_person"}));
    assert!(!is_error, "{text}");
    assert_eq!(text, "p.name:String\nada\n(1 rows)");
    let (text, is_error) = call(&mcp, "query", json!({"pin": "missing"}));
    assert!(is_error);
    assert!(text.contains("missing"), "{text}");
    let (text, is_error) = call(&mcp, "query", json!({}));
    assert!(is_error);
    assert_eq!(text, "pass exactly one of `text` or `pin`");
    let (text, is_error) = call(
        &mcp,
        "query",
        json!({"text": "nodes(Person) as p", "limit": -1}),
    );
    assert!(is_error);
    assert_eq!(text, "`limit` must be a non-negative integer, got -1");
}

#[test]
fn explain_prints_canonical_text_or_a_positioned_error() {
    let (_directory, mcp) = seeded("explain", &[]);
    let (text, is_error) = call(
        &mcp,
        "explain",
        json!({"text": "nodes(Person) as p | filter p.age > 40 | project p.name"}),
    );
    assert!(!is_error, "{text}");
    assert_eq!(
        text,
        "nodes(Person) as p | filter p.age > 40 | project p.name"
    );
    let (text, is_error) = call(
        &mcp,
        "explain",
        json!({"text": "nodes(Person) as p | fliter"}),
    );
    assert!(is_error);
    assert!(!text.is_empty());
    let (text, is_error) = call(&mcp, "explain", json!({}));
    assert!(is_error);
    assert_eq!(text, "`text` is required");
}

#[test]
fn unknown_tool_is_a_tool_level_error() {
    let (_directory, mcp) = seeded("unknown", &[]);
    let (text, is_error) = call(&mcp, "dump_everything", json!({}));
    assert!(is_error);
    assert_eq!(
        text,
        "unknown tool `dump_everything`; tools: schema, ask, query, explain"
    );
}
