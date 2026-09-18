//! End-to-end KNN recall baseline through the public embedded facade.
//!
//! Future ANN work (including the HNSW wave and quantization) must cite this
//! corpus: LCG multiplier `6364136223846793005`, increment
//! `1442695040888963407`, corpus seed `0xd3_70_db_20_00`, L2 query seed
//! `0x12_34_56_78_9a_bc_de_f0`, cosine query seed `0x0c_05_1e_20_26`,
//! `N = 2_000`, `dim = 64`, metrics `{l2, cosine}`, and `k = {1, 10, 100}`.
//! Changing either seed invalidates cross-task recall comparisons.

use std::{
    env, fs,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use devondb::{Database, DevonError, Plan, QueryResult, Statement, text};
use devondb_plan::{
    expr::{BinaryOp, Expr, Metric},
    ops::{Operator, PLAN_VERSION},
    text::parser::Parsed,
};
use devondb_types::{logical_type::LogicalType, schema::Column, value::Value};

const PAGE_SIZE: u32 = 4096;
const TABLE: &str = "Corpus";
const VECTOR_COLUMN: &str = "embedding";
const N: usize = 2_000;
const DIM: usize = 64;
const K_VALUES: [u64; 3] = [1, 10, 100];
const CORPUS_SEED: u64 = 0xd3_70_db_20_00;
const L2_QUERY_SEED: u64 = 0x12_34_56_78_9a_bc_de_f0;
const COSINE_QUERY_SEED: u64 = 0x0c_05_1e_20_26;
const ROW_ID_BASE: i64 = 10_000;

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
            "devondb-knn-{label}-{timestamp}-{sequence}-{}",
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

struct Lcg(u64);

impl Lcg {
    fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn next_f32(&mut self) -> f32 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        let fraction = (self.0 >> 40) as f32 / (1_u32 << 24) as f32;
        2.0 * fraction - 1.0
    }

    fn vector(&mut self) -> Vec<f32> {
        (0..DIM).map(|_| self.next_f32()).collect()
    }
}

struct Corpus {
    vectors: Vec<Vec<f32>>,
    l2_query: Vec<f32>,
    cosine_query: Vec<f32>,
}

