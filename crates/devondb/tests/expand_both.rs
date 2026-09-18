//! Public-facade regression coverage for source-bound `expand ... both`.

use std::path::Path;

use devondb::{Database, DevonError, Options, Plan, QueryResult, Statement, text};
use devondb_storage::{catalog::Catalog, pager::Pager};
use devondb_types::value::Value;
use tempfile::TempDir;

const PAGE_SIZE: u32 = 4096;
const COMPANY_BOTH: &str = concat!(
    "nodes(Company) as c | expand Works both as p | ",
    "project c.name as company, p.name as person"
);
const COMPANY_IN: &str = concat!(
    "nodes(Company) as c | expand Works in as p | ",
    "project c.name as company, p.name as person"
);
const PERSON_BOTH: &str = concat!(
    "nodes(Person) as p | expand Works both as c | ",
    "project p.name as person, c.name as company"
);
const PERSON_OUT: &str = concat!(
    "nodes(Person) as p | expand Works out as c | ",
    "project p.name as person, c.name as company"
);
const COMPANY_PIN: &str = "works both from company";
const PERSON_PIN: &str = "works both from person";

#[test]
fn heterogeneous_both_uses_the_source_binding_and_replays_byte_identical_pins() {
    let directory = TempDir::new().expect("temporary directory");
    let path = directory.path().join("heterogeneous.devondb");
    let mut database = heterogeneous_database(&path);

    // Keep a visible DML effect on Person: Company -> Person therefore uses
    // the hole-aware snapshot while Person -> Company uses the dense one.
    execute_text(
        &mut database,
        "update Person set name = \"Linus Torvalds\" where id = 3",
    );

    let (company_plan, company_bytes) = canonical_plan(COMPANY_BOTH);
    let (person_plan, person_bytes) = canonical_plan(PERSON_BOTH);
    let company_both = database.run(&company_plan).expect("Company both");
    let company_in = run_text(&mut database, COMPANY_IN);
    assert_eq!(company_both, company_in);
    assert_eq!(company_both.rows, company_rows());

    let person_both = database.run(&person_plan).expect("Person both");
    let person_out = run_text(&mut database, PERSON_OUT);
    assert_eq!(person_both, person_out);
    assert_eq!(person_both.rows, person_rows());

    execute_text(
        &mut database,
        &format!("pin \"{COMPANY_PIN}\" as {COMPANY_BOTH}"),
    );
    execute_text(
        &mut database,
        &format!("pin \"{PERSON_PIN}\" as {PERSON_BOTH}"),
    );
    database.checkpoint().expect("checkpoint graph and pins");
    drop(database);

    assert_pin_bytes(&path, COMPANY_PIN, &company_bytes);
    assert_pin_bytes(&path, PERSON_PIN, &person_bytes);

    let mut reopened = Database::open(&path).expect("reopen graph database");
    assert_eq!(
        reopened.run(&company_plan).expect("reopened Company both"),
        company_both
    );
    assert_eq!(
        reopened.run(&person_plan).expect("reopened Person both"),
        person_both
    );
    assert_eq!(
        reopened.run_pin(COMPANY_PIN).expect("replay Company pin"),
        company_both
    );
    assert_eq!(
        reopened.run_pin(PERSON_PIN).expect("replay Person pin"),
        person_both
    );
}

#[test]
fn self_relationship_both_is_out_then_in_with_a_self_loop_once() {
    let directory = TempDir::new().expect("temporary directory");
    let path = directory.path().join("self.devondb");
    let mut database = Database::create(&path, PAGE_SIZE).expect("create self graph");
    for statement in [
        "create node table Person (id Int64 primary key, name String)",
        "create rel table Knows from Person to Person",
        concat!(
            "insert into Person values (1, \"Ada\"), (2, \"Bob\"), ",
            "(3, \"Carol\"), (4, \"Dan\")"
        ),
        concat!(
            "insert rel into Knows values (3 -> 1), (1 -> 2), ",
            "(1 -> 1), (4 -> 1), (1 -> 3)"
        ),
    ] {
        execute_text(&mut database, statement);
    }

    let out = run_text(
        &mut database,
        "nodes(Person) as p | filter p.id = 1 | expand Knows out as k | project k.name",
    );
    let incoming = run_text(
        &mut database,
        "nodes(Person) as p | filter p.id = 1 | expand Knows in as k | project k.name",
    );
    let both = run_text(
        &mut database,
        "nodes(Person) as p | filter p.id = 1 | expand Knows both as k | project k.name",
    );

    assert_eq!(out.rows, names(&["Bob", "Ada", "Carol"]));
    assert_eq!(incoming.rows, names(&["Carol", "Ada", "Dan"]));
    assert_eq!(both.rows, names(&["Bob", "Ada", "Carol", "Carol", "Dan"]));
}

