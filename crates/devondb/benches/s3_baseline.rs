//! Criterion typed-chunk baseline (`docs/SCALE.md` §5/§6.1). Every timed
//! read uses the public `Database` facade over the same 100k-row corpus shape
//! as the scan baseline; copy loads a generated CSV through `CsvReader`.

use std::fs;
use std::hint::black_box;
use std::path::PathBuf;

use criterion::{Criterion, criterion_group, criterion_main};
use devondb::{Database, Plan};
use devondb_plan::expr::Metric;
use devondb_plan::ops::KnnMode;
use devondb_types::logical_type::LogicalType;
use devondb_types::schema::Column;
use devondb_types::value::Value;

const PAGE_SIZE: u32 = 4096;
const ROW_COUNT: usize = 100_000;
const RIGHT_ROW_COUNT: usize = 10_000;
const GROUP_COUNT: usize = 100;
const VECTOR_DIMENSION: usize = 64;
const SELECTIVE_ROW_COUNT: usize = 1_000;

struct Fixture {
    _directory: tempfile::TempDir,
    csv_path: PathBuf,
    database: Database,
}

fn s3_baseline(c: &mut Criterion) {
    let mut fixture = fixture();
    let filter_1_percent = query("nodes(Records) as r | filter r.id < 1000 | project r.id");
    let filter_half = query("nodes(Records) as r | filter r.group_key < 50 | project r.id");
    let projection = query("nodes(Records) as r | project r.value_0");
    let aggregate_count = query(
        "nodes(Records) as r | aggregate count(r.id) as row_count by r.group_key | \
         project r.group_key",
    );
    let aggregate_sum = query(
        "nodes(Records) as r | aggregate sum(r.amount) as amount_sum by r.group_key | \
         project r.group_key",
    );
    let aggregate_avg = query(
        "nodes(Records) as r | aggregate avg(r.score) as score_avg by r.group_key | \
         project r.group_key",
    );
    let hash_join = query(
        "let right = nodes(Keys) as k; nodes(Records) as r | \
         join right on r.join_key = k.join_code | project r.id",
    );
    let knn_exact = knn_plan();

    bench_query(
        c,
        &mut fixture.database,
        "filter_1_percent",
        &filter_1_percent,
        SELECTIVE_ROW_COUNT,
    );
    bench_query(
        c,
        &mut fixture.database,
        "filter_50_percent",
        &filter_half,
        ROW_COUNT / 2,
    );
    bench_query(
        c,
        &mut fixture.database,
        "projection_1_of_8",
        &projection,
        ROW_COUNT,
    );
    bench_query(
        c,
        &mut fixture.database,
        "aggregate_count_by_100",
        &aggregate_count,
        GROUP_COUNT,
    );
    bench_query(
        c,
        &mut fixture.database,
        "aggregate_sum_by_100",
        &aggregate_sum,
        GROUP_COUNT,
    );
    bench_query(
        c,
        &mut fixture.database,
        "aggregate_avg_by_100",
        &aggregate_avg,
        GROUP_COUNT,
    );
    bench_query(
        c,
        &mut fixture.database,
        "hash_join_100k_x_10k",
        &hash_join,
        RIGHT_ROW_COUNT,
    );
    bench_query(c, &mut fixture.database, "knn_exact_k10", &knn_exact, 10);

    c.bench_function("copy_csv_100k_rows", |bencher| {
        bencher.iter_batched_ref(
            || fresh_copy_database(&fixture._directory),
            |database| {
                load_copy(database, &fixture.csv_path);
            },
            criterion::BatchSize::PerIteration,
        );
    });
}

fn fixture() -> Fixture {
    let directory = tempfile::tempdir().expect("create S3 benchmark directory");
    let csv_path = write_csv(&directory);
    let mut database = Database::create(directory.path().join("s3.devondb"), PAGE_SIZE)
        .expect("create S3 benchmark database");
    create_tables(&mut database);
    insert_rows(&mut database);
    database.checkpoint().expect("checkpoint S3 benchmark rows");

    Fixture {
        _directory: directory,
        csv_path,
        database,
    }
}

fn create_tables(database: &mut Database) {
    database
        .execute(&devondb::Statement::CreateNodeTable {
            name: "Records".to_owned(),
            columns: record_columns(),
        })
        .expect("create S3 benchmark records table");
    database
        .execute(&devondb::Statement::CreateNodeTable {
            name: "Keys".to_owned(),
            columns: vec![
                column("id", LogicalType::Int64, true),
                column("join_code", LogicalType::Int64, false),
            ],
        })
        .expect("create S3 benchmark keys table");
}

