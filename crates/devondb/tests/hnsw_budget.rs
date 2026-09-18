//! Public-facade gates for the HNSW hard-budget fallback contract.
//!
//! The corpus reuses `tests/knn.rs`'s pinned LCG multiplier
//! `6364136223846793005`, increment `1442695040888963407`, corpus seed
//! `0xd3_70_db_20_00`, L2 query seed `0x12_34_56_78_9a_bc_de_f0`, and
//! cosine query seed `0x0c_05_1e_20_26`. The row count and wider dimension
//! are calibrated together: at the minimum public budget the materialized
//! rows fit, but rows plus the ANN arenas do not, while the streaming exact
//! top-k remains comfortably bounded.

use std::{
    env,
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use devondb::{Database, Options, Plan, QueryResult, Statement};
use devondb_plan::{
    expr::Metric,
    ops::{KnnMode, Operator, PLAN_VERSION},
};
use devondb_storage::pager::Pager;
use devondb_types::{logical_type::LogicalType, schema::Column, value::Value};

const PAGE_SIZE: u32 = 4096;
const TIGHT_LIMIT: usize = 1024 * 1024;
const LARGE_LIMIT: usize = 32 * 1024 * 1024;
const TABLE: &str = "Corpus";
const VECTOR_COLUMN: &str = "embedding";

const FALLBACK_DB_ID: [u8; 16] = *b"hnsw-budget-fall";
const TAIL_DB_ID: [u8; 16] = *b"hnsw-budget-tail";

const LCG_MULTIPLIER: u64 = 6_364_136_223_846_793_005;
const LCG_INCREMENT: u64 = 1_442_695_040_888_963_407;
const CORPUS_SEED: u64 = 0xd3_70_db_20_00;
const L2_QUERY_SEED: u64 = 0x12_34_56_78_9a_bc_de_f0;
const COSINE_QUERY_SEED: u64 = 0x0c_05_1e_20_26;
const FALLBACK_ROWS: usize = 1_600;
const FALLBACK_DIM: usize = 64;
const FALLBACK_PAYLOAD_BYTES: usize = 235;
const K_VALUES: [u64; 3] = [1, 10, 100];
const ROW_ID_BASE: i64 = 10_000;

const TAIL_DIM: usize = 256;
const INDEXED_ROWS: usize = 64;
const TAIL_ROWS: usize = 230;
const NEAR_TAIL_ROWS: usize = 32;
const TAIL_ID_BASE: i64 = 100_000;
const TRIGGER_ID: i64 = 1_000_000;

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
            "devondb-hnsw-budget-{label}-{}-{timestamp}-{sequence}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("create test directory");
        Self { path }
    }

    fn database(&self) -> PathBuf {
        self.path.join("db.devondb")
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

struct Lcg(u64);

impl Lcg {
    const fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn next_f32(&mut self) -> f32 {
        self.0 = self
            .0
            .wrapping_mul(LCG_MULTIPLIER)
            .wrapping_add(LCG_INCREMENT);
        let fraction = (self.0 >> 40) as f32 / (1_u32 << 24) as f32;
        2.0 * fraction - 1.0
    }

    fn vector(&mut self, dimension: usize) -> Vec<f32> {
        (0..dimension).map(|_| self.next_f32()).collect()
    }
}

struct FallbackCorpus {
    vectors: Vec<Vec<f32>>,
    l2_query: Vec<f32>,
    cosine_query: Vec<f32>,
}

impl FallbackCorpus {
    fn generate() -> Self {
        let mut corpus = Lcg::new(CORPUS_SEED);
        Self {
            vectors: (0..FALLBACK_ROWS)
                .map(|_| corpus.vector(FALLBACK_DIM))
                .collect(),
            l2_query: Lcg::new(L2_QUERY_SEED).vector(FALLBACK_DIM),
            cosine_query: Lcg::new(COSINE_QUERY_SEED).vector(FALLBACK_DIM),
        }
    }

    fn query(&self, metric: Metric) -> &[f32] {
        match metric {
            Metric::L2 => &self.l2_query,
            Metric::Cosine => &self.cosine_query,
        }
    }
}

#[derive(Debug)]
struct Neighbor {
    id: i64,
    distance: f64,
}

#[test]
fn ann_arena_pressure_falls_back_exactly_without_spilling_or_leaking_budget() {
    let corpus = FallbackCorpus::generate();
    for metric in [Metric::L2, Metric::Cosine] {
        assert_metric_falls_back(&corpus, metric);
    }
}

fn assert_metric_falls_back(corpus: &FallbackCorpus, metric: Metric) {
    let directory = TestDirectory::new(metric_name(metric));
    let path = directory.database();
    seed_indexed_corpus(&path, corpus, metric);
    let mut database = open_with_limit(&path, TIGHT_LIMIT);

    for k in K_VALUES {
        let query = corpus.query(metric);
        let approximate = database
            .run(&knn_plan(query, k, metric, KnnMode::Approximate))
            .expect("approximate query falls back within the hard budget");
        assert_no_spill(&path);

        // This immediately following query is the public proof that the failed
        // ANN reservation returned to baseline rather than leaking a charge.
        let exact = database
            .run(&knn_plan(query, k, metric, KnnMode::Exact))
            .expect("subsequent query succeeds after ANN fallback");
        assert_eq!(
            approximate,
            exact,
            "fallback output differs from exact mode for {} k={k}",
            metric_name(metric)
        );
        assert_matches_scalar(&approximate, &corpus.vectors, query, metric, k);
        assert_no_spill(&path);
    }
}

#[test]
fn checkpoint_budget_pressure_keeps_exact_tail_visible_then_catches_up() {
    let directory = TestDirectory::new("checkpoint-tail");
    let path = directory.database();
    seed_tail_index(&path);

    let query = vec![0.0; TAIL_DIM];
    let nearest_id = TAIL_ID_BASE + i64::try_from(TAIL_ROWS - 1).expect("tail id fits");
    let mut tight = open_with_limit(&path, TIGHT_LIMIT);
    insert_tail_rows(&mut tight);
    tight
        .checkpoint()
        .expect("tight checkpoint publishes its contiguous catch-up prefix");
    drop(tight);

    let mut recovered = open_with_limit(&path, TIGHT_LIMIT);
    let before = recovered
        .run(&knn_plan(&query, 8, Metric::L2, KnnMode::Approximate))
        .expect("uncovered rows remain searchable after reopen");
    let expected_tail_ids = (TAIL_ROWS - 8..TAIL_ROWS)
        .rev()
        .map(|offset| TAIL_ID_BASE + i64::try_from(offset).expect("tail id fits"))
        .collect::<Vec<_>>();
    assert_eq!(result_ids(&before), expected_tail_ids);
    assert_eq!(result_ids(&before).first(), Some(&nearest_id));
    assert_nearest_is_strictly_closer_than_indexed(&before);

    let exact = recovered
        .run(&knn_plan(&query, 8, Metric::L2, KnnMode::Exact))
        .expect("exact query succeeds after tail-union query");
    assert_eq!(before, exact, "exact-tail union changed the visible top-k");
    assert_no_spill(&path);

    // A small WAL tail makes the next public checkpoint retry index catch-up;
    // this row is deliberately too far away to change the asserted top-k.
    insert_one(&mut recovered, TRIGGER_ID, constant_vector(10_000.0));
    drop(recovered);

    let mut roomy = open_with_limit(&path, LARGE_LIMIT);
    roomy
        .checkpoint()
        .expect("larger budget completes checkpoint tail catch-up");
    let after = roomy
        .run(&knn_plan(&query, 8, Metric::L2, KnnMode::Approximate))
        .expect("query succeeds after full catch-up");
    assert_eq!(after, before, "catch-up changed row visibility or ranking");
    assert_no_spill(&path);
}

fn seed_indexed_corpus(path: &Path, corpus: &FallbackCorpus, metric: Metric) {
    // Pin the db_id: HNSW level draws are seeded from it, so a random id from
    // `Database::create` gives every run a differently shaped graph — and the
    // budgets here are deliberately calibrated near the limit, so an unlucky
    // shape can make the test flaky. A fixed id makes the graph reproducible.
    drop(Pager::create(path, PAGE_SIZE, FALLBACK_DB_ID).expect("create fallback database"));
    let mut database = Database::open(path).expect("open fallback database");
    create_fallback_table(&mut database);
    let payload = "x".repeat(FALLBACK_PAYLOAD_BYTES);
    let rows = corpus
        .vectors
        .iter()
        .enumerate()
        .map(|(offset, vector)| {
            vec![
                Value::Int64(row_id(offset)),
                Value::Vector(vector.clone()),
                Value::String(payload.clone()),
            ]
        })
        .collect();
    database
        .execute(&Statement::InsertNode {
            table: TABLE.to_owned(),
            rows,
        })
        .expect("insert fallback corpus");
    create_index(&mut database, metric);
}

fn seed_tail_index(path: &Path) {
    // Pinned for the same reason as `seed_indexed_corpus`: reproducible graph
    // shape under a knife-edge budget.
    drop(Pager::create(path, PAGE_SIZE, TAIL_DB_ID).expect("create tail database"));
    let mut database = Database::open(path).expect("open tail database");
    create_table(&mut database, TAIL_DIM);
    let rows = (0..INDEXED_ROWS)
        .map(|offset| {
            vec![
                Value::Int64(i64::try_from(offset).expect("base id fits")),
                Value::Vector(constant_vector(100.0 + offset as f32)),
            ]
        })
        .collect();
    database
        .execute(&Statement::InsertNode {
            table: TABLE.to_owned(),
            rows,
        })
        .expect("insert indexed prefix");
    create_index(&mut database, Metric::L2);
}

fn insert_tail_rows(database: &mut Database) {
    let far_rows = TAIL_ROWS - NEAR_TAIL_ROWS;
    let rows = (0..TAIL_ROWS)
        .map(|offset| {
            let vector = if offset < far_rows {
                constant_vector(1_000.0 + offset as f32)
            } else {
                let step = TAIL_ROWS - 1 - offset;
                let mut vector = vec![0.0; TAIL_DIM];
                vector[0] = step as f32 / 100.0;
                vector
            };
            vec![
                Value::Int64(TAIL_ID_BASE + i64::try_from(offset).expect("tail id fits")),
                Value::Vector(vector),
            ]
        })
        .collect();
    database
        .execute(&Statement::InsertNode {
            table: TABLE.to_owned(),
            rows,
        })
        .expect("commit rows even when HNSW proposal exceeds its budget");
}

fn create_table(database: &mut Database, dimension: usize) {
    database
        .execute(&Statement::CreateNodeTable {
            name: TABLE.to_owned(),
            columns: vec![
                Column {
                    name: "id".to_owned(),
                    ty: LogicalType::Int64,
                    primary_key: true,
                },
                Column {
                    name: VECTOR_COLUMN.to_owned(),
                    ty: LogicalType::Vector {
                        dim: u32::try_from(dimension).expect("test dimension fits u32"),
                    },
                    primary_key: false,
                },
            ],
        })
        .expect("create corpus table");
}

fn create_fallback_table(database: &mut Database) {
    database
        .execute(&Statement::CreateNodeTable {
            name: TABLE.to_owned(),
            columns: vec![
                Column {
                    name: "id".to_owned(),
                    ty: LogicalType::Int64,
                    primary_key: true,
                },
                Column {
                    name: VECTOR_COLUMN.to_owned(),
                    ty: LogicalType::Vector {
                        dim: FALLBACK_DIM as u32,
                    },
                    primary_key: false,
                },
                Column {
                    name: "payload".to_owned(),
                    ty: LogicalType::String,
                    primary_key: false,
                },
            ],
        })
        .expect("create fallback corpus table");
}

fn create_index(database: &mut Database, metric: Metric) {
    database
        .execute(&Statement::CreateHnswIndex {
            name: format!("corpus_embedding_{}", metric_name(metric)),
            table: TABLE.to_owned(),
            column: VECTOR_COLUMN.to_owned(),
            metric,
        })
        .expect("create HNSW index as the sole statement of its execute call");
}

fn insert_one(database: &mut Database, id: i64, vector: Vec<f32>) {
    database
        .execute(&Statement::InsertNode {
            table: TABLE.to_owned(),
            rows: vec![vec![Value::Int64(id), Value::Vector(vector)]],
        })
        .expect("insert checkpoint trigger row");
}

fn open_with_limit(path: &Path, memory_limit: usize) -> Database {
    Database::open_with(
        path,
        Options {
            page_size: PAGE_SIZE,
            memory_limit,
        },
    )
    .expect("open database with explicit memory limit")
}

fn knn_plan(query: &[f32], k: u64, metric: Metric, mode: KnnMode) -> Plan {
    Plan {
        v: PLAN_VERSION,
        plan: Operator::KnnScan {
            table: TABLE.to_owned(),
            column: VECTOR_COLUMN.to_owned(),
            query: query.to_vec().into(),
            k,
            metric,
            mode,
        },
    }
}

fn assert_matches_scalar(
    result: &QueryResult,
    vectors: &[Vec<f32>],
    query: &[f32],
    metric: Metric,
    k: u64,
) {
    let actual = fallback_result_neighbors(result);
    let expected = scalar_ground_truth(vectors, query, metric, k);
    assert_eq!(
        actual
            .iter()
            .map(|neighbor| neighbor.id)
            .collect::<Vec<_>>(),
        expected
            .iter()
            .map(|neighbor| neighbor.id)
            .collect::<Vec<_>>(),
        "fallback ids differ from scalar ground truth for {} k={k}",
        metric_name(metric)
    );
    for (actual, expected) in actual.iter().zip(expected) {
        assert_close(actual.distance, expected.distance);
    }
}

fn scalar_ground_truth(
    vectors: &[Vec<f32>],
    query: &[f32],
    metric: Metric,
    k: u64,
) -> Vec<Neighbor> {
    let mut neighbors = vectors
        .iter()
        .enumerate()
        .map(|(offset, vector)| Neighbor {
            id: row_id(offset),
            distance: f64::from(scalar_distance(vector, query, metric)),
        })
        .collect::<Vec<_>>();
    neighbors.sort_by(|left, right| {
        left.distance
            .total_cmp(&right.distance)
            .then_with(|| left.id.cmp(&right.id))
    });
    neighbors.truncate(usize::try_from(k).expect("test k fits usize"));
    neighbors
}

fn scalar_distance(left: &[f32], right: &[f32], metric: Metric) -> f32 {
    match metric {
        Metric::L2 => left
            .iter()
            .zip(right)
            .fold(0.0_f32, |sum, (left, right)| {
                let difference = left - right;
                sum + difference * difference
            })
            .sqrt(),
        Metric::Cosine => {
            let (dot, left_norm, right_norm) = left.iter().zip(right).fold(
                (0.0_f32, 0.0_f32, 0.0_f32),
                |(dot, left_norm, right_norm), (left, right)| {
                    (
                        dot + left * right,
                        left_norm + left * left,
                        right_norm + right * right,
                    )
                },
            );
            1.0 - dot / (left_norm.sqrt() * right_norm.sqrt())
        }
    }
}

fn result_neighbors(result: &QueryResult, dimension: usize) -> Vec<Neighbor> {
    result
        .rows
        .iter()
        .map(|row| match row.as_slice() {
            [
                Value::Int64(id),
                Value::Vector(vector),
                Value::Float64(distance),
            ] => {
                assert_eq!(vector.len(), dimension);
                Neighbor {
                    id: *id,
                    distance: *distance,
                }
            }
            other => panic!("unexpected KNN row: {other:?}"),
        })
        .collect()
}

fn fallback_result_neighbors(result: &QueryResult) -> Vec<Neighbor> {
    result
        .rows
        .iter()
        .map(|row| match row.as_slice() {
            [
                Value::Int64(id),
                Value::Vector(vector),
                Value::String(payload),
                Value::Float64(distance),
            ] => {
                assert_eq!(vector.len(), FALLBACK_DIM);
                assert_eq!(payload.len(), FALLBACK_PAYLOAD_BYTES);
                Neighbor {
                    id: *id,
                    distance: *distance,
                }
            }
            other => panic!("unexpected fallback KNN row: {other:?}"),
        })
        .collect()
}

fn result_ids(result: &QueryResult) -> Vec<i64> {
    result_neighbors(result, TAIL_DIM)
        .into_iter()
        .map(|neighbor| neighbor.id)
        .collect()
}

fn assert_nearest_is_strictly_closer_than_indexed(result: &QueryResult) {
    let neighbors = result_neighbors(result, TAIL_DIM);
    let nearest = neighbors.first().expect("tail query returns a nearest row");
    assert_eq!(nearest.distance, 0.0);
    let nearest_indexed_distance = (TAIL_DIM as f64).sqrt() * 100.0;
    assert!(nearest.distance < nearest_indexed_distance);
}

fn constant_vector(value: f32) -> Vec<f32> {
    vec![value; TAIL_DIM]
}

fn row_id(offset: usize) -> i64 {
    ROW_ID_BASE + i64::try_from(offset).expect("row id fits i64")
}

fn metric_name(metric: Metric) -> &'static str {
    match metric {
        Metric::L2 => "l2",
        Metric::Cosine => "cosine",
    }
}

