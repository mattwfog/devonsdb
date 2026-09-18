//! Executor charge accounting (`docs/SCALE.md` §6.6): every charge site uses
//! `Column::approx_bytes` for typed columns (`len × width` + bitmap words for
//! fixed-width storage, per-value approx bytes for `Boxed`) where a `Chunk`
//! is what is held, and keeps per-value `Value::approx_bytes` where a
//! materialized row is what is held (sort/aggregate/hash-join record
//! buffers, spill runs). These tests pin both halves with budget probes:
//! an exact-limit probe passes iff the charged number is the §6.6 number.
//!
//! Every data set is fixed and deterministic — no randomness anywhere.

use std::{
    collections::{HashMap, VecDeque},
    fs,
    path::PathBuf,
    sync::{Arc, Mutex, PoisonError},
    time::{SystemTime, UNIX_EPOCH},
};

use devondb_exec::{
    chunk::{CHUNK_CAPACITY, Chunk, ChunkBuilder},
    column::Column,
    hnsw::{DistanceAccessor, FullScanFactory, KnnScan},
    operators::{Aggregate, Filter, Project, Sort, SpillConfig},
    source::ChunkSource,
};
use devondb_plan::{
    expr::{Expr, Metric},
    ops::{AggregateFunction, SortOrder},
};
use devondb_storage::{
    budget::MemoryBudget,
    hnsw::types::{GraphAccess, HnswConfig, HnswMetric, NavigationEncoding},
};
use devondb_types::{DevonError, DevonResult, logical_type::LogicalType, value::Value};

/// One full chunk of Int64 rows.
const ROW_COUNT: usize = CHUNK_CAPACITY;
/// The §6.6 number for an all-valid Int64 column: `len × width`, no bitmap.
const INT64_COLUMN_BYTES: usize = ROW_COUNT * 8;
/// `Value::approx_bytes` for Int64.
const INT64_VALUE_BYTES: usize = 16;
/// `BUFFERED_ROW_OVERHEAD_BYTES` in operators.rs: the per-materialized-row
/// writer-policy estimate.
const ROW_OVERHEAD_BYTES: usize = 64;
/// Sort/Aggregate charge for one Int64 row with one Int64 key:
/// 64 + 16 (value) + 16 (key), because these operators hold a materialized
/// `Vec<Value>` row.
const INT64_SORT_ROW_BYTES: usize = ROW_OVERHEAD_BYTES + 2 * INT64_VALUE_BYTES;
/// Total buffered charge for one full Int64 chunk through Sort/Aggregate.
const INT64_BUFFERED_TOTAL: usize = ROW_COUNT * INT64_SORT_ROW_BYTES;

/// The spill-run counter is process-global, so probes that read it serialize
/// to keep their deltas attributable.
static SERIAL: Mutex<()> = Mutex::new(());

struct VecSource {
    chunks: VecDeque<Chunk>,
}

impl VecSource {
    fn new(chunks: Vec<Chunk>) -> Self {
        Self {
            chunks: chunks.into(),
        }
    }
}

impl ChunkSource for VecSource {
    fn next_chunk(&mut self) -> DevonResult<Option<Chunk>> {
        Ok(self.chunks.pop_front())
    }
}

fn int64_chunk() -> Chunk {
    build_chunk(
        vec![LogicalType::Int64],
        (0..ROW_COUNT)
            .map(|id| vec![Value::Int64(id as i64)])
            .collect(),
    )
}

fn string_chunk(payload: &str) -> Chunk {
    build_chunk(
        vec![LogicalType::String],
        (0..ROW_COUNT)
            .map(|_| vec![Value::String(payload.to_owned())])
            .collect(),
    )
}

fn build_chunk(types: Vec<LogicalType>, rows: Vec<Vec<Value>>) -> Chunk {
    let mut builder = ChunkBuilder::new(types);
    for row in rows {
        builder.push_row(row).unwrap();
    }
    builder.finish()
}

fn id_column_map() -> HashMap<String, usize> {
    HashMap::from([("id".to_owned(), 0)])
}

fn drain(source: &mut dyn ChunkSource) -> DevonResult<Vec<Chunk>> {
    let mut chunks = Vec::new();
    while let Some(chunk) = source.next_chunk()? {
        chunks.push(chunk);
    }
    Ok(chunks)
}

fn ids(chunks: &[Chunk]) -> Vec<i64> {
    chunks
        .iter()
        .flat_map(Chunk::rows)
        .map(|row| match &row[0] {
            Value::Int64(value) => *value,
            other => panic!("expected Int64 id, got {other}"),
        })
        .collect()
}

