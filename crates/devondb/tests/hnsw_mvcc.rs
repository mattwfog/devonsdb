//! Public-facade gates for `docs/HNSW.md` section 9.3.
//!
//! Fixtures use the KNN corpus LCG verbatim: multiplier
//! `6364136223846793005`, increment `1442695040888963407`, corpus seed
//! `0xd3_70_db_20_00`, and L2 query seed `0x12_34_56_78_9a_bc_de_f0`.

use std::{
    collections::BTreeSet,
    env,
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    sync::{Arc, Barrier},
    thread,
    time::{SystemTime, UNIX_EPOCH},
};

use devondb::{Database, DevonError, Options, QueryResult};
use devondb_plan::{
    expr::Metric,
    ops::{KnnMode, Operator, PLAN_VERSION, Plan},
    statement::Statement,
};
use devondb_types::{logical_type::LogicalType, schema::Column, value::Value};

const PAGE_SIZE: u32 = 4096;
const DIMENSION: usize = 16;
const BASE_ROWS: usize = 96;
const BASE_ID: i64 = 10_000;
const LCG_MULTIPLIER: u64 = 6_364_136_223_846_793_005;
const LCG_INCREMENT: u64 = 1_442_695_040_888_963_407;
const CORPUS_SEED: u64 = 0xd3_70_db_20_00;
const QUERY_SEED: u64 = 0x12_34_56_78_9a_bc_de_f0;
const DELTA_SEED: u64 = 0x0c_05_1e_20_26;
const TIGHT_MEMORY_LIMIT: usize = 1024 * 1024;
const BUDGET_HOLD_BYTES: usize = 880 * 1024;
const BUDGET_ROWS: usize = 1024;
const BUDGET_DIMENSION: usize = 32;

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
            "devondb-hnsw-mvcc-{label}-{}-{timestamp}-{sequence}",
            std::process::id()
        ));
        fs::create_dir(&path).unwrap();
        Self { path }
    }

    fn file(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }

    fn database(&self) -> PathBuf {
        self.file("db.devondb")
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

struct Lcg(u64);

impl Lcg {
    fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn vector(&mut self, dimension: usize) -> Vec<f32> {
        (0..dimension)
            .map(|_| {
                self.0 = self
                    .0
                    .wrapping_mul(LCG_MULTIPLIER)
                    .wrapping_add(LCG_INCREMENT);
                let fraction = (self.0 >> 40) as f32 / (1_u32 << 24) as f32;
                2.0 * fraction - 1.0
            })
            .collect()
    }
}

#[test]
fn snapshot_pins_rows_root_and_delta_chain_as_one_view() {
    // Kills HNSW section 4.3: a snapshot cannot mix fresh topology with old rows.
    let (_directory, database) = indexed_database("snapshot", BASE_ROWS, DIMENSION);
    let query = query_vector(DIMENSION);
    let plan = approximate_plan(&query, 8);
    let pinned = database.snapshot();
    let before = pinned.run(&plan).unwrap();

    let mut committed = seeded_rows(20_000, 3, DIMENSION, DELTA_SEED);
    committed.push(row(20_003, query.clone()));
    commit_rows(&database, committed);

    let after = pinned.run(&plan).unwrap();
    let fresh = database.snapshot().run(&plan).unwrap();
    assert_eq!(neighbor_signature(&after), neighbor_signature(&before));
    assert_nearest(&fresh, 20_003);
}

#[test]
fn proposal_budget_failure_commits_base_row_into_exact_tail() {
    // Kills HNSW sections 5.2(5) and 4.2(7): proposal failure must leave a tail.
    let directory = TestDirectory::new("proposal-budget-tail");
    let path = directory.database();
    seed_budget_database(&path);
    let mut database = Database::open_with(
        &path,
        Options {
            page_size: PAGE_SIZE,
            memory_limit: TIGHT_MEMORY_LIMIT,
        },
    )
    .unwrap();
    let mut budget_holder = database.begin().unwrap();
    budget_holder.execute(&budget_hold_insert()).unwrap();

    let query = query_vector(BUDGET_DIMENSION);
    database
        .execute(&insert_statement(vec![row(90_000, query.clone())]))
        .unwrap();
    budget_holder.abort();

    let result = database
        .run(&approximate_plan(&query, 1))
        .expect("fresh approximate query must fit after releasing the temporary charge");
    assert_nearest(&result, 90_000);
}

#[test]
fn checkpoint_copy_on_write_preserves_the_pinned_root_pages() {
    // Kills HNSW section 5.5(3-9): checkpoint must not overwrite pinned pages.
    let (_directory, mut database) = indexed_database("checkpoint-cow", BASE_ROWS, DIMENSION);
    let query = query_vector(DIMENSION);
    let plan = approximate_plan(&query, 8);
    let pinned = database.snapshot();
    let before = pinned.run(&plan).unwrap();

    let mut committed = seeded_rows(21_000, 5, DIMENSION, DELTA_SEED);
    committed.push(row(21_005, query.clone()));
    commit_rows(&database, committed);
    database.checkpoint().unwrap();

    let after = pinned.run(&plan).unwrap();
    let fresh = database.snapshot().run(&plan).unwrap();
    assert_eq!(neighbor_signature(&after), neighbor_signature(&before));
    assert_nearest(&fresh, 21_005);
}

#[test]
fn checkpointed_root_reopens_with_identical_ann_ids_and_distances() {
    // Kills HNSW section 5.5 publication determinism across a clean reopen.
    let (directory, mut database) = indexed_database("clean-reopen", BASE_ROWS, DIMENSION);
    let query = query_vector(DIMENSION);
    commit_rows(&database, seeded_rows(22_000, 7, DIMENSION, DELTA_SEED));
    commit_rows(&database, vec![row(22_007, query.clone())]);
    let plan = approximate_plan(&query, 12);
    let before = database.run(&plan).unwrap();

    database.checkpoint().unwrap();
    drop(database);
    let mut reopened = Database::open(directory.database()).unwrap();
    let after = reopened.run(&plan).unwrap();

    assert_eq!(neighbor_signature(&after), neighbor_signature(&before));
}

#[test]
fn crash_copy_before_checkpoint_recovers_every_ack_and_searches_wal_tail() {
    // Kills HNSW section 5.4: recovery must expose every post-root WAL row as tail.
    let (directory, database) = indexed_database("crash-before", BASE_ROWS, DIMENSION);
    let source = directory.database();
    let crash = directory.file("crash-before-copy.devondb");
    let query = query_vector(DIMENSION);
    let first = seeded_rows(23_000, 3, DIMENSION, DELTA_SEED);
    let mut second = seeded_rows(23_003, 3, DIMENSION, DELTA_SEED ^ 1);
    second.push(row(23_006, query.clone()));
    commit_rows(&database, first.clone());
    commit_rows(&database, second.clone());

    copy_live_database(&source, &crash);
    let mut recovered = Database::open(&crash).unwrap();
    let mut expected = base_ids(BASE_ROWS);
    expected.extend(row_ids(&first));
    expected.extend(row_ids(&second));
    expected.sort_unstable();

    assert_eq!(sorted_scan_ids(&mut recovered), expected);
    assert_nearest(
        &recovered.run(&approximate_plan(&query, 1)).unwrap(),
        23_006,
    );
}

#[test]
fn crash_copy_after_root_publish_skips_checkpointed_wal_without_duplicates() {
    // Kills FORMAT WAL rule 6 and HNSW section 5.5(7-8): published groups skip replay.
    let (directory, mut database) = indexed_database("crash-after", BASE_ROWS, DIMENSION);
    let source = directory.database();
    let crash = directory.file("crash-after-copy.devondb");
    let saved_wal = directory.file("pre-truncation.wal");
    let query = query_vector(DIMENSION);
    let mut committed = seeded_rows(24_000, 5, DIMENSION, DELTA_SEED);
    committed.push(row(24_005, query.clone()));
    commit_rows(&database, committed.clone());
    fs::copy(wal_path(&source), &saved_wal).unwrap();

    database.checkpoint().unwrap();
    let plan = approximate_plan(&query, 10);
    let published = database.run(&plan).unwrap();
    fs::copy(&source, &crash).unwrap();
    fs::copy(&saved_wal, wal_path(&crash)).unwrap();
    drop(database);

    let mut recovered = Database::open(&crash).unwrap();
    let ids = sorted_scan_ids(&mut recovered);
    let unique = ids.iter().copied().collect::<BTreeSet<_>>();
    assert_eq!(ids.len(), BASE_ROWS + committed.len());
    assert_eq!(unique.len(), ids.len());
    assert_eq!(
        neighbor_signature(&recovered.run(&plan).unwrap()),
        neighbor_signature(&published)
    );
}

#[test]
fn index_build_publication_keeps_racing_commits_in_the_exact_tail() {
    // Kills HNSW section 5.6: build publication must retain its older coverage.
    let directory = TestDirectory::new("build-race");
    let path = directory.database();
    let mut database = Database::create(&path, PAGE_SIZE).unwrap();
    create_corpus(&mut database, DIMENSION);
    database
        .execute(&insert_statement(seeded_rows(
            BASE_ID,
            BASE_ROWS,
            DIMENSION,
            CORPUS_SEED,
        )))
        .unwrap();
    let query = query_vector(DIMENSION);
    let mut builder = database.begin().unwrap();
    builder.execute(&create_index_statement()).unwrap();

    let mut racing = seeded_rows(25_000, 4, DIMENSION, DELTA_SEED);
    racing.push(row(25_004, query.clone()));
    commit_rows(&database, racing);
    builder.commit().unwrap();

    assert_nearest(&database.run(&approximate_plan(&query, 1)).unwrap(), 25_004);
}

#[test]
fn indexed_same_primary_key_writers_have_one_success_and_one_conflict() {
    // Kills HNSW section 5.3: derived adjacency must add no user conflict.
    let (_directory, mut database) = indexed_database("conflict", BASE_ROWS, DIMENSION);
    let query = query_vector(DIMENSION);
    let mut first = database.begin().unwrap();
    let mut second = database.begin().unwrap();
    let insert = insert_statement(vec![row(26_000, query.clone())]);
    first.execute(&insert).unwrap();
    second.execute(&insert).unwrap();
    let start = Arc::new(Barrier::new(3));
    let handles = [first, second].map(|transaction| {
        let start = Arc::clone(&start);
        thread::spawn(move || {
            start.wait();
            transaction.commit()
        })
    });
    start.wait();

    let results = handles.map(|handle| handle.join().unwrap());
    assert_one_success_one_conflict(results);
    let ids = sorted_scan_ids(&mut database);
    assert_eq!(ids.iter().filter(|id| **id == 26_000).count(), 1);
    assert_nearest(&database.run(&approximate_plan(&query, 1)).unwrap(), 26_000);
}

fn indexed_database(label: &str, row_count: usize, dimension: usize) -> (TestDirectory, Database) {
    let directory = TestDirectory::new(label);
    let mut database = Database::create(directory.database(), PAGE_SIZE).unwrap();
    create_corpus(&mut database, dimension);
    database
        .execute(&insert_statement(seeded_rows(
            BASE_ID,
            row_count,
            dimension,
            CORPUS_SEED,
        )))
        .unwrap();
    create_index(&mut database);
    (directory, database)
}

fn seed_budget_database(path: &Path) {
    let mut database = Database::create(path, PAGE_SIZE).unwrap();
    create_corpus(&mut database, BUDGET_DIMENSION);
    database
        .execute(&Statement::CreateNodeTable {
            name: "BudgetHold".to_owned(),
            columns: vec![
                column("id", LogicalType::Int64, true),
                column("payload", LogicalType::String, false),
            ],
        })
        .unwrap();
    database
        .execute(&insert_statement(seeded_rows(
            BASE_ID,
            BUDGET_ROWS,
            BUDGET_DIMENSION,
            CORPUS_SEED,
        )))
        .unwrap();
    create_index(&mut database);
}

fn create_corpus(database: &mut Database, dimension: usize) {
    database
        .execute(&Statement::CreateNodeTable {
            name: "Corpus".to_owned(),
            columns: vec![
                column("id", LogicalType::Int64, true),
                column(
                    "embedding",
                    LogicalType::Vector {
                        dim: u32::try_from(dimension).unwrap(),
                    },
                    false,
                ),
            ],
        })
        .unwrap();
}

fn create_index(database: &mut Database) {
    database.execute(&create_index_statement()).unwrap();
}

fn create_index_statement() -> Statement {
    Statement::CreateHnswIndex {
        name: "corpus_embedding_l2".to_owned(),
        table: "Corpus".to_owned(),
        column: "embedding".to_owned(),
        metric: Metric::L2,
    }
}

fn column(name: &str, ty: LogicalType, primary_key: bool) -> Column {
    Column {
        name: name.to_owned(),
        ty,
        primary_key,
    }
}

fn seeded_rows(first: i64, count: usize, dimension: usize, seed: u64) -> Vec<Vec<Value>> {
    let mut lcg = Lcg::new(seed);
    (0..count)
        .map(|offset| {
            row(
                first + i64::try_from(offset).unwrap(),
                lcg.vector(dimension),
            )
        })
        .collect()
}

fn query_vector(dimension: usize) -> Vec<f32> {
    Lcg::new(QUERY_SEED).vector(dimension)
}

fn row(id: i64, vector: Vec<f32>) -> Vec<Value> {
    vec![Value::Int64(id), Value::Vector(vector)]
}

fn insert_statement(rows: Vec<Vec<Value>>) -> Statement {
    Statement::InsertNode {
        table: "Corpus".to_owned(),
        rows,
    }
}

fn budget_hold_insert() -> Statement {
    Statement::InsertNode {
        table: "BudgetHold".to_owned(),
        rows: vec![vec![
            Value::Int64(1),
            Value::String("x".repeat(BUDGET_HOLD_BYTES)),
        ]],
    }
}

fn commit_rows(database: &Database, rows: Vec<Vec<Value>>) {
    let mut transaction = database.begin().unwrap();
    transaction.execute(&insert_statement(rows)).unwrap();
    transaction.commit().unwrap();
}

fn approximate_plan(query: &[f32], k: u64) -> Plan {
    Plan {
        v: PLAN_VERSION,
        plan: Operator::KnnScan {
            table: "Corpus".to_owned(),
            column: "embedding".to_owned(),
            query: query.to_vec().into(),
            k,
            metric: Metric::L2,
            mode: KnnMode::Approximate,
        },
    }
}

fn scan_plan() -> Plan {
    Plan {
        v: PLAN_VERSION,
        plan: Operator::ScanNodes {
            table: "Corpus".to_owned(),
            binding: "c".to_owned(),
        },
    }
}

fn neighbor_signature(result: &QueryResult) -> Vec<(i64, u64)> {
    result
        .rows
        .iter()
        .map(|row| match row.as_slice() {
            [Value::Int64(id), Value::Vector(_), Value::Float64(distance)] => {
                (*id, distance.to_bits())
            }
            other => panic!("unexpected approximate KNN row: {other:?}"),
        })
        .collect()
}

fn assert_nearest(result: &QueryResult, expected_id: i64) {
    let signature = neighbor_signature(result);
    assert_eq!(
        signature.first().map(|neighbor| neighbor.0),
        Some(expected_id)
    );
    assert_eq!(
        signature.first().map(|neighbor| neighbor.1),
        Some(0.0_f64.to_bits())
    );
}

fn sorted_scan_ids(database: &mut Database) -> Vec<i64> {
    let mut ids = database
        .run(&scan_plan())
        .unwrap()
        .rows
        .into_iter()
        .map(|row| match row.first() {
            Some(Value::Int64(id)) => *id,
            other => panic!("unexpected scan id: {other:?}"),
        })
        .collect::<Vec<_>>();
    ids.sort_unstable();
    ids
}

fn base_ids(count: usize) -> Vec<i64> {
    (0..count)
        .map(|offset| BASE_ID + i64::try_from(offset).unwrap())
        .collect()
}

fn row_ids(rows: &[Vec<Value>]) -> Vec<i64> {
    rows.iter()
        .map(|row| match row.first() {
            Some(Value::Int64(id)) => *id,
            other => panic!("unexpected fixture id: {other:?}"),
        })
        .collect()
}

fn copy_live_database(source: &Path, target: &Path) {
    fs::copy(source, target).unwrap();
    fs::copy(wal_path(source), wal_path(target)).unwrap();
}

fn wal_path(database: &Path) -> PathBuf {
    let mut path = OsString::from(database.as_os_str());
    path.push("-wal");
    PathBuf::from(path)
}

fn assert_one_success_one_conflict(results: [Result<(), DevonError>; 2]) {
    let mut successes = 0;
    let mut conflicts = 0;
    for result in results {
        match result {
            Ok(()) => successes += 1,
            Err(DevonError::TransactionConflict { .. }) => conflicts += 1,
            Err(other) => panic!("indexed PK contender returned the wrong error: {other}"),
        }
    }
    assert_eq!(successes, 1);
    assert_eq!(conflicts, 1);
}
