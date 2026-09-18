//! Public-facade end-to-end coverage for conditional, date, and decimal expressions.

use devondb::{
    Database, Plan, QueryResult, Statement,
    text::{self, parser::Parsed},
};
use devondb_types::{Decimal128, value::Value};
use tempfile::{TempDir, tempdir};

const PAGE_SIZE: u32 = 4096;
const MICROS_PER_DAY: i64 = 86_400_000_000;

#[test]
fn a6_projection_composes_every_family_and_nests_lazy_selection_in_arithmetic() {
    let (_directory, mut database) = database_with(&[
        concat!(
            "create node table Reading (id Int64 primary key, enabled Bool, ",
            "primary_value Int64, fallback_value Int64, candidate Int64, happened Timestamp)"
        ),
        concat!(
            "insert into Reading values ",
            "(1, true, 10, 20, 5, timestamp(\"1970-01-02T12:30:00Z\")), ",
            "(2, false, 30, 7, null, timestamp(\"1969-12-31T23:59:59.999999Z\")), ",
            "(3, null, 40, null, 50, null)"
        ),
    ]);
    let result = run_text(
        &mut database,
        concat!(
            "nodes(Reading) as r | project ",
            "coalesce(if(r.enabled, r.primary_value, null), r.fallback_value) + 5 as nested, ",
            "least(r.primary_value, r.fallback_value, r.candidate) as minimum, ",
            "greatest(r.primary_value, r.fallback_value, r.candidate) as maximum, ",
            "date_trunc(\"day\", r.happened) as day"
        ),
    );

    assert_eq!(result.columns, ["nested", "minimum", "maximum", "day"]);
    assert_eq!(
        result.rows,
        vec![
            vec![int(15), int(5), int(20), timestamp(MICROS_PER_DAY)],
            vec![int(12), int(7), int(30), timestamp(-MICROS_PER_DAY)],
            vec![Value::Null, int(40), int(50), Value::Null],
        ]
    );
}

#[test]
fn a6_filter_accepts_an_if_coalesce_predicate() {
    let (_directory, mut database) = database_with(&[
        concat!(
            "create node table Gate (id Int64 primary key, enabled Bool, ",
            "primary_ok Bool, backup_ok Bool)"
        ),
        concat!(
            "insert into Gate values (1, true, null, true), (2, false, true, true), ",
            "(3, null, true, true), (4, true, false, true), (5, true, null, null)"
        ),
    ]);
    let result = run_text(
        &mut database,
        concat!(
            "nodes(Gate) as g | ",
            "filter if(g.enabled, coalesce(g.primary_ok, g.backup_ok), false) | ",
            "project g.id as id"
        ),
    );

    assert_eq!(result.columns, ["id"]);
    assert_eq!(result.rows, vec![vec![int(1)]]);
}

#[test]
fn a6_date_trunc_groups_three_sorted_utc_days_including_pre_epoch() {
    let (_directory, mut database) = database_with(&[
        "create node table Event (id Int64 primary key, happened Timestamp)",
        concat!(
            "insert into Event values ",
            "(1, timestamp(\"1969-12-31T23:59:59.999999Z\")), ",
            "(2, timestamp(\"1970-01-01T00:00:00Z\")), ",
            "(3, timestamp(\"1970-01-01T23:59:59.999999Z\")), ",
            "(4, timestamp(\"1970-01-02T00:00:00Z\")), ",
            "(5, timestamp(\"1970-01-02T18:00:00Z\"))"
        ),
    ]);
    let result = run_text(
        &mut database,
        concat!(
            "nodes(Event) as e | project ",
            "date_trunc(\"day\", e.happened) as `bucket.day`, e.id | ",
            "aggregate count(e.id) as event_count by bucket.day | sort bucket.day"
        ),
    );

    assert_eq!(result.columns, ["bucket.day", "event_count"]);
    assert_eq!(
        result.rows,
        vec![
            vec![timestamp(-MICROS_PER_DAY), int(1)],
            vec![timestamp(0), int(2)],
            vec![timestamp(MICROS_PER_DAY), int(2)],
        ]
    );
}

#[test]
fn a6_decimal_extrema_keep_exact_scale_skip_nulls_and_return_null_when_all_null() {
    let (_directory, mut database) = database_with(&[
        concat!(
            "create node table Quote (id Int64 primary key, ",
            "first Decimal(10, 2), second Decimal(10, 2), third Decimal(10, 2))"
        ),
        concat!(
            "insert into Quote values ",
            "(1, decimal(\"12.50\"), decimal(\"7.25\"), decimal(\"19.00\")), ",
            "(2, null, decimal(\"5.50\"), decimal(\"9.75\")), ",
            "(3, null, null, null)"
        ),
    ]);
    let result = run_text(
        &mut database,
        concat!(
            "nodes(Quote) as q | project q.id as id, ",
            "least(q.first, q.second, q.third) as minimum, ",
            "greatest(q.first, q.second, q.third) as maximum"
        ),
    );

    assert_eq!(result.columns, ["id", "minimum", "maximum"]);
    assert_eq!(
        result.rows,
        vec![
            vec![int(1), decimal("7.25"), decimal("19.00")],
            vec![int(2), decimal("5.50"), decimal("9.75")],
            vec![int(3), Value::Null, Value::Null],
        ]
    );
}

