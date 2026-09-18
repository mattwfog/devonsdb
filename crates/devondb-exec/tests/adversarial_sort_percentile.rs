//! Adversarial sort and exact Decimal percentile coverage.
//!
//! Sort fixtures keep every input chunk within `CHUNK_CAPACITY` and verify
//! deterministic output across chunk boundaries. Equal ascending keys retain
//! input order. Decimal `percentile_cont` skips NULLs and interpolates the
//! exact mean of the flanking values when the rank is fractional. The spill
//! fixture uses a 384-byte budget: each buffered aggregate row charges 88
//! bytes, allowing four rows per run while retaining one merge-front row per
//! run file.

use std::{
    collections::{HashMap, VecDeque},
    fs,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use devondb_exec::{
    chunk::{CHUNK_CAPACITY, Chunk, ChunkBuilder},
    operators::{Aggregate, Sort, SpillConfig},
    source::ChunkSource,
};
use devondb_plan::{expr::Expr, ops::AggregateFunction, ops::SortOrder};
use devondb_storage::budget::MemoryBudget;
use devondb_types::{DevonError, decimal::Decimal128, logical_type::LogicalType, value::Value};

fn columns(names: &[(&str, usize)]) -> HashMap<String, usize> {
    names
        .iter()
        .map(|(name, index)| ((*name).to_owned(), *index))
        .collect()
}

fn chunk(types: &[LogicalType], rows: &[Vec<Value>]) -> Chunk {
    let mut builder = ChunkBuilder::new(types.to_vec());
    for row in rows {
        builder.push_row(row.clone()).unwrap();
    }
    builder.finish()
}

fn split_chunks(types: &[LogicalType], rows: Vec<Vec<Value>>, widths: &[usize]) -> Vec<Chunk> {
    assert!(
        widths.iter().all(|width| *width <= CHUNK_CAPACITY),
        "chunk width exceeds CHUNK_CAPACITY {CHUNK_CAPACITY}: {widths:?}"
    );
    assert_eq!(
        widths.iter().sum::<usize>(),
        rows.len(),
        "chunk widths must cover every input row"
    );
    let mut cursor = 0;
    widths
        .iter()
        .map(|width| {
            let next = (cursor + width).min(rows.len());
            let part = chunk(types, &rows[cursor..next]);
            cursor = next;
            part
        })
        .collect()
}

fn collect(source: &mut dyn ChunkSource) -> Result<Vec<Chunk>, DevonError> {
    let mut chunks = Vec::new();
    while let Some(output) = source.next_chunk()? {
        chunks.push(output);
    }
    Ok(chunks)
}

fn rows_of(chunks: &[Chunk]) -> Vec<Vec<Value>> {
    chunks
        .iter()
        .flat_map(|chunk| {
            (0..chunk.row_count()).map(move |row| {
                (0..chunk.column_count())
                    .map(|column| chunk.value(row, column).unwrap())
                    .collect::<Vec<_>>()
            })
        })
        .collect()
}

fn drain_rows(source: &mut dyn ChunkSource) -> Result<Vec<Vec<Value>>, DevonError> {
    collect(source).map(|found| rows_of(&found))
}

fn decimal(value: i128, scale: u8) -> Value {
    Value::Decimal(Decimal128::new(value, scale).unwrap())
}

fn decimal_type(precision: u8, scale: u8) -> LogicalType {
    LogicalType::Decimal { precision, scale }
}

fn spill_config(limit: Option<usize>) -> (SpillConfig, TempDirGuard) {
    let guard = TempDirGuard::new();
    (
        SpillConfig {
            budget: Arc::new(limit.map_or_else(MemoryBudget::unlimited, MemoryBudget::new)),
            tmp_dir: guard.path.clone(),
        },
        guard,
    )
}

struct VecSource(VecDeque<Chunk>);

impl VecSource {
    fn new(chunks: Vec<Chunk>) -> Self {
        Self(chunks.into())
    }
}

impl ChunkSource for VecSource {
    fn next_chunk(&mut self) -> Result<Option<Chunk>, DevonError> {
        Ok(self.0.pop_front())
    }
}

#[derive(Debug)]
struct TempDirGuard {
    path: std::path::PathBuf,
}

impl TempDirGuard {
    fn new() -> Self {
        static SEQUENCE: AtomicU64 = AtomicU64::new(0);
        let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        Self {
            path: std::env::temp_dir().join(format!(
                "devondb-exec-adversarial-258-{}-{nanos}-{sequence}",
                std::process::id()
            )),
        }
    }

    fn spilled_files(&self) -> usize {
        fs::read_dir(&self.path)
            .map(|entries| entries.filter_map(Result::ok).count())
            .unwrap_or_default()
    }
}

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// PLAN_IR § Design rules (determinism) and § Query operators (`Sort`:
/// total order): identical plans have one deterministic meaning, so two
/// runs and every chunking of the same input produce byte-identical
/// output. Every chunk width respects `CHUNK_CAPACITY`. Keys are distinct —
/// 7_919 is coprime with 4_097 — so this pins the total order itself,
/// independent of tie-breaking.
#[test]
fn sort_is_identical_across_two_runs_and_chunk_boundaries() {
    let types = [LogicalType::Int64];
    let rows: Vec<_> = (0..4_097_i64)
        .map(|value| vec![Value::Int64((value * 7_919) % 4_097)])
        .collect();
    let width_cases: [Vec<usize>; 4] = [
        vec![CHUNK_CAPACITY, CHUNK_CAPACITY, 1],
        vec![1; 4_097],
        vec![1, CHUNK_CAPACITY, 4_097 - 1 - CHUNK_CAPACITY],
        vec![7, 1_024, CHUNK_CAPACITY, 1_018],
    ];
    let mut baseline = None;
    for run in 0..2 {
        for widths in &width_cases {
            let (config, _guard) = spill_config(None);
            let mut sorted = Sort::new(
                Box::new(VecSource::new(split_chunks(&types, rows.clone(), widths))),
                vec![(Expr::Col("r.value".into()), SortOrder::Asc)],
                columns(&[("r.value", 0)]),
                config,
            );
            let actual = drain_rows(&mut sorted).unwrap();
            match &baseline {
                None => {
                    assert_eq!(actual.len(), 4_097);
                    assert_eq!(actual.first().unwrap()[0], Value::Int64(0));
                    assert_eq!(actual.last().unwrap()[0], Value::Int64(4_096));
                    baseline = Some(actual);
                }
                Some(expected) => {
                    assert_eq!(&actual, expected, "run {run} with widths {widths:?}");
                }
            }
        }
    }
}

/// PLAN_IR § Query operators (`Sort`: total order) plus § Design rules
/// (one deterministic meaning): equal ascending keys keep input order —
/// stability is the only tie-break compatible with a total, deterministic
/// order. This is pinned in memory and through the external merge: a
/// 1_024-byte budget buffers about seven rows per spill run
/// (one row charges ~136 bytes: 64-byte row overhead, Int64 value and key,
/// String payload), and the merge buffers one charged front row per run
/// (~822 bytes for 6 runs) — a 256-byte budget would spill but starve the
/// merge, ending in `BudgetExceeded` instead of exercising it.
#[test]
fn equal_sort_keys_preserve_input_order_at_seven_row_chunks() {
    let types = [LogicalType::Int64, LogicalType::String];
    let keyed = |(key, order): &(i64, &str)| {
        vec![
            Value::Int64(key % 3),
            Value::String(format!("{order}-{key}")),
        ]
    };
    let rows: Vec<Vec<Value>> = (0..21_i64)
        .flat_map(|key| [(key, "first"), (key, "second")])
        .map(|pair| keyed(&pair))
        .collect();
    let expected: Vec<Vec<Value>> = (0..3_i64)
        .flat_map(|bucket| {
            (0..21_i64)
                .filter(move |key| key % 3 == bucket)
                .flat_map(|key| [(key, "first"), (key, "second")])
        })
        .map(|pair| keyed(&pair))
        .collect();
    for budget in [None, Some(1024)] {
        let (config, guard) = spill_config(budget);
        let mut sorted = Sort::new(
            Box::new(VecSource::new(split_chunks(&types, rows.clone(), &[7; 6]))),
            vec![(Expr::Col("r.key".into()), SortOrder::Asc)],
            columns(&[("r.key", 0), ("r.tag", 1)]),
            config,
        );
        assert_eq!(
            drain_rows(&mut sorted).unwrap(),
            expected,
            "budget {budget:?}"
        );
        if budget.is_some() {
            // Spill run files are unlinked when the operator drops
            // (`SpillFiles::drop`), so count them while it is alive.
            assert!(guard.spilled_files() > 0, "expected spill files");
        }
    }
}

/// the expression spec § Exact Decimal `percentile_cont` ("NULL, ordering,
/// and interpolation semantics"): NULLs are skipped; for N ordered
/// non-null values, r = (N - 1) / 2 selects the middle value when
/// integral, else the exact arithmetic mean of the flanking values. Here
/// N = 4 gives r = 1.5, so the median is the exact mean of 2.0000 and
/// 3.0000: 2.5000, representable at scale 4 with no rounding. Pinned in
/// memory and through the spill merge at a 384-byte budget (one buffered
/// row charges 88 bytes, so four rows fit per run and the fifth spills).
#[test]
fn percentile_median_skips_nulls_interpolates_exactly() {
    let ty = decimal_type(20, 4);
    let rows = vec![
        vec![decimal(10_000, 4)],
        vec![Value::Null],
        vec![decimal(20_000, 4)],
        vec![decimal(30_000, 4)],
        vec![decimal(40_000, 4)],
    ];
    let (spilled_config, spilled_guard) = spill_config(Some(384));
    let expected = decimal(25_000, 4);
    for (config, label) in [
        (SpillConfig::unbounded(), "unbounded"),
        (spilled_config, "spill"),
    ] {
        let mut aggregate = Aggregate::new(
            Box::new(VecSource::new(vec![chunk(&[ty], &rows)])),
            Vec::new(),
            vec![(
                AggregateFunction::PercentileCont,
                Expr::Col("r.value".into()),
            )],
            columns(&[("r.value", 0)]),
            vec![ty],
            config,
        );
        let found = drain_rows(&mut aggregate).unwrap_or_else(|error| panic!("{label}: {error}"));
        assert_eq!(
            found,
            vec![vec![expected.clone()]],
            "{label} percentile mismatch"
        );
        if label == "spill" {
            // Spill run files are unlinked when the operator drops
            // (`SpillFiles::drop`), so count them while it is alive.
            assert!(spilled_guard.spilled_files() > 0, "expected spill files");
        }
    }
}
