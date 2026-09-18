//! Write transactions and snapshots over the committed MVCC overlay
//! (`docs/MVCC.md` §4–§5).

use std::{collections::BTreeMap, sync::Arc};

use devondb_plan::{ops::Plan, statement::Statement};
use devondb_storage::{
    catalog::Catalog,
    hnsw::types::HnswConfig,
    overlay::{
        DDL_OP_OVERHEAD_BYTES, DdlOp, MAP_ENTRY_OVERHEAD_BYTES, OVERLAY_EDGE_OVERHEAD_BYTES,
        PublishedState, REL_ENDPOINT_TOMBSTONES_OVERHEAD_BYTES,
        REL_TOMBSTONE_OFFSET_OVERHEAD_BYTES, ROW_OVERHEAD_BYTES, RelEndpointTombstones,
        SCHEMA_COLUMN_OVERHEAD_BYTES,
    },
};
use devondb_types::{DevonError, DevonResult, schema::NodeTableSchema, value::Value};

use crate::database::{QueryResult, Shared};

/// A read-only pin of one immutable committed database state.
pub struct Snapshot {
    pub(crate) shared: Arc<Shared>,
    pub(crate) state: Arc<PublishedState>,
}

impl Snapshot {
    pub(crate) fn new(shared: Arc<Shared>, state: Arc<PublishedState>) -> Self {
        Self { shared, state }
    }

    /// Validates and runs a plan against this snapshot's pinned state.
    pub fn run(&self, plan: &Plan) -> DevonResult<QueryResult> {
        crate::database::run_snapshot(self, plan)
    }

    /// The pinned state's base catalog generation — the free-page pin key
    /// (`docs/FREE_PAGES.md` § The pin horizon).
    ///
    /// NOT part of the supported API surface; a diagnostic for the pin-horizon
    /// negative-control test with no stability promise.
    #[doc(hidden)]
    #[must_use]
    pub fn catalog_generation(&self) -> u64 {
        self.state.catalog_generation
    }
}

/// A relationship insert whose endpoint keys are resolved only at commit.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PendingEdge {
    pub(crate) from_key: Value,
    pub(crate) to_key: Value,
    pub(crate) values: Vec<Value>,
}

/// A PK-addressed detach intent whose physical offset is derived at commit.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PendingDetach {
    pub(crate) table: String,
    pub(crate) key: Value,
}

/// Validated HNSW creation intent; construction runs under the commit lock.
#[derive(Debug)]
pub(crate) struct PendingHnswIndex {
    pub(crate) name: String,
    pub(crate) table: String,
    pub(crate) column: String,
    pub(crate) schema: NodeTableSchema,
    pub(crate) config: HnswConfig,
}

/// Transaction-local writes, ordered canonically by table and by statement.
#[derive(Debug, Default)]
pub(crate) struct WriteSet {
    pub(crate) nodes: BTreeMap<String, Vec<Vec<Value>>>,
    /// Per node table, full replacement rows staged by `update`, folded so
    /// each primary key appears at most once (`docs/UI.md` §12.1-§12.2).
    pub(crate) node_updates: BTreeMap<String, Vec<Vec<Value>>>,
    /// Per node table, primary keys staged by `delete`; delete wins over an
    /// earlier staged update of the same key (FORMAT.md WAL rule 3).
    pub(crate) node_deletes: BTreeMap<String, Vec<Value>>,
    pub(crate) edges: BTreeMap<String, Vec<PendingEdge>>,
    /// Detach statements in statement order. Offsets never enter this list.
    pub(crate) detaches: Vec<PendingDetach>,
    /// Execute-time resolved copy used only for transaction-view reads.
    pub(crate) rel_tombstones: BTreeMap<String, RelEndpointTombstones>,
    pub(crate) ddl: Vec<DdlOp>,
    pub(crate) inserted_pks: BTreeMap<String, Vec<Value>>,
    pub(crate) hnsw_create: Option<PendingHnswIndex>,
    pub(crate) charged: usize,
}

impl WriteSet {
    pub(crate) fn is_empty(&self) -> bool {
        self.nodes.is_empty()
            && self.node_updates.is_empty()
            && self.node_deletes.is_empty()
            && self.edges.is_empty()
            && self.detaches.is_empty()
            && self.rel_tombstones.is_empty()
            && self.ddl.is_empty()
            && self.hnsw_create.is_none()
    }

