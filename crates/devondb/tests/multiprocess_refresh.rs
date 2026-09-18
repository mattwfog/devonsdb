use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

use devondb::{Database, DevonError, Statement};
use devondb_plan::ops::{Operator, Plan};
use devondb_storage::lock::{LockPaths, PublicationGate};
use devondb_types::{logical_type::LogicalType, schema::Column, value::Value};
use tempfile::{TempDir, tempdir};

const PAGE_SIZE: u32 = 4096;

struct Fixture {
    _directory: TempDir,
    path: PathBuf,
    writer: Database,
    follower: Database,
}

impl Fixture {
    fn new() -> Self {
        let (directory, path, writer) = activated_writer();
        let follower = Database::open_read_only(&path).unwrap();
        Self {
            _directory: directory,
            path,
            writer,
            follower,
        }
    }
}

#[test]
fn multiprocess_refresh_observes_new_commits_and_rows() {
    let mut fixture = Fixture::new();
    let before = fixture.follower.observed_commit_lsn();
    insert_people(&mut fixture.writer, &[(1, "Ada"), (2, "Grace")]);

    assert!(fixture.follower.refresh().unwrap());
    assert!(fixture.follower.observed_commit_lsn() > before);
    assert_eq!(
        scan_rows(&fixture.follower),
        people(&[(1, "Ada"), (2, "Grace")])
    );
}

#[test]
fn multiprocess_refresh_keeps_a_pinned_snapshot_stable_across_checkpoint() {
    let mut fixture = Fixture::new();
    insert_people(&mut fixture.writer, &[(1, "Ada")]);
    assert!(fixture.follower.refresh().unwrap());
    let pinned = fixture.follower.snapshot();
    let pinned_before = pinned.run(&scan_plan()).unwrap().rows;

    insert_people(&mut fixture.writer, &[(2, "Grace")]);
    fixture.writer.checkpoint().unwrap();

    assert_eq!(pinned.run(&scan_plan()).unwrap().rows, pinned_before);
    assert_eq!(pinned_before, people(&[(1, "Ada")]));
    assert_eq!(
        scan_rows(&fixture.follower),
        people(&[(1, "Ada"), (2, "Grace")])
    );
}

#[test]
fn multiprocess_refresh_rebases_after_checkpoint_and_wal_reset() {
    let mut fixture = Fixture::new();
    insert_people(&mut fixture.writer, &[(1, "Ada")]);
    fixture.writer.checkpoint().unwrap();
    insert_people(&mut fixture.writer, &[(2, "Grace")]);

    assert!(fixture.follower.refresh().unwrap());
    assert_eq!(
        scan_rows(&fixture.follower),
        people(&[(1, "Ada"), (2, "Grace")])
    );
}

#[test]
fn multiprocess_refresh_gate_contention_serves_the_old_state() {
    let mut fixture = Fixture::new();
    insert_people(&mut fixture.writer, &[(1, "Ada")]);
    let lock_paths = LockPaths::for_main(&fixture.path).unwrap();
    let gate = PublicationGate::open(&lock_paths).unwrap();
    let guard = gate.try_exclusive().unwrap();

    assert!(!fixture.follower.refresh().unwrap());
    assert!(scan_rows(&fixture.follower).is_empty());

    drop(guard);
    assert!(fixture.follower.refresh().unwrap());
    assert_eq!(scan_rows(&fixture.follower), people(&[(1, "Ada")]));
}

#[test]
fn multiprocess_refresh_first_open_returns_busy_under_gate_contention() {
    let (_directory, path, writer) = activated_writer();
    let lock_paths = LockPaths::for_main(&path).unwrap();
    let gate = PublicationGate::open(&lock_paths).unwrap();
    let guard = gate.try_exclusive().unwrap();

    let error = Database::open_read_only(&path).err().unwrap();
    assert!(matches!(error, DevonError::Busy { .. }));

    drop(guard);
    let follower = Database::open_read_only(&path).unwrap();
    drop(follower);
    drop(writer);
}

#[test]
fn multiprocess_refresh_observed_lsn_never_decreases() {
    let mut fixture = Fixture::new();
    let mut observed = vec![fixture.follower.observed_commit_lsn()];

    insert_people(&mut fixture.writer, &[(1, "Ada")]);
    assert!(fixture.follower.refresh().unwrap());
    observed.push(fixture.follower.observed_commit_lsn());
    fixture.writer.checkpoint().unwrap();
    assert!(fixture.follower.refresh().unwrap());
    observed.push(fixture.follower.observed_commit_lsn());
    insert_people(&mut fixture.writer, &[(2, "Grace")]);
    assert!(fixture.follower.refresh().unwrap());
    observed.push(fixture.follower.observed_commit_lsn());
    fixture.writer.checkpoint().unwrap();
    assert!(fixture.follower.refresh().unwrap());
    observed.push(fixture.follower.observed_commit_lsn());
    insert_people(&mut fixture.writer, &[(3, "Linus")]);
    assert!(fixture.follower.refresh().unwrap());
    observed.push(fixture.follower.observed_commit_lsn());

    assert!(observed.windows(2).all(|pair| pair[0] <= pair[1]));
}