impl Corpus {
    fn generate() -> Self {
        let mut corpus_lcg = Lcg::new(CORPUS_SEED);
        let vectors = (0..N).map(|_| corpus_lcg.vector()).collect::<Vec<_>>();
        let l2_query = Lcg::new(L2_QUERY_SEED).vector();
        let cosine_query = Lcg::new(COSINE_QUERY_SEED).vector();
        assert_eq!(vectors.len(), N);
        assert!(vectors.iter().all(|vector| vector.len() == DIM));
        Self {
            vectors,
            l2_query,
            cosine_query,
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
fn programmatic_l2_is_exact_for_recall_baseline_k_values() {
    assert_programmatic_exact("programmatic-l2", Metric::L2);
}

#[test]
fn programmatic_cosine_is_exact_for_recall_baseline_k_values() {
    assert_programmatic_exact("programmatic-cosine", Metric::Cosine);
}

#[test]
fn text_l2_is_exact_for_recall_baseline_k_values() {
    assert_text_exact("text-l2", Metric::L2);
}

#[test]
fn text_cosine_is_exact_for_recall_baseline_k_values() {
    assert_text_exact("text-cosine", Metric::Cosine);
}

#[test]
fn knn_results_are_identical_after_checkpoint_and_reopen() {
    let (directory, mut database, corpus) = create_corpus_database("persistence");
    let mut before = Vec::new();
    for metric in [Metric::L2, Metric::Cosine] {
        for k in K_VALUES {
            let query = corpus.query(metric);
            before.push(database.run(&knn_plan(query, k, metric)).expect("run plan"));
            before.push(run_text_knn(&mut database, query, k, metric));
        }
    }

    database.checkpoint().expect("checkpoint corpus");
    drop(database);
    let mut reopened = Database::open(directory.db_path()).expect("reopen checkpointed corpus");

    let mut after = Vec::new();
    for metric in [Metric::L2, Metric::Cosine] {
        for k in K_VALUES {
            let query = corpus.query(metric);
            after.push(reopened.run(&knn_plan(query, k, metric)).expect("run plan"));
            after.push(run_text_knn(&mut reopened, query, k, metric));
        }
    }
    assert_eq!(after, before);
}

#[test]
fn knn_binding_composes_with_pk_and_explicit_distance_filters() {
    let (_directory, mut database, corpus) = create_corpus_database("composition");
    let query = corpus.query(Metric::L2);
    let query_text = vector_text(query);
    let raw = run_text_query(
        &mut database,
        &format!("knn({TABLE}.{VECTOR_COLUMN}, {query_text}, 100, l2)"),
    );
    let raw_neighbors = extract_neighbors(&raw);

    let id_threshold = ROW_ID_BASE + i64::try_from(N / 2).expect("N/2 fits i64");
    let projected = run_text_query(
        &mut database,
        &format!(
            "knn({TABLE}.{VECTOR_COLUMN}, {query_text}, 100, l2) | \
             filter {TABLE}.id > {id_threshold} | project {TABLE}.id"
        ),
    );
    let actual_ids = extract_single_id_column(&projected);
    let expected_ids = raw_neighbors
        .iter()
        .filter(|neighbor| neighbor.id > id_threshold)
        .map(|neighbor| neighbor.id)
        .collect::<Vec<_>>();
    assert!(
        !actual_ids.is_empty(),
        "PK filter should retain corpus neighbors"
    );
    assert!(actual_ids.len() < raw_neighbors.len());
    assert_eq!(actual_ids, expected_ids);

    let distance_threshold = middle_distance_threshold(&raw_neighbors, &corpus, query);
    let distance_expr = format!("distance({TABLE}.{VECTOR_COLUMN}, {query_text}, l2)");
    let distance_filtered = run_text_query(
        &mut database,
        &format!(
            "knn({TABLE}.{VECTOR_COLUMN}, {query_text}, 100, l2) | \
             filter {distance_expr} <= {distance_threshold:?} | \
             project {TABLE}.id as id, {distance_expr} as computed_distance"
        ),
    );
    assert_explicit_distances(
        &distance_filtered,
        &raw_neighbors,
        &corpus,
        query,
        distance_threshold,
    );
}

#[test]
fn knn_output_distance_column_is_not_referenceable() {
    let (_directory, mut database, corpus) = create_corpus_database("output-only");
    let plan = Plan {
        v: PLAN_VERSION,
        plan: Operator::Filter {
            predicate: Expr::Binary {
                op: BinaryOp::Lt,
                left: Box::new(Expr::Col("distance".to_owned())),
                right: Box::new(Expr::Lit(Value::Float64(1.0))),
            },
            input: Box::new(knn_operator(corpus.query(Metric::L2), 10, Metric::L2)),
        },
    };

    let DevonError::InvalidArgument { context } = database.run(&plan).expect_err("invalid plan")
    else {
        panic!("output-only distance reference should be invalid");
    };
    assert!(context.contains("distance"), "unexpected error: {context}");
    assert!(
        context.contains("binding.column"),
        "unexpected error: {context}"
    );
}

fn assert_programmatic_exact(label: &str, metric: Metric) {
    let (_directory, mut database, corpus) = create_corpus_database(label);
    for k in K_VALUES {
        let query = corpus.query(metric);
        let result = database.run(&knn_plan(query, k, metric)).expect("run plan");
        assert_matches_ground_truth(&result, &corpus.vectors, query, metric, k);
    }
}

fn assert_text_exact(label: &str, metric: Metric) {
    let (_directory, mut database, corpus) = create_corpus_database(label);
    for k in K_VALUES {
        let query = corpus.query(metric);
        let result = run_text_knn(&mut database, query, k, metric);
        assert_matches_ground_truth(&result, &corpus.vectors, query, metric, k);
    }
}

fn create_corpus_database(label: &str) -> (TestDirectory, Database, Corpus) {
    let directory = TestDirectory::new(label);
    let mut database = Database::create(directory.db_path(), PAGE_SIZE).expect("create database");
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
                    ty: LogicalType::Vector { dim: DIM as u32 },
                    primary_key: false,
                },
            ],
        })
        .expect("create corpus table");

    let corpus = Corpus::generate();
    let rows = corpus
        .vectors
        .iter()
        .enumerate()
        .map(|(row_order, vector)| {
            vec![
                Value::Int64(row_id(row_order)),
                Value::Vector(vector.clone()),
            ]
        })
        .collect();
    database
        .execute(&Statement::InsertNode {
            table: TABLE.to_owned(),
            rows,
        })
        .expect("insert corpus through embedded API");
    (directory, database, corpus)
}

