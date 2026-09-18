//! Persisted scalar-v2 node columns (`SCALAR_TYPES_V2`, FORMAT bit 7).

use std::{
    env,
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::Duration,
};

use devondb::{Database, Plan, Statement};
use devondb_storage::{pager::Pager, superblock::SCALAR_TYPES_V2_FLAG};
use devondb_types::{
    Decimal128,
    logical_type::LogicalType,
    schema::{Column, RelTableSchema},
    value::Value,
};
use tempfile::tempdir;

const PAGE_SIZE: u32 = 4096;
const CRASH_CHILD_ENV: &str = "DEVONDB_SCALAR_V2_CRASH_CHILD";
const CRASH_PATH_ENV: &str = "DEVONDB_SCALAR_V2_CRASH_PATH";

fn scalar_types() -> [LogicalType; 4] {
    [
        LogicalType::Timestamp,
        LogicalType::Bytes,
        LogicalType::Decimal {
            precision: 38,
            scale: 0,
        },
        LogicalType::Json,
    ]
}

fn column(name: &str, ty: LogicalType, primary_key: bool) -> Column {
    Column {
        name: name.to_owned(),
        ty,
        primary_key,
    }
}

#[test]
fn relationship_properties_reject_scalar_v2_types() {
    for ty in scalar_types() {
        let error = RelTableSchema::new(
            "Carries".to_owned(),
            "Scalar".to_owned(),
            "Scalar".to_owned(),
            vec![column("value", ty, false)],
        )
        .expect_err("scalar-v2 relationship properties must stay rejected");
        assert!(
            error
                .to_string()
                .contains("relationship properties are not supported"),
            "{error}"
        );
    }
}

#[test]
fn scalar_columns_roundtrip() {
    let directory = tempdir().expect("temporary directory");
    let path = directory.path().join("roundtrip.devondb");
    let mut database = Database::create(&path, PAGE_SIZE).expect("create database");
    database
        .execute(&scalar_schema())
        .expect("scalar-v2 node DDL is live");
    database
        .execute(&scalar_insert())
        .expect("insert scalar-v2 rows");
    database.checkpoint().expect("checkpoint scalar-v2 rows");
    drop(database);

    let pager = Pager::open(&path).expect("open below facade");
    let flags = pager.superblock().feature_flags;
    assert_eq!(
        flags & SCALAR_TYPES_V2_FLAG,
        SCALAR_TYPES_V2_FLAG,
        "feature bit 7 must be set on a scalar-v2 file (flags {flags:#x})"
    );
    drop(pager);

    let mut reopened = Database::open(&path).expect("reopen scalar-v2 database");
    assert_eq!(scan_rows(&mut reopened), scalar_rows());
}

#[test]
fn database_without_scalar_columns_keeps_feature_bit_clear() {
    let directory = tempdir().expect("temporary directory");
    let path = directory.path().join("plain.devondb");
    let mut database = Database::create(&path, PAGE_SIZE).expect("create database");
    database
        .execute(&Statement::CreateNodeTable {
            name: "Plain".to_owned(),
            columns: vec![column("id", LogicalType::Int64, true)],
        })
        .expect("create plain table");
    database.checkpoint().expect("checkpoint plain catalog");
    drop(database);

    let pager = Pager::open(path).expect("open below facade");
    assert_eq!(pager.superblock().feature_flags & SCALAR_TYPES_V2_FLAG, 0);
}