#[test]
fn multiprocess_refresh_follower_refuses_mutations_and_writer_continues() {
    let mut fixture = Fixture::new();

    assert_follower_read_only(fixture.follower.execute(&create_other_table()));
    assert_follower_read_only(fixture.follower.execute(&insert_statement(1, "Ada")));
    assert_follower_read_only(fixture.follower.checkpoint());
    assert_follower_read_only(fixture.follower.execute(&Statement::CopyNode {
        table: "Person".to_owned(),
        path: fixture.path.with_extension("csv").display().to_string(),
        sort_by: None,
    }));

    insert_people(&mut fixture.writer, &[(1, "Ada")]);
    assert!(fixture.follower.refresh().unwrap());
    assert_eq!(scan_rows(&fixture.follower), people(&[(1, "Ada")]));
}

#[test]
fn multiprocess_refresh_ignores_a_torn_wal_tail() {
    let mut fixture = Fixture::new();
    insert_people(&mut fixture.writer, &[(1, "Ada")]);
    let mut wal = OpenOptions::new()
        .append(true)
        .open(wal_path(&fixture.path))
        .unwrap();
    wal.write_all(b"torn-tail-garbage").unwrap();
    wal.sync_all().unwrap();
    drop(wal);

    assert!(fixture.follower.refresh().unwrap());
    assert_eq!(scan_rows(&fixture.follower), people(&[(1, "Ada")]));
}

#[test]
fn multiprocess_refresh_requires_offline_activation() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("database.devondb");
    drop(Database::create(&path, PAGE_SIZE).unwrap());

    let error = Database::open_read_only(&path).err().unwrap();

    assert!(matches!(error, DevonError::InvalidArgument { .. }));
    assert!(error.to_string().contains("activate_multiprocess"));
}

fn activated_writer() -> (TempDir, PathBuf, Database) {
    let directory = tempdir().unwrap();
    let path = directory.path().join("database.devondb");
    let mut database = Database::create(&path, PAGE_SIZE).unwrap();
    database.execute(&create_person_table()).unwrap();
    drop(database);
    Database::activate_multiprocess(&path).unwrap();
    let writer = Database::open(&path).unwrap();
    (directory, path, writer)
}

fn create_person_table() -> Statement {
    Statement::CreateNodeTable {
        name: "Person".to_owned(),
        columns: vec![
            Column {
                name: "id".to_owned(),
                ty: LogicalType::Int64,
                primary_key: true,
            },
            Column {
                name: "name".to_owned(),
                ty: LogicalType::String,
                primary_key: false,
            },
        ],
    }
}

fn create_other_table() -> Statement {
    Statement::CreateNodeTable {
        name: "Other".to_owned(),
        columns: vec![Column {
            name: "id".to_owned(),
            ty: LogicalType::Int64,
            primary_key: true,
        }],
    }
}

fn insert_people(database: &mut Database, rows: &[(i64, &str)]) {
    database
        .execute(&Statement::InsertNode {
            table: "Person".to_owned(),
            rows: people(rows),
        })
        .unwrap();
}

fn insert_statement(id: i64, name: &str) -> Statement {
    Statement::InsertNode {
        table: "Person".to_owned(),
        rows: people(&[(id, name)]),
    }
}

fn people(rows: &[(i64, &str)]) -> Vec<Vec<Value>> {
    rows.iter()
        .map(|(id, name)| vec![Value::Int64(*id), Value::String((*name).to_owned())])
        .collect()
}

fn scan_plan() -> Plan {
    Plan {
        v: 0,
        plan: Operator::ScanNodes {
            table: "Person".to_owned(),
            binding: "person".to_owned(),
        },
    }
}

fn scan_rows(database: &Database) -> Vec<Vec<Value>> {
    database.snapshot().run(&scan_plan()).unwrap().rows
}

fn assert_follower_read_only(result: Result<(), DevonError>) {
    let error = result.unwrap_err();
    match error {
        DevonError::ReadOnly { context } => assert!(context.contains("follower")),
        other => panic!("expected follower ReadOnly error, got {other}"),
    }
}

fn wal_path(path: &Path) -> PathBuf {
    let mut suffixed = path.as_os_str().to_os_string();
    suffixed.push("-wal");
    suffixed.into()
}