#[test]
fn scalar_cardinality_stops_at_two_rows_under_the_shared_budget() {
    const ROW_COUNT: i64 = 10_000;
    const PAYLOAD_BYTES: usize = 128;
    const MEMORY_LIMIT: usize = 1024 * 1024;

    let directory = TempDir::new().expect("temporary directory");
    let path = directory.path().join("scalar-budget.devondb");
    let mut database = Database::create(&path, PAGE_SIZE).expect("create scalar database");
    execute_text(
        &mut database,
        "create node table Probe (id Int64 primary key)",
    );
    execute_text(
        &mut database,
        "create node table Big (id Int64 primary key, payload String)",
    );
    execute_text(&mut database, "insert into Probe values (1)");
    insert_big_rows(&mut database, ROW_COUNT, PAYLOAD_BYTES);
    database.checkpoint().expect("checkpoint scalar fixture");
    drop(database);

    let mut database = Database::open_with(
        &path,
        Options {
            page_size: PAGE_SIZE,
            memory_limit: MEMORY_LIMIT,
        },
    )
    .expect("reopen with tight memory limit");

    let full_error = database
        .run(&query("nodes(Big) as b | project b.payload"))
        .expect_err("the full ten-thousand-row result must exceed one MiB");
    assert!(
        matches!(full_error, DevonError::BudgetExceeded { .. }),
        "unexpected full-materialization error: {full_error}"
    );

    let scalar = concat!(
        "nodes(Probe) as p | limit 1 | project ",
        "scalar(nodes(Big) as b | project b.payload) as value"
    );
    let error = database
        .run(&canonical_plan(scalar).0)
        .expect_err("two scalar rows must terminate with the cardinality error");
    assert_eq!(
        error.to_string(),
        "invalid argument: scalar subquery returned more than one row"
    );
}

fn heterogeneous_database(path: &Path) -> Database {
    let mut database = Database::create(path, PAGE_SIZE).expect("create graph database");
    for statement in [
        "create node table Person (id Int64 primary key, name String)",
        "create node table Company (id Int64 primary key, name String)",
        "create rel table Works from Person to Company",
        concat!(
            "insert into Person values (1, \"Ada\"), (2, \"Grace\"), ",
            "(3, \"Linus\")"
        ),
        "insert into Company values (10, \"Acme\"), (20, \"Beta\")",
        concat!(
            "insert rel into Works values (1 -> 10), (2 -> 10), ",
            "(1 -> 20), (3 -> 10)"
        ),
    ] {
        execute_text(&mut database, statement);
    }
    database.checkpoint().expect("checkpoint base graph");
    database
}

fn insert_big_rows(database: &mut Database, row_count: i64, payload_bytes: usize) {
    let payload = "x".repeat(payload_bytes);
    for start in (0..row_count).step_by(500) {
        let end = (start + 500).min(row_count);
        let values = (start..end)
            .map(|id| format!("({id}, \"{payload}\")"))
            .collect::<Vec<_>>()
            .join(", ");
        execute_text(database, &format!("insert into Big values {values}"));
    }
}

fn statement(input: &str) -> Statement {
    let text::parser::Parsed::Statement(statement) =
        text::parser::parse(input).unwrap_or_else(|error| panic!("parse `{input}`: {error}"))
    else {
        panic!("expected statement: {input}");
    };
    statement.stmt
}

fn execute_text(database: &mut Database, input: &str) {
    database
        .execute(&statement(input))
        .unwrap_or_else(|error| panic!("execute `{input}`: {error}"));
}

fn query(input: &str) -> Plan {
    let text::parser::Parsed::Query(plan) =
        text::parser::parse(input).unwrap_or_else(|error| panic!("parse `{input}`: {error}"))
    else {
        panic!("expected query: {input}");
    };
    plan
}

fn canonical_plan(input: &str) -> (Plan, Vec<u8>) {
    let plan = query(input);
    let canonical = text::printer::print_plan(&plan).expect("print canonical plan");
    assert_eq!(canonical, input, "query is not canonical text");
    let json = plan.to_json().expect("serialize canonical plan");
    let reparsed = query(&canonical);
    assert_eq!(reparsed, plan, "canonical text changed the plan IR");
    assert_eq!(
        reparsed
            .to_json()
            .expect("reserialize canonical plan")
            .as_bytes(),
        json.as_bytes(),
        "canonical plan JSON changed byte-for-byte after round-trip"
    );
    let value: serde_json::Value =
        serde_json::from_str(&json).expect("decode canonical plan for pin storage");
    let stored_bytes = serde_json::to_vec(&value).expect("encode canonical pin value");
    (plan, stored_bytes)
}

fn run_text(database: &mut Database, input: &str) -> QueryResult {
    database
        .run(&canonical_plan(input).0)
        .unwrap_or_else(|error| panic!("run `{input}`: {error}"))
}

fn assert_pin_bytes(path: &Path, name: &str, expected: &[u8]) {
    let pager = Pager::open(path).expect("open pager for pin inspection");
    let catalog = Catalog::load(&pager).expect("load catalog for pin inspection");
    let pin = catalog
        .pins()
        .iter()
        .find(|pin| pin.name == name)
        .unwrap_or_else(|| panic!("missing pin `{name}`"));
    assert_eq!(
        serde_json::to_vec(&pin.plan).expect("serialize stored canonical plan"),
        expected,
        "stored canonical plan bytes changed"
    );
}

fn company_rows() -> Vec<Vec<Value>> {
    [
        ("Acme", "Ada"),
        ("Acme", "Grace"),
        ("Acme", "Linus Torvalds"),
        ("Beta", "Ada"),
    ]
    .into_iter()
    .map(|(company, person)| vec![string(company), string(person)])
    .collect()
}

fn person_rows() -> Vec<Vec<Value>> {
    [
        ("Ada", "Acme"),
        ("Ada", "Beta"),
        ("Grace", "Acme"),
        ("Linus Torvalds", "Acme"),
    ]
    .into_iter()
    .map(|(person, company)| vec![string(person), string(company)])
    .collect()
}

fn names(values: &[&str]) -> Vec<Vec<Value>> {
    values.iter().map(|value| vec![string(value)]).collect()
}

fn string(value: &str) -> Value {
    Value::String(value.to_owned())
}