    /// Estimates the charge for staging one DML row or key against `table`
    /// in the given map, mirroring the insert-side charge law.
    // This helper is reserved for the DML commit path.
    #[allow(dead_code)]
    pub(crate) fn node_dml_bytes(
        map: &BTreeMap<String, Vec<Vec<Value>>>,
        table: &str,
        row: &[Value],
    ) -> DevonResult<usize> {
        let mut total = 0;
        if !map.contains_key(table) {
            estimate_map_entry(&mut total, table)?;
        }
        checked_accumulate(&mut total, ROW_OVERHEAD_BYTES)?;
        estimate_values(&mut total, row)?;
        Ok(total)
    }

    pub(crate) fn node_insert_bytes(
        &self,
        table: &str,
        rows: &[Vec<Value>],
        keys: &[Value],
    ) -> DevonResult<usize> {
        let mut total = 0;
        if !self.nodes.contains_key(table) {
            estimate_map_entry(&mut total, table)?;
        }
        if !self.inserted_pks.contains_key(table) {
            estimate_map_entry(&mut total, table)?;
        }
        for row in rows {
            checked_accumulate(&mut total, ROW_OVERHEAD_BYTES)?;
            estimate_values(&mut total, row)?;
        }
        estimate_values(&mut total, keys)?;
        Ok(total)
    }

    pub(crate) fn rel_insert_bytes(
        &self,
        table: &str,
        edges: &[PendingEdge],
        from_table: &str,
        to_table: &str,
        from_claims_staged: bool,
        to_claims_staged: bool,
    ) -> DevonResult<usize> {
        let mut total = 0;
        if !self.edges.contains_key(table) {
            estimate_map_entry(&mut total, table)?;
        }
        if !from_claims_staged {
            estimate_map_entry(&mut total, from_table)?;
        }
        if from_table != to_table && !to_claims_staged {
            estimate_map_entry(&mut total, to_table)?;
        }
        for edge in edges {
            checked_accumulate(&mut total, OVERLAY_EDGE_OVERHEAD_BYTES)?;
            checked_accumulate(&mut total, edge.from_key.approx_bytes())?;
            checked_accumulate(&mut total, edge.to_key.approx_bytes())?;
            estimate_values(&mut total, &edge.values)?;
            // The pending edge owns the first copies above; the conflict
            // summary owns one de-duplicated copy per endpoint table. Charging
            // both endpoint values here is a conservative O(edges) upper
            // bound that avoids allocating the summary before reservation.
            checked_accumulate(&mut total, edge.from_key.approx_bytes())?;
            checked_accumulate(&mut total, edge.to_key.approx_bytes())?;
        }
        Ok(total)
    }

    pub(crate) fn detach_bytes(
        &self,
        intent: &PendingDetach,
        tombstones: &[(String, bool, u64)],
    ) -> DevonResult<usize> {
        let mut total = ROW_OVERHEAD_BYTES;
        checked_accumulate(&mut total, intent.table.len())?;
        checked_accumulate(&mut total, intent.key.approx_bytes())?;
        let has_table_claim = self
            .detaches
            .iter()
            .any(|staged| staged.table == intent.table);
        if !has_table_claim {
            estimate_map_entry(&mut total, &intent.table)?;
        }
        if !self
            .detaches
            .iter()
            .any(|staged| staged.table == intent.table && staged.key == intent.key)
        {
            checked_accumulate(&mut total, intent.key.approx_bytes())?;
        }
        for (index, (rel, from, offset)) in tombstones.iter().enumerate() {
            if !self.rel_tombstones.contains_key(rel)
                && !tombstones[..index]
                    .iter()
                    .any(|(previous, _, _)| previous == rel)
            {
                estimate_map_entry(&mut total, rel)?;
                checked_accumulate(&mut total, REL_ENDPOINT_TOMBSTONES_OVERHEAD_BYTES)?;
            }
            let already_staged = self.rel_tombstones.get(rel).is_some_and(|roles| {
                if *from {
                    roles.from_offsets.contains(offset)
                } else {
                    roles.to_offsets.contains(offset)
                }
            });
            let duplicated_here =
                tombstones[..index]
                    .iter()
                    .any(|(previous_rel, previous_from, previous_offset)| {
                        previous_rel == rel && previous_from == from && previous_offset == offset
                    });
            if !already_staged && !duplicated_here {
                checked_accumulate(&mut total, REL_TOMBSTONE_OFFSET_OVERHEAD_BYTES)?;
            }
        }
        Ok(total)
    }