fn assert_close(actual: f64, expected: f64) {
    let tolerance = 1.0e-5 * expected.abs().max(1.0);
    assert!(
        (actual - expected).abs() <= tolerance,
        "actual {actual}, expected {expected}, tolerance {tolerance}"
    );
}

fn assert_no_spill(database_path: &Path) {
    let spill_path = spill_tmp_path(database_path);
    if !spill_path.exists() {
        return;
    }
    // Every live handle owns a locked subdirectory here, so the
    // directory is never empty while the database is open — only `.run`
    // files are evidence of an actual spill.
    let mut runs = Vec::new();
    for entry in fs::read_dir(&spill_path).expect("read database spill directory") {
        let path = entry.expect("read spill entry").path();
        if !path.is_dir() {
            runs.push(path);
            continue;
        }
        for inner in fs::read_dir(&path).expect("read spill handle directory") {
            let inner = inner.expect("read spill handle entry").path();
            if inner
                .extension()
                .is_some_and(|extension| extension == "run")
            {
                runs.push(inner);
            }
        }
    }
    assert!(
        runs.is_empty(),
        "HNSW query created spill files under {}: {runs:?}",
        spill_path.display()
    );
}

fn spill_tmp_path(path: &Path) -> PathBuf {
    let mut path = OsString::from(path.as_os_str());
    path.push(".tmp");
    PathBuf::from(path)
}