/// A spill config whose budget is capped at exactly `limit` bytes, with a
/// unique temporary directory the returned guard removes on drop.
fn spill_config(limit: usize, label: &str) -> (SpillConfig, SpillDir) {
    let dir = SpillDir::new(label);
    let config = SpillConfig {
        budget: Arc::new(MemoryBudget::new(limit)),
        tmp_dir: dir.path(),
    };
    (config, dir)
}

struct SpillDir(PathBuf);

impl SpillDir {
    fn new(label: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        Self(std::env::temp_dir().join(format!(
            "devondb-typed-charges-{label}-{}-{nanos}",
            std::process::id()
        )))
    }

    fn path(&self) -> PathBuf {
        self.0.clone()
    }
}

impl Drop for SpillDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// A 2048-row Int64 chunk keeps its typed storage — and therefore the exact
/// §6.6 accounting number (`len × width`, no bitmap words) — as it flows
/// through the streaming Filter and Project operators. Neither operator
/// holds rows across chunks, so neither has a charge site; the probe here is
/// that the chunk they hand downstream still accounts to 16384 B rather than
/// the per-value estimate of 32768 B.
#[test]
fn project_and_filter_preserve_the_66_number() {
    let chunk = int64_chunk();
    let column = chunk.column(0).unwrap();
    assert!(
        matches!(column, Column::Int64 { .. }),
        "scan-shaped Int64 rows must hold typed storage"
    );
    assert_eq!(column.approx_bytes(), INT64_COLUMN_BYTES);

    let mut filter = Filter::new(
        Box::new(VecSource::new(vec![chunk])),
        Expr::Lit(Value::Bool(true)),
        id_column_map(),
    );
    let filtered = filter.next_chunk().unwrap().unwrap();
    assert_eq!(filtered.row_count(), ROW_COUNT);
    assert_eq!(
        filtered.column(0).unwrap().approx_bytes(),
        INT64_COLUMN_BYTES
    );

    let mut project = Project::new(
        Box::new(VecSource::new(vec![filtered])),
        vec![(Expr::Col("id".to_owned()), "id".to_owned())],
        id_column_map(),
        vec![LogicalType::Int64],
    )
    .unwrap();
    let projected = project.next_chunk().unwrap().unwrap();
    assert_eq!(projected.row_count(), ROW_COUNT);
    assert_eq!(
        projected.column(0).unwrap().approx_bytes(),
        INT64_COLUMN_BYTES
    );
    assert!(project.next_chunk().unwrap().is_none());
}

/// Rows fed to the brute-force KNN probe: one full chunk plus a one-row
/// tail, so the result spans two chunks and the output charge stays held
/// (and observable) until the tail chunk is drained.
const BRUTE_ROWS: usize = ROW_COUNT + 1;
/// The §6.6 number for the brute result (Int64 id, 2-dim Vector, Float64
/// distance): `len × width` for the two fixed-width columns, per-value
/// 32 + 4×2 for the boxed vectors.
const HNSW_OUTPUT_BYTES: usize = BRUTE_ROWS * 8 + BRUTE_ROWS * (32 + 4 * 2) + BRUTE_ROWS * 8;

/// The HNSW output working set holds `Chunk`s, so `output_charge_bytes` uses
/// `Column::approx_bytes`: the result charges 2049×8 + 2049×40 + 2049×8 =
/// 114_744 B. Materialized-row accounting would charge
/// 64 + 2049×(64+16+40+16) = 278_728 B, more than double.
#[test]
fn hnsw_output_charges_typed_columns_by_the_66_number() {
    const _: () = assert!(HNSW_OUTPUT_BYTES == 114_744);
    // Materialized-row estimate for the same output: 64 + 2049 × (64+16+40+16).
    const PRE_250_OUTPUT_BYTES: usize = 64 + BRUTE_ROWS * (64 + 16 + 40 + 16);
    const _: () = assert!(PRE_250_OUTPUT_BYTES == 278_728);

    let budget = MemoryBudget::new(HNSW_OUTPUT_BYTES);
    let mut scan = brute_knn_scan(&budget);

    let first = scan.next_chunk().unwrap().unwrap();
    assert_eq!(first.row_count(), ROW_COUNT);
    assert_eq!(
        budget.charged(),
        HNSW_OUTPUT_BYTES,
        "held output must charge exactly the §6.6 column bytes"
    );
    // Draining the one-row tail chunk empties the output and releases the
    // reservation.
    assert_eq!(scan.next_chunk().unwrap().unwrap().row_count(), 1);
    assert_eq!(
        budget.charged(),
        0,
        "drained output must release its charge"
    );
    assert!(scan.next_chunk().unwrap().is_none());
}

