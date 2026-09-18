//! Read-safe feature flags force read-only open (`docs/FORMAT.md`
//! § Feature flag registry, `docs/HNSW.md` §8.2): a file enabling a
//! read-safe flag this build does not fully support keeps base data
//! readable while writes and checkpoints are refused.

use std::{
    env, fs,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use devondb::{Database, DevonError, Plan, Statement};
use devondb_plan::ops::{Operator, PLAN_VERSION};
use devondb_storage::{pager::Pager, superblock::RESERVED_READ_SAFE_FLAG};
use devondb_types::{logical_type::LogicalType, schema::Column, value::Value};

const PAGE_SIZE: u32 = 4096;

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
            "devondb-read-only-{label}-{timestamp}-{sequence}-{}",
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
fn read_safe_unsupported_flag_opens_read_only() {
    let directory = TestDirectory::new("flagged");
    let path = directory.db_path();

    let mut database = Database::create(&path, PAGE_SIZE).expect("create database");
    database
        .execute(&Statement::CreateNodeTable {
            name: "Person".to_owned(),
            columns: vec![Column {
                name: "id".to_owned(),
                ty: LogicalType::Int64,
                primary_key: true,
            }],
        })
        .expect("create table");
    database
        .execute(&Statement::InsertNode {
            table: "Person".to_owned(),
            rows: vec![vec![Value::Int64(7)]],
        })
        .expect("insert row");
    database.checkpoint().expect("checkpoint");
    drop(database);

    // A future devondb enabled a read-safe feature this build does not
    // fully support: simulate with the pre-allocated reserved bit. (HNSW's
    // own bit no longer qualifies — this build fully supports it.)
    let pager = Pager::open(&path).expect("open pager");
    let mut superblock = pager.superblock();
    superblock.feature_flags |= RESERVED_READ_SAFE_FLAG;
    superblock.checkpoint_lsn += 1;
    pager
        .commit_superblock(superblock)
        .expect("set feature bit");
    drop(pager);

    let mut flagged = Database::open(&path).expect("read-safe flag must still open");

    let result = flagged
        .run(&Plan {
            v: PLAN_VERSION,
            plan: Operator::ScanNodes {
                table: "Person".to_owned(),
                binding: "p".to_owned(),
            },
        })
        .expect("reads must work in read-only mode");
    assert_eq!(result.rows, vec![vec![Value::Int64(7)]]);

    let write = flagged.execute(&Statement::InsertNode {
        table: "Person".to_owned(),
        rows: vec![vec![Value::Int64(8)]],
    });
    assert!(
        matches!(write, Err(DevonError::ReadOnly { .. })),
        "write must be refused read-only, got {write:?}"
    );
    let begin = flagged.begin().map(|_| ());
    assert!(
        matches!(begin, Err(DevonError::ReadOnly { .. })),
        "begin must be refused read-only, got {begin:?}"
    );
    let checkpoint = flagged.checkpoint();
    assert!(
        matches!(checkpoint, Err(DevonError::ReadOnly { .. })),
        "checkpoint must be refused read-only, got {checkpoint:?}"
    );
}

#[test]
fn read_only_open_leaves_the_wal_sidecar_untouched() {
    let directory = TestDirectory::new("wal-footprint");
    let path = directory.db_path();
    let wal_path = path.with_extension("devondb-wal");

    let mut database = Database::create(&path, PAGE_SIZE).expect("create database");
    database
        .execute(&Statement::CreateNodeTable {
            name: "Person".to_owned(),
            columns: vec![Column {
                name: "id".to_owned(),
                ty: LogicalType::Int64,
                primary_key: true,
            }],
        })
        .expect("create table");
    database
        .execute(&Statement::InsertNode {
            table: "Person".to_owned(),
            rows: vec![vec![Value::Int64(7)]],
        })
        .expect("insert row");
    drop(database);

    let pager = Pager::open(&path).expect("open pager");
    let mut superblock = pager.superblock();
    superblock.feature_flags |= RESERVED_READ_SAFE_FLAG;
    superblock.checkpoint_lsn += 1;
    pager
        .commit_superblock(superblock)
        .expect("set feature bit");
    drop(pager);

    // Uncheckpointed committed WAL rows must still be visible read-only,
    // and the sidecar bytes must be byte-identical after the open.
    let wal_before = fs::read(&wal_path).expect("read WAL before");
    let mut flagged = Database::open(&path).expect("open read-only with WAL");
    let result = flagged
        .run(&Plan {
            v: PLAN_VERSION,
            plan: Operator::ScanNodes {
                table: "Person".to_owned(),
                binding: "p".to_owned(),
            },
        })
        .expect("read-only scan over WAL-recovered rows");
    assert_eq!(result.rows, vec![vec![Value::Int64(7)]]);
    drop(flagged);
    let wal_after = fs::read(&wal_path).expect("read WAL after");
    assert_eq!(wal_before, wal_after, "read-only open modified the WAL");

    // An absent sidecar (clean-close state) must stay absent.
    fs::remove_file(&wal_path).expect("remove WAL");
    let flagged = Database::open(&path).expect("open read-only without WAL");
    drop(flagged);
    assert!(!wal_path.exists(), "read-only open created the WAL sidecar");
}
