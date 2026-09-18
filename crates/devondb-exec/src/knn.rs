//! Brute-force KNN source operator over a [`crate::source::ChunkSource`].

use std::{
    cmp::Ordering,
    collections::{BinaryHeap, VecDeque},
    sync::Arc,
};

use devondb_plan::expr::Metric;
use devondb_storage::budget::MemoryBudget;
use devondb_types::{DevonError, DevonResult, logical_type::LogicalType, value::Value};

use crate::{
    chunk::{Chunk, ChunkBuilder},
    simd,
    source::ChunkSource,
};

/// Name of the output column appended by [`KnnScan`].
pub const DISTANCE_COLUMN_NAME: &str = "distance";

/// A blocking source operator that returns the `k` nearest vector rows.
///
/// The source is scanned once. Selection retains only a max-heap of at most
/// `k` cloned rows, then emits those rows in ascending distance order with a
/// trailing [`LogicalType::Float64`] distance column.
pub struct KnnScan {
    source: Box<dyn ChunkSource>,
    vector_column: usize,
    query: Vec<f32>,
    k: u64,
    metric: Metric,
    initialized: bool,
    output: VecDeque<Chunk>,
    budget: Arc<MemoryBudget>,
    charged_bytes: usize,
}

impl KnnScan {
    /// Creates a KNN scan over a table source.
    ///
    /// `vector_column` is the declaration-order column index in every source
    /// chunk. The facade remains responsible for exposing source column names
    /// and [`DISTANCE_COLUMN_NAME`] in result metadata.
    #[must_use]
    pub fn new(
        source: Box<dyn ChunkSource>,
        vector_column: usize,
        query: Vec<f32>,
        k: u64,
        metric: Metric,
    ) -> Self {
        Self {
            source,
            vector_column,
            query,
            k,
            metric,
            initialized: false,
            output: VecDeque::new(),
            budget: Arc::new(MemoryBudget::unlimited()),
            charged_bytes: 0,
        }
    }

    /// Shares the statement-wide memory budget for retained candidate rows.
    #[must_use]
    pub fn with_budget(mut self, budget: Arc<MemoryBudget>) -> Self {
        self.budget = budget;
        self
    }

    fn initialize(&mut self) -> DevonResult<()> {
        if self.k == 0 {
            return Ok(());
        }
        let mut heap = BinaryHeap::new();
        let mut source_types = None;
        let mut row_order = 0_u64;
        while let Some(chunk) = self.source.next_chunk()? {
            validate_source_chunk(&chunk, self.vector_column, &mut source_types)?;
            self.scan_chunk(&chunk, &mut row_order, &mut heap)?;
        }
        if heap.is_empty() {
            return Ok(());
        }
        let types = source_types.ok_or_else(|| {
            invalid_argument("KnnScan selected rows without a source chunk schema")
        })?;
        self.output = build_output(types, heap)?;
        Ok(())
    }

    fn scan_chunk(
        &mut self,
        chunk: &Chunk,
        row_order: &mut u64,
        heap: &mut BinaryHeap<Candidate>,
    ) -> DevonResult<()> {
        for row in 0..chunk.row_count() {
            let order = *row_order;
            *row_order = row_order
                .checked_add(1)
                .ok_or_else(|| invalid_argument("KnnScan row order exceeds u64::MAX"))?;
            let Some(distance) = self.row_distance(chunk, row, order)? else {
                continue;
            };
            let candidate = Candidate {
                distance,
                row_order: order,
                values: clone_row(chunk, row)?,
            };
            self.retain_candidate(heap, candidate)?;
        }
        Ok(())
    }

