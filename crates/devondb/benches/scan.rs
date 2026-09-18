//! Criterion scan baseline (`docs/SCALE.md` §5.6): full scan, zone-map-pruned
//! selective scan, unpruned selective scan, and projection through the real
//! `Database` read path. The numbers quantify the typed-chunk tradeoffs.

use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};
use devondb::{Database, Plan, Statement};
use devondb_types::{logical_type::LogicalType, schema::Column, value::Value};

const PAGE_SIZE: u32 = 4096;
const ROW_COUNT: usize = 100_000;
const SELECTIVE_ROW_COUNT: usize = 1_000;
const PRUNED_START: i64 = 4_096;
const PRUNED_END: i64 = PRUNED_START + SELECTIVE_ROW_COUNT as i64;
const SHUFFLE_MULTIPLIER: usize = 7_919;

fn scan_baseline(c: &mut Criterion) {
    let directory = tempfile::tempdir().expect("create scan benchmark directory");
    let path = directory.path().join("scan.devondb");
    let mut database = fixture(&path);

    let full_scan = query("nodes(T) as t | project t.id");
    let pruned_scan = query(&format!(
        "nodes(T) as t | filter t.id >= {PRUNED_START} and t.id < {PRUNED_END} | project t.id"
    ));
    let unpruned_filter = query("nodes(T) as t | filter t.name = \"selected\" | project t.id");
    let projection = query("nodes(T) as t | project t.score");
    // This §6.7 pair compares a full scan materializing every column with a
    // single-column projection. The §5.6 `full_scan` above projects one
    // column, so it cannot show the projection advantage the typed scan
    // path provides; this one can.
    let full_scan_all_columns = query("nodes(T) as t | project t.id, t.score, t.name");

    bench_query(c, &mut database, "full_scan", &full_scan, ROW_COUNT);
    bench_query(
        c,
        &mut database,
        "full_scan_all_columns",
        &full_scan_all_columns,
        ROW_COUNT,
    );
    bench_query(
        c,
        &mut database,
        "pruned_scan",
        &pruned_scan,
        SELECTIVE_ROW_COUNT,
    );
    bench_query(
        c,
        &mut database,
        "unpruned_filter",
        &unpruned_filter,
        SELECTIVE_ROW_COUNT,
    );
    bench_query(c, &mut database, "projection", &projection, ROW_COUNT);
}

fn fixture(path: &std::path::Path) -> Database {
    let mut database = Database::create(path, PAGE_SIZE).expect("create scan benchmark database");
    database
        .execute(&Statement::CreateNodeTable {
            name: "T".to_owned(),
            columns: vec![
                column("id", LogicalType::Int64, true),
                column("score", LogicalType::Float64, false),
                column("name", LogicalType::String, false),
            ],
        })
        .expect("create scan benchmark table");
    database
        .execute(&Statement::InsertNode {
            table: "T".to_owned(),
            rows: (0..ROW_COUNT).map(fixture_row).collect(),
        })
        .expect("insert scan benchmark rows");
    database
        .checkpoint()
        .expect("checkpoint scan benchmark rows");
    database
}

fn fixture_row(id: usize) -> Vec<Value> {
    let shuffled = (id * SHUFFLE_MULTIPLIER) % ROW_COUNT;
    let name = if shuffled < SELECTIVE_ROW_COUNT {
        "selected"
    } else {
        "other"
    };
    vec![
        Value::Int64(id as i64),
        Value::Float64(id as f64 / 10.0),
        Value::String(name.to_owned()),
    ]
}

fn column(name: &str, ty: LogicalType, primary_key: bool) -> Column {
    Column {
        name: name.to_owned(),
        ty,
        primary_key,
    }
}

fn query(text: &str) -> Plan {
    let devondb::text::parser::Parsed::Query(plan) =
        devondb::text::parser::parse(text).expect("parse scan benchmark query")
    else {
        panic!("expected query plan: {text}");
    };
    plan
}

fn bench_query(
    c: &mut Criterion,
    database: &mut Database,
    name: &str,
    plan: &Plan,
    expected_rows: usize,
) {
    let actual_rows = database
        .run(plan)
        .expect("validate scan benchmark query")
        .rows
        .len();
    assert_eq!(actual_rows, expected_rows, "wrong row count for {name}");

    c.bench_function(name, |b| {
        b.iter(|| {
            let row_count = database
                .run(plan)
                .expect("run scan benchmark query")
                .rows
                .len();
            black_box(row_count);
        });
    });
}

criterion_group!(benches, scan_baseline);
criterion_main!(benches);
