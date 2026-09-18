//! Public-facade end-to-end coverage for DevonPlan text joins.

use devondb::{
    Database, Plan, QueryResult, Statement,
    text::{self, parser::Parsed},
};
use devondb_types::value::Value;
use tempfile::{TempDir, tempdir};

const PAGE_SIZE: u32 = 4096;

#[test]
fn join_inner_and_left_pin_order_null_padding_and_column_order() {
    let (_directory, mut database) = database_with(&[
        "create node table Person (id Int64 primary key, name String, city_code String)",
        "create node table City (id Int64 primary key, code String, name String)",
        concat!(
            "insert into Person values (1, \"Ada\", \"LON\"), ",
            "(2, \"Grace\", \"PAR\"), (3, \"Linus\", null), (4, \"Edsger\", \"XXX\")"
        ),
        concat!(
            "insert into City values (10, \"LON\", \"London west\"), ",
            "(20, \"PAR\", \"Paris\"), (30, \"LON\", \"London east\")"
        ),
    ]);
    let inner = run_text(
        &mut database,
        concat!(
            "let cities = nodes(City) as c; ",
            "nodes(Person) as p | join cities on p.city_code = c.code"
        ),
    );

    assert_eq!(
        inner.columns,
        ["p.id", "p.name", "p.city_code", "c.id", "c.code", "c.name"]
    );
    assert_eq!(
        inner.rows,
        vec![
            person_city(1, "Ada", string("LON"), 10, "LON", "London west"),
            person_city(1, "Ada", string("LON"), 30, "LON", "London east"),
            person_city(2, "Grace", string("PAR"), 20, "PAR", "Paris"),
        ]
    );

    let left = run_text(
        &mut database,
        concat!(
            "let cities = nodes(City) as c; ",
            "nodes(Person) as p | left join cities on p.city_code = c.code"
        ),
    );
    assert_eq!(left.columns, inner.columns);
    assert_eq!(
        left.rows,
        vec![
            person_city(1, "Ada", string("LON"), 10, "LON", "London west"),
            person_city(1, "Ada", string("LON"), 30, "LON", "London east"),
            person_city(2, "Grace", string("PAR"), 20, "PAR", "Paris"),
            vec![
                int(3),
                string("Linus"),
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
            ],
            vec![
                int(4),
                string("Edsger"),
                string("XXX"),
                Value::Null,
                Value::Null,
                Value::Null,
            ],
        ]
    );
}

#[test]
fn join_reuses_one_let_subplan_twice() {
    let (_directory, mut database) = database_with(&[
        "create node table Person (id Int64 primary key, city_code String)",
        "create node table City (id Int64 primary key, code String, name String)",
        "insert into Person values (1, \"LON\"), (2, \"PAR\")",
        concat!(
            "insert into City values (10, \"LON\", \"London west\"), ",
            "(20, \"PAR\", \"Paris\"), (30, \"LON\", \"London east\")"
        ),
    ]);
    let result = run_text(
        &mut database,
        concat!(
            "let cities = nodes(City) as c; ",
            "nodes(Person) as p | join cities on p.city_code = c.code | ",
            "aggregate count(c.id) as `stats.matches` by p.city_code | ",
            "sort p.city_code | join cities on p.city_code = c.code"
        ),
    );

    assert_eq!(
        result.columns,
        ["p.city_code", "stats.matches", "c.id", "c.code", "c.name"]
    );
    assert_eq!(
        result.rows,
        vec![
            vec![
                string("LON"),
                int(2),
                int(10),
                string("LON"),
                string("London west"),
            ],
            vec![
                string("LON"),
                int(2),
                int(30),
                string("LON"),
                string("London east"),
            ],
            vec![
                string("PAR"),
                int(1),
                int(20),
                string("PAR"),
                string("Paris"),
            ],
        ]
    );
}