/// One byte below the §6.6 number the same brute-force output refuses with
/// `BudgetExceeded` — the charge is all-or-nothing at the exact column byte
/// count, with no per-row overhead slack left to absorb it.
#[test]
fn hnsw_output_one_byte_short_of_the_66_number_fails() {
    let budget = MemoryBudget::new(HNSW_OUTPUT_BYTES - 1);
    let mut scan = brute_knn_scan(&budget);

    let DevonError::BudgetExceeded { context } = scan.next_chunk().unwrap_err() else {
        panic!("expected BudgetExceeded one byte below the §6.6 number");
    };
    assert!(
        context.contains("brute-force KNN final result rows"),
        "failure must name the charge site: {context}"
    );
    assert_eq!(budget.charged(), 0, "a refused charge must not leak");
}

/// A metric mismatch between index config and query forces the exact
/// brute-force fallback, which is one of the two `output_charge_bytes` call
/// sites (hnsw.rs `run_full_brute`); the ANN path charges the same function
/// for its final result rows.
fn brute_knn_scan(budget: &MemoryBudget) -> KnnScan<'_, impl GraphAccess> {
    let types = vec![LogicalType::Int64, LogicalType::Vector { dim: 2 }];
    let row = |id: usize| vec![Value::Int64(id as i64), Value::Vector(vec![1.0, 0.0])];
    let full = build_chunk(types.clone(), (0..ROW_COUNT).map(row).collect());
    let tail_row = build_chunk(types, vec![row(ROW_COUNT)]);
    let chunks = vec![full, tail_row];
    let distance: Box<DistanceAccessor<'_>> = Box::new(|_| Ok(0.0));
    let tail: Box<dyn ChunkSource> = Box::new(VecSource::new(Vec::new()));
    let full_scan: Box<FullScanFactory<'_>> =
        Box::new(move || Box::new(VecSource::new(chunks.clone())) as Box<dyn ChunkSource>);
    KnnScan::new(
        EmptyGraph,
        HnswConfig::with_defaults(7, HnswMetric::L2, NavigationEncoding::F32),
        distance,
        None,
        tail,
        full_scan,
        1,
        vec![1.0, 0.0],
        BRUTE_ROWS as u64,
        Metric::Cosine,
        budget,
    )
}

/// Graph view consumed only by the ANN path; the brute-force fallback drops
/// it untouched.
struct EmptyGraph;

impl GraphAccess for EmptyGraph {
    fn entry(&self) -> Option<(u64, u8)> {
        None
    }

    fn layer_count(&self) -> u8 {
        0
    }

    fn covered_rows(&self) -> u64 {
        0
    }

    fn neighbors(&self, _layer: u8, _node: u64, scratch: &mut Vec<u64>) -> DevonResult<()> {
        scratch.clear();
        Ok(())
    }
}

/// Sort holds materialized `Vec<Value>` rows with evaluated keys, so it uses
/// per-value accounting. A full Int64 chunk charges exactly
/// 2048×96 = 196_608 B. At that limit no spill run is created; one byte below
/// it the operator must spill to make progress.
#[test]
fn sort_charges_the_unchanged_per_value_constant_for_int64_rows() {
    let _serial = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);

    let runs_before = devondb_exec::operators::spill_runs_created();
    let (config, _dir) = spill_config(INT64_BUFFERED_TOTAL, "sort-exact");
    let mut sort = Sort::new(
        Box::new(VecSource::new(vec![int64_chunk()])),
        vec![(Expr::Col("id".to_owned()), SortOrder::Asc)],
        id_column_map(),
        config,
    );
    let output = drain(&mut sort).unwrap();
    assert_eq!(ids(&output), (0..ROW_COUNT as i64).collect::<Vec<_>>());
    assert_eq!(
        devondb_exec::operators::spill_runs_created(),
        runs_before,
        "an exactly-sufficient budget must not spill"
    );

    let (config, _dir) = spill_config(INT64_BUFFERED_TOTAL - 1, "sort-short");
    let mut sort = Sort::new(
        Box::new(VecSource::new(vec![int64_chunk()])),
        vec![(Expr::Col("id".to_owned()), SortOrder::Asc)],
        id_column_map(),
        config,
    );
    let output = drain(&mut sort).unwrap();
    assert_eq!(ids(&output), (0..ROW_COUNT as i64).collect::<Vec<_>>());
    assert!(
        devondb_exec::operators::spill_runs_created() > runs_before,
        "one byte below the exact charge must force a spill"
    );
}

