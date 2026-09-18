//! Public-facade end-to-end coverage for spend-summary and anomaly queries.

use devondb::{
    Database, Plan, QueryResult, Statement,
    text::{self, parser::Parsed},
};
use devondb_types::{Decimal128, value::Value};
use tempfile::{TempDir, tempdir};

const PAGE_SIZE: u32 = 4096;
const MICROS_PER_DAY: i64 = 86_400_000_000;

const SPEND_SUMMARY_PLAN: &str = concat!(
    "nodes(SpendDay) as d | aggregate sum(d.cost) as `daily.cost` by d.day | ",
    "filter d.day = timestamp(\"1970-04-11T00:00:00Z\") | project ",
    "daily.cost as `summary.today`, scalar(nodes(SpendDay) as w | ",
    "aggregate sum(w.cost) as `week.cost` by w.day | filter ",
    "w.day >= date_add(\"day\", timestamp(\"1970-04-11T00:00:00Z\"), -6) | ",
    "aggregate sum(week.cost) as total) as `summary.week`, ",
    "scalar(nodes(SpendDay) as m | aggregate sum(m.cost) as `month.cost` by m.day | ",
    "filter m.day >= date_add(\"day\", timestamp(\"1970-04-11T00:00:00Z\"), -29) | ",
    "aggregate sum(month.cost) as total) as `summary.month`, ",
    "scalar(nodes(SpendDay) as b | aggregate sum(b.cost) as `baseline.cost` by b.day | ",
    "filter b.day >= date_add(\"day\", timestamp(\"1970-04-11T00:00:00Z\"), -90) ",
    "and b.day < timestamp(\"1970-04-11T00:00:00Z\") | aggregate ",
    "percentile_cont(decimal(\"0.5\"), baseline.cost) as median) as `summary.baseline` | ",
    "project round(summary.today, 0) as today_cost, round(summary.week, 0) as last7d_cost, ",
    "round(summary.month, 0) as last30d_cost, round(summary.baseline, 0) as baseline_daily_cost, ",
    "round_div(summary.today, summary.baseline, 1) as today_vs_baseline"
);

const SPEND_ANOMALY_PLAN: &str = concat!(
    "nodes(SpendDay) as d | aggregate sum(d.cost) as `daily.cost` by d.day | project ",
    "d.day as `anomaly.day`, daily.cost as `anomaly.cost`, ",
    "scalar(nodes(SpendDay) as h | aggregate sum(h.cost) as `historical.cost` by h.day | ",
    "filter h.day >= date_add(\"day\", d.day, -30) and h.day < d.day | aggregate ",
    "percentile_cont(decimal(\"0.5\"), historical.cost) as median) as `anomaly.median` | ",
    "filter if(anomaly.median > decimal(\"0.00\"), ",
    "round_div(anomaly.cost, anomaly.median, 1) > decimal(\"2.00\"), false) | project ",
    "anomaly.day as day, round(anomaly.cost, 0) as cost, ",
    "round(anomaly.median, 0) as trailing_median, ",
    "round_div(anomaly.cost, anomaly.median, 1) as multiple | sort anomaly.day desc"
);

const CARDINALITY_ERROR_PLAN: &str = concat!(
    "nodes(SpendDay) as d | aggregate sum(d.cost) as `daily.cost` by d.day | ",
    "filter d.day = timestamp(\"1970-04-11T00:00:00Z\") | project ",
    "scalar(nodes(SpendDay) as b | aggregate sum(b.cost) as `baseline.cost` by b.day | ",
    "filter b.day >= date_add(\"day\", timestamp(\"1970-04-11T00:00:00Z\"), -90) ",
    "and b.day < timestamp(\"1970-04-11T00:00:00Z\") | ",
    "project baseline.cost as cost) as baseline"
);

#[test]
fn a6b_consumer_plans_have_exact_canonical_text_fixpoints() {
    for input in [
        SPEND_SUMMARY_PLAN,
        SPEND_ANOMALY_PLAN,
        CARDINALITY_ERROR_PLAN,
    ] {
        canonical_plan(input);
    }
}

#[test]
fn a6b_spend_summary_runs_the_complete_exact_decimal_shape() {
    let (_directory, mut database) = database_with(&[
        spend_day_schema(),
        concat!(
            "insert into SpendDay values ",
            "(1, timestamp(\"1970-01-10T00:00:00Z\"), decimal(\"999.00\")), ",
            "(2, timestamp(\"1970-01-11T00:00:00Z\"), decimal(\"10.00\")), ",
            "(3, timestamp(\"1970-02-20T00:00:00Z\"), decimal(\"20.00\")), ",
            "(4, timestamp(\"1970-03-12T00:00:00Z\"), decimal(\"30.00\")), ",
            "(5, timestamp(\"1970-03-13T00:00:00Z\"), decimal(\"40.00\")), ",
            "(6, timestamp(\"1970-04-05T00:00:00Z\"), decimal(\"25.25\")), ",
            "(7, timestamp(\"1970-04-05T00:00:00Z\"), decimal(\"24.75\")), ",
            "(8, timestamp(\"1970-04-10T00:00:00Z\"), decimal(\"60.00\")), ",
            "(9, timestamp(\"1970-04-11T00:00:00Z\"), decimal(\"50.25\")), ",
            "(10, timestamp(\"1970-04-11T00:00:00Z\"), decimal(\"50.25\")), ",
            "(11, timestamp(\"1970-04-12T00:00:00Z\"), decimal(\"200.00\")), ",
            "(12, null, decimal(\"5000.00\"))"
        ),
    ]);

    let result = run_text(&mut database, SPEND_SUMMARY_PLAN);

    assert_eq!(
        result.columns,
        [
            "today_cost",
            "last7d_cost",
            "last30d_cost",
            "baseline_daily_cost",
            "today_vs_baseline",
        ]
    );
    assert_eq!(
        result.rows,
        vec![vec![
            decimal("101.00"),
            decimal("411.00"),
            decimal("451.00"),
            decimal("35.00"),
            decimal("2.90"),
        ]]
    );
}

