//! Executor regression tests for Decimal averages, KnnScan heap budgeting,
//! streaming-aggregate error precedence, blocking-operator terminal state,
//! percentile scale validation across spill paths, spill row-length bounds,
//! exact Int64/Float64 comparison, and Vector ordering restrictions.

use std::{
    cell::Cell,
    collections::{HashMap, VecDeque},
    fs,
    path::PathBuf,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering as AtomicOrdering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

use devondb_exec::{
    chunk::{Chunk, ChunkBuilder},
    eval::evaluate,
    knn::KnnScan,
    operators::{Aggregate, Sort, SpillConfig},
    source::{ChunkSource, OuterBindings, ScalarSubqueryExecutor, ScalarSubqueryResult},
};
use devondb_plan::{
    expr::{BinaryOp, Expr, Metric},
    ops::{AggregateFunction, Operator, SortOrder},
};
use devondb_storage::budget::MemoryBudget;
use devondb_types::{
    DevonError, DevonResult, decimal::Decimal128, logical_type::LogicalType, value::Value,
};

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

fn split_chunks(types: &[LogicalType], rows: &[Vec<Value>], widths: &[usize]) -> Vec<Chunk> {
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

fn drain_rows(source: &mut dyn ChunkSource) -> DevonResult<Vec<Vec<Value>>> {
    let mut rows = Vec::new();
    while let Some(chunk) = source.next_chunk()? {
        for row in 0..chunk.row_count() {
            rows.push(
                (0..chunk.column_count())
                    .map(|column| {
                        chunk
                            .value(row, column)
                            .unwrap_or_else(|| panic!("missing value at {row}/{column}"))
                    })
                    .collect::<Vec<_>>(),
            );
        }
    }
    Ok(rows)
}

fn decimal(digits: i128, scale: u8) -> Value {
    Value::Decimal(Decimal128::new(digits, scale).unwrap())
}

fn decimal_type(precision: u8, scale: u8) -> LogicalType {
    LogicalType::Decimal { precision, scale }
}

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

struct TempDirGuard {
    path: PathBuf,
}

impl TempDirGuard {
    fn new() -> Self {
        static SEQUENCE: AtomicU64 = AtomicU64::new(0);
        let sequence = SEQUENCE.fetch_add(1, AtomicOrdering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        Self {
            path: std::env::temp_dir().join(format!(
                "devondb-exec-review-fixes-{}-{nanos}-{sequence}",
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

fn streaming_aggregate(
    chunks: Vec<Chunk>,
    function: AggregateFunction,
    output_type: LogicalType,
) -> Aggregate {
    Aggregate::new(
        Box::new(VecSource::new(chunks)),
        Vec::new(),
        vec![(function, Expr::Col("r.value".into()))],
        columns(&[("r.value", 0)]),
        vec![output_type],
        SpillConfig::unbounded(),
    )
}

/// `avg` accepts Decimal and accumulates exactly, with one rounding to
/// Float64 at finalize (PLAN_IR.md: avg returns Float64).
#[test]
fn avg_over_decimal_is_exact_until_one_final_rounding() {
    let ty = decimal_type(38, 2);
    let cases: [(Vec<Value>, Value); 3] = [
        // Sum stays an exact Decimal; 5.00 / 3 rounds once at finalize.
        (
            vec![decimal(100, 2), decimal(200, 2), decimal(200, 2)],
            Value::Float64(5.0 / 3.0),
        ),
        // Negative digits accumulate exactly.
        (
            vec![decimal(-100, 2), decimal(-200, 2)],
            Value::Float64(-1.5),
        ),
        // NULLs are skipped and do not count.
        (
            vec![Value::Null, decimal(200, 2), Value::Null, decimal(400, 2)],
            Value::Float64(3.0),
        ),
    ];
    for (rows, expected) in cases {
        let chunks = vec![chunk(
            &[ty],
            &rows.into_iter().map(|v| vec![v]).collect::<Vec<_>>(),
        )];
        let mut aggregate =
            streaming_aggregate(chunks, AggregateFunction::Avg, LogicalType::Float64);
        let found = drain_rows(&mut aggregate).unwrap();
        assert_eq!(found, vec![vec![expected]]);
    }

    // All-NULL input averages to NULL.
    let mut aggregate = streaming_aggregate(
        vec![chunk(&[ty], &[vec![Value::Null], vec![Value::Null]])],
        AggregateFunction::Avg,
        LogicalType::Float64,
    );
    assert_eq!(drain_rows(&mut aggregate).unwrap(), vec![vec![Value::Null]]);

    // Int64 averaging still works through the same state.
    let mut aggregate = streaming_aggregate(
        vec![chunk(
            &[LogicalType::Int64],
            &[vec![Value::Int64(1)], vec![Value::Int64(2)]],
        )],
        AggregateFunction::Avg,
        LogicalType::Float64,
    );
    assert_eq!(
        drain_rows(&mut aggregate).unwrap(),
        vec![vec![Value::Float64(1.5)]]
    );

    // Non-numeric input is refused with the shared aggregate-type error.
    let mut aggregate = streaming_aggregate(
        vec![chunk(
            &[LogicalType::String],
            &[vec![Value::String("nope".into())]],
        )],
        AggregateFunction::Avg,
        LogicalType::Float64,
    );
    let error = aggregate.next_chunk().unwrap_err();
    assert!(
        error
            .to_string()
            .contains("aggregate `avg` does not accept input type String"),
        "{error}"
    );
}

/// Decimal `avg` overflow raises the same checked error `sum` uses.
#[test]
fn avg_decimal_overflow_uses_the_sum_checked_error() {
    let ty = decimal_type(38, 0);
    // Two 38-digit values whose sum needs 39 digits.
    let big = i128::pow(10, 38) - 1;
    let rows = vec![vec![decimal(big, 0)], vec![decimal(big, 0)]];
    let make = || vec![chunk(&[ty], &rows)];

    let mut sum = streaming_aggregate(make(), AggregateFunction::Sum, ty);
    let sum_error = sum.next_chunk().unwrap_err().to_string();
    assert!(
        sum_error.contains("aggregate `sum` overflowed Decimal precision 38"),
        "{sum_error}"
    );

    let mut avg = streaming_aggregate(make(), AggregateFunction::Avg, LogicalType::Float64);
    let avg_error = avg.next_chunk().unwrap_err().to_string();
    assert_eq!(avg_error, sum_error);
}

/// Retained KnnScan heap rows are charged to the shared budget, released on
/// eviction, and released at drop.
#[test]
fn knn_scan_heap_rows_are_charged_released_on_eviction_and_at_drop() {
    // One retained dim-1 vector row charges 32 + 4 = 36 bytes.
    let row_bytes = 36_usize;
    let k = 2_usize;
    let budget = Arc::new(MemoryBudget::new(row_bytes * k));
    let rows: Vec<Vec<Value>> = (0..4)
        .map(|index| vec![Value::Vector(vec![index as f32])])
        .collect();
    let mut scan = KnnScan::new(
        Box::new(VecSource::new(split_chunks(
            &[LogicalType::Vector { dim: 1 }],
            &rows,
            &[1, 3],
        ))),
        0,
        vec![0.0],
        k as u64,
        Metric::L2,
    )
    .with_budget(Arc::clone(&budget));
    let found = drain_rows(&mut scan).unwrap();
    assert_eq!(found.len(), k);
    // Four rows passed through but only k remain charged: evicted rows were
    // released (otherwise row three would have exceeded the 2-row budget).
    assert_eq!(budget.charged(), row_bytes * k);
    drop(scan);
    assert_eq!(budget.charged(), 0);
}

/// A tight budget turns silent heap growth into `BudgetExceeded`.
#[test]
fn knn_scan_tight_budget_fails_instead_of_growing() {
    let budget = Arc::new(MemoryBudget::new(40));
    let rows: Vec<Vec<Value>> = (0..4)
        .map(|index| vec![Value::Vector(vec![index as f32, 0.0])])
        .collect();
    let mut scan = KnnScan::new(
        Box::new(VecSource::new(vec![chunk(
            &[LogicalType::Vector { dim: 2 }],
            &rows,
        )])),
        0,
        vec![0.0, 0.0],
        4,
        Metric::L2,
    )
    .with_budget(Arc::clone(&budget));
    let error = scan.next_chunk().unwrap_err();
    assert!(
        matches!(error, DevonError::BudgetExceeded { .. }),
        "{error}"
    );
    assert!(
        error.to_string().contains("KnnScan retained row"),
        "{error}"
    );
    drop(scan);
    assert_eq!(budget.charged(), 0);
}

/// A scalar-subquery executor that returns a two-row result (an error when
/// consumed) exactly when the correlated outer value is the sentinel.
struct TwoRowsAtSentinel {
    sentinel: i64,
}

impl ScalarSubqueryExecutor for TwoRowsAtSentinel {
    fn memory_budget(&self) -> Arc<MemoryBudget> {
        Arc::new(MemoryBudget::unlimited())
    }

    fn execute(
        &self,
        _plan: &Operator,
        outer: &OuterBindings,
    ) -> DevonResult<ScalarSubqueryResult> {
        let values = match outer.get("r.value") {
            Some(Value::Int64(value)) if *value == self.sentinel => {
                vec![Value::Int64(0), Value::Int64(0)]
            }
            _ => vec![Value::Int64(0)],
        };
        Ok(ScalarSubqueryResult {
            output_type: LogicalType::Int64,
            values,
        })
    }
}

/// The first recorded streaming-aggregate error wins; a later
/// upstream/subquery error must not mask it at any chunk boundary.
#[test]
fn streaming_aggregate_first_error_wins_across_chunkings() {
    // Sum overflows at row 1 (i64::MAX + 1); the correlated scalar subquery
    // misbehaves (two rows) at row 2, whose evaluation would mask the
    // overflow if the overflow were deferred.
    let rows = vec![
        vec![Value::Int64(i64::MAX)],
        vec![Value::Int64(1)],
        vec![Value::Int64(7)],
    ];
    let correlated_plan = Operator::Filter {
        predicate: Expr::Binary {
            op: BinaryOp::Eq,
            left: Box::new(Expr::Col("t.v".into())),
            right: Box::new(Expr::Col("r.value".into())),
        },
        input: Box::new(Operator::ScanNodes {
            table: "t".into(),
            binding: "t".into(),
        }),
    };
    for widths in [&[2_usize, 1][..], &[1, 1, 1][..]] {
        let mut aggregate = Aggregate::new(
            Box::new(VecSource::new(split_chunks(
                &[LogicalType::Int64],
                &rows,
                widths,
            ))),
            Vec::new(),
            vec![
                (AggregateFunction::Sum, Expr::Col("r.value".into())),
                (
                    AggregateFunction::Sum,
                    Expr::Scalar {
                        plan: Box::new(correlated_plan.clone()),
                    },
                ),
            ],
            columns(&[("r.value", 0)]),
            vec![LogicalType::Int64, LogicalType::Int64],
            SpillConfig::unbounded(),
        )
        .with_scalar_executor(Arc::new(TwoRowsAtSentinel { sentinel: 7 }));
        let error = aggregate.next_chunk().unwrap_err();
        assert!(
            error
                .to_string()
                .contains("aggregate `sum` overflowed for Int64 inputs"),
            "widths {widths:?}: {error}"
        );
    }
}

/// A probe source that counts pulls and always fails.
struct FailingProbe {
    pulls: Rc<Cell<usize>>,
}

impl ChunkSource for FailingProbe {
    fn next_chunk(&mut self) -> DevonResult<Option<Chunk>> {
        self.pulls.set(self.pulls.get() + 1);
        Err(DevonError::InvalidArgument {
            context: "probe upstream failure".into(),
        })
    }
}

/// After failed initialization, Sort and Aggregate latch the terminal error:
/// later `next_chunk` calls return the same error without re-pulling the
/// upstream.
#[test]
fn blocking_operators_latch_terminal_initialization_error() {
    let sort_pulls = Rc::new(Cell::new(0));
    let mut sorted = Sort::new(
        Box::new(FailingProbe {
            pulls: Rc::clone(&sort_pulls),
        }),
        vec![(Expr::Col("r.value".into()), SortOrder::Asc)],
        columns(&[("r.value", 0)]),
        SpillConfig::unbounded(),
    );
    let first = sorted.next_chunk().unwrap_err().to_string();
    let second = sorted.next_chunk().unwrap_err().to_string();
    let third = sorted.next_chunk().unwrap_err().to_string();
    assert_eq!(first, second);
    assert_eq!(second, third);
    assert!(first.contains("probe upstream failure"), "{first}");
    assert_eq!(sort_pulls.get(), 1, "Sort re-pulled a failed upstream");

    let aggregate_pulls = Rc::new(Cell::new(0));
    let mut aggregate = Aggregate::new(
        Box::new(FailingProbe {
            pulls: Rc::clone(&aggregate_pulls),
        }),
        vec![Expr::Col("r.value".into())],
        vec![(AggregateFunction::Count, Expr::Col("r.value".into()))],
        columns(&[("r.value", 0)]),
        vec![LogicalType::Int64, LogicalType::Int64],
        SpillConfig::unbounded(),
    );
    let first = aggregate.next_chunk().unwrap_err().to_string();
    let second = aggregate.next_chunk().unwrap_err().to_string();
    let third = aggregate.next_chunk().unwrap_err().to_string();
    assert_eq!(first, second);
    assert_eq!(second, third);
    assert!(first.contains("probe upstream failure"), "{first}");
    assert_eq!(
        aggregate_pulls.get(),
        1,
        "Aggregate re-pulled a failed upstream"
    );
}

/// Percentile Decimal scale validation is per-group in both the in-memory and
/// spill paths. A group that mixes scales fails with the
/// same error text whether it is caught at in-memory validation, at
/// spill-run write time, or at merge time across two spill rounds.
#[test]
fn percentile_mixed_scale_within_a_group_errors_identically_across_spill_modes() {
    let rows = [
        vec![Value::Int64(0), decimal(100, 2)],
        vec![Value::Int64(0), decimal(200, 3)],
        vec![Value::Int64(0), decimal(300, 2)],
    ];
    // Chunk 2 carries the scale-3 row so every value is well-formed at
    // construction; the mixed scale only appears inside the aggregate.
    let build = || {
        vec![
            chunk(
                &[LogicalType::Int64, decimal_type(38, 2)],
                &[rows[0].clone()],
            ),
            chunk(
                &[LogicalType::Int64, decimal_type(38, 3)],
                &[rows[1].clone()],
            ),
            chunk(
                &[LogicalType::Int64, decimal_type(38, 2)],
                &[rows[2].clone()],
            ),
        ]
    };
    let percentile = |chunks, config| {
        Aggregate::new(
            Box::new(VecSource::new(chunks)),
            vec![Expr::Col("r.group".into())],
            vec![(
                AggregateFunction::PercentileCont,
                Expr::Col("r.value".into()),
            )],
            columns(&[("r.group", 0), ("r.value", 1)]),
            vec![LogicalType::Int64, decimal_type(38, 2)],
            config,
        )
    };

    let (unbounded, _guard) = spill_config(None);
    let in_memory = drain_rows(&mut percentile(build(), unbounded)).unwrap_err();
    assert!(
        in_memory
            .to_string()
            .contains("cannot combine Decimal values with scales 2 and 3"),
        "{in_memory}"
    );

    // Budget 250 fits two 104-byte rows, so the third row forces a spill
    // run whose group segment mixes scales: caught at spill-run write time.
    // (The failed run leaves no file behind; the identical error text is
    // the parity signal.)
    let (write_config, _write_guard) = spill_config(Some(250));
    let at_write = drain_rows(&mut percentile(build(), write_config)).unwrap_err();
    assert_eq!(at_write.to_string(), in_memory.to_string());

    // Twenty rows with the scale-3 value alone in the tail: budget 1000
    // spills twice (runs of 9, 10, and 1 rows), so the merge across spill
    // rounds combines the scales and must fail the same way.
    let (merge_config, merge_guard) = spill_config(Some(1000));
    let many_scale_two: Vec<Vec<Value>> = (0..19)
        .map(|index| vec![Value::Int64(0), decimal(100 + index, 2)])
        .collect();
    let two_runs = || {
        vec![
            chunk(&[LogicalType::Int64, decimal_type(38, 2)], &many_scale_two),
            chunk(
                &[LogicalType::Int64, decimal_type(38, 3)],
                &[vec![Value::Int64(0), decimal(200, 3)]],
            ),
        ]
    };
    let at_merge = drain_rows(&mut percentile(two_runs(), merge_config)).unwrap_err();
    assert_eq!(at_merge.to_string(), in_memory.to_string());
    // Spill files are unlinked when the aggregate's SpillFiles drops — here
    // the temporary operator is already gone — so the identical error text
    // is the parity signal; the budget arithmetic (20 rows x 104 bytes
    // against a 1000-byte limit) forces three spill rounds.
    let _ = merge_guard;
}

/// Scales that differ only across groups are validated independently; both
/// paths behave identically. Here the scale-3 group collides with
/// the declared output type at output build, identically in both modes.
#[test]
fn percentile_mixed_scale_across_groups_behaves_identically_across_spill_modes() {
    // Ten scale-2 rows in group 0 and ten scale-3 rows in group 1; the
    // budget forces spill runs while leaving merge headroom.
    let group_zero: Vec<Vec<Value>> = (0..10)
        .map(|index| vec![Value::Int64(0), decimal(100 + index, 2)])
        .collect();
    let group_one: Vec<Vec<Value>> = (0..10)
        .map(|index| vec![Value::Int64(1), decimal(100 + index, 3)])
        .collect();
    let build = || {
        vec![
            chunk(&[LogicalType::Int64, decimal_type(38, 2)], &group_zero),
            chunk(&[LogicalType::Int64, decimal_type(38, 3)], &group_one),
        ]
    };
    let outcomes: Vec<String> = [None, Some(1000)]
        .into_iter()
        .map(|limit| {
            let (config, _guard) = spill_config(limit);
            let mut aggregate = Aggregate::new(
                Box::new(VecSource::new(build())),
                vec![Expr::Col("r.group".into())],
                vec![(
                    AggregateFunction::PercentileCont,
                    Expr::Col("r.value".into()),
                )],
                columns(&[("r.group", 0), ("r.value", 1)]),
                vec![LogicalType::Int64, decimal_type(38, 2)],
                config,
            );
            match drain_rows(&mut aggregate) {
                Ok(rows) => format!("ok: {rows:?}"),
                Err(error) => format!("err: {error}"),
            }
        })
        .collect();
    assert_eq!(outcomes[0], outcomes[1]);
    assert!(outcomes[0].starts_with("err: "), "{}", outcomes[0]);
}

/// Minimal rows round-trip through forced spill. Writer and reader share one
/// `MIN..=MAX` row-length range; the writer refuses out-of-range rows with
/// `InvalidArgument`, never a reader-side `Corrupt`.
#[test]
fn spill_round_trips_minimal_rows_under_forced_spill() {
    let rows: Vec<Vec<Value>> = [5_i64, 3, 4, 1, 2, 0]
        .into_iter()
        .map(|value| vec![Value::Int64(value)])
        .collect();
    // One sort row charges 96 bytes; a 300-byte budget spills on row four
    // and leaves room for the two merge front rows (2 × 96).
    let (config, guard) = spill_config(Some(300));
    let mut sorted = Sort::new(
        Box::new(VecSource::new(split_chunks(
            &[LogicalType::Int64],
            &rows,
            &[2, 2, 2],
        ))),
        vec![(Expr::Col("r.value".into()), SortOrder::Asc)],
        columns(&[("r.value", 0)]),
        config,
    );
    let found = drain_rows(&mut sorted).unwrap();
    assert_eq!(
        found,
        (0..=5_i64)
            .map(|value| vec![Value::Int64(value)])
            .collect::<Vec<_>>()
    );
    assert!(guard.spilled_files() > 0, "expected forced spill");
}

/// Int64 ∘ Float64 comparison is exact, with no lossy `as f64` conversion.
#[test]
fn int_float_comparison_is_exact_at_2_to_53_and_2_to_63() {
    let eval_bool = |op: BinaryOp, left: Value, right: Value| {
        evaluate(
            &Expr::Binary {
                op,
                left: Box::new(Expr::Lit(left)),
                right: Box::new(Expr::Lit(right)),
            },
            &chunk(&[LogicalType::Bool], &[vec![Value::Null]]),
            &columns(&[]),
        )
        .unwrap()[0]
            .clone()
    };
    let int = Value::Int64;
    let float = Value::Float64;

    // 2^53 + 1 is not representable as f64; the literal rounds to 2^53 + 2.
    assert_eq!(
        eval_bool(
            BinaryOp::Eq,
            int(9_007_199_254_740_993),
            float(9_007_199_254_740_993.0)
        ),
        Value::Bool(false)
    );
    assert_eq!(
        eval_bool(
            BinaryOp::Gt,
            int(9_007_199_254_740_993),
            float(9_007_199_254_740_992.0)
        ),
        Value::Bool(true)
    );
    assert_eq!(
        eval_bool(
            BinaryOp::Eq,
            int(9_007_199_254_740_992),
            float(9_007_199_254_740_992.0)
        ),
        Value::Bool(true)
    );
    // i64::MAX rounds up to 2^63 as f64: it is strictly less, never equal.
    assert_eq!(
        eval_bool(
            BinaryOp::Lt,
            int(i64::MAX),
            float(9_223_372_036_854_775_808.0)
        ),
        Value::Bool(true)
    );
    assert_eq!(
        eval_bool(
            BinaryOp::Eq,
            int(i64::MAX),
            float(9_223_372_036_854_775_807.0)
        ),
        Value::Bool(false)
    );
    // -2^63 is exactly representable.
    assert_eq!(
        eval_bool(
            BinaryOp::Eq,
            int(i64::MIN),
            float(-9_223_372_036_854_775_808.0)
        ),
        Value::Bool(true)
    );
    // Fractional parts decide ties on the truncated integer, both ways.
    assert_eq!(
        eval_bool(BinaryOp::Lt, int(3), float(3.5)),
        Value::Bool(true)
    );
    assert_eq!(
        eval_bool(BinaryOp::Gt, float(3.5), int(3)),
        Value::Bool(true)
    );
    assert_eq!(
        eval_bool(BinaryOp::Gt, int(-3), float(-3.5)),
        Value::Bool(true)
    );
    assert_eq!(
        eval_bool(BinaryOp::Lt, float(-3.5), int(-3)),
        Value::Bool(true)
    );
    assert_eq!(
        eval_bool(BinaryOp::Le, int(3), float(3.0)),
        Value::Bool(true)
    );
}

/// NaN is unordered, exactly as in Float64 ∘ Float64 comparison:
/// every operator but `!=` is false, in both operand orders.
#[test]
fn int_float_comparison_with_nan_is_unordered() {
    let eval_bool = |op: BinaryOp, left: Value, right: Value| {
        evaluate(
            &Expr::Binary {
                op,
                left: Box::new(Expr::Lit(left)),
                right: Box::new(Expr::Lit(right)),
            },
            &chunk(&[LogicalType::Bool], &[vec![Value::Null]]),
            &columns(&[]),
        )
        .unwrap()[0]
            .clone()
    };
    for op in [
        BinaryOp::Eq,
        BinaryOp::Lt,
        BinaryOp::Le,
        BinaryOp::Gt,
        BinaryOp::Ge,
    ] {
        assert_eq!(
            eval_bool(op, Value::Int64(1), Value::Float64(f64::NAN)),
            Value::Bool(false),
            "Int64 {op:?} NaN"
        );
        assert_eq!(
            eval_bool(op, Value::Float64(f64::NAN), Value::Int64(1)),
            Value::Bool(false),
            "NaN {op:?} Int64"
        );
    }
    assert_eq!(
        eval_bool(BinaryOp::Ne, Value::Int64(1), Value::Float64(f64::NAN)),
        Value::Bool(true)
    );
    assert_eq!(
        eval_bool(BinaryOp::Ne, Value::Float64(f64::NAN), Value::Int64(1)),
        Value::Bool(true)
    );
    // Infinity orders by sign against every Int64.
    assert_eq!(
        eval_bool(
            BinaryOp::Lt,
            Value::Int64(i64::MAX),
            Value::Float64(f64::INFINITY)
        ),
        Value::Bool(true)
    );
    assert_eq!(
        eval_bool(
            BinaryOp::Gt,
            Value::Int64(i64::MIN),
            Value::Float64(f64::NEG_INFINITY)
        ),
        Value::Bool(true)
    );
}

/// Vector is not scalar-comparable (PLAN_IR.md expression typing:
/// comparisons require Bool, String, or numeric), and PLAN_IR's GeoPoint
/// staging clause makes comparison, ordering, and sort keys one triple for
/// non-scalars — so sort keys, min/max, and `=` all refuse Vector.
#[test]
fn vector_values_are_not_orderable_or_comparable() {
    let ty = LogicalType::Vector { dim: 2 };
    let rows = vec![
        vec![Value::Vector(vec![1.0, 2.0])],
        vec![Value::Vector(vec![0.0, 0.0])],
    ];

    // Sort keys reject Vector.
    let mut sorted = Sort::new(
        Box::new(VecSource::new(vec![chunk(&[ty], &rows)])),
        vec![(Expr::Col("r.v".into()), SortOrder::Asc)],
        columns(&[("r.v", 0)]),
        SpillConfig::unbounded(),
    );
    let error = sorted.next_chunk().unwrap_err();
    assert!(error.to_string().contains("cannot order"), "{error}");

    // min/max reject Vector through the same ordering law.
    for function in [AggregateFunction::Min, AggregateFunction::Max] {
        let mut aggregate = Aggregate::new(
            Box::new(VecSource::new(vec![chunk(&[ty], &rows)])),
            Vec::new(),
            vec![(function, Expr::Col("r.v".into()))],
            columns(&[("r.v", 0)]),
            vec![ty],
            SpillConfig::unbounded(),
        );
        let error = aggregate.next_chunk().unwrap_err();
        assert!(error.to_string().contains("cannot order"), "{error}");
    }

    // `=` on Vectors is refused by the expression comparison law.
    let error = evaluate(
        &Expr::Binary {
            op: BinaryOp::Eq,
            left: Box::new(Expr::Lit(Value::Vector(vec![1.0]))),
            right: Box::new(Expr::Lit(Value::Vector(vec![1.0]))),
        },
        &chunk(&[LogicalType::Bool], &[vec![Value::Null]]),
        &columns(&[]),
    )
    .unwrap_err();
    assert!(error.to_string().contains("Vector"), "{error}");
}
