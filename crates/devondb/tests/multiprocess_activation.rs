use std::{
    env, fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use devondb::{Database, DevonError, Statement};
use devondb_storage::{
    lock::{LockPaths, PublicationGate},
    pager::Pager,
    superblock::{MULTIPROCESS_COORDINATION_FLAG, RESERVED_READ_SAFE_FLAG},
};
use devondb_types::{logical_type::LogicalType, schema::Column, value::Value};

const PAGE_SIZE: u32 = 4096;
const SUPERBLOCK_HEADER_LEN: usize = 64;
const FEATURE_FLAGS_OFFSET: usize = 16;
const CHECKSUM_OFFSET: usize = 60;

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct TestDirectory {
    path: PathBuf,
}

impl TestDirectory {
    fn new(label: &str) -> Self {
        let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = env::temp_dir().join(format!(
            "devondb-multiprocess-activation-{label}-{timestamp}-{sequence}-{}",
            std::process::id()
        ));
        fs::create_dir_all(&path).unwrap();
        Self { path }
    }

    fn database_path(&self) -> PathBuf {
        self.path.join("database.devondb")
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

#[test]
fn multiprocess_activation_sets_bit_durably_in_both_slots() {
    let directory = TestDirectory::new("durable");
    let path = create_database(&directory);

    Database::activate_multiprocess(&path).unwrap();

    let flags = slot_feature_flags(&path);
    assert!(
        flags
            .iter()
            .all(|flags| flags & MULTIPROCESS_COORDINATION_FLAG != 0)
    );
    assert_eq!(
        Pager::open(&path).unwrap().superblock().feature_flags & MULTIPROCESS_COORDINATION_FLAG,
        MULTIPROCESS_COORDINATION_FLAG
    );
}

#[test]
fn multiprocess_activation_is_idempotent() {
    let directory = TestDirectory::new("idempotent");
    let path = create_database(&directory);
    Database::activate_multiprocess(&path).unwrap();
    let main_before = fs::read(&path).unwrap();
    let wal_before = fs::read(wal_path(&path)).unwrap();

    Database::activate_multiprocess(&path).unwrap();

    assert_eq!(fs::read(&path).unwrap(), main_before);
    assert_eq!(fs::read(wal_path(&path)).unwrap(), wal_before);
}

#[test]
fn multiprocess_activation_refuses_a_live_writable_handle() {
    let directory = TestDirectory::new("busy");
    let path = directory.database_path();
    let database = Database::create(&path, PAGE_SIZE).unwrap();

    let activation = Database::activate_multiprocess(&path);

    assert!(matches!(activation, Err(DevonError::Busy { .. })));
    drop(database);
}

#[test]
fn multiprocess_activation_refuses_a_live_publication_guard() {
    let directory = TestDirectory::new("publication-busy");
    let path = create_database(&directory);
    let lock_paths = LockPaths::for_main(&path).unwrap();
    let publication_gate = PublicationGate::open(&lock_paths).unwrap();
    let _reader = publication_gate.try_shared().unwrap();

    let activation = Database::activate_multiprocess(&path);

    assert!(matches!(activation, Err(DevonError::Busy { .. })));
}

#[test]
fn multiprocess_activation_survives_writes_and_checkpoint() {
    let directory = TestDirectory::new("sticky");
    let path = create_database(&directory);
    Database::activate_multiprocess(&path).unwrap();

    let mut database = Database::open(&path).unwrap();
    database.execute(&worker_schema()).unwrap();
    database.execute(&worker_row()).unwrap();
    database.checkpoint().unwrap();
    drop(database);

    let pager = Pager::open(&path).unwrap();
    assert_eq!(
        pager.superblock().feature_flags & MULTIPROCESS_COORDINATION_FLAG,
        MULTIPROCESS_COORDINATION_FLAG
    );
}

#[test]
fn multiprocess_activation_bit_doctored_into_a_file_is_supported() {
    let directory = TestDirectory::new("doctored-supported");
    let path = create_database(&directory);
    doctor_feature_flag(&path, MULTIPROCESS_COORDINATION_FLAG);

    let database = Database::open(&path).unwrap();

    drop(database);
}

#[test]
fn multiprocess_activation_refuses_read_safe_unsupported_file() {
    let directory = TestDirectory::new("read-only");
    let path = create_database(&directory);
    doctor_feature_flag(&path, RESERVED_READ_SAFE_FLAG);

    let read_only = Database::open(&path).unwrap();
    drop(read_only);
    let activation = Database::activate_multiprocess(&path);

    assert!(matches!(activation, Err(DevonError::ReadOnly { .. })));
}

fn create_database(directory: &TestDirectory) -> PathBuf {
    let path = directory.database_path();
    drop(Database::create(&path, PAGE_SIZE).unwrap());
    path
}

fn worker_schema() -> Statement {
    Statement::CreateNodeTable {
        name: "Worker".to_owned(),
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

fn worker_row() -> Statement {
    Statement::InsertNode {
        table: "Worker".to_owned(),
        rows: vec![vec![Value::Int64(1), Value::String("writer".to_owned())]],
    }
}

fn doctor_feature_flag(path: &Path, flag: u64) {
    let mut bytes = fs::read(path).unwrap();
    for slot in 0..2 {
        let offset = slot * PAGE_SIZE as usize;
        let flags_offset = offset + FEATURE_FLAGS_OFFSET;
        let flags = u64::from_le_bytes(
            bytes[flags_offset..flags_offset + size_of::<u64>()]
                .try_into()
                .unwrap(),
        ) | flag;
        bytes[flags_offset..flags_offset + size_of::<u64>()].copy_from_slice(&flags.to_le_bytes());
        let checksum = crc32c::crc32c(&bytes[offset..offset + CHECKSUM_OFFSET]);
        bytes[offset + CHECKSUM_OFFSET..offset + SUPERBLOCK_HEADER_LEN]
            .copy_from_slice(&checksum.to_le_bytes());
    }
    fs::write(path, bytes).unwrap();
}

fn slot_feature_flags(path: &Path) -> [u64; 2] {
    let bytes = fs::read(path).unwrap();
    std::array::from_fn(|slot| {
        let offset = slot * PAGE_SIZE as usize + FEATURE_FLAGS_OFFSET;
        u64::from_le_bytes(bytes[offset..offset + size_of::<u64>()].try_into().unwrap())
    })
}

fn wal_path(path: &Path) -> PathBuf {
    path.with_extension("devondb-wal")
}
