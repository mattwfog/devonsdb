use std::{
    collections::BTreeMap,
    env, fs,
    path::{Path, PathBuf},
    process::Command,
    time::SystemTime,
};

use devondb::{Database, DevonError, Statement, Value};
use devondb_plan::statement::RelRow;
use devondb_types::{logical_type::LogicalType, schema::Column};
use tempfile::tempdir;

const PAGE_SIZE: u32 = 4096;
const AUTOCHECKPOINT_ENV: &str = "DEVONDB_AUTOCHECKPOINT";
const RECOVERY_CHILD_ENV: &str = "DEVONDB_READ_ONLY_OPEN_RECOVERY_CHILD";

#[derive(Debug, Clone, PartialEq, Eq)]
struct FileState {
    directory: bool,
    len: u64,
    modified: SystemTime,
}

#[test]
fn regular_inspection_replays_wal_without_any_filesystem_change() {
    if run_autocheckpoint_off_child(
        "regular_inspection_replays_wal_without_any_filesystem_change",
        RECOVERY_CHILD_ENV,
    ) {
        return;
    }
    let directory = tempdir().unwrap();
    let path = directory.path().join("wal.devondb");
    let mut database = fixture(&path);
    database
        .execute(&Statement::DetachDeleteNode {
            table: "Person".into(),
            key_column: "id".into(),
            key: Value::Int64(2),
        })
        .unwrap();
    drop(database);

    let wal = PathBuf::from(format!("{}-wal", path.display()));
    let spill = PathBuf::from(format!("{}.tmp", path.display()));
    fs::remove_dir_all(&spill).unwrap();
    assert!(fs::metadata(&wal).unwrap().len() > 0);
    let before = snapshot(directory.path());
    let inspection = Database::open_inspect(&path).unwrap();
    let during = snapshot(directory.path());
    assert_eq!(during, before, "inspection open changed directory bytes");

    let nodes = inspection
        .visible_nodes("Person")
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(nodes.len(), 1);
    assert_eq!(nodes[0].values[0], Value::Int64(1));
    let relationships = inspection
        .visible_relationships("Knows")
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert!(relationships.is_empty(), "detach-deleted edge leaked");
    drop(inspection);

    assert_eq!(snapshot(directory.path()), before);
    assert!(fs::metadata(&wal).unwrap().len() > 0, "WAL was healed");
    assert!(!spill.exists(), "inspection created spill state");
}

#[test]
fn inspection_refuses_an_active_writer_without_creating_state() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("busy.devondb");
    let writer = fixture(&path);
    let before = snapshot(directory.path());
    let error = match Database::open_inspect(&path) {
        Ok(_) => panic!("inspection unexpectedly opened beside a writer"),
        Err(error) => error,
    };
    assert!(matches!(error, DevonError::Busy { .. }), "{error}");
    assert_eq!(snapshot(directory.path()), before);
    drop(writer);
}

#[cfg(feature = "pack")]
#[test]
fn pack_inspection_creates_no_pack_tmp_directory() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("source.devondb");
    let pack = directory.path().join("source.pack");
    let mut database = fixture(&path);
    database.pack(&pack, 1).unwrap();
    drop(database);

    let before = snapshot(directory.path());
    let inspection = Database::open_inspect(&pack).unwrap();
    assert_eq!(snapshot(directory.path()), before);
    assert_eq!(inspection.visible_nodes("Person").unwrap().count(), 2);
    drop(inspection);
    assert_eq!(snapshot(directory.path()), before);
    assert!(!PathBuf::from(format!("{}.tmp", pack.display())).exists());
}

fn fixture(path: &Path) -> Database {
    let mut database = Database::create(path, PAGE_SIZE).unwrap();
    database
        .execute(&Statement::CreateNodeTable {
            name: "Person".into(),
            columns: vec![
                column("id", LogicalType::Int64, true),
                column("name", LogicalType::String, false),
            ],
        })
        .unwrap();
    database
        .execute(&Statement::CreateRelTable {
            name: "Knows".into(),
            from: "Person".into(),
            to: "Person".into(),
            columns: vec![column("weight", LogicalType::Float64, false)],
        })
        .unwrap();
    database
        .execute(&Statement::InsertNode {
            table: "Person".into(),
            rows: vec![
                vec![Value::Int64(2), Value::String("Grace".into())],
                vec![Value::Int64(1), Value::String("Ada".into())],
            ],
        })
        .unwrap();
    database
        .execute(&Statement::InsertRel {
            table: "Knows".into(),
            rows: vec![RelRow {
                from_key: Value::Int64(1),
                to_key: Value::Int64(2),
                values: vec![Value::Float64(0.75)],
            }],
        })
        .unwrap();
    database
}

fn column(name: &str, ty: LogicalType, primary_key: bool) -> Column {
    Column {
        name: name.into(),
        ty,
        primary_key,
    }
}

fn run_autocheckpoint_off_child(test_name: &str, child_env: &str) -> bool {
    if env::var_os(child_env).is_some() {
        return false;
    }
    let output = Command::new(env::current_exe().unwrap())
        .args(["--exact", test_name, "--nocapture", "--test-threads=1"])
        .env(child_env, "1")
        .env(AUTOCHECKPOINT_ENV, "off")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "auto-checkpoint-off child failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    true
}

fn snapshot(root: &Path) -> BTreeMap<PathBuf, FileState> {
    let mut states = BTreeMap::new();
    snapshot_directory(root, root, &mut states);
    states
}

fn snapshot_directory(root: &Path, directory: &Path, states: &mut BTreeMap<PathBuf, FileState>) {
    let mut entries = fs::read_dir(directory)
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    entries.sort_by_key(fs::DirEntry::file_name);
    for entry in entries {
        let path = entry.path();
        let metadata = entry.metadata().unwrap();
        states.insert(
            path.strip_prefix(root).unwrap().to_path_buf(),
            FileState {
                directory: metadata.is_dir(),
                len: metadata.len(),
                modified: metadata.modified().unwrap(),
            },
        );
        if metadata.is_dir() {
            snapshot_directory(root, &path, states);
        }
    }
}