/// The buffered Aggregate holds the same materialized-row shape (group key +
/// aggregate input values), so it uses the same charge and probes identically.
#[test]
fn aggregate_charges_the_unchanged_per_value_constant_for_int64_rows() {
    let _serial = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);

    let runs_before = devondb_exec::operators::spill_runs_created();
    let (config, _dir) = spill_config(INT64_BUFFERED_TOTAL, "aggregate-exact");
    let mut aggregate = id_count_aggregate(config);
    let output = drain(&mut aggregate).unwrap();
    assert_eq!(
        output.iter().map(Chunk::row_count).sum::<usize>(),
        ROW_COUNT,
        "one group per distinct id"
    );
    assert_eq!(
        devondb_exec::operators::spill_runs_created(),
        runs_before,
        "an exactly-sufficient budget must not spill"
    );

    let (config, _dir) = spill_config(INT64_BUFFERED_TOTAL - 1, "aggregate-short");
    let mut aggregate = id_count_aggregate(config);
    let output = drain(&mut aggregate).unwrap();
    assert_eq!(
        output.iter().map(Chunk::row_count).sum::<usize>(),
        ROW_COUNT
    );
    assert!(
        devondb_exec::operators::spill_runs_created() > runs_before,
        "one byte below the exact charge must force a spill"
    );
}

fn id_count_aggregate(config: SpillConfig) -> Aggregate {
    Aggregate::new(
        Box::new(VecSource::new(vec![int64_chunk()])),
        vec![Expr::Col("id".to_owned())],
        vec![(AggregateFunction::Count, Expr::Col("id".to_owned()))],
        id_column_map(),
        vec![LogicalType::Int64, LogicalType::Int64],
        config,
    )
}

/// §6.6 leaves `Boxed` columns on per-value approx bytes, so an all-String
/// chunk uses the same per-value charge as a materialized row:
/// `Column::approx_bytes` equals Σ `Value::approx_bytes` (2048×42 = 86_016
/// for a 10-byte payload), and Sort charges
/// 2048×(64+42+42) = 303_104 B.
#[test]
fn string_chunks_charge_exactly_the_pre_change_constant() {
    let _serial = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);

    let payload = "x".repeat(10);
    let value_bytes = Value::String(payload.clone()).approx_bytes();
    assert_eq!(value_bytes, 32 + 10);
    let chunk = string_chunk(&payload);
    assert!(
        matches!(chunk.column(0), Some(Column::Boxed(_))),
        "String columns stay boxed under §6.6"
    );
    assert_eq!(
        chunk.column(0).unwrap().approx_bytes(),
        ROW_COUNT * value_bytes,
        "boxed columns charge the pre-seam per-value sum"
    );

    let sort_row_bytes = ROW_OVERHEAD_BYTES + 2 * value_bytes;
    let exact = ROW_COUNT * sort_row_bytes;
    let runs_before = devondb_exec::operators::spill_runs_created();
    let (config, _dir) = spill_config(exact, "string-sort-exact");
    let mut sort = Sort::new(
        Box::new(VecSource::new(vec![string_chunk(&payload)])),
        vec![(Expr::Col("id".to_owned()), SortOrder::Asc)],
        id_column_map(),
        config,
    );
    let output = drain(&mut sort).unwrap();
    assert_eq!(
        output.iter().map(Chunk::row_count).sum::<usize>(),
        ROW_COUNT
    );
    assert_eq!(
        devondb_exec::operators::spill_runs_created(),
        runs_before,
        "the pre-change constant must still be exactly sufficient"
    );

    let (config, _dir) = spill_config(exact - 1, "string-sort-short");
    let mut sort = Sort::new(
        Box::new(VecSource::new(vec![string_chunk(&payload)])),
        vec![(Expr::Col("id".to_owned()), SortOrder::Asc)],
        id_column_map(),
        config,
    );
    let output = drain(&mut sort).unwrap();
    assert_eq!(
        output.iter().map(Chunk::row_count).sum::<usize>(),
        ROW_COUNT
    );
    assert!(
        devondb_exec::operators::spill_runs_created() > runs_before,
        "one byte below the pre-change constant must force a spill"
    );
}