#[test]
fn a6_decimal_nullif_shape_runs_through_the_public_text_path() {
    let (_directory, mut database) = database_with(&[
        concat!(
            "create node table Spend (id Int64 primary key, ",
            "a Decimal(10, 2), b Decimal(12, 2))"
        ),
        concat!(
            "insert into Spend values ",
            "(1, decimal(\"12.50\"), decimal(\"12.50\")), ",
            "(2, decimal(\"12.50\"), decimal(\"9.75\")), ",
            "(3, null, decimal(\"1.00\")), ",
            "(4, decimal(\"2.00\"), null), (5, null, null)"
        ),
    ]);
    let result = run_text(
        &mut database,
        concat!(
            "nodes(Spend) as s | project s.id as id, ",
            "if(s.a = s.b, null, s.a) as nullif_amount"
        ),
    );

    assert_eq!(result.columns, ["id", "nullif_amount"]);
    assert_eq!(
        result.rows,
        vec![
            vec![int(1), Value::Null],
            vec![int(2), decimal("12.50")],
            vec![int(3), Value::Null],
            vec![int(4), decimal("2.00")],
            vec![int(5), Value::Null],
        ]
    );
}

#[test]
fn a6_if_and_coalesce_do_not_evaluate_division_by_zero_in_untaken_branches() {
    let (_directory, mut database) = database_with(&[
        "create node table Choice (id Int64 primary key, selected Bool, value Int64)",
        "insert into Choice values (1, true, 10), (2, false, 20)",
    ]);
    let result = run_text(
        &mut database,
        concat!(
            "nodes(Choice) as c | project c.id as id, ",
            "if(c.selected, c.value, 1 / if(c.selected, 0, 1)) as lazy_if, ",
            "coalesce(if(c.selected, c.value, null), 1 / if(c.selected, 0, 1)) ",
            "as lazy_coalesce"
        ),
    );

    assert_eq!(result.columns, ["id", "lazy_if", "lazy_coalesce"]);
    assert_eq!(
        result.rows,
        vec![vec![int(1), int(10), int(10)], vec![int(2), int(1), int(1)]]
    );
}

#[test]
fn a6_refusal_messages_are_exact_for_decimal_avg_date_unit_and_if_arity() {
    let (_directory, mut database) = database_with(&[
        concat!(
            "create node table Measurement (id Int64 primary key, ",
            "amount Decimal(10, 2), happened Timestamp)"
        ),
        concat!(
            "insert into Measurement values ",
            "(1, decimal(\"1.25\"), timestamp(\"1970-01-01T00:00:00Z\"))"
        ),
    ]);

    let avg_error = database
        .run(&query(
            "nodes(Measurement) as m | aggregate avg(m.amount) as mean",
        ))
        .expect_err("Decimal avg must be refused");
    assert_eq!(
        avg_error.to_string(),
        "invalid argument: Aggregate: avg over Decimal would round; compute sum and count and divide consumer-side"
    );

    let bad_unit = "nodes(Measurement) as m | project date_trunc(\"hour\", m.happened) as bucket";
    let bad_unit_position = bad_unit.find("\"hour\"").expect("unit is present") + 1;
    assert_eq!(
        parse_error(bad_unit),
        format!(
            "invalid argument: DevonPlan text parse error at position {bad_unit_position}: \
             unknown `date_trunc` unit `hour`; accepted unit is `day`; offending token \"\\\"hour\\\"\""
        )
    );

    let bad_if = "nodes(Measurement) as m | project if(true, 1) as broken";
    let bad_if_position = bad_if.find(") as broken").expect("if close is present") + 1;
    assert_eq!(
        parse_error(bad_if),
        format!(
            "invalid argument: DevonPlan text parse error at position {bad_if_position}: \
             `if` has wrong arity: expected exactly 3 arguments, got 2; offending token \")\""
        )
    );
}

#[test]
fn a6_join_projects_coalesce_then_groups_and_sorts_by_utc_day() {
    let (_directory, mut database) = database_with(&[
        concat!(
            "create node table Ledger (id Int64 primary key, match_key String, ",
            "happened Timestamp, x Int64)"
        ),
        "create node table Price (id Int64 primary key, match_key String, y Int64)",
        concat!(
            "insert into Ledger values ",
            "(1, \"A\", timestamp(\"1969-12-31T23:00:00Z\"), null), ",
            "(2, \"B\", timestamp(\"1970-01-01T01:00:00Z\"), 3), ",
            "(3, \"C\", timestamp(\"1970-01-01T20:00:00Z\"), null), ",
            "(4, \"D\", timestamp(\"1970-01-02T12:00:00Z\"), 7)"
        ),
        concat!(
            "insert into Price values ",
            "(10, \"A\", 2), (20, \"B\", 30), (30, \"C\", 5), (40, \"D\", 70)"
        ),
    ]);
    let result = run_text(
        &mut database,
        concat!(
            "let prices = nodes(Price) as r; ",
            "nodes(Ledger) as l | join prices on l.match_key = r.match_key | ",
            "project coalesce(l.x, r.y) as `metric.value`, ",
            "date_trunc(\"day\", l.happened) as `bucket.day` | ",
            "aggregate sum(metric.value) as total by bucket.day | sort bucket.day"
        ),
    );

    assert_eq!(result.columns, ["bucket.day", "total"]);
    assert_eq!(
        result.rows,
        vec![
            vec![timestamp(-MICROS_PER_DAY), int(2)],
            vec![timestamp(0), int(8)],
            vec![timestamp(MICROS_PER_DAY), int(7)],
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

fn parse_error(input: &str) -> String {
    text::parser::parse(input)
        .expect_err("input must be refused")
        .to_string()
}

fn int(value: i64) -> Value {
    Value::Int64(value)
}

fn timestamp(micros: i64) -> Value {
    Value::Timestamp(micros)
}

fn decimal(value: &str) -> Value {
    Value::Decimal(value.parse::<Decimal128>().expect("valid decimal"))
}