fn insert_rows(database: &mut Database) {
    let records = (0..ROW_COUNT)
        .map(|id| {
            vec![
                Value::Int64(id as i64),
                Value::Int64((id % GROUP_COUNT) as i64),
                Value::Float64(id as f64),
                Value::Float64(id as f64 / 100.0),
                Value::Int64((id % RIGHT_ROW_COUNT) as i64),
                record_value(id),
                Value::String(format!("value-{id}")),
                Value::Bool(id % 2 == 0),
            ]
        })
        .collect();
    let keys = (0..RIGHT_ROW_COUNT)
        .map(|id| vec![Value::Int64(id as i64), Value::Int64((id * 10) as i64)])
        .collect();
    insert(database, "Records", records);
    insert(database, "Keys", keys);
}

fn record_columns() -> Vec<Column> {
    vec![
        column("id", LogicalType::Int64, true),
        column("group_key", LogicalType::Int64, false),
        column("amount", LogicalType::Float64, false),
        column("score", LogicalType::Float64, false),
        column("join_key", LogicalType::Int64, false),
        column("embedding", vector_type(), false),
        column("value_0", LogicalType::String, false),
        column("padding", LogicalType::Bool, false),
    ]
}

fn record_value(id: usize) -> Value {
    Value::Vector(vector(id))
}

fn vector(id: usize) -> Vec<f32> {
    (0..VECTOR_DIMENSION)
        .map(|dimension| ((id + dimension) % 17) as f32 / 17.0)
        .collect()
}

fn knn_plan() -> Plan {
    devondb_plan::ops::Plan {
        v: 0,
        plan: devondb_plan::ops::Operator::KnnScan {
            table: "Records".to_owned(),
            column: "embedding".to_owned(),
            query: vector(0).into(),
            k: 10,
            metric: Metric::L2,
            mode: KnnMode::Exact,
        },
    }
}

fn write_csv(directory: &tempfile::TempDir) -> PathBuf {
    let path = directory.path().join("records.csv");
    let mut csv = String::from("id,group_key,amount,score,join_key,embedding,value_0,padding\n");
    for id in 0..ROW_COUNT {
        let values = [
            id.to_string(),
            (id % GROUP_COUNT).to_string(),
            format!("{:.1}", id as f64),
            format!("{:.3}", id as f64 / 100.0),
            (id % RIGHT_ROW_COUNT).to_string(),
            format!(
                "\"{}\"",
                serde_json::to_string(&vector(id)).expect("serialize vector")
            ),
            format!("value-{id}"),
            (id % 2 == 0).to_string(),
        ];
        csv.push_str(&values.join(","));
        csv.push('\n');
    }
    fs::write(&path, csv).expect("write S3 benchmark CSV");
    path
}

fn fresh_copy_database(directory: &tempfile::TempDir) -> Database {
    let sequence = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("valid system time")
        .as_nanos();
    let path = directory.path().join(format!("copy-{sequence}.devondb"));
    let mut database = Database::create(path, PAGE_SIZE).expect("create copy database");
    database
        .execute(&devondb::Statement::CreateNodeTable {
            name: "Copied".to_owned(),
            columns: record_columns(),
        })
        .expect("create copy table");
    database
}

fn load_copy(database: &mut Database, path: &std::path::Path) {
    let statement = match devondb_plan::text::parser::parse(&format!(
        "copy Copied from \"{}\"",
        path.display()
    ))
    .expect("parse benchmark copy")
    {
        devondb_plan::text::parser::Parsed::Statement(envelope) => envelope.stmt,
        devondb_plan::text::parser::Parsed::Query(_) => panic!("expected copy statement"),
    };
    database
        .execute(&statement)
        .expect("execute benchmark copy");
}

fn insert(database: &mut Database, table: &str, rows: Vec<Vec<Value>>) {
    database
        .execute(&devondb::Statement::InsertNode {
            table: table.to_owned(),
            rows,
        })
        .expect("insert S3 benchmark rows");
}

fn column(name: &str, ty: LogicalType, primary_key: bool) -> Column {
    Column {
        name: name.to_owned(),
        ty,
        primary_key,
    }
}

fn vector_type() -> LogicalType {
    LogicalType::Vector {
        dim: VECTOR_DIMENSION as u32,
    }
}

fn query(text: &str) -> Plan {
    match devondb_plan::text::parser::parse(text).expect("parse S3 benchmark query") {
        devondb_plan::text::parser::Parsed::Query(plan) => plan,
        devondb_plan::text::parser::Parsed::Statement(_) => panic!("expected query: {text}"),
    }
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
        .expect("validate S3 benchmark query")
        .rows
        .len();
    assert_eq!(actual_rows, expected_rows, "wrong row count for {name}");
    c.bench_function(name, |bencher| {
        bencher.iter(|| {
            let row_count = database
                .run(plan)
                .expect("run S3 benchmark query")
                .rows
                .len();
            black_box(row_count);
        });
    });
}

criterion_group!(benches, s3_baseline);
criterion_main!(benches);