    fn row_distance(&self, chunk: &Chunk, row: usize, order: u64) -> DevonResult<Option<f32>> {
        let value = chunk.value(row, self.vector_column).ok_or_else(|| {
            invalid_argument(format!(
                "KnnScan row {order} is missing vector column {}",
                self.vector_column
            ))
        })?;
        let vector = match value {
            Value::Null => return Ok(None),
            Value::Vector(vector) => vector,
            other => {
                return Err(invalid_argument(format!(
                    "KnnScan row {order} vector column {} has non-Vector value {other}",
                    self.vector_column
                )));
            }
        };
        if vector.len() != self.query.len() {
            return Err(invalid_argument(format!(
                "KnnScan vector length {} does not match query length {} at row {order}",
                vector.len(),
                self.query.len()
            )));
        }
        Ok(Some(match self.metric {
            Metric::L2 => simd::l2_squared(&vector, &self.query).sqrt(),
            Metric::Cosine => simd::cosine_distance(&vector, &self.query),
        }))
    }
}

impl ChunkSource for KnnScan {
    fn next_chunk(&mut self) -> DevonResult<Option<Chunk>> {
        if !self.initialized {
            self.initialize()?;
            self.initialized = true;
        }
        Ok(self.output.pop_front())
    }
}

struct Candidate {
    distance: f32,
    row_order: u64,
    values: Vec<Value>,
}

impl Drop for KnnScan {
    fn drop(&mut self) {
        self.budget.release(self.charged_bytes);
    }
}

impl PartialEq for Candidate {
    fn eq(&self, other: &Self) -> bool {
        self.distance.total_cmp(&other.distance) == Ordering::Equal
            && self.row_order == other.row_order
    }
}

impl Eq for Candidate {}

