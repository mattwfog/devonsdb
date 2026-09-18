//! Facade-level insert-or-replace coverage for MVCC, conflicts, checkpoints,
//! and crash recovery.

use std::{
    env,
    ffi::OsString,
    fs,
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::Duration,
    time::{SystemTime, UNIX_EPOCH},
};

use devondb::{Database, DevonError, Plan, Statement};
use devondb_types::logical_type::LogicalType;
use devondb_types::schema::Column;
use devondb_types::value::Value;

const PAGE_SIZE: u32 = 4096;
const CRASH_CHILD_ENV: &str = "DEVONDB_UPSERT_CRASH_CHILD";
const CRASH_PATH_ENV: &str = "DEVONDB_UPSERT_CRASH_PATH";

fn column(name: &str, ty: LogicalType, primary_key: bool) -> Column {
    Column {
        name: name.to_owned(),
        ty,
        primary_key,
    }
}

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
            "devondb-upsert-seam-{label}-{timestamp}-{sequence}-{}",
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
fn upsert_into_empty_table_inserts() {
    let directory = TestDirectory::new("insert");
    let mut database = people_database(&directory.db_path(), &[]);

    database
        .execute(&upsert_people(&[(1, "Ada", "London")]))
        .expect("upsert absent primary key");

    assert_eq!(people_rows(&mut database), rows(&[(1, "Ada", "London")]));
}

#[test]
fn upsert_existing_primary_key_replaces_the_whole_row() {
    let directory = TestDirectory::new("replace");
    let mut database = people_database(&directory.db_path(), &[(1, "old-name", "old-city")]);

    database
        .execute(&upsert_people(&[(1, "new-name", "new-city")]))
        .expect("upsert present primary key");

    assert_eq!(
        people_rows(&mut database),
        rows(&[(1, "new-name", "new-city")])
    );
}

#[test]
fn mixed_upsert_batch_inserts_and_replaces() {
    let directory = TestDirectory::new("mixed");
    let mut database = people_database(&directory.db_path(), &[(1, "old", "Boston")]);

    database
        .execute(&upsert_people(&[
            (1, "replaced", "London"),
            (2, "inserted", "Paris"),
        ]))
        .expect("mixed upsert");

    assert_eq!(
        people_rows(&mut database),
        rows(&[(1, "replaced", "London"), (2, "inserted", "Paris"),])
    );
}

#[test]
fn repeated_primary_key_in_one_upsert_keeps_the_last_row() {
    let directory = TestDirectory::new("last-write");
    let mut database = people_database(&directory.db_path(), &[]);

    database
        .execute(&upsert_people(&[
            (1, "first", "Boston"),
            (1, "second", "London"),
            (1, "last", "Paris"),
        ]))
        .expect("repeated-key upsert");

    assert_eq!(people_rows(&mut database), rows(&[(1, "last", "Paris")]));
}

#[test]
fn upsert_sees_inserts_and_updates_from_earlier_transaction_statements() {
    let directory = TestDirectory::new("prior-statements");
    let mut database = people_database(&directory.db_path(), &[(2, "old", "Boston")]);
    let mut transaction = database.begin().expect("begin transaction");
    transaction
        .execute(&insert_rows(rows(&[(1, "inserted-first", "Rome")])))
        .expect("stage preceding insert");
    transaction
        .execute(&Statement::UpdateNode {
            table: "People".to_owned(),
            set: vec![devondb_plan::statement::SetItem {
                column: "name".to_owned(),
                value: Value::String("updated-first".to_owned()),
            }],
            key_column: "id".to_owned(),
            key: Value::Int64(2),
        })
        .expect("stage preceding update");
    transaction
        .execute(&upsert_people(&[
            (1, "upserted-insert", "London"),
            (2, "upserted-update", "Paris"),
        ]))
        .expect("upsert transaction-local rows");
    transaction.commit().expect("commit transaction");

    assert_eq!(
        people_rows(&mut database),
        rows(&[
            (1, "upserted-insert", "London"),
            (2, "upserted-update", "Paris"),
        ])
    );
}

