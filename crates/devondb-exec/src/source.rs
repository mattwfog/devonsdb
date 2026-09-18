//! `ChunkSource` and `NeighborSource`: the pull interfaces between
//! executor operators and the storage-backed world.
//!
//! Every operator consumes chunks from an upstream source and is itself a
//! source, forming Volcano-style pull pipelines that move one chunk
//! (~2048 rows) at a time. Storage-backed scans and adjacency implement
//! these traits in the `Database` facade layer.

use std::collections::HashMap;
use std::sync::Arc;

use devondb_plan::ops::{Direction, Operator};
use devondb_storage::budget::MemoryBudget;
use devondb_types::{DevonResult, logical_type::LogicalType, value::Value};

use crate::chunk::Chunk;

/// A pull source of row chunks.
pub trait ChunkSource {
    /// Returns the next chunk, or `None` when the source is exhausted.
    ///
    /// After returning `None` or an error, further calls have unspecified
    /// results — callers stop pulling.
    fn next_chunk(&mut self) -> DevonResult<Option<Chunk>>;
}

/// Values fixed from one selected outer row while a scalar subquery runs.
///
/// Keys are complete `binding.column` references in the containing pipeline,
/// stored under the same ASCII-lowercase fold used by DevonPlan name
/// resolution. Every lookup must fold its reference before querying the map.
/// The executor implementing [`ScalarSubqueryExecutor`] captures the query's
/// snapshot; this map supplies only the row-local correlation values.
pub type OuterBindings = HashMap<String, Value>;

/// The bounded result returned by one scalar-subquery operator-tree run.
#[derive(Debug, Clone, PartialEq)]
pub struct ScalarSubqueryResult {
    /// The embedded query's sole output-column type.
    pub output_type: LogicalType,
    /// Zero, one, or at least two values from the embedded query.
    ///
    /// Implementations MUST stop pulling after the second row: two values are
    /// sufficient for the executor to report the scalar cardinality error.
    /// Retained rows and execution state MUST charge the statement's shared
    /// memory budget returned by [`ScalarSubqueryExecutor::memory_budget`].
    pub values: Vec<Value>,
}

/// Runs embedded operator trees for scalar expressions against one snapshot.
///
/// A database-backed implementation captures the same immutable snapshot as
/// the containing query. It must apply `outer` as fixed visible bindings and
/// return no more than the first two rows of the query's single output column.
/// It MUST stop pulling as soon as the second row is observed and MUST charge
/// all retained rows and execution state to the containing statement's shared
/// memory budget.
pub trait ScalarSubqueryExecutor {
    /// Returns the containing statement's shared memory budget.
    fn memory_budget(&self) -> Arc<MemoryBudget>;

    /// Executes `plan` with the selected outer-row bindings fixed.
    fn execute(&self, plan: &Operator, outer: &OuterBindings) -> DevonResult<ScalarSubqueryResult>;
}

/// Adjacency and node materialization for the `Expand` operator.
///
/// Node identity is the *node offset*: a node's zero-based position in its
/// table's checkpointed storage (`docs/FORMAT.md` § Rel table adjacency).
/// Implementations resolve the rel table's endpoint tables themselves; the
/// operator never sees table names beyond the rel's.
pub trait NeighborSource {
    /// Returns the neighbor node offsets reached from node `from` through
    /// `rel` in `direction`, in edge insertion order.
    ///
    /// For [`Direction::Both`], out-edges come first, then in-edges; a
    /// self-loop edge contributes only once, on the out side.
    fn neighbors(&mut self, rel: &str, direction: Direction, from: u64) -> DevonResult<Vec<u64>>;

    /// Returns the property row of the node at `offset` in the neighbor table
    /// determined by (`rel`, `direction`, source table). `Out` uses the
    /// to-table and `In` uses the from-table. For `Both` on differing endpoint
    /// tables, a from-table source is exactly `Out` and a to-table source is
    /// exactly `In`; coincident endpoint tables use the ordered union above.
    fn node_row(&mut self, rel: &str, direction: Direction, offset: u64)
    -> DevonResult<Vec<Value>>;
}

/// One directional edge occurrence paired with its relationship properties.
#[derive(Debug, Clone)]
pub struct EdgeNeighbor {
    /// Reached node's physical offset in the pinned snapshot.
    pub offset: u64,
    /// Relationship properties in schema declaration order.
    pub values: Vec<Value>,
}

/// Immutable property adjacency and node rows for relationship expansion.
pub trait EdgeNeighborSource {
    /// Returns a shared incident list; each physical occurrence stays distinct.
    fn edges(&self, from: u64) -> DevonResult<Arc<[EdgeNeighbor]>>;
    /// Borrows the reached node's property values without cloning them.
    fn node_row(&self, offset: u64) -> DevonResult<&[Value]>;
}