impl PartialOrd for Candidate {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Candidate {
    fn cmp(&self, other: &Self) -> Ordering {
        self.distance
            .total_cmp(&other.distance)
            .then_with(|| self.row_order.cmp(&other.row_order))
    }
}

fn validate_source_chunk(
    chunk: &Chunk,
    vector_column: usize,
    source_types: &mut Option<Vec<LogicalType>>,
) -> DevonResult<()> {
    if vector_column >= chunk.column_count() {
        return Err(invalid_argument(format!(
            "KnnScan vector column {vector_column} is out of range for {} source columns",
            chunk.column_count()
        )));
    }
    if let Some(expected) = source_types {
        if expected.as_slice() != chunk.types() {
            return Err(invalid_argument(
                "KnnScan source schema changed between chunks",
            ));
        }
    } else {
        *source_types = Some(chunk.types().to_vec());
    }
    Ok(())
}

impl KnnScan {
    fn retain_candidate(
        &mut self,
        heap: &mut BinaryHeap<Candidate>,
        candidate: Candidate,
    ) -> DevonResult<()> {
        let bytes = buffered_values_charge(&candidate.values)?;
        if (heap.len() as u128) < u128::from(self.k) {
            if !self.budget.charge_or_reclaim(bytes) {
                return Err(DevonError::BudgetExceeded {
                    context: format!(
                        "KnnScan retained row requests {bytes} bytes with {} charged against a {} byte limit",
                        self.budget.charged(),
                        self.budget.limit()
                    ),
                });
            }
            self.charged_bytes = self
                .charged_bytes
                .checked_add(bytes)
                .ok_or_else(|| invalid_argument("KnnScan memory estimate exceeds usize::MAX"))?;
            heap.push(candidate);
            return Ok(());
        }
        let Some(worst) = heap.peek() else {
            return Err(invalid_argument("KnnScan heap is missing its worst row"));
        };
        if candidate >= *worst {
            return Ok(());
        }
        if !self.budget.charge_or_reclaim(bytes) {
            return Err(DevonError::BudgetExceeded {
                context: format!(
                    "KnnScan retained row requests {bytes} bytes with {} charged against a {} byte limit",
                    self.budget.charged(),
                    self.budget.limit()
                ),
            });
        }
        self.charged_bytes = self
            .charged_bytes
            .checked_add(bytes)
            .ok_or_else(|| invalid_argument("KnnScan memory estimate exceeds usize::MAX"))?;
        let evicted = heap.pop();
        if let Some(evicted) = evicted {
            let released = buffered_values_charge(&evicted.values)?;
            self.charged_bytes = self.charged_bytes.saturating_sub(released);
            self.budget.release(released);
        }
        heap.push(candidate);
        Ok(())
    }
}

fn buffered_values_charge(values: &[Value]) -> DevonResult<usize> {
    values.iter().try_fold(0_usize, |total, value| {
        total
            .checked_add(value.approx_bytes())
            .ok_or_else(|| invalid_argument("KnnScan row memory estimate exceeds usize::MAX"))
    })
}

fn build_output(
    mut types: Vec<LogicalType>,
    heap: BinaryHeap<Candidate>,
) -> DevonResult<VecDeque<Chunk>> {
    types.push(LogicalType::Float64);
    let mut output = VecDeque::new();
    let mut builder = ChunkBuilder::new(types.clone());
    for candidate in heap.into_sorted_vec() {
        if builder.is_full() {
            output.push_back(builder.finish());
            builder = ChunkBuilder::new(types.clone());
        }
        let mut row = candidate.values;
        row.push(Value::Float64(f64::from(candidate.distance)));
        builder.push_row(row)?;
    }
    output.push_back(builder.finish());
    Ok(output)
}

fn clone_row(chunk: &Chunk, row: usize) -> DevonResult<Vec<Value>> {
    (0..chunk.column_count())
        .map(|column| {
            chunk.value(row, column).ok_or_else(|| {
                invalid_argument(format!(
                    "KnnScan cannot copy missing row {row}, column {column}"
                ))
            })
        })
        .collect()
}

fn invalid_argument(context: impl Into<String>) -> DevonError {
    DevonError::InvalidArgument {
        context: context.into(),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use devondb_plan::expr::Metric;
    use devondb_types::{DevonError, DevonResult, logical_type::LogicalType, value::Value};

    use super::{DISTANCE_COLUMN_NAME, KnnScan};
    use crate::{
        chunk::{CHUNK_CAPACITY, Chunk, ChunkBuilder},
        source::ChunkSource,
    };

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

    #[test]
    fn knn_l2_returns_exact_known_neighbors_and_schema() {
        let rows = vec![
            row(1, Some(vec![0.0, 0.0])),
            row(2, Some(vec![1.0, 0.0])),
            row(3, Some(vec![0.0, 2.0])),
            row(4, Some(vec![1.0, 1.0])),
        ];
        let output = run(rows, vec![0.0, 0.0], 3, Metric::L2);

        assert_eq!(DISTANCE_COLUMN_NAME, "distance");
        assert_eq!(ids(&output), vec![1, 2, 4]);
        assert_distances(&output, &[0.0, 1.0, 2.0_f64.sqrt()]);
        assert_eq!(
            output[0].types(),
            &[
                LogicalType::Int64,
                LogicalType::String,
                LogicalType::Vector { dim: 2 },
                LogicalType::Float64,
            ]
        );
    }

    #[test]
    fn knn_cosine_returns_exact_known_neighbors() {
        let rows = vec![
            row(1, Some(vec![1.0, 0.0])),
            row(2, Some(vec![1.0, 1.0])),
            row(3, Some(vec![0.0, 1.0])),
            row(4, Some(vec![-1.0, 0.0])),
        ];
        let output = run(rows, vec![1.0, 0.0], 3, Metric::Cosine);

        assert_eq!(ids(&output), vec![1, 2, 3]);
        assert_distances(&output, &[0.0, 1.0 - 1.0 / 2.0_f64.sqrt(), 1.0]);
    }

    #[test]
    fn knn_k_larger_than_row_count_returns_every_non_null_row() {
        let rows = vec![
            row(8, Some(vec![3.0, 0.0])),
            row(9, Some(vec![1.0, 0.0])),
            row(10, Some(vec![2.0, 0.0])),
        ];
        let output = run(rows, vec![0.0, 0.0], 100, Metric::L2);

        assert_eq!(ids(&output), vec![9, 10, 8]);
        assert_distances(&output, &[1.0, 2.0, 3.0]);
    }

    #[test]
    fn knn_k_zero_returns_no_chunks() {
        let source = VecSource::new(vec![fixture_chunk(vec![row(1, Some(vec![0.0, 0.0]))])]);
        let mut knn = KnnScan::new(Box::new(source), 2, vec![0.0, 0.0], 0, Metric::L2);

        assert!(knn.next_chunk().unwrap().is_none());
        assert!(knn.next_chunk().unwrap().is_none());
    }

    #[test]
    fn knn_ties_follow_global_source_row_order_across_chunks() {
        let chunks = vec![
            fixture_chunk(vec![
                row(30, Some(vec![1.0, 0.0])),
                row(10, Some(vec![-1.0, 0.0])),
            ]),
            fixture_chunk(vec![
                row(20, Some(vec![0.0, 1.0])),
                row(40, Some(vec![0.0, -1.0])),
            ]),
        ];
        let output = collect(KnnScan::new(
            Box::new(VecSource::new(chunks)),
            2,
            vec![0.0, 0.0],
            3,
            Metric::L2,
        ));

        assert_eq!(ids(&output), vec![30, 10, 20]);
    }

    #[test]
    fn knn_skips_null_vectors() {
        let rows = vec![
            row(1, None),
            row(2, Some(vec![2.0, 0.0])),
            row(3, None),
            row(4, Some(vec![1.0, 0.0])),
        ];
        let output = run(rows, vec![0.0, 0.0], 4, Metric::L2);

        assert_eq!(ids(&output), vec![4, 2]);
    }

    #[test]
    fn knn_dimension_mismatch_names_both_lengths() {
        let types = vec![LogicalType::Int64, LogicalType::Vector { dim: 3 }];
        let mut builder = ChunkBuilder::new(types);
        builder
            .push_row(vec![Value::Int64(1), Value::Vector(vec![1.0, 2.0, 3.0])])
            .unwrap();
        let source = VecSource::new(vec![builder.finish()]);
        let mut knn = KnnScan::new(Box::new(source), 1, vec![1.0, 2.0], 1, Metric::L2);

        let DevonError::InvalidArgument { context } = knn.next_chunk().unwrap_err() else {
            panic!("expected InvalidArgument");
        };
        assert!(context.contains("vector length 3"));
        assert!(context.contains("query length 2"));
    }

    #[test]
    fn knn_distance_column_matches_scalar_references_for_both_metrics() {
        let query = seeded_vector(17, 0x1234_5678);
        let vector = seeded_vector(17, 0x8765_4321);
        for metric in [Metric::L2, Metric::Cosine] {
            let output = run_dim(
                vec![vec![Value::Int64(1), Value::Vector(vector.clone())]],
                query.clone(),
                1,
                metric,
                17,
                1,
            );
            let actual = distances(&output)[0];
            let expected = scalar_distance(&vector, &query, metric);
            assert_close(actual, expected);
        }
    }

    #[test]
    fn knn_outputs_more_than_one_chunk_without_losing_tie_order() {
        let rows = (0..CHUNK_CAPACITY + 3)
            .map(|index| vec![Value::Int64(index as i64), Value::Vector(vec![1.0])])
            .collect::<Vec<_>>();
        let types = vec![LogicalType::Int64, LogicalType::Vector { dim: 1 }];
        let chunks = rows
            .chunks(CHUNK_CAPACITY)
            .map(|rows| make_chunk(types.clone(), rows.to_vec()))
            .collect();
        let output = collect(KnnScan::new(
            Box::new(VecSource::new(chunks)),
            1,
            vec![0.0],
            (CHUNK_CAPACITY + 3) as u64,
            Metric::L2,
        ));

        assert_eq!(output.len(), 2);
        assert_eq!(output[0].row_count(), CHUNK_CAPACITY);
        assert_eq!(output[1].row_count(), 3);
        assert_eq!(
            ids(&output),
            (0..CHUNK_CAPACITY as i64 + 3).collect::<Vec<_>>()
        );
    }

    fn row(id: i64, vector: Option<Vec<f32>>) -> Vec<Value> {
        vec![
            Value::Int64(id),
            Value::String(format!("row-{id}")),
            vector.map_or(Value::Null, Value::Vector),
        ]
    }

    fn fixture_chunk(rows: Vec<Vec<Value>>) -> Chunk {
        make_chunk(
            vec![
                LogicalType::Int64,
                LogicalType::String,
                LogicalType::Vector { dim: 2 },
            ],
            rows,
        )
    }

    fn make_chunk(types: Vec<LogicalType>, rows: Vec<Vec<Value>>) -> Chunk {
        let mut builder = ChunkBuilder::new(types);
        for row in rows {
            builder.push_row(row).unwrap();
        }
        builder.finish()
    }

    fn run(rows: Vec<Vec<Value>>, query: Vec<f32>, k: u64, metric: Metric) -> Vec<Chunk> {
        run_dim(rows, query, k, metric, 2, 2)
    }

    fn run_dim(
        rows: Vec<Vec<Value>>,
        query: Vec<f32>,
        k: u64,
        metric: Metric,
        dim: u32,
        vector_column: usize,
    ) -> Vec<Chunk> {
        let types = if vector_column == 2 {
            vec![
                LogicalType::Int64,
                LogicalType::String,
                LogicalType::Vector { dim },
            ]
        } else {
            vec![LogicalType::Int64, LogicalType::Vector { dim }]
        };
        let source = VecSource::new(vec![make_chunk(types, rows)]);
        collect(KnnScan::new(
            Box::new(source),
            vector_column,
            query,
            k,
            metric,
        ))
    }

    fn collect(mut source: KnnScan) -> Vec<Chunk> {
        let mut chunks = Vec::new();
        while let Some(chunk) = source.next_chunk().unwrap() {
            chunks.push(chunk);
        }
        chunks
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

    fn distances(chunks: &[Chunk]) -> Vec<f64> {
        chunks
            .iter()
            .flat_map(Chunk::rows)
            .map(|row| match row.last() {
                Some(Value::Float64(value)) => *value,
                other => panic!("expected trailing Float64 distance, got {other:?}"),
            })
            .collect()
    }

    fn assert_distances(chunks: &[Chunk], expected: &[f64]) {
        let actual = distances(chunks);
        assert_eq!(actual.len(), expected.len());
        for (actual, expected) in actual.into_iter().zip(expected) {
            assert_close(actual, *expected);
        }
    }

    fn assert_close(actual: f64, expected: f64) {
        let tolerance = 1.0e-4 * expected.abs().max(1.0);
        assert!(
            (actual - expected).abs() <= tolerance,
            "actual {actual}, expected {expected}, tolerance {tolerance}"
        );
    }

    fn seeded_vector(length: usize, seed: u64) -> Vec<f32> {
        let mut state = seed;
        (0..length)
            .map(|_| {
                state = state
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1_442_695_040_888_963_407);
                2.0 * (state >> 40) as f32 / (1_u32 << 24) as f32 - 1.0
            })
            .collect()
    }

    fn scalar_distance(left: &[f32], right: &[f32], metric: Metric) -> f64 {
        match metric {
            Metric::L2 => f64::from(
                left.iter()
                    .zip(right)
                    .map(|(left, right)| (left - right) * (left - right))
                    .sum::<f32>()
                    .sqrt(),
            ),
            Metric::Cosine => {
                let (dot, norm_left, norm_right) = left.iter().zip(right).fold(
                    (0.0_f32, 0.0_f32, 0.0_f32),
                    |(dot, norm_left, norm_right), (left, right)| {
                        (
                            dot + left * right,
                            norm_left + left * left,
                            norm_right + right * right,
                        )
                    },
                );
                f64::from(1.0 - dot / (norm_left.sqrt() * norm_right.sqrt()))
            }
        }
    }
}