fn knn_plan(query: &[f32], k: u64, metric: Metric) -> Plan {
    Plan {
        v: PLAN_VERSION,
        plan: knn_operator(query, k, metric),
    }
}

fn knn_operator(query: &[f32], k: u64, metric: Metric) -> Operator {
    Operator::KnnScan {
        table: TABLE.to_owned(),
        column: VECTOR_COLUMN.to_owned(),
        query: query.to_vec().into(),
        k,
        metric,
        mode: devondb_plan::ops::KnnMode::Exact,
    }
}

fn run_text_knn(database: &mut Database, query: &[f32], k: u64, metric: Metric) -> QueryResult {
    run_text_query(
        database,
        &format!(
            "knn({TABLE}.{VECTOR_COLUMN}, {}, {k}, {})",
            vector_text(query),
            metric_name(metric)
        ),
    )
}

fn run_text_query(database: &mut Database, input: &str) -> QueryResult {
    let plan = match text::parser::parse(input).expect("text query parses") {
        Parsed::Query(plan) => plan,
        Parsed::Statement(_) => panic!("expected query, parsed statement"),
    };
    database.run(&plan).expect("text query executes")
}

fn vector_text(vector: &[f32]) -> String {
    let elements = vector
        .iter()
        .map(|value| format!("{value:?}"))
        .collect::<Vec<_>>()
        .join(", ");
    format!("[{elements}]")
}

fn metric_name(metric: Metric) -> &'static str {
    match metric {
        Metric::L2 => "l2",
        Metric::Cosine => "cosine",
    }
}

fn assert_matches_ground_truth(
    result: &QueryResult,
    vectors: &[Vec<f32>],
    query: &[f32],
    metric: Metric,
    k: u64,
) {
    assert_eq!(
        result.columns,
        [
            format!("{TABLE}.id"),
            format!("{TABLE}.{VECTOR_COLUMN}"),
            "distance".to_owned(),
        ]
    );
    let actual = extract_neighbors(result);
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
        "KNN ids differ from exact scalar ground truth for {} k={k}",
        metric_name(metric)
    );
    assert!(
        actual
            .windows(2)
            .all(|pair| pair[0].distance.total_cmp(&pair[1].distance).is_le()),
        "KNN distances are not ascending for {} k={k}",
        metric_name(metric)
    );
    for (actual, expected) in actual.iter().zip(&expected) {
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
        .map(|(row_order, vector)| Neighbor {
            id: row_id(row_order),
            distance: f64::from(scalar_distance_f32(vector, query, metric)),
        })
        .collect::<Vec<_>>();
    neighbors.sort_by(|left, right| {
        left.distance
            .total_cmp(&right.distance)
            .then_with(|| left.id.cmp(&right.id))
    });
    neighbors.truncate(usize::try_from(k).expect("baseline k fits usize"));
    neighbors
}