    pub(crate) fn ddl_bytes(ddl: &DdlOp) -> DevonResult<usize> {
        let mut total = DDL_OP_OVERHEAD_BYTES;
        match ddl {
            DdlOp::CreateNodeTable(schema) => {
                checked_accumulate(&mut total, schema.name().len())?;
                estimate_columns(&mut total, schema.columns())?;
            }
            DdlOp::CreateRelTable(schema) => {
                checked_accumulate(&mut total, schema.name().len())?;
                checked_accumulate(&mut total, schema.from().len())?;
                checked_accumulate(&mut total, schema.to().len())?;
                estimate_columns(&mut total, schema.columns())?;
            }
        }
        Ok(total)
    }
}

fn estimate_map_entry(total: &mut usize, table: &str) -> DevonResult<()> {
    checked_accumulate(total, MAP_ENTRY_OVERHEAD_BYTES)?;
    checked_accumulate(total, table.len())
}

fn estimate_columns(
    total: &mut usize,
    columns: &[devondb_types::schema::Column],
) -> DevonResult<()> {
    for column in columns {
        checked_accumulate(total, SCHEMA_COLUMN_OVERHEAD_BYTES)?;
        checked_accumulate(total, column.name.len())?;
    }
    Ok(())
}

fn estimate_values(total: &mut usize, values: &[Value]) -> DevonResult<()> {
    for value in values {
        checked_accumulate(total, value.approx_bytes())?;
    }
    Ok(())
}

fn checked_accumulate(total: &mut usize, bytes: usize) -> DevonResult<()> {
    *total = total
        .checked_add(bytes)
        .ok_or_else(|| DevonError::BudgetExceeded {
            context: "write set byte estimate exceeds usize::MAX".to_owned(),
        })?;
    Ok(())
}

/// An optimistic write transaction with snapshot isolation.
pub struct Transaction {
    pub(crate) shared: Arc<Shared>,
    pub(crate) txn_id: u64,
    pub(crate) state: Arc<PublishedState>,
    pub(crate) catalog: Arc<Catalog>,
    pub(crate) writes: WriteSet,
    pub(crate) poisoned: Option<String>,
    pub(crate) registered: bool,
}

impl Transaction {
    pub(crate) fn new(shared: Arc<Shared>, txn_id: u64, state: Arc<PublishedState>) -> Self {
        let catalog = Arc::clone(&state.catalog);
        Self {
            shared,
            txn_id,
            state,
            catalog,
            writes: WriteSet::default(),
            poisoned: None,
            registered: true,
        }
    }

    /// Validates a statement against the transaction view and buffers its writes.
    pub fn execute(&mut self, statement: &Statement) -> DevonResult<()> {
        if let Some(error) = self.poison_error() {
            return Err(error);
        }
        let result = crate::database::execute_transaction(self, statement);
        if let Err(DevonError::BudgetExceeded { context }) = &result {
            self.poisoned = Some(context.clone());
        }
        result
    }

    /// Runs a plan against the transaction snapshot plus its own writes.
    pub fn run(&mut self, plan: &Plan) -> DevonResult<QueryResult> {
        if let Some(error) = self.poison_error() {
            return Err(error);
        }
        crate::database::run_transaction(self, plan)
    }

    /// Commits all buffered statements atomically, consuming the transaction.
    pub fn commit(mut self) -> DevonResult<()> {
        if let Some(error) = self.poison_error() {
            self.finish();
            return Err(error);
        }
        crate::database::commit_transaction(&mut self)
    }

    /// Aborts this transaction and releases its snapshot and registration.
    pub fn abort(mut self) {
        self.finish();
    }

    pub(crate) fn finish(&mut self) {
        let charged = std::mem::take(&mut self.writes.charged);
        self.shared.budget.release(charged);
        if self.registered {
            self.shared.deregister_write_txn(self.txn_id);
            self.registered = false;
        }
    }

    fn poison_error(&self) -> Option<DevonError> {
        self.poisoned
            .as_ref()
            .map(|context| DevonError::BudgetExceeded {
                context: context.clone(),
            })
    }
}

impl Drop for Transaction {
    fn drop(&mut self) {
        self.finish();
    }
}