#[test]
fn join_chain_spans_three_tables() {
    let (_directory, mut database) = database_with(&[
        "create node table Engineer (id Int64 primary key, name String, team_id Int64)",
        "create node table Team (id Int64 primary key, name String, org_id Int64)",
        "create node table Org (id Int64 primary key, name String)",
        "insert into Engineer values (1, \"Ada\", 10), (2, \"Grace\", 20), (3, \"No team\", 99)",
        "insert into Team values (10, \"Compiler\", 100), (20, \"Runtime\", 200)",
        "insert into Org values (100, \"Research\"), (200, \"Product\")",
    ]);
    let result = run_text(
        &mut database,
        concat!(
            "let teams = nodes(Team) as t; let orgs = nodes(Org) as o; ",
            "nodes(Engineer) as e | join teams on e.team_id = t.id | ",
            "join orgs on t.org_id = o.id | project e.name as engineer, ",
            "t.name as team, o.name as org"
        ),
    );

    assert_eq!(result.columns, ["engineer", "team", "org"]);
    assert_eq!(
        result.rows,
        vec![
            vec![string("Ada"), string("Compiler"), string("Research")],
            vec![string("Grace"), string("Runtime"), string("Product")],
        ]
    );
}

#[test]
fn join_timestamp_keys_never_match_nulls() {
    let (_directory, mut database) = database_with(&[
        "create node table Event (id Int64 primary key, happened Timestamp, label String)",
        "create node table Marker (id Int64 primary key, happened Timestamp, label String)",
        concat!(
            "insert into Event values ",
            "(1, timestamp(\"1970-01-01T00:00:00Z\"), \"epoch\"), ",
            "(2, null, \"missing\"), ",
            "(3, timestamp(\"1970-01-01T00:00:01Z\"), \"one second\")"
        ),
        concat!(
            "insert into Marker values ",
            "(10, timestamp(\"1970-01-01T00:00:01Z\"), \"matched\"), ",
            "(20, null, \"must not match\")"
        ),
    ]);
    let result = run_text(
        &mut database,
        concat!(
            "let markers = nodes(Marker) as m; ",
            "nodes(Event) as e | left join markers on e.happened = m.happened"
        ),
    );

    assert_eq!(
        result.columns,
        [
            "e.id",
            "e.happened",
            "e.label",
            "m.id",
            "m.happened",
            "m.label"
        ]
    );
    assert_eq!(
        result.rows,
        vec![
            vec![
                int(1),
                Value::Timestamp(0),
                string("epoch"),
                Value::Null,
                Value::Null,
                Value::Null,
            ],
            vec![
                int(2),
                Value::Null,
                string("missing"),
                Value::Null,
                Value::Null,
                Value::Null,
            ],
            vec![
                int(3),
                Value::Timestamp(1_000_000),
                string("one second"),
                int(10),
                Value::Timestamp(1_000_000),
                string("matched"),
            ],
        ]
    );
}

#[test]
fn join_aggregate_sort_limit_pipeline() {
    let (_directory, mut database) = database_with(&[
        "create node table Account (id Int64 primary key, desk String)",
        "create node table Trade (id Int64 primary key, account_id Int64, amount Int64)",
        "insert into Account values (1, \"Alpha\"), (2, \"Beta\"), (3, \"Gamma\")",
        concat!(
            "insert into Trade values (10, 1, 5), (11, 1, 7), ",
            "(12, 2, 20), (13, 3, 1)"
        ),
    ]);
    let result = run_text(
        &mut database,
        concat!(
            "let trades = nodes(Trade) as t; ",
            "nodes(Account) as a | join trades on a.id = t.account_id | ",
            "aggregate sum(t.amount) as `metrics.total`, ",
            "count(t.id) as `metrics.count` by a.desk | ",
            "sort metrics.total desc | limit 2"
        ),
    );

    assert_eq!(result.columns, ["a.desk", "metrics.total", "metrics.count"]);
    assert_eq!(
        result.rows,
        vec![
            vec![string("Beta"), int(20), int(1)],
            vec![string("Alpha"), int(12), int(2)],
        ]
    );
}