fn scalar_distance_f32(left: &[f32], right: &[f32], metric: Metric) -> f32 {
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

fn scalar_l2_f64(left: &[f32], right: &[f32]) -> f64 {
    left.iter()
        .zip(right)
        .map(|(left, right)| {
            let difference = f64::from(*left) - f64::from(*right);
            difference * difference
        })
        .sum::<f64>()
        .sqrt()
}

fn extract_neighbors(result: &QueryResult) -> Vec<Neighbor> {
    result
        .rows
        .iter()
        .map(|row| match row.as_slice() {
            [
                Value::Int64(id),
                Value::Vector(vector),
                Value::Float64(distance),
            ] => {
                assert_eq!(vector.len(), DIM);
                Neighbor {
                    id: *id,
                    distance: *distance,
                }
            }
            other => panic!("unexpected KNN row: {other:?}"),
        })
        .collect()
}

fn extract_single_id_column(result: &QueryResult) -> Vec<i64> {
    assert_eq!(result.columns, [format!("{TABLE}.id")]);
    result
        .rows
        .iter()
        .map(|row| match row.as_slice() {
            [Value::Int64(id)] => *id,
            other => panic!("unexpected projected row: {other:?}"),
        })
        .collect()
}

fn middle_distance_threshold(raw: &[Neighbor], corpus: &Corpus, query: &[f32]) -> f64 {
    let mut distances = raw
        .iter()
        .map(|neighbor| scalar_l2_f64(vector_for_id(corpus, neighbor.id), query))
        .collect::<Vec<_>>();
    distances.sort_by(f64::total_cmp);
    let lower = distances[distances.len() / 2 - 1];
    let upper = distances[distances.len() / 2];
    assert!(lower < upper, "middle baseline distances must differ");
    lower / 2.0 + upper / 2.0
}

fn assert_explicit_distances(
    result: &QueryResult,
    raw: &[Neighbor],
    corpus: &Corpus,
    query: &[f32],
    threshold: f64,
) {
    assert_eq!(result.columns, ["id", "computed_distance"]);
    let expected_ids = raw
        .iter()
        .filter(|neighbor| scalar_l2_f64(vector_for_id(corpus, neighbor.id), query) <= threshold)
        .map(|neighbor| neighbor.id)
        .collect::<Vec<_>>();
    let mut actual_ids = Vec::new();
    for row in &result.rows {
        let [Value::Int64(id), Value::Float64(computed)] = row.as_slice() else {
            panic!("unexpected explicit-distance row: {row:?}");
        };
        let output = raw
            .iter()
            .find(|neighbor| neighbor.id == *id)
            .expect("filtered id came from KNN output");
        let scalar = scalar_l2_f64(vector_for_id(corpus, *id), query);
        assert_eq!(*computed, scalar);
        assert_close(*computed, output.distance);
        actual_ids.push(*id);
    }
    assert!(!actual_ids.is_empty());
    assert!(actual_ids.len() < raw.len());
    assert_eq!(actual_ids, expected_ids);
}

fn vector_for_id(corpus: &Corpus, id: i64) -> &[f32] {
    let index = usize::try_from(id - ROW_ID_BASE).expect("corpus id maps to row order");
    &corpus.vectors[index]
}

fn row_id(row_order: usize) -> i64 {
    ROW_ID_BASE + i64::try_from(row_order).expect("corpus row order fits i64")
}

fn assert_close(actual: f64, expected: f64) {
    let tolerance = 1.0e-5 * expected.abs().max(1.0);
    assert!(
        (actual - expected).abs() <= tolerance,
        "actual {actual}, expected {expected}, tolerance {tolerance}"
    );
}

const B1_ROTATION_SEED: u64 = 0x48_4e_53_57_20_26_08_03;

struct RecallMeasurement {
    representation: &'static str,
    metric: Metric,
    k: u64,
    matches: usize,
}

impl RecallMeasurement {
    fn recall(&self) -> f64 {
        self.matches as f64 / self.k as f64
    }
}

#[test]
fn approximate_hnsw_f32_l2_meets_binding_recall_gates() {
    let measurements = measure_hnsw_fixture(
        "hnsw-recall-f32-l2",
        "f32",
        LogicalType::Vector { dim: DIM as u32 },
        Metric::L2,
    );

    assert_recall_gate(&measurements[0], 1, 1, 1.00);
    assert_recall_gate(&measurements[1], 10, 9, 0.90);
    assert_recall_gate(&measurements[2], 100, 95, 0.95);
}

#[test]
fn approximate_hnsw_f32_cosine_meets_binding_recall_gates() {
    let measurements = measure_hnsw_fixture(
        "hnsw-recall-f32-cosine",
        "f32",
        LogicalType::Vector { dim: DIM as u32 },
        Metric::Cosine,
    );

    assert_recall_gate(&measurements[0], 1, 1, 1.00);
    assert_recall_gate(&measurements[1], 10, 9, 0.90);
    assert_recall_gate(&measurements[2], 100, 95, 0.95);
}

#[test]
fn approximate_hnsw_b1_f32_rescore_cosine_meets_binding_recall_gates() {
    let measurements = measure_hnsw_fixture(
        "hnsw-recall-b1-cosine",
        "b1+f32-rescore",
        LogicalType::VectorEncoded {
            dim: DIM as u32,
            encoding: devondb_types::logical_type::VectorEncoding::B1 {
                rotation_seed: B1_ROTATION_SEED,
                rescore: devondb_types::logical_type::B1Rescore::F32,
            },
        },
        Metric::Cosine,
    );

    assert_recall_gate(&measurements[0], 1, 1, 1.00);
    assert_recall_gate(&measurements[1], 10, 8, 0.80);
    assert_recall_gate(&measurements[2], 100, 90, 0.90);
}

fn measure_hnsw_fixture(
    label: &str,
    representation: &'static str,
    vector_type: LogicalType,
    metric: Metric,
) -> [RecallMeasurement; K_VALUES.len()] {
    let (_directory, mut database, corpus) =
        create_hnsw_corpus_database(label, vector_type, metric);
    let query = corpus.query(metric);
    K_VALUES.map(|k| {
        measure_approximate_recall(
            &mut database,
            &corpus.vectors,
            query,
            representation,
            metric,
            k,
        )
    })
}

fn create_hnsw_corpus_database(
    label: &str,
    vector_type: LogicalType,
    metric: Metric,
) -> (TestDirectory, Database, Corpus) {
    let directory = TestDirectory::new(label);
    let mut database = Database::create(directory.db_path(), PAGE_SIZE).expect("create database");
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
                    ty: vector_type,
                    primary_key: false,
                },
            ],
        })
        .expect("create HNSW recall table");
    let corpus = Corpus::generate();
    insert_hnsw_corpus(&mut database, &corpus);
    database
        .execute(&Statement::CreateHnswIndex {
            name: "corpus_embedding_hnsw".to_owned(),
            table: TABLE.to_owned(),
            column: VECTOR_COLUMN.to_owned(),
            metric,
        })
        .expect("create HNSW index in its own transaction");
    (directory, database, corpus)
}