#[test]
fn upsert_values_survive_checkpoint_and_reopen() {
    let directory = TestDirectory::new("checkpoint");
    let path = directory.db_path();
    let mut database = people_database(&path, &[(1, "old", "Boston")]);
    database
        .execute(&upsert_people(&[
            (1, "replaced", "London"),
            (2, "inserted", "Paris"),
        ]))
        .expect("upsert before checkpoint");

    database.checkpoint().expect("checkpoint upsert values");
    assert_eq!(
        people_rows(&mut database),
        rows(&[(1, "replaced", "London"), (2, "inserted", "Paris"),])
    );
    drop(database);

    let mut reopened = Database::open(&path).expect("reopen checkpointed database");
    assert_eq!(
        people_rows(&mut reopened),
        rows(&[(1, "replaced", "London"), (2, "inserted", "Paris"),])
    );
}

#[test]
fn acknowledged_upsert_recovers_after_kill() {
    let directory = TestDirectory::new("recovery");
    let path = directory.db_path();
    let mut database = people_database(&path, &[(1, "old", "Boston")]);
    database.checkpoint().expect("checkpoint crash-test seed");
    drop(database);

    let mut child = spawn_crash_child(&path);
    let mut stdout = BufReader::new(child.stdout.take().expect("child stdout"));
    read_until_ack(&mut stdout);
    child.kill().expect("SIGKILL upsert child");
    assert!(!child.wait().expect("wait for killed child").success());
    assert_child_stderr_empty(&mut child);
    assert!(
        fs::metadata(wal_path(&path))
            .expect("upsert WAL metadata")
            .len()
            > 0
    );

    let mut recovered = Database::open(&path).expect("recover upsert WAL");
    assert_eq!(
        people_rows(&mut recovered),
        rows(&[(1, "recovered", "London"), (2, "inserted", "Paris"),])
    );
}

#[test]
fn upsert_crash_child_process() {
    if env::var_os(CRASH_CHILD_ENV).is_none() {
        return;
    }
    let path = PathBuf::from(env::var_os(CRASH_PATH_ENV).expect("crash database path"));
    let mut database = Database::open(path).expect("open crash database");
    database
        .execute(&upsert_people(&[
            (1, "recovered", "London"),
            (2, "inserted", "Paris"),
        ]))
        .expect("commit crash upsert");
    println!("UPSERT_ACK");
    std::io::stdout()
        .flush()
        .expect("flush crash acknowledgement");
    thread::sleep(Duration::from_secs(60));
}

#[test]
fn reader_snapshot_before_upsert_keeps_old_values() {
    let directory = TestDirectory::new("snapshot");
    let mut database = people_database(&directory.db_path(), &[(1, "old", "Boston")]);
    let before = database.snapshot();

    database
        .execute(&upsert_people(&[
            (1, "new", "London"),
            (2, "inserted", "Paris"),
        ]))
        .expect("commit upsert after snapshot");

    assert_eq!(
        people_rows(&mut database),
        rows(&[(1, "new", "London"), (2, "inserted", "Paris")])
    );
    assert_eq!(
        sorted_rows(before.run(&people_scan()).expect("run old snapshot").rows),
        rows(&[(1, "old", "Boston")])
    );
}

#[test]
fn concurrent_upserts_of_one_primary_key_conflict() {
    let directory = TestDirectory::new("conflict");
    let mut database = people_database(&directory.db_path(), &[(1, "old", "Boston")]);
    database.checkpoint().expect("checkpoint conflict seed");
    let mut winner = database.begin().expect("begin winner");
    let mut loser = database.begin().expect("begin loser");
    winner
        .execute(&upsert_people(&[(1, "winner", "London")]))
        .expect("stage winner");
    loser
        .execute(&upsert_people(&[(1, "loser", "Paris")]))
        .expect("stage loser");

    winner.commit().expect("commit winner");
    let error = loser.commit().expect_err("second upsert must conflict");
    assert!(matches!(error, DevonError::TransactionConflict { .. }));
    assert_eq!(people_rows(&mut database), rows(&[(1, "winner", "London")]));
}

#[test]
fn upsert_arity_and_type_errors_match_insert() {
    let directory = TestDirectory::new("validation");
    let mut database = people_database(&directory.db_path(), &[]);
    let wrong_arity = vec![vec![Value::Int64(1)]];
    let wrong_type = vec![vec![
        Value::Int64(1),
        Value::Int64(2),
        Value::String("London".to_owned()),
    ]];

    assert_eq!(
        statement_error(&mut database, insert_rows(wrong_arity.clone())),
        statement_error(&mut database, upsert_rows(wrong_arity))
    );
    assert_eq!(
        statement_error(&mut database, insert_rows(wrong_type.clone())),
        statement_error(&mut database, upsert_rows(wrong_type))
    );
}

