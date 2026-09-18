//! Engine-owned checkpoint-policy regression tests.
//!
//! These tests intentionally use the production defaults: there is no test
//! environment override or persisted knob. The size threshold is
//! `max(4 MiB, current main-file bytes / 8)`, while a final clean writer-handle
//! close drains any smaller committed WAL. Commit durability remains WAL-first.

use std::{
    env, fs,
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::Duration,
};

use devondb::{Database, Statement, text::parser::Parsed};
use devondb_storage::pager::Pager;
use devondb_types::{logical_type::LogicalType, schema::Column, value::Value};
use tempfile::tempdir;

const PAGE_SIZE: u32 = 4096;
const DEFAULT_MIN_WAL_BYTES: u64 = 4 * 1024 * 1024;
const CRASH_CHILD_ENV: &str = "DEVONDB_AUTOCHECKPOINT_CRASH_CHILD";
const CRASH_PATH_ENV: &str = "DEVONDB_AUTOCHECKPOINT_CRASH_PATH";
const CRASH_ACK: &str = "AUTOCHECKPOINT_WAL_ACK";

#[test]
fn wal_crossing_default_size_threshold_checkpoints_and_reopens_all_rows() {
    let directory = tempdir().expect("create auto-checkpoint directory");
    let path = directory.path().join("size-threshold.devondb");
    let mut database = seeded_database(&path);
    let generation_before = checkpoint_lsn(&path);

    let rows = (0..3)
        .map(|id| row(id, &"x".repeat(1_500_000)))
        .collect::<Vec<_>>();
    database
        .execute(&Statement::InsertNode {
            table: "Entry".to_owned(),
            rows,
        })
        .expect("commit enough WAL bytes to cross the default threshold");

    assert_eq!(wal_len(&path), 0, "size-triggered checkpoint truncates WAL");
    assert!(
        checkpoint_lsn(&path) > generation_before,
        "size-triggered checkpoint must publish a newer superblock generation"
    );
    drop(database);

    let mut reopened = Database::open(&path).expect("reopen size-checkpointed database");
    assert_eq!(
        scanned_rows(&mut reopened),
        vec![(0, 1_500_000), (1, 1_500_000), (2, 1_500_000)]
    );
}

#[cfg(unix)]
#[test]
fn kill_between_auto_checkpoints_replays_wal_without_loss() {
    let directory = tempdir().expect("create crash-recovery directory");
    let path = directory.path().join("kill-between.devondb");
    drop(seeded_database(&path));

    let mut child = spawn_crash_child(&path);
    read_until_ack(&mut child);
    let pending_wal = wal_len(&path);
    assert!(pending_wal > 0, "acknowledged commit must have WAL bytes");
    assert!(
        pending_wal < DEFAULT_MIN_WAL_BYTES,
        "fixture must remain between size-triggered checkpoints"
    );
    child.kill().expect("SIGKILL auto-checkpoint child");
    assert!(
        !child.wait().expect("wait for killed child").success(),
        "SIGKILL child unexpectedly exited successfully"
    );
    assert_child_stderr_empty(&mut child);

    let mut recovered = Database::open(&path).expect("replay killed child's WAL");
    assert_eq!(
        scanned_rows(&mut recovered),
        vec![(7, 64 * 1024), (8, 64 * 1024)]
    );
    assert!(
        wal_len(&path) > 0,
        "open replay must not pretend WAL recovery was a checkpoint"
    );
}

#[test]
fn autocheckpoint_crash_child_process() {
    if env::var_os(CRASH_CHILD_ENV).is_none() {
        return;
    }
    let path = PathBuf::from(env::var_os(CRASH_PATH_ENV).expect("child database path"));
    let mut database = Database::open(path).expect("child opens database");
    database
        .execute(&Statement::InsertNode {
            table: "Entry".to_owned(),
            rows: vec![
                row(7, &"a".repeat(64 * 1024)),
                row(8, &"b".repeat(64 * 1024)),
            ],
        })
        .expect("child commits rows below the size threshold");
    println!("{CRASH_ACK}");
    std::io::stdout()
        .flush()
        .expect("flush child acknowledgement");
    thread::sleep(Duration::from_secs(60));
}

