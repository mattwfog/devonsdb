use devondb::{Database, QueryResult, txn::Snapshot};
use devondb_plan::{
    expr::Metric,
    ops::{KnnMode, Operator, Plan},
    statement::Statement,
};
use devondb_types::{logical_type::LogicalType, schema::Column, value::Value};
use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

const PAGE_SIZE: u32 = 4096;
const DIMENSION: u32 = 4;
static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct TestDirectory {
    path: PathBuf,
}

impl TestDirectory {
    fn new() -> Self {
        let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "devondb-hnsw-facade-{}-{timestamp}-{sequence}",
            std::process::id()
        ));
        fs::create_dir(&path).unwrap();
        Self { path }
    }

    fn database(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn column(name: &str, ty: LogicalType, primary_key: bool) -> Column {
    Column {
        name: name.to_owned(),
        ty,
        primary_key,
    }
}

fn create_corpus(database: &mut Database) {
    database
        .execute(&Statement::CreateNodeTable {
            name: "Corpus".to_owned(),
            columns: vec![
                column("id", LogicalType::Int64, true),
                column("embedding", LogicalType::Vector { dim: DIMENSION }, false),
            ],
        })
        .unwrap();
}

fn insert_rows(database: &mut Database, first: i64, count: usize) {
    let rows = (0..count)
        .map(|offset| {
            let id = first + offset as i64;
            vec![
                Value::Int64(id),
                Value::Vector(vec![id as f32, id as f32 / 2.0, 1.0, -1.0]),
            ]
        })
        .collect();
    database
        .execute(&Statement::InsertNode {
            table: "Corpus".to_owned(),
            rows,
        })
        .unwrap();
}

fn insert_vector(database: &mut Database, id: i64, vector: Vec<f32>) {
    database
        .execute(&Statement::InsertNode {
            table: "Corpus".to_owned(),
            rows: vec![vec![Value::Int64(id), Value::Vector(vector)]],
        })
        .unwrap();
}

fn create_index(database: &mut Database) {
    database
        .execute(&Statement::CreateHnswIndex {
            name: "corpus_embedding_l2".to_owned(),
            table: "Corpus".to_owned(),
            column: "embedding".to_owned(),
            metric: Metric::L2,
        })
        .unwrap();
}

fn knn(mode: KnnMode, query: Vec<f32>, k: u64) -> Plan {
    Plan {
        v: 0,
        plan: Operator::KnnScan {
            table: "Corpus".to_owned(),
            column: "embedding".to_owned(),
            query: query.into(),
            k,
            metric: Metric::L2,
            mode,
        },
    }
}

fn result_ids(database: &mut Database, mode: KnnMode, query: Vec<f32>, k: u64) -> Vec<i64> {
    ids(database.run(&knn(mode, query, k)).unwrap())
}

fn snapshot_ids(snapshot: &Snapshot, mode: KnnMode, query: Vec<f32>, k: u64) -> Vec<i64> {
    ids(snapshot.run(&knn(mode, query, k)).unwrap())
}

fn ids(result: QueryResult) -> Vec<i64> {
    result
        .rows
        .into_iter()
        .map(|row| match row.first() {
            Some(Value::Int64(id)) => *id,
            other => panic!("unexpected HNSW id value: {other:?}"),
        })
        .collect()
}

#[test]
fn feature_bit_round_trip_opens_indexed_file_writable() {
    let directory = TestDirectory::new();
    let path = directory.database("feature-bit.devondb");
    let mut database = Database::create(&path, PAGE_SIZE).unwrap();
    create_corpus(&mut database);
    insert_rows(&mut database, 0, 8);
    create_index(&mut database);
    drop(database);

    let mut reopened = Database::open(&path).unwrap();
    insert_rows(&mut reopened, 8, 1);
    reopened.checkpoint().unwrap();
}

#[test]
fn approximate_scan_uses_published_index_and_matches_exact_small_n() {
    let directory = TestDirectory::new();
    let path = directory.database("small-n.devondb");
    let mut database = Database::create(&path, PAGE_SIZE).unwrap();
    create_corpus(&mut database);
    create_index(&mut database);
    insert_rows(&mut database, 0, 512);

    let query = vec![217.0, 108.5, 1.0, -1.0];
    let mut exact = result_ids(&mut database, KnnMode::Exact, query.clone(), 10);
    let mut approximate = result_ids(&mut database, KnnMode::Approximate, query, 10);
    exact.sort_unstable();
    approximate.sort_unstable();
    assert_eq!(approximate, exact);
}

#[test]
fn pinned_snapshot_is_unchanged_while_fresh_snapshot_sees_indexed_commit() {
    let directory = TestDirectory::new();
    let path = directory.database("snapshot.devondb");
    let mut database = Database::create(&path, PAGE_SIZE).unwrap();
    create_corpus(&mut database);
    insert_rows(&mut database, 0, 32);
    create_index(&mut database);

    let query = vec![1000.0, 500.0, 1.0, -1.0];
    let pinned = database.snapshot();
    let before = snapshot_ids(&pinned, KnnMode::Approximate, query.clone(), 1);
    insert_vector(&mut database, 1000, query.clone());
    let after = snapshot_ids(&pinned, KnnMode::Approximate, query.clone(), 1);
    let fresh = result_ids(&mut database, KnnMode::Approximate, query, 1);

    assert_eq!(after, before);
    assert_eq!(fresh, vec![1000]);
}

#[test]
fn an_existing_recovery_tail_keeps_later_commits_visible() {
    let directory = TestDirectory::new();
    let path = directory.database("tail-visible.devondb");
    let mut database = Database::create(&path, PAGE_SIZE).unwrap();
    create_corpus(&mut database);
    insert_rows(&mut database, 0, 32);
    create_index(&mut database);
    insert_vector(&mut database, 1000, vec![1000.0, 500.0, 1.0, -1.0]);
    drop(database);

    let mut recovered = Database::open(&path).unwrap();
    let query = vec![1001.0, 500.5, 1.0, -1.0];
    insert_vector(&mut recovered, 1001, query.clone());
    assert_eq!(
        result_ids(&mut recovered, KnnMode::Approximate, query, 1),
        vec![1001]
    );
}

#[test]
fn checkpoint_and_reopen_preserve_approximate_results() {
    let directory = TestDirectory::new();
    let path = directory.database("checkpoint-reopen.devondb");
    let mut database = Database::create(&path, PAGE_SIZE).unwrap();
    create_corpus(&mut database);
    insert_rows(&mut database, 0, 96);
    create_index(&mut database);
    insert_rows(&mut database, 96, 16);
    let query = vec![103.0, 51.5, 1.0, -1.0];
    let before = database
        .run(&knn(KnnMode::Approximate, query.clone(), 8))
        .unwrap();
    database.checkpoint().unwrap();
    drop(database);

    let mut reopened = Database::open(&path).unwrap();
    let after = reopened.run(&knn(KnnMode::Approximate, query, 8)).unwrap();
    assert_eq!(after, before);
}

#[test]
fn recovered_rows_without_checkpoint_participate_via_exact_tail() {
    let directory = TestDirectory::new();
    let path = directory.database("recovery-tail.devondb");
    let mut database = Database::create(&path, PAGE_SIZE).unwrap();
    create_corpus(&mut database);
    insert_rows(&mut database, 0, 32);
    create_index(&mut database);
    let query = vec![777.0, 388.5, 1.0, -1.0];
    insert_vector(&mut database, 777, query.clone());
    drop(database);

    let mut recovered = Database::open(&path).unwrap();
    assert_eq!(
        result_ids(&mut recovered, KnnMode::Approximate, query, 1),
        vec![777]
    );
}

#[test]
fn exact_mode_results_do_not_change_when_index_is_installed() {
    let directory = TestDirectory::new();
    let path = directory.database("exact-ignores.devondb");
    let mut database = Database::create(&path, PAGE_SIZE).unwrap();
    create_corpus(&mut database);
    insert_rows(&mut database, 0, 64);
    let query = vec![31.0, 15.5, 1.0, -1.0];
    let before = database
        .run(&knn(KnnMode::Exact, query.clone(), 7))
        .unwrap();
    create_index(&mut database);
    let after = database.run(&knn(KnnMode::Exact, query, 7)).unwrap();
    assert_eq!(after, before);
}