#[test]
fn a6b_spend_anomaly_correlates_the_window_and_applies_emit_conditions() {
    let (_directory, mut database) = database_with(&[
        spend_day_schema(),
        concat!(
            "insert into SpendDay values ",
            "(1, timestamp(\"1970-01-01T00:00:00Z\"), decimal(\"0.00\")), ",
            "(2, timestamp(\"1970-01-02T00:00:00Z\"), decimal(\"10.00\")), ",
            "(3, timestamp(\"1970-01-03T00:00:00Z\"), null), ",
            "(4, timestamp(\"1970-01-11T00:00:00Z\"), decimal(\"4.25\")), ",
            "(5, timestamp(\"1970-01-11T00:00:00Z\"), decimal(\"5.75\")), ",
            "(6, timestamp(\"1970-01-21T00:00:00Z\"), decimal(\"10.00\")), ",
            "(7, timestamp(\"1970-02-01T00:00:00Z\"), decimal(\"50.50\")), ",
            "(8, timestamp(\"1970-02-02T00:00:00Z\"), decimal(\"19.00\")), ",
            "(9, timestamp(\"1970-02-03T00:00:00Z\"), null), ",
            "(10, timestamp(\"1970-02-10T00:00:00Z\"), decimal(\"25.00\")), ",
            "(11, null, decimal(\"9000.00\"))"
        ),
    ]);

    let result = run_text(&mut database, SPEND_ANOMALY_PLAN);

    assert_eq!(
        result.columns,
        ["day", "cost", "trailing_median", "multiple"]
    );
    assert_eq!(
        result.rows,
        vec![vec![
            timestamp(31 * MICROS_PER_DAY),
            decimal("51.00"),
            decimal("10.00"),
            decimal("5.10"),
        ]]
    );
}

#[test]
fn a6b_empty_baseline_window_and_null_day_produce_null_finishing_values() {
    let (_directory, mut database) = database_with(&[
        spend_day_schema(),
        concat!(
            "insert into SpendDay values ",
            "(1, timestamp(\"1970-04-11T00:00:00Z\"), decimal(\"5.00\")), ",
            "(2, null, decimal(\"999.00\"))"
        ),
    ]);

    let result = run_text(&mut database, SPEND_SUMMARY_PLAN);

    assert_eq!(
        result.rows,
        vec![vec![
            decimal("5.00"),
            decimal("5.00"),
            decimal("5.00"),
            Value::Null,
            Value::Null,
        ]]
    );
}

#[test]
fn a6b_zero_baseline_refuses_decimal_division_through_the_facade() {
    let (_directory, mut database) = database_with(&[
        spend_day_schema(),
        concat!(
            "insert into SpendDay values ",
            "(1, timestamp(\"1970-04-10T00:00:00Z\"), decimal(\"0.00\")), ",
            "(2, timestamp(\"1970-04-11T00:00:00Z\"), decimal(\"5.00\"))"
        ),
    ]);

    let error = database
        .run(&canonical_plan(SPEND_SUMMARY_PLAN))
        .expect_err("a zero baseline must be refused");

    assert_eq!(
        error.to_string(),
        "invalid argument: decimal division by zero"
    );
}

#[test]
fn a6b_scalar_cardinality_error_is_reported_through_the_facade() {
    let (_directory, mut database) = database_with(&[
        spend_day_schema(),
        concat!(
            "insert into SpendDay values ",
            "(1, timestamp(\"1970-04-09T00:00:00Z\"), decimal(\"10.00\")), ",
            "(2, timestamp(\"1970-04-10T00:00:00Z\"), decimal(\"20.00\")), ",
            "(3, timestamp(\"1970-04-11T00:00:00Z\"), decimal(\"30.00\"))"
        ),
    ]);

    let error = database
        .run(&canonical_plan(CARDINALITY_ERROR_PLAN))
        .expect_err("a scalar subquery with two rows must be refused");

    assert_eq!(
        error.to_string(),
        "invalid argument: scalar subquery returned more than one row"
    );
}

fn spend_day_schema() -> &'static str {
    "create node table SpendDay (id Int64 primary key, day Timestamp, cost Decimal(12, 2))"
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

fn canonical_plan(input: &str) -> Plan {
    let plan = query(input);
    let printed = text::printer::print_plan(&plan).expect("print canonical plan");
    assert_eq!(printed, input, "canonical spelling changed");
    let reparsed = query(&printed);
    assert_eq!(
        reparsed, plan,
        "canonical text must round-trip to the same IR"
    );
    assert_eq!(
        text::printer::print_plan(&reparsed).expect("reprint canonical plan"),
        printed,
        "canonical text must be a printing fixpoint"
    );
    plan
}

fn query(input: &str) -> Plan {
    match text::parser::parse(input).unwrap_or_else(|error| panic!("parse `{input}`: {error}")) {
        Parsed::Query(plan) => plan,
        Parsed::Statement(_) => panic!("expected query: {input}"),
    }
}

fn run_text(database: &mut Database, input: &str) -> QueryResult {
    database
        .run(&canonical_plan(input))
        .unwrap_or_else(|error| panic!("run `{input}`: {error}"))
}

fn timestamp(micros: i64) -> Value {
    Value::Timestamp(micros)
}

fn decimal(value: &str) -> Value {
    Value::Decimal(value.parse::<Decimal128>().expect("valid decimal"))
}