#[test]
fn one_hundred_redundant_manual_checkpoints_grow_main_file_by_zero_bytes() {
    let directory = tempdir().expect("create redundant-checkpoint directory");
    let path = directory.path().join("manual-hint.devondb");
    let mut database = seeded_database(&path);
    database
        .execute(&Statement::InsertNode {
            table: "Entry".to_owned(),
            rows: vec![row(1, "stable")],
        })
        .expect("insert stable row");
    database.checkpoint().expect("first checkpoint");
    let file_len_after_first = fs::metadata(&path).expect("main-file metadata").len();
    let generation_after_first = checkpoint_lsn(&path);

    for call in 0..100 {
        database.checkpoint().expect("redundant checkpoint hint");
        assert_eq!(
            fs::metadata(&path).expect("main-file metadata").len(),
            file_len_after_first,
            "redundant checkpoint call {call} grew the main file"
        );
    }

    assert_eq!(wal_len(&path), 0);
    assert_eq!(checkpoint_lsn(&path), generation_after_first);
}

#[test]
fn clean_close_checkpoints_a_committed_wal_below_the_size_threshold() {
    let directory = tempdir().expect("create clean-close directory");
    let path = directory.path().join("clean-close.devondb");
    let mut database = seeded_database(&path);
    let generation_before = checkpoint_lsn(&path);
    database
        .execute(&Statement::InsertNode {
            table: "Entry".to_owned(),
            rows: vec![row(42, "close publishes this small WAL")],
        })
        .expect("commit small WAL");
    assert!(wal_len(&path) > 0);
    assert!(wal_len(&path) < DEFAULT_MIN_WAL_BYTES);

    drop(database);

    assert_eq!(wal_len(&path), 0, "clean close must truncate committed WAL");
    assert!(checkpoint_lsn(&path) > generation_before);
    let mut reopened = Database::open(&path).expect("reopen clean-close publication");
    assert_eq!(scanned_rows(&mut reopened), vec![(42, 30)]);
}

fn seeded_database(path: &Path) -> Database {
    let mut database = Database::create(path, PAGE_SIZE).expect("create database");
    database
        .execute(&Statement::CreateNodeTable {
            name: "Entry".to_owned(),
            columns: vec![
                Column {
                    name: "id".to_owned(),
                    ty: LogicalType::Int64,
                    primary_key: true,
                },
                Column {
                    name: "body".to_owned(),
                    ty: LogicalType::String,
                    primary_key: false,
                },
            ],
        })
        .expect("create Entry table");
    database.checkpoint().expect("publish seed schema");
    database
}

fn row(id: i64, body: &str) -> Vec<Value> {
    vec![Value::Int64(id), Value::String(body.to_owned())]
}

fn scanned_rows(database: &mut Database) -> Vec<(i64, usize)> {
    let Parsed::Query(plan) =
        devondb::text::parser::parse("nodes(Entry) as e | sort e.id | project e.id, e.body")
            .expect("parse Entry scan")
    else {
        panic!("expected query plan");
    };
    database
        .run(&plan)
        .expect("scan Entry")
        .rows
        .into_iter()
        .map(|values| match values.as_slice() {
            [Value::Int64(id), Value::String(body)] => (*id, body.len()),
            other => panic!("unexpected Entry row {other:?}"),
        })
        .collect()
}

fn checkpoint_lsn(path: &Path) -> u64 {
    Pager::open(path)
        .expect("open pager for generation")
        .superblock()
        .checkpoint_lsn
}

fn wal_len(path: &Path) -> u64 {
    fs::metadata(wal_path(path)).expect("WAL metadata").len()
}

fn wal_path(path: &Path) -> PathBuf {
    PathBuf::from(format!("{}-wal", path.display()))
}

#[cfg(unix)]
fn spawn_crash_child(path: &Path) -> Child {
    Command::new(env::current_exe().expect("current test executable"))
        .args([
            "--exact",
            "autocheckpoint_crash_child_process",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CRASH_CHILD_ENV, "1")
        .env(CRASH_PATH_ENV, path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn auto-checkpoint crash child")
}

#[cfg(unix)]
fn read_until_ack(child: &mut Child) {
    let mut stdout = BufReader::new(child.stdout.take().expect("child stdout"));
    loop {
        let mut line = String::new();
        assert_ne!(
            stdout.read_line(&mut line).expect("read child stdout"),
            0,
            "child exited before acknowledging its WAL commit"
        );
        if line.contains(CRASH_ACK) {
            return;
        }
    }
}

#[cfg(unix)]
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
