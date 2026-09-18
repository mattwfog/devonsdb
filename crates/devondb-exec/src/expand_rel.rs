//! Pull execution for property-preserving relationship expansion.

use crate::{
    chunk::Chunk,
    source::{ChunkSource, EdgeNeighbor, EdgeNeighborSource},
};
use devondb_storage::budget::MemoryBudget;
use devondb_types::{DevonError, DevonResult, logical_type::LogicalType, value::Value};
use std::sync::Arc;

/// Follows each input node and appends its neighbor and relationship values.
pub struct ExpandRel {
    upstream: Box<dyn ChunkSource>,
    neighbors: Box<dyn EdgeNeighborSource>,
    offset_column: usize,
    appended_types: Vec<LogicalType>,
    input: Option<Chunk>,
    row: usize,
    pending: Option<Pending>,
    budget: Arc<MemoryBudget>,
    charged: usize,
    input_bytes: usize,
}

struct Pending {
    input: Vec<Value>,
    edges: Arc<[EdgeNeighbor]>,
    next: usize,
}

impl ExpandRel {
    /// Creates a traversal with appended types: hidden offset, node properties,
    /// and relationship properties. Retained scratch uses the shared budget.
    pub fn new(
        upstream: Box<dyn ChunkSource>,
        neighbors: Box<dyn EdgeNeighborSource>,
        offset_column: usize,
        appended_types: Vec<LogicalType>,
        budget: Arc<MemoryBudget>,
    ) -> Self {
        Self {
            upstream,
            neighbors,
            offset_column,
            appended_types,
            input: None,
            row: 0,
            pending: None,
            budget,
            charged: 0,
            input_bytes: 0,
        }
    }

    fn reserve(&mut self, bytes: usize) -> DevonResult<()> {
        if bytes <= self.charged {
            return Ok(());
        }
        let extra = bytes - self.charged;
        if !self.budget.charge_or_reclaim(extra) {
            return Err(DevonError::BudgetExceeded {
                context: format!(
                    "ExpandRel scratch requested={bytes} charged={} limit={}",
                    self.budget.charged(),
                    self.budget.limit()
                ),
            });
        }
        self.charged = bytes;
        Ok(())
    }

    fn pull(&mut self) -> DevonResult<bool> {
        let Some(chunk) = self.upstream.next_chunk()? else {
            return Ok(false);
        };
        let mut bytes = chunk
            .column_count()
            .checked_mul(chunk.row_count() * size_of::<Value>())
            .ok_or_else(overflow)?;
        for column in 0..chunk.column_count() {
            bytes = bytes
                .checked_add(chunk.column(column).ok_or_else(overflow)?.approx_bytes())
                .ok_or_else(overflow)?;
        }
        self.input_bytes = bytes.checked_mul(3).ok_or_else(overflow)?;
        self.reserve(self.input_bytes)?;
        self.input = Some(chunk);
        self.row = 0;
        Ok(true)
    }

    fn prepare(&mut self) -> DevonResult<()> {
        let chunk = self.input.as_ref().ok_or_else(overflow)?;
        let values = (0..chunk.column_count())
            .map(|column| chunk.value(self.row, column).ok_or_else(overflow))
            .collect::<DevonResult<Vec<_>>>()?;
        let offset = match values.get(self.offset_column) {
            Some(Value::Int64(value)) if *value >= 0 => *value as u64,
            _ => {
                return Err(DevonError::Corrupt {
                    context: "ExpandRel source offset is not a nonnegative Int64".into(),
                });
            }
        };
        let edges = self.neighbors.edges(offset)?;
        if edges.is_empty() {
            self.row += 1;
        } else {
            self.pending = Some(Pending {
                input: values,
                edges,
                next: 0,
            });
        }
        Ok(())
    }

