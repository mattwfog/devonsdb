//! Criterion baseline for batch upsert scaling. Each timed iteration
//! runs one public-facade transaction against a newly prepared database.

use criterion::{BatchSize, BenchmarkId, Criterion, criterion_group, criterion_main};
use devondb::{Database, Statement};
use devondb_types::{logical_type::LogicalType, schema::Column, value::Value};

const PAGE_SIZE: u32 = 4096;
const BATCH_SIZES: [usize; 3] = [1_000, 4_000, 16_000];
const LARGE_BATCH_SIZE: usize = 16_000;
const INSERT_BASELINE_SIZE: usize = 4_000;

struct Fixture {
    _directory: tempfile::TempDir,
    database: Database,
}

fn upsert_batch(c: &mut Criterion) {
    bench_upserts(c, "fresh_upsert", false);
    bench_upserts(c, "re_upsert", true);
    bench_insert_baseline(c);
}

fn bench_upserts(c: &mut Criterion, name: &str, seed_rows: bool) {
    let mut group = c.benchmark_group(name);
    for row_count in BATCH_SIZES {
        if row_count == LARGE_BATCH_SIZE {
            group.sample_size(10);
        }
        let statement = upsert_statement(row_count);
        group.bench_with_input(
            BenchmarkId::from_parameter(row_count),
            &statement,
            |bencher, statement| {
                bencher.iter_batched_ref(
                    || fixture(seed_rows.then_some(statement)),
                    |fixture| {
                        fixture
                            .database
                            .execute(statement)
                            .expect("execute benchmark upsert");
                    },
                    BatchSize::PerIteration,
                );
            },
        );
    }
    group.finish();
}

fn bench_insert_baseline(c: &mut Criterion) {
    let statement = insert_statement(INSERT_BASELINE_SIZE);
    let mut group = c.benchmark_group("insert_baseline");
    group.bench_with_input(
        BenchmarkId::from_parameter(INSERT_BASELINE_SIZE),
        &statement,
        |bencher, statement| {
            bencher.iter_batched_ref(
                || fixture(None),
                |fixture| {
                    fixture
                        .database
                        .execute(statement)
                        .expect("execute benchmark insert");
                },
                BatchSize::PerIteration,
            );
        },
    );
    group.finish();
}

fn fixture(initial_upsert: Option<&Statement>) -> Fixture {
    let directory = tempfile::tempdir().expect("create upsert benchmark directory");
    let path = directory.path().join("upsert.devondb");
    let mut database = Database::create(path, PAGE_SIZE).expect("create benchmark database");
    database
        .execute(&Statement::CreateNodeTable {
            name: "Records".to_owned(),
            columns: vec![
                column("key", LogicalType::String, true),
                column("ordinal", LogicalType::Int64, false),
                column("score", LogicalType::Float64, false),
            ],
        })
        .expect("create benchmark table");
    if let Some(statement) = initial_upsert {
        database
            .execute(statement)
            .expect("seed benchmark rows with upsert");
    }
    Fixture {
        _directory: directory,
        database,
    }
}

fn upsert_statement(row_count: usize) -> Statement {
    Statement::UpsertNode {
        table: "Records".to_owned(),
        rows: benchmark_rows(row_count),
    }
}

fn insert_statement(row_count: usize) -> Statement {
    Statement::InsertNode {
        table: "Records".to_owned(),
        rows: benchmark_rows(row_count),
    }
}

fn benchmark_rows(row_count: usize) -> Vec<Vec<Value>> {
    (0..row_count)
        .map(|ordinal| {
            vec![
                Value::String(format!("key-{ordinal:05}")),
                Value::Int64(ordinal as i64),
                Value::Float64(ordinal as f64 / 10.0),
            ]
        })
        .collect()
}

fn column(name: &str, ty: LogicalType, primary_key: bool) -> Column {
    Column {
        name: name.to_owned(),
        ty,
        primary_key,
    }
}

criterion_group!(benches, upsert_batch);
criterion_main!(benches);