fn insert_hnsw_corpus(database: &mut Database, corpus: &Corpus) {
    let rows = corpus
        .vectors
        .iter()
        .enumerate()
        .map(|(row_order, vector)| {
            vec![
                Value::Int64(row_id(row_order)),
                Value::Vector(vector.clone()),
            ]
        })
        .collect();
    database
        .execute(&Statement::InsertNode {
            table: TABLE.to_owned(),
            rows,
        })
        .expect("insert HNSW recall corpus");
}

fn measure_approximate_recall(
    database: &mut Database,
    vectors: &[Vec<f32>],
    query: &[f32],
    representation: &'static str,
    metric: Metric,
    k: u64,
) -> RecallMeasurement {
    let plan = approximate_knn_plan(query, k, metric);
    let first = database.run(&plan).expect("run approximate KNN plan");
    let second = database.run(&plan).expect("repeat approximate KNN plan");
    assert_eq!(
        second,
        first,
        "approximate KNN is nondeterministic for {representation}/{} k={k}",
        metric_name(metric)
    );
    let actual = validated_approximate_neighbors(&first, vectors, query, metric, k);
    let exact = scalar_ground_truth(vectors, query, metric, k);
    let exact_ids = exact
        .iter()
        .map(|neighbor| neighbor.id)
        .collect::<std::collections::BTreeSet<_>>();
    let matches = actual
        .iter()
        .filter(|neighbor| exact_ids.contains(&neighbor.id))
        .count();
    let measurement = RecallMeasurement {
        representation,
        metric,
        k,
        matches,
    };
    eprintln!(
        "HNSW recall {representation}/{} k={k}: {:.2} ({matches}/{k})",
        metric_name(metric),
        measurement.recall()
    );
    measurement
}

