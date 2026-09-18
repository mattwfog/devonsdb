//! Embedded-API regressions for rejecting non-finite values before WAL append.

use std::{
    env, fs,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use devondb::{Database, DevonError, Plan, Statement};
use devondb_plan::ops::Operator;
use devondb_types::{logical_type::LogicalType, schema::Column, value::Value};

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

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
            "devondb-nonfinite-{label}-{timestamp}-{sequence}-{}",
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

#[test]
fn nan_float64_insert_is_rejected_without_poisoning_the_database() {
    let directory = TestDirectory::new("float64-nan");
    let path = directory.db_path();
    let mut database = Database::create(&path, 4096).expect("create database");
    create_table(
        &mut database,
        "Reading",
        vec![
            column("id", LogicalType::Int64, true),
            column("score", LogicalType::Float64, false),
        ],
    );
    insert_rows(
        &mut database,
        "Reading",
        vec![vec![Value::Int64(1), Value::Float64(1.25)]],
    );

    let error = database
        .execute(&Statement::InsertNode {
            table: "Reading".to_owned(),
            rows: vec![vec![Value::Int64(2), Value::Float64(f64::NAN)]],
        })
        .expect_err("NaN must be rejected at commit");
    assert_invalid_mentions(error, "Reading", "Float64");

    let expected = vec![
        vec![Value::Int64(1), Value::Float64(1.25)],
        vec![Value::Int64(2), Value::Float64(2.5)],
    ];
    insert_rows(&mut database, "Reading", vec![expected[1].clone()]);
    assert_eq!(scan_rows(&mut database, "Reading"), expected);
    drop(database);

    let mut reopened = Database::open(&path).expect("reopen after rejected NaN");
    assert_eq!(scan_rows(&mut reopened, "Reading"), expected);
}

#[test]
fn nan_vector_insert_is_rejected_without_poisoning_the_database() {
    let directory = TestDirectory::new("vector-nan");
    let path = directory.db_path();
    let mut database = Database::create(&path, 4096).expect("create database");
    create_table(
        &mut database,
        "Embedding",
        vec![
            column("id", LogicalType::Int64, true),
            column("value", LogicalType::Vector { dim: 2 }, false),
        ],
    );
    insert_rows(
        &mut database,
        "Embedding",
        vec![vec![Value::Int64(1), Value::Vector(vec![0.25, 0.75])]],
    );

    let error = database
        .execute(&Statement::InsertNode {
            table: "Embedding".to_owned(),
            rows: vec![vec![Value::Int64(2), Value::Vector(vec![1.0, f32::NAN])]],
        })
        .expect_err("a NaN vector element must be rejected at commit");
    assert_invalid_mentions(error, "Embedding", "Vector");

    let expected = vec![
        vec![Value::Int64(1), Value::Vector(vec![0.25, 0.75])],
        vec![Value::Int64(2), Value::Vector(vec![0.5, -0.5])],
    ];
    insert_rows(&mut database, "Embedding", vec![expected[1].clone()]);
    assert_eq!(scan_rows(&mut database, "Embedding"), expected);
    drop(database);

    let mut reopened = Database::open(&path).expect("reopen after rejected NaN vector");
    assert_eq!(scan_rows(&mut reopened, "Embedding"), expected);
}

#[test]
fn infinities_are_rejected_and_finite_data_survives_reopen() {
    let directory = TestDirectory::new("infinities");
    let path = directory.db_path();
    let mut database = Database::create(&path, 4096).expect("create database");
    create_table(
        &mut database,
        "Measurement",
        vec![
            column("id", LogicalType::Int64, true),
            column("score", LogicalType::Float64, false),
            column("embedding", LogicalType::Vector { dim: 2 }, false),
        ],
    );
    let finite = vec![
        Value::Int64(1),
        Value::Float64(4.5),
        Value::Vector(vec![1.0, -1.0]),
    ];
    insert_rows(&mut database, "Measurement", vec![finite.clone()]);

    let float_error = database
        .execute(&Statement::InsertNode {
            table: "Measurement".to_owned(),
            rows: vec![vec![
                Value::Int64(2),
                Value::Float64(f64::INFINITY),
                Value::Vector(vec![0.0, 1.0]),
            ]],
        })
        .expect_err("positive infinity must be rejected");
    assert_invalid_mentions(float_error, "Measurement", "Float64");

    let vector_error = database
        .execute(&Statement::InsertNode {
            table: "Measurement".to_owned(),
            rows: vec![vec![
                Value::Int64(2),
                Value::Float64(1.0),
                Value::Vector(vec![f32::NEG_INFINITY, 1.0]),
            ]],
        })
        .expect_err("negative infinity must be rejected");
    assert_invalid_mentions(vector_error, "Measurement", "Vector");

    assert_eq!(
        scan_rows(&mut database, "Measurement"),
        vec![finite.clone()]
    );
    drop(database);

    let mut reopened = Database::open(&path).expect("reopen after rejected infinities");
    assert_eq!(scan_rows(&mut reopened, "Measurement"), vec![finite]);
}

fn create_table(database: &mut Database, name: &str, columns: Vec<Column>) {
    database
        .execute(&Statement::CreateNodeTable {
            name: name.to_owned(),
            columns,
        })
        .expect("create node table");
}

fn insert_rows(database: &mut Database, table: &str, rows: Vec<Vec<Value>>) {
    database
        .execute(&Statement::InsertNode {
            table: table.to_owned(),
            rows,
        })
        .expect("insert finite rows");
}

fn scan_rows(database: &mut Database, table: &str) -> Vec<Vec<Value>> {
    database
        .run(&Plan {
            v: 0,
            plan: Operator::ScanNodes {
                table: table.to_owned(),
                binding: "n".to_owned(),
            },
        })
        .expect("scan table")
        .rows
}

fn column(name: &str, ty: LogicalType, primary_key: bool) -> Column {
    Column {
        name: name.to_owned(),
        ty,
        primary_key,
    }
}

fn assert_invalid_mentions(error: DevonError, table: &str, value_kind: &str) {
    let DevonError::InvalidArgument { context } = error else {
        panic!("expected InvalidArgument, got {error}");
    };
    assert!(context.contains(table));
    assert!(context.contains(value_kind));
}