#[test]
fn kill_after_scalar_insert_ack_recovers_rows() {
    let directory = tempdir().expect("temporary directory");
    let path = directory.path().join("recovery.devondb");
    let mut database = Database::create(&path, PAGE_SIZE).expect("create database");
    database.execute(&scalar_schema()).expect("create schema");
    database.checkpoint().expect("publish scalar schema");
    drop(database);

    let mut child = spawn_crash_child(&path);
    let mut stdout = BufReader::new(child.stdout.take().expect("child stdout"));
    read_until_ack(&mut stdout);
    child.kill().expect("SIGKILL scalar child");
    assert!(!child.wait().expect("wait for scalar child").success());
    assert_child_stderr_empty(&mut child);

    let mut recovered = Database::open(&path).expect("recover scalar-v2 WAL");
    assert_eq!(scan_rows(&mut recovered), scalar_rows());
    recovered.checkpoint().expect("checkpoint recovered rows");
    drop(recovered);
    let mut reopened = Database::open(&path).expect("reopen recovered scalar rows");
    assert_eq!(scan_rows(&mut reopened), scalar_rows());
}

#[test]
fn scalar_insert_crash_child_process() {
    if env::var_os(CRASH_CHILD_ENV).is_none() {
        return;
    }
    let path = PathBuf::from(env::var_os(CRASH_PATH_ENV).expect("child database path"));
    let mut database = Database::open(path).expect("child opens database");
    database
        .execute(&scalar_insert())
        .expect("child inserts scalar rows");
    println!("SCALAR_V2_ACK");
    std::io::stdout().flush().expect("flush scalar child ACK");
    thread::sleep(Duration::from_secs(60));
}

fn scalar_schema() -> Statement {
    Statement::CreateNodeTable {
        name: "Scalar".to_owned(),
        columns: vec![
            column("id", LogicalType::Int64, true),
            column("created_at", LogicalType::Timestamp, false),
            column("payload", LogicalType::Bytes, false),
            column(
                "amount",
                LogicalType::Decimal {
                    precision: 38,
                    scale: 0,
                },
                false,
            ),
            column("document", LogicalType::Json, false),
        ],
    }
}

fn scalar_insert() -> Statement {
    Statement::InsertNode {
        table: "Scalar".to_owned(),
        rows: scalar_rows(),
    }
}

fn scalar_rows() -> Vec<Vec<Value>> {
    let extreme = 10_i128.pow(38) - 1;
    vec![
        vec![
            Value::Int64(1),
            Value::Timestamp(0),
            Value::Bytes(Vec::new()),
            decimal(extreme),
            Value::Json(r#"{"outer":{"items":[1,true,null],"empty":{}}}"#.to_owned()),
        ],
        vec![
            Value::Int64(2),
            Value::Timestamp(i64::MIN),
            Value::Bytes(vec![0, 0xff, 0x80, b'd', b'b']),
            decimal(-extreme),
            Value::Json(r#"["nested",{"depth":2}]"#.to_owned()),
        ],
        vec![
            Value::Int64(3),
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
        ],
    ]
}

fn decimal(digits: i128) -> Value {
    Value::Decimal(Decimal128::new(digits, 0).expect("valid scale"))
}

fn scan_rows(database: &mut Database) -> Vec<Vec<Value>> {
    database
        .run(
            &Plan::from_json(
                r#"{"v":0,"plan":{"op":"ScanNodes","table":"Scalar","binding":"row"}}"#,
            )
            .expect("scalar scan plan"),
        )
        .expect("scan scalar rows")
        .rows
}

fn spawn_crash_child(path: &Path) -> Child {
    Command::new(env::current_exe().expect("current test executable"))
        .args([
            "--exact",
            "scalar_insert_crash_child_process",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CRASH_CHILD_ENV, "1")
        .env(CRASH_PATH_ENV, path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn scalar crash child")
}

fn read_until_ack(stdout: &mut BufReader<impl Read>) {
    loop {
        let mut line = String::new();
        assert_ne!(
            stdout
                .read_line(&mut line)
                .expect("read scalar child output"),
            0,
            "scalar child exited before acknowledgement"
        );
        if line.contains("SCALAR_V2_ACK") {
            return;
        }
    }
}

fn assert_child_stderr_empty(child: &mut Child) {
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .expect("child stderr")
        .read_to_string(&mut stderr)
        .expect("read scalar child stderr");
    assert!(
        stderr.is_empty(),
        "unexpected scalar child stderr: {stderr}"
    );
}