    fn emit(&mut self, builder: &mut PropertyChunk, output_bytes: &mut usize) -> DevonResult<()> {
        let pending = self.pending.as_ref().ok_or_else(overflow)?;
        let edge = pending.edges.get(pending.next).ok_or_else(overflow)?;
        let node = self.neighbors.node_row(edge.offset)?;
        let row_bytes = pending
            .input
            .iter()
            .chain(node)
            .chain(&edge.values)
            .try_fold(size_of::<Value>(), |sum, value| {
                sum.checked_add(value.approx_bytes()).ok_or_else(overflow)
            })?;
        *output_bytes = output_bytes
            .checked_add(row_bytes.checked_mul(3).ok_or_else(overflow)?)
            .ok_or_else(overflow)?;
        self.reserve(
            self.input_bytes
                .checked_add(*output_bytes)
                .ok_or_else(overflow)?,
        )?;
        let pending = self.pending.as_mut().ok_or_else(overflow)?;
        let edge = &pending.edges[pending.next];
        let mut output = pending.input.clone();
        output.push(Value::Int64(
            i64::try_from(edge.offset).map_err(|_| overflow())?,
        ));
        output.extend_from_slice(self.neighbors.node_row(edge.offset)?);
        output.extend_from_slice(&edge.values);
        builder.push_row(output)?;
        pending.next += 1;
        if pending.next == pending.edges.len() {
            self.pending = None;
            self.row += 1;
        }
        Ok(())
    }
}

impl ChunkSource for ExpandRel {
    fn next_chunk(&mut self) -> DevonResult<Option<Chunk>> {
        loop {
            if self.input.is_none() && !self.pull()? {
                return Ok(None);
            }
            let input = self.input.as_ref().ok_or_else(overflow)?;
            let mut types = input.types().to_vec();
            types.extend_from_slice(&self.appended_types);
            let mut output_bytes = types
                .len()
                .checked_mul(OUTPUT_ROWS * size_of::<Value>() * 3)
                .ok_or_else(overflow)?;
            self.reserve(
                self.input_bytes
                    .checked_add(output_bytes)
                    .ok_or_else(overflow)?,
            )?;
            let mut builder = PropertyChunk::new(types);
            let mut emitted = false;
            while !builder.is_full() {
                if self.pending.is_some() {
                    self.emit(&mut builder, &mut output_bytes)?;
                    emitted = true;
                } else if self
                    .input
                    .as_ref()
                    .is_none_or(|input| self.row == input.row_count())
                {
                    self.input = None;
                    break;
                } else {
                    self.prepare()?;
                }
            }
            if emitted {
                return Ok(Some(builder.finish()?));
            }
        }
    }
}

impl Drop for ExpandRel {
    fn drop(&mut self) {
        self.budget.release(self.charged);
    }
}

fn overflow() -> DevonError {
    DevonError::Corrupt {
        context: "ExpandRel working set overflow or missing row value".into(),
    }
}

// Smaller output batches cap the column builder's reserved slots on edge
// devices while still respecting the executor's maximum chunk capacity.
const OUTPUT_ROWS: usize = 128;

struct PropertyChunk {
    types: Vec<LogicalType>,
    columns: Vec<Vec<Value>>,
    rows: usize,
}

impl PropertyChunk {
    fn new(types: Vec<LogicalType>) -> Self {
        let columns = types
            .iter()
            .map(|_| Vec::with_capacity(OUTPUT_ROWS))
            .collect();
        Self {
            types,
            columns,
            rows: 0,
        }
    }

    fn push_row(&mut self, row: Vec<Value>) -> DevonResult<()> {
        if row.len() != self.types.len()
            || row
                .iter()
                .zip(&self.types)
                .any(|(value, ty)| !value.matches_type(ty))
        {
            return Err(DevonError::Corrupt {
                context: "ExpandRel property row does not match its schema".into(),
            });
        }
        for (column, value) in self.columns.iter_mut().zip(row) {
            column.push(value);
        }
        self.rows += 1;
        Ok(())
    }

    fn is_full(&self) -> bool {
        self.rows == OUTPUT_ROWS
    }

    fn finish(self) -> DevonResult<Chunk> {
        let columns = self
            .types
            .iter()
            .zip(self.columns)
            .map(|(ty, values)| crate::column::Column::from_values(ty, values))
            .collect();
        Chunk::from_columns(self.types, columns)
    }
}
