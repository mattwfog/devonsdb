use std::path::Path;

use devondb::{Database, DevonError, Plan, Statement};
use devondb_plan::{
    expr::Metric,
    ops::{KnnMode, Operator},
};
use devondb_storage::lock::LockPaths;
use devondb_types::{logical_type::LogicalType, schema::Column, value::Value};
use tempfile::tempdir;

const PAGE_SIZE: u32 = 4096;

fn column(name: &str, ty: LogicalType, primary_key: bool) -> Column {
    Column {
        name: name.to_owned(),
        ty,
        primary_key,
    }
}

fn scan(table: &str) -> Plan {
    Plan {
        v: 0,
        plan: Operator::ScanNodes {
            table: table.to_owned(),
            binding: table.to_owned(),
        },
    }
}

fn assert_writer_busy(result: Result<Database, DevonError>) {
    match result {
        Err(error @ DevonError::Busy { .. }) => {
            assert!(
                error.to_string().contains("writer lease"),
                "busy error did not identify the writer lease: {error}"
            );
        }
        Err(error) => panic!("expected writer-lease Busy error, got {error}"),
        Ok(_) => panic!("expected the second writable open to be busy"),
    }
}

fn create_people(database: &mut Database) {
    database
        .execute(&Statement::CreateNodeTable {
            name: "Person".to_owned(),
            columns: vec![
                column("id", LogicalType::Int64, true),
                column("name", LogicalType::String, false),
            ],
        })
        .unwrap();
}

#[test]
fn multiprocess_locks_second_writable_open_is_busy_until_drop() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("database.devondb");
    drop(Database::create(&path, PAGE_SIZE).unwrap());

    let first = Database::open(&path).unwrap();
    assert_writer_busy(Database::open(&path));
    drop(first);

    let _successor = Database::open(&path).unwrap();
}

#[test]
fn multiprocess_locks_create_owns_lease_and_creates_both_sidecars() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("database.devondb");
    let first = Database::create(&path, PAGE_SIZE).unwrap();

    assert_writer_busy(Database::open(&path));
    let paths = LockPaths::for_main(&path).unwrap();
    assert!(paths.writer.is_file());
    assert!(paths.publish.is_file());

    drop(first);
}

#[cfg(unix)]
#[test]
fn multiprocess_locks_symlink_open_uses_canonical_identity() {
    use std::os::unix::fs::symlink;

    let directory = tempdir().unwrap();
    let real = directory.path().join("real.devondb");
    let link = directory.path().join("link.devondb");
    let first = Database::create(&real, PAGE_SIZE).unwrap();
    symlink(&real, &link).unwrap();

    assert_writer_busy(Database::open(&link));
    drop(first);
}

#[test]
fn multiprocess_locks_drop_preserves_sidecars_for_fresh_open() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("database.devondb");
    let database = Database::create(&path, PAGE_SIZE).unwrap();
    let paths = LockPaths::for_main(&path).unwrap();

    drop(database);
    assert!(paths.writer.is_file());
    assert!(paths.publish.is_file());

    let _fresh = Database::open(&path).unwrap();
}

#[test]
fn multiprocess_locks_commit_checkpoint_and_reopen_preserve_rows() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("database.devondb");
    let mut database = Database::create(&path, PAGE_SIZE).unwrap();
    create_people(&mut database);
    let expected = vec![
        vec![Value::Int64(1), Value::String("Ada".to_owned())],
        vec![Value::Int64(2), Value::String("Grace".to_owned())],
    ];
    database
        .execute(&Statement::InsertNode {
            table: "Person".to_owned(),
            rows: expected.clone(),
        })
        .unwrap();

    let before_checkpoint = database.run(&scan("Person")).unwrap().rows;
    assert_eq!(before_checkpoint, expected);
    database.checkpoint().unwrap();
    drop(database);

    let mut reopened = Database::open(&path).unwrap();
    assert_eq!(reopened.run(&scan("Person")).unwrap().rows, expected);
}

#[test]
fn multiprocess_locks_copy_and_hnsw_publications_succeed_end_to_end() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("database.devondb");
    let csv = directory.path().join("people.csv");
    std::fs::write(&csv, "id,name\n1,Ada\n2,Grace\n").unwrap();
    let mut database = Database::create(&path, PAGE_SIZE).unwrap();
    create_people(&mut database);
    database
        .execute(&Statement::CopyNode {
            table: "Person".to_owned(),
            path: path_text(&csv),
            sort_by: None,
        })
        .unwrap();
    assert_eq!(database.run(&scan("Person")).unwrap().rows.len(), 2);

    database
        .execute(&Statement::CreateNodeTable {
            name: "Corpus".to_owned(),
            columns: vec![
                column("id", LogicalType::Int64, true),
                column("embedding", LogicalType::Vector { dim: 2 }, false),
            ],
        })
        .unwrap();
    database
        .execute(&Statement::InsertNode {
            table: "Corpus".to_owned(),
            rows: vec![
                vec![Value::Int64(1), Value::Vector(vec![0.0, 0.0])],
                vec![Value::Int64(2), Value::Vector(vec![5.0, 5.0])],
            ],
        })
        .unwrap();
    database
        .execute(&Statement::CreateHnswIndex {
            name: "corpus_embedding_l2".to_owned(),
            table: "Corpus".to_owned(),
            column: "embedding".to_owned(),
            metric: Metric::L2,
        })
        .unwrap();
    drop(database);

    let mut reopened = Database::open(&path).unwrap();
    let nearest = reopened
        .run(&Plan {
            v: 0,
            plan: Operator::KnnScan {
                table: "Corpus".to_owned(),
                column: "embedding".to_owned(),
                query: vec![0.0, 0.0].into(),
                k: 1,
                metric: Metric::L2,
                mode: KnnMode::Approximate,
            },
        })
        .unwrap();
    assert_eq!(nearest.rows.len(), 1);
    assert_eq!(nearest.rows[0][0], Value::Int64(1));
}

fn path_text(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}
