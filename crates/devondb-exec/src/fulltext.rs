//! Budgeted BM25 top-k over a snapshot-correct, precharged row source.
use std::{cmp::Ordering, collections::BinaryHeap, mem::size_of, sync::Arc};

use crate::{chunk::Chunk, column::Column, source::ChunkSource};
use devondb_storage::{budget::MemoryBudget, fulltext::Bm25Score};
use devondb_types::{DevonError, DevonResult, logical_type::LogicalType, value::Value};

/// An owned reservation which releases only when its working set is dropped.
pub struct Reservation {
    budget: Arc<MemoryBudget>,
    bytes: usize,
}
impl Reservation {
    /// Reserves working memory before allocating it.
    pub fn new(budget: Arc<MemoryBudget>, bytes: usize) -> DevonResult<Self> {
        let mut result = Self { budget, bytes: 0 };
        result.resize(bytes)?;
        Ok(result)
    }
    /// Reconciles the reservation after growth or release.
    pub fn resize(&mut self, bytes: usize) -> DevonResult<()> {
        if bytes > self.bytes && !self.budget.charge_or_reclaim(bytes - self.bytes) {
            return Err(DevonError::BudgetExceeded {
                context: format!(
                    "TextScan working set requested {} bytes with {} charged and limit {}",
                    bytes - self.bytes,
                    self.budget.charged(),
                    self.budget.limit()
                ),
            });
        }
        if bytes < self.bytes {
            self.budget.release(self.bytes - bytes);
        }
        self.bytes = bytes;
        Ok(())
    }
}
impl Drop for Reservation {
    fn drop(&mut self) {
        self.budget.release(self.bytes);
    }
}

/// Per-row scoring supplied by the facade, retaining query statistics and index.
pub type RowScorer = dyn Fn(&Chunk, usize) -> DevonResult<Option<Bm25Score>>;

/// A blocking bounded heap whose order is score descending, then PK ascending.
pub struct TextScan {
    source: Box<dyn ChunkSource>,
    scorer: Box<RowScorer>,
    k: usize,
    key_column: usize,
    types: Vec<LogicalType>,
    pending: Option<Vec<Candidate>>,
    rows: Reservation,
    retained_bytes: usize,
    _heap: Reservation,
}
impl TextScan {
    /// Constructs a scan; the source/scorer own their decode and query charges.
    pub fn new(
        source: Box<dyn ChunkSource>,
        scorer: Box<RowScorer>,
        k: usize,
        key_column: usize,
        mut types: Vec<LogicalType>,
        budget: Arc<MemoryBudget>,
    ) -> DevonResult<Self> {
        let bytes = k
            .checked_mul(size_of::<Candidate>())
            .and_then(|n| n.checked_add(types.len() * 32))
            .ok_or_else(|| invalid("TextScan k heap exceeds address space"))?;
        let heap = Reservation::new(Arc::clone(&budget), bytes)?;
        types.push(LogicalType::Float64);
        Ok(Self {
            source,
            scorer,
            k,
            key_column,
            types,
            pending: None,
            rows: Reservation::new(budget, 0)?,
            retained_bytes: 0,
            _heap: heap,
        })
    }

    fn initialize(&mut self) -> DevonResult<()> {
        let mut heap = BinaryHeap::<Candidate>::new();
        heap.try_reserve_exact(self.k)
            .map_err(|_| invalid("TextScan heap allocation failed"))?;
        while let Some(chunk) = self.source.next_chunk()? {
            for row in 0..chunk.row_count() {
                let Some(score) = (self.scorer)(&chunk, row)? else {
                    continue;
                };
                self.consider(&mut heap, &chunk, row, score)?;
            }
        }
        let mut ranked = heap.into_sorted_vec();
        ranked.reverse();
        self.pending = Some(ranked);
        Ok(())
    }

    fn consider(
        &mut self,
        heap: &mut BinaryHeap<Candidate>,
        chunk: &Chunk,
        row: usize,
        score: Bm25Score,
    ) -> DevonResult<()> {
        let key = chunk
            .value(row, self.key_column)
            .ok_or_else(|| invalid("TextScan row has no primary key"))?;
        if !matches!(key, Value::Int64(_) | Value::String(_)) {
            return Err(invalid("TextScan primary key must be Int64 or String"));
        }
        if heap.len() == self.k {
            let Some(worst) = heap.peek() else {
                return Ok(());
            };
            if score < worst.score
                || (score == worst.score
                    && compare_key(&key, &worst.values[self.key_column]) != Ordering::Less)
            {
                return Ok(());
            }
            if let Some(removed) = heap.pop() {
                self.retained_bytes -= removed.bytes;
            }
        }
        let bytes = row_bytes(chunk, row)?;
        let total = self
            .retained_bytes
            .checked_add(bytes)
            .ok_or_else(|| invalid("TextScan retained rows overflow"))?;
        // Retained rows, the output column builder, and conversion scratch can coexist.
        self.rows.resize(
            total
                .checked_mul(3)
                .ok_or_else(|| invalid("TextScan output peak overflows"))?,
        )?;
        let values = (0..chunk.column_count())
            .map(|column| {
                chunk
                    .value(row, column)
                    .ok_or_else(|| invalid("TextScan source row is incomplete"))
            })
            .collect::<DevonResult<Vec<_>>>()?;
        heap.push(Candidate {
            score,
            values,
            key_column: self.key_column,
            bytes,
        });
        self.retained_bytes = total;
        Ok(())
    }
}
impl ChunkSource for TextScan {
    fn next_chunk(&mut self) -> DevonResult<Option<Chunk>> {
        if self.pending.is_none() {
            self.initialize()?;
        }
        let Some(mut candidate) = self.pending.as_mut().and_then(Vec::pop) else {
            return Ok(None);
        };
        candidate
            .values
            .push(Value::Float64(candidate.score as f64 / 4_294_967_296.0));
        let columns = self
            .types
            .iter()
            .zip(candidate.values)
            .map(|(ty, value)| Column::from_values(ty, vec![value]))
            .collect();
        Chunk::from_columns(self.types.clone(), columns).map(Some)
    }
}

struct Candidate {
    score: Bm25Score,
    values: Vec<Value>,
    key_column: usize,
    bytes: usize,
}
impl PartialEq for Candidate {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
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
        other.score.cmp(&self.score).then_with(|| {
            compare_key(
                &self.values[self.key_column],
                &other.values[other.key_column],
            )
        })
    }
}
fn compare_key(left: &Value, right: &Value) -> Ordering {
    match (left, right) {
        (Value::Int64(a), Value::Int64(b)) => a.cmp(b),
        (Value::String(a), Value::String(b)) => a.cmp(b),
        _ => Ordering::Equal,
    }
}
fn row_bytes(chunk: &Chunk, row: usize) -> DevonResult<usize> {
    let mut bytes = (chunk.column_count() + 1) * size_of::<Value>();
    for column in 0..chunk.column_count() {
        let column = chunk
            .column(column)
            .ok_or_else(|| invalid("TextScan source column missing"))?;
        let value_bytes = column
            .value_ref_boxed(row)
            .map_or(size_of::<Value>(), Value::approx_bytes);
        bytes = bytes
            .checked_add(value_bytes)
            .ok_or_else(|| invalid("TextScan row size overflows"))?;
    }
    Ok(bytes)
}
fn invalid(context: &str) -> DevonError {
    DevonError::InvalidArgument {
        context: context.into(),
    }
}