#[test]
fn join_severed_validation_and_parse_refusals_have_exact_messages() {
    let (_directory, mut database) =
        database_with(&["create node table Person (id Int64 primary key, name String)"]);
    let duplicate = concat!(
        "let people = nodes(Person) as p; ",
        "nodes(Person) as p | join people on p.id = p.id"
    );
    let duplicate_error = database
        .run(&query(duplicate))
        .expect_err("duplicate join binding must fail validation");
    assert_eq!(
        duplicate_error.to_string(),
        "invalid argument: HashJoin: binding `p` is present in both inputs"
    );

    let non_equi = concat!(
        "let people = nodes(Person) as right; ",
        "nodes(Person) as left | join people on left.id > right.id"
    );
    let non_equi_error =
        text::parser::parse(non_equi).expect_err("non-equality join predicate must fail parsing");
    let position = non_equi.find("left.id >").expect("predicate exists") + 1;
    assert_eq!(
        non_equi_error.to_string(),
        format!(
            "invalid argument: DevonPlan text parse error at position {position}: \
             non-equi predicates belong in a following `filter` stage; offending token \"left\""
        )
    );
}

#[test]
fn join_inside_transaction_sees_uncommitted_upserts() {
    let (_directory, mut database) = database_with(&[
        "create node table Person (id Int64 primary key, name String, city_id Int64)",
        "create node table City (id Int64 primary key, name String)",
    ]);
    let join_text = concat!(
        "let cities = nodes(City) as c; ",
        "nodes(Person) as p | join cities on p.city_id = c.id | ",
        "project p.name as person, c.name as city"
    );
    let mut transaction = database.begin().expect("begin transaction");
    transaction
        .execute(&statement(
            "upsert Person values (1, \"Ada\", 10), (2, \"Grace\", 20)",
        ))
        .expect("upsert people inside transaction");
    transaction
        .execute(&statement(
            "upsert City values (10, \"London\"), (20, \"New York\")",
        ))
        .expect("upsert cities inside transaction");

    assert!(
        database
            .run(&query(join_text))
            .expect("outside snapshot runs")
            .rows
            .is_empty(),
        "uncommitted upserts must remain private"
    );
    let result = transaction
        .run(&query(join_text))
        .expect("transaction-local join runs");
    assert_eq!(result.columns, ["person", "city"]);
    assert_eq!(
        result.rows,
        vec![
            vec![string("Ada"), string("London")],
            vec![string("Grace"), string("New York")],
        ]
    );
}

fn database_with(statements: &[&str]) -> (TempDir, Database) {
    let directory = tempdir().expect("create test directory");
    let mut database =
        Database::create(directory.path().join("db.devondb"), PAGE_SIZE).expect("create database");
    for input in statements {
        database
            .execute(&statement(input))
            .unwrap_or_else(|error| panic!("execute `{input}`: {error}"));
    }
    (directory, database)
}

fn statement(input: &str) -> Statement {
    match text::parser::parse(input).unwrap_or_else(|error| panic!("parse `{input}`: {error}")) {
        Parsed::Statement(envelope) => envelope.stmt,
        Parsed::Query(_) => panic!("expected statement: {input}"),
    }
}

fn query(input: &str) -> Plan {
    match text::parser::parse(input).unwrap_or_else(|error| panic!("parse `{input}`: {error}")) {
        Parsed::Query(plan) => plan,
        Parsed::Statement(_) => panic!("expected query: {input}"),
    }
}

fn run_text(database: &mut Database, input: &str) -> QueryResult {
    database
        .run(&query(input))
        .unwrap_or_else(|error| panic!("run `{input}`: {error}"))
}

fn person_city(
    person_id: i64,
    person_name: &str,
    city_code: Value,
    city_id: i64,
    matched_code: &str,
    city_name: &str,
) -> Vec<Value> {
    vec![
        int(person_id),
        string(person_name),
        city_code,
        int(city_id),
        string(matched_code),
        string(city_name),
    ]
}

fn int(value: i64) -> Value {
    Value::Int64(value)
}

fn string(value: &str) -> Value {
    Value::String(value.to_owned())
}