fn approximate_knn_plan(query: &[f32], k: u64, metric: Metric) -> Plan {
    Plan {
        v: PLAN_VERSION,
        plan: Operator::KnnScan {
            table: TABLE.to_owned(),
            column: VECTOR_COLUMN.to_owned(),
            query: query.to_vec().into(),
            k,
            metric,
            mode: devondb_plan::ops::KnnMode::Approximate,
        },
    }
}

fn validated_approximate_neighbors(
    result: &QueryResult,
    vectors: &[Vec<f32>],
    query: &[f32],
    metric: Metric,
    k: u64,
) -> Vec<Neighbor> {
    assert_eq!(
        result.columns,
        [
            format!("{TABLE}.id"),
            format!("{TABLE}.{VECTOR_COLUMN}"),
            "distance".to_owned(),
        ]
    );
    assert_eq!(result.rows.len(), k as usize, "approximate KNN row count");
    let mut seen = std::collections::BTreeSet::new();
    let mut neighbors = Vec::with_capacity(result.rows.len());
    for row in &result.rows {
        let [
            Value::Int64(id),
            Value::Vector(vector),
            Value::Float64(distance),
        ] = row.as_slice()
        else {
            panic!("unexpected approximate KNN row: {row:?}");
        };
        let row_order = visible_row_order(*id, vectors.len());
        assert!(seen.insert(*id), "duplicate approximate KNN id {id}");
        assert_eq!(
            vector, &vectors[row_order],
            "wrong vector returned for id {id}"
        );
        let scalar = f64::from(scalar_distance_f32(&vectors[row_order], query, metric));
        assert_close(*distance, scalar);
        neighbors.push(Neighbor {
            id: *id,
            distance: *distance,
        });
    }
    assert_approximate_order(&neighbors, vectors, query, metric, k);
    neighbors
}

fn visible_row_order(id: i64, vector_count: usize) -> usize {
    let row_order = id
        .checked_sub(ROW_ID_BASE)
        .and_then(|offset| usize::try_from(offset).ok())
        .expect("approximate KNN id maps to a visible row offset");
    assert!(row_order < vector_count, "invisible KNN row id {id}");
    row_order
}

fn assert_approximate_order(
    neighbors: &[Neighbor],
    vectors: &[Vec<f32>],
    query: &[f32],
    metric: Metric,
    k: u64,
) {
    for pair in neighbors.windows(2) {
        let output_order = pair[0].distance.total_cmp(&pair[1].distance);
        assert!(
            output_order.is_lt() || (output_order.is_eq() && pair[0].id < pair[1].id),
            "approximate distances are not ascending with node-offset ties for {} k={k}",
            metric_name(metric)
        );
        let left = scalar_distance_f32(vector_for_visible_id(vectors, pair[0].id), query, metric);
        let right = scalar_distance_f32(vector_for_visible_id(vectors, pair[1].id), query, metric);
        let scalar_order = left.total_cmp(&right);
        assert!(
            scalar_order.is_lt() || (scalar_order.is_eq() && pair[0].id < pair[1].id),
            "approximate ids violate scalar distance/node-offset order for {} k={k}",
            metric_name(metric)
        );
    }
}

fn vector_for_visible_id(vectors: &[Vec<f32>], id: i64) -> &[f32] {
    &vectors[visible_row_order(id, vectors.len())]
}

fn assert_recall_gate(
    measurement: &RecallMeasurement,
    expected_k: u64,
    required_matches: usize,
    threshold: f64,
) {
    assert_eq!(measurement.k, expected_k);
    assert!(
        measurement.matches >= required_matches,
        "HNSW recall gate failed for {}/{} k={}: measured {:.2} ({}/{}), required {:.2}",
        measurement.representation,
        metric_name(measurement.metric),
        measurement.k,
        measurement.recall(),
        measurement.matches,
        measurement.k,
        threshold
    );
}