fn people_database(path: &Path, initial: &[(i64, &str, &str)]) -> Database {
    let mut database = Database::create(path, PAGE_SIZE).expect("create database");
    database
        .execute(&Statement::CreateNodeTable {
            name: "People".to_owned(),
            columns: vec![
                column("id", LogicalType::Int64, true),
                column("name", LogicalType::String, false),
                column("city", LogicalType::String, false),
            ],
        })
        .expect("create people table");
    if !initial.is_empty() {
        database
            .execute(&insert_rows(rows(initial)))
            .expect("insert initial people");
    }
    database
}

fn upsert_people(input: &[(i64, &str, &str)]) -> Statement {
    upsert_rows(rows(input))
}

fn insert_rows(rows: Vec<Vec<Value>>) -> Statement {
    Statement::InsertNode {
        table: "People".to_owned(),
        rows,
    }
}

fn upsert_rows(rows: Vec<Vec<Value>>) -> Statement {
    Statement::UpsertNode {
        table: "People".to_owned(),
        rows,
    }
}

fn rows(input: &[(i64, &str, &str)]) -> Vec<Vec<Value>> {
    input
        .iter()
        .map(|(id, name, city)| {
            vec![
                Value::Int64(*id),
                Value::String((*name).to_owned()),
                Value::String((*city).to_owned()),
            ]
        })
        .collect()
}

fn people_scan() -> Plan {
    Plan::from_json(r#"{"v":0,"plan":{"op":"ScanNodes","table":"People","binding":"p"}}"#)
        .expect("people scan plan")
}

fn people_rows(database: &mut Database) -> Vec<Vec<Value>> {
    sorted_rows(
        database
            .run(&people_scan())
            .expect("scan people table")
            .rows,
    )
}

fn sorted_rows(mut rows: Vec<Vec<Value>>) -> Vec<Vec<Value>> {
    rows.sort_by_key(|row| match row.first() {
        Some(Value::Int64(id)) => *id,
        other => panic!("unexpected primary key: {other:?}"),
    });
    rows
}

fn statement_error(database: &mut Database, statement: Statement) -> String {
    database
        .execute(&statement)
        .expect_err("invalid statement must fail")
        .to_string()
}

fn spawn_crash_child(path: &Path) -> Child {
    Command::new(env::current_exe().expect("current test executable"))
        .args([
            "--exact",
            "upsert_crash_child_process",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CRASH_CHILD_ENV, "1")
        .env(CRASH_PATH_ENV, path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn upsert crash child")
}

fn read_until_ack(stdout: &mut BufReader<impl Read>) {
    loop {
        let mut line = String::new();
        assert_ne!(
            stdout.read_line(&mut line).expect("read child stdout"),
            0,
            "upsert child exited before acknowledgement"
        );
        if line.contains("UPSERT_ACK") {
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
        .expect("read child stderr");
    assert!(stderr.is_empty(), "unexpected child stderr: {stderr}");
}

fn wal_path(path: &Path) -> PathBuf {
    let mut with_suffix = OsString::from(path.as_os_str());
    with_suffix.push("-wal");
    PathBuf::from(with_suffix)
}

/// The canonical upsert spelling survives a serde roundtrip and prints
/// per the grammar in `docs/PLAN_IR.md` § Statements. The parser must accept
/// this exact output.
#[test]
fn upsert_statement_serde_and_printer_spelling() {
    let statement = Statement::UpsertNode {
        table: "People".to_owned(),
        rows: vec![vec![Value::Int64(1), Value::String("ada".to_owned())]],
    };
    let json = serde_json::to_string(&statement).expect("serialize");
    assert!(
        json.contains(r#""stmt":"UpsertNode""#),
        "tagged serde spelling, got: {json}"
    );
    let back: Statement = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back, statement);

    let envelope = devondb_plan::statement::StatementEnvelope {
        v: 0,
        stmt: statement,
    };
    let printed = devondb_plan::text::printer::print_statement(&envelope).expect("canonical print");
    assert_eq!(printed, r#"upsert People values (1, "ada")"#);
}
