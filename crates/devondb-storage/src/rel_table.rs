//! Rel-table storage: WAL-durable edge inserts, checkpoint merges into
//! fwd/bwd CSR groups, and neighbor reads (`docs/FORMAT.md` § Rel table
//! adjacency, binding).
//!
//! Model mirrors `node_table`: edges are durable in the WAL the moment
//! `insert` returns and buffer in memory as the delta overlay;
//! `checkpoint` merges the overlay by rewriting exactly the CSR groups
//! whose edge sets changed, in both directions, and publishes them through
//! the catalog storage map; reads overlay buffered edges on the CSR.

mod property_neighbors;

use std::collections::{BTreeMap, BTreeSet};

use devondb_types::{
    DevonError, DevonResult,
    logical_type::LogicalType,
    schema::{RelTableSchema, fold},
    value::Value,
};
use serde::{Deserialize, Serialize};

use crate::budget::MemoryBudget;
use crate::catalog::{Catalog, RelStorage};
use crate::csr_group::CsrGroup;
use crate::node_group::NodeGroup;
use crate::overlay::RelEndpointTombstones;
use crate::pager::Pager;
use crate::wal::WalWriter;

/// A storage-level relationship traversal direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// Follow source-to-destination adjacency.
    Out,
    /// Follow destination-to-source adjacency.
    In,
    /// Return outgoing neighbors followed by incoming neighbors.
    Both,
}

/// A monotone node-offset mapping for one checkpoint epoch.
///
/// The representation is proportional to deleted rows: surviving offset
/// `x` maps to `x - rank(deleted < x)`, while a deleted offset maps to
/// `None`. No row-count-sized mapping array is constructed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OffsetRemap {
    old_domain: u64,
    deleted: Vec<u64>,
}

/// The measured affected group that blocked detach checkpoint compaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetachCheckpointBlocked {
    /// Relationship table display name.
    pub relationship: String,
    /// Physical CSR direction (`fwd` or `bwd`).
    pub direction: String,
    /// Replacement CSR group index.
    pub group: usize,
    /// Scratch bytes requested for the group.
    pub requested: usize,
    /// Shared-accountant bytes charged when the request failed.
    pub charged: usize,
    /// Configured shared memory limit.
    pub limit: usize,
}

/// WAL-backed storage for one catalog relationship table.
pub struct RelTable {
    schema: RelTableSchema,
    buffered_edges: Vec<Edge>,
    endpoint_tombstones: RelEndpointTombstones,
}

/// One relationship row returned by [`RelScanCursor`].
#[derive(Debug, Clone, PartialEq)]
pub struct ScannedRelationship {
    /// Source-node offset.
    pub from: u64,
    /// Destination-node offset.
    pub to: u64,
    /// Property values in catalog declaration order.
    pub values: Vec<Value>,
}

/// Forward-only cursor over persisted CSR followed by buffered edges.
///
/// At most one CSR group is decoded at a time. Endpoint tombstones are
/// applied before rows are yielded, so memory does not grow with edge count.
pub struct RelScanCursor<'a> {
    table: &'a RelTable,
    pager: &'a Pager,
    source_groups: &'a [u64],
    fwd_groups: &'a [u64],
    source_columns: usize,
    neighbor_rows: u64,
    types: Vec<LogicalType>,
    group_index: usize,
    next_group_start: u64,
    group_start: u64,
    group: Option<CsrGroup>,
    slot: usize,
    edge_index: usize,
    buffered_index: usize,
    base_finished: bool,
    finished: bool,
}

#[derive(Debug, Clone, PartialEq)]
struct Edge {
    from: u64,
    to: u64,
    values: Vec<Value>,
}

struct PendingEdge<'a> {
    slot: usize,
    neighbor: u64,
    values: &'a [Value],
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WalRecord {
    rel: String,
    from: u64,
    to: u64,
    values: Vec<Value>,
}

#[derive(Debug)]
struct EndpointLayout {
    row_counts: Vec<usize>,
    starts: Vec<u64>,
    total_rows: u64,
}

struct ScratchCharge<'a> {
    budget: &'a MemoryBudget,
    bytes: usize,
}

impl OffsetRemap {
    /// Creates a remap over `old_domain`, canonicalizing the deleted offsets
    /// into strictly increasing order.
    pub fn new(old_domain: u64, mut deleted: Vec<u64>) -> DevonResult<Self> {
        deleted.sort_unstable();
        deleted.dedup();
        if let Some(offset) = deleted.iter().find(|offset| **offset >= old_domain) {
            return Err(corrupt(format!(
                "deleted node offset {offset} is outside old epoch domain {old_domain}"
            )));
        }
        Ok(Self {
            old_domain,
            deleted,
        })
    }

    /// Creates an identity remap over `old_domain`.
    #[must_use]
    pub const fn identity(old_domain: u64) -> Self {
        Self {
            old_domain,
            deleted: Vec::new(),
        }
    }

    /// Returns the old epoch's complete assigned physical domain.
    #[must_use]
    pub const fn old_domain(&self) -> u64 {
        self.old_domain
    }

    /// Returns the compacted epoch's physical domain.
    pub fn new_domain(&self) -> DevonResult<u64> {
        let deleted = u64::try_from(self.deleted.len())
            .map_err(|_| corrupt("deleted-offset rank exceeds u64"))?;
        self.old_domain
            .checked_sub(deleted)
            .ok_or_else(|| corrupt("deleted-offset rank exceeds the old epoch domain"))
    }

    /// Returns whether this mapping preserves every old offset.
    #[must_use]
    pub fn is_identity(&self) -> bool {
        self.deleted.is_empty()
    }

    /// Returns the sorted deleted-offset representation.
    #[must_use]
    pub fn deleted_offsets(&self) -> &[u64] {
        &self.deleted
    }

    /// Maps one old-epoch offset into the compacted epoch.
    pub fn map(&self, old_offset: u64) -> DevonResult<Option<u64>> {
        if old_offset >= self.old_domain {
            return Err(corrupt(format!(
                "node offset {old_offset} is outside old epoch domain {}",
                self.old_domain
            )));
        }
        match self.deleted.binary_search(&old_offset) {
            Ok(_) => Ok(None),
            Err(rank) => {
                let rank =
                    u64::try_from(rank).map_err(|_| corrupt("deleted-offset rank exceeds u64"))?;
                Ok(Some(old_offset - rank))
            }
        }
    }

    fn survivor_range(&self, start: u64, end: u64) -> DevonResult<Option<(u64, u64)>> {
        if start >= end {
            return Ok(None);
        }
        let mut first = start;
        while first < end && self.deleted.binary_search(&first).is_ok() {
            first += 1;
        }
        if first == end {
            return Ok(None);
        }
        let mut last = end - 1;
        while last > first && self.deleted.binary_search(&last).is_ok() {
            last -= 1;
        }
        let mapped_first = self
            .map(first)?
            .ok_or_else(|| corrupt("first survivor mapped to deletion"))?;
        let mapped_last = self
            .map(last)?
            .ok_or_else(|| corrupt("last survivor mapped to deletion"))?;
        Ok(Some((mapped_first, mapped_last + 1)))
    }
}

impl DetachCheckpointBlocked {
    /// Returns the canonical six-field budget context.
    #[must_use]
    pub fn context(&self) -> String {
        format!(
            "DETACH_CHECKPOINT_BLOCKED relationship={} direction={} group={} requested={} charged={} limit={}",
            self.relationship, self.direction, self.group, self.requested, self.charged, self.limit
        )
    }

    /// Rebuilds a measured detach checkpoint failure from its public error.
    #[must_use]
    pub fn from_error(error: &DevonError) -> Option<Self> {
        let DevonError::BudgetExceeded { context } = error else {
            return None;
        };
        let fields = context.strip_prefix("DETACH_CHECKPOINT_BLOCKED relationship=")?;
        let (fields, limit) = fields.rsplit_once(" limit=")?;
        let (fields, charged) = fields.rsplit_once(" charged=")?;
        let (fields, requested) = fields.rsplit_once(" requested=")?;
        let (fields, group) = fields.rsplit_once(" group=")?;
        let (relationship, direction) = fields.rsplit_once(" direction=")?;
        Some(Self {
            relationship: relationship.to_owned(),
            direction: direction.to_owned(),
            group: group.parse().ok()?,
            requested: requested.parse().ok()?,
            charged: charged.parse().ok()?,
            limit: limit.parse().ok()?,
        })
    }
}

impl ScratchCharge<'_> {
    fn reserve<'a>(
        budget: &'a MemoryBudget,
        bytes: usize,
        failure: DetachCheckpointBlocked,
    ) -> DevonResult<ScratchCharge<'a>> {
        if !budget.charge_or_reclaim(bytes) {
            return Err(DevonError::BudgetExceeded {
                context: DetachCheckpointBlocked {
                    charged: budget.charged(),
                    ..failure
                }
                .context(),
            });
        }
        Ok(ScratchCharge { budget, bytes })
    }
}

impl Drop for ScratchCharge<'_> {
    fn drop(&mut self) {
        self.budget.release(self.bytes);
    }
}

impl<'a> RelScanCursor<'a> {
    fn new(table: &'a RelTable, pager: &'a Pager, catalog: &'a Catalog) -> DevonResult<Self> {
        ensure_catalog_schema(&table.schema, catalog)?;
        let source =
            catalog
                .node_table(table.schema.from())
                .ok_or_else(|| DevonError::NotFound {
                    what: format!("node table `{}`", table.schema.from()),
                })?;
        let destination =
            catalog
                .node_table(table.schema.to())
                .ok_or_else(|| DevonError::NotFound {
                    what: format!("node table `{}`", table.schema.to()),
                })?;
        let source_groups = catalog
            .table_storage(source.name())
            .map_or(&[][..], |storage| storage.groups.as_slice());
        let destination_groups = catalog
            .table_storage(destination.name())
            .map_or(&[][..], |storage| storage.groups.as_slice());
        let storage = catalog.rel_storage(table.schema.name());
        let fwd_groups = storage.map_or(&[][..], |state| state.fwd.as_slice());
        let bwd_count = storage.map_or(0, |state| state.bwd.len());
        if fwd_groups.len() > source_groups.len() {
            return Err(corrupt(
                "relationship forward storage has more groups than its source table",
            ));
        }
        if bwd_count > destination_groups.len() {
            return Err(corrupt(
                "relationship backward storage has more groups than its destination table",
            ));
        }
        let neighbor_rows = node_row_total(pager, destination, destination_groups)?;
        Ok(Self {
            table,
            pager,
            source_groups,
            fwd_groups,
            source_columns: source.columns().len(),
            neighbor_rows,
            types: schema_types(&table.schema),
            group_index: 0,
            next_group_start: 0,
            group_start: 0,
            group: None,
            slot: 0,
            edge_index: 0,
            buffered_index: 0,
            base_finished: false,
            finished: false,
        })
    }

    fn next_visible(&mut self) -> DevonResult<Option<ScannedRelationship>> {
        loop {
            let edge = if self.base_finished {
                self.next_buffered()
            } else if let Some(edge) = self.next_checkpointed()? {
                Some(edge)
            } else {
                self.base_finished = true;
                self.next_buffered()
            };
            let Some(edge) = edge else {
                return Ok(None);
            };
            if !self.table.edge_is_tombstoned(edge.from, edge.to) {
                return Ok(Some(edge));
            }
        }
    }

    fn next_checkpointed(&mut self) -> DevonResult<Option<ScannedRelationship>> {
        loop {
            if let Some(edge) = self.next_group_edge()? {
                return Ok(Some(edge));
            }
            if !self.load_next_group()? {
                return Ok(None);
            }
        }
    }

    fn next_group_edge(&mut self) -> DevonResult<Option<ScannedRelationship>> {
        let Some(group) = &self.group else {
            return Ok(None);
        };
        while self.slot < group.row_count() {
            let range = group
                .edge_range(self.slot)
                .ok_or_else(|| corrupt("CSR group is missing a covered source slot"))?;
            if self.edge_index < range.end {
                let edge_index = self.edge_index;
                self.edge_index += 1;
                let slot = u64::try_from(self.slot)
                    .map_err(|_| corrupt("CSR source slot exceeds u64::MAX"))?;
                let from = self
                    .group_start
                    .checked_add(slot)
                    .ok_or_else(|| corrupt("relationship source offset exceeds u64::MAX"))?;
                return Ok(Some(ScannedRelationship {
                    from,
                    to: csr_neighbor(group, edge_index)?,
                    values: csr_values(group, edge_index)?,
                }));
            }
            self.slot += 1;
            self.edge_index = range.end;
        }
        self.group = None;
        Ok(None)
    }

    fn load_next_group(&mut self) -> DevonResult<bool> {
        while let Some(group_id) = self.source_groups.get(self.group_index) {
            let index = self.group_index;
            self.group_index += 1;
            let row_count =
                read_node_group_row_count(self.pager, *group_id, self.source_columns, index)?;
            self.group_start = self.next_group_start;
            self.next_group_start = add_row_count(self.next_group_start, row_count)?;
            let Some(csr_id) = stored_group_id(self.fwd_groups, index) else {
                continue;
            };
            let group =
                CsrGroup::read_checked(self.pager, csr_id, &self.types, self.neighbor_rows)?;
            validate_csr_row_count(&group, row_count, index)?;
            self.group = Some(group);
            self.slot = 0;
            self.edge_index = 0;
            return Ok(true);
        }
        Ok(false)
    }

    fn next_buffered(&mut self) -> Option<ScannedRelationship> {
        let edge = self.table.buffered_edges.get(self.buffered_index)?;
        self.buffered_index += 1;
        Some(ScannedRelationship {
            from: edge.from,
            to: edge.to,
            values: edge.values.clone(),
        })
    }
}

impl Iterator for RelScanCursor<'_> {
    type Item = DevonResult<ScannedRelationship>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.finished {
            return None;
        }
        match self.next_visible() {
            Ok(Some(edge)) => Some(Ok(edge)),
            Ok(None) => {
                self.finished = true;
                None
            }
            Err(error) => {
                self.finished = true;
                Some(Err(error))
            }
        }
    }
}

fn node_row_total(
    pager: &Pager,
    schema: &devondb_types::schema::NodeTableSchema,
    group_ids: &[u64],
) -> DevonResult<u64> {
    let mut total = 0_u64;
    for (index, group_id) in group_ids.iter().copied().enumerate() {
        let rows = read_node_group_row_count(pager, group_id, schema.columns().len(), index)?;
        total = add_row_count(total, rows)?;
    }
    Ok(total)
}

fn add_row_count(total: u64, rows: usize) -> DevonResult<u64> {
    let rows = u64::try_from(rows).map_err(|_| corrupt("node row count exceeds u64::MAX"))?;
    total
        .checked_add(rows)
        .ok_or_else(|| corrupt("node row count exceeds u64::MAX"))
}

impl EndpointLayout {
    fn locate(&self, offset: u64) -> Option<(usize, usize)> {
        if offset >= self.total_rows {
            return None;
        }
        let group = self.starts.partition_point(|start| *start <= offset) - 1;
        let slot = usize::try_from(offset - self.starts[group]).ok()?;
        Some((group, slot))
    }
}

impl RelTable {
    /// Creates an empty in-memory delta buffer for a relationship table.
    #[must_use]
    pub const fn new(schema: RelTableSchema) -> Self {
        Self {
            schema,
            buffered_edges: Vec::new(),
            endpoint_tombstones: RelEndpointTombstones {
                from_offsets: std::collections::BTreeSet::new(),
                to_offsets: std::collections::BTreeSet::new(),
            },
        }
    }

    /// Returns this table's validated schema.
    #[must_use]
    pub fn schema(&self) -> &RelTableSchema {
        &self.schema
    }

    /// Replaces the endpoint-role predicate applied by read operations.
    ///
    /// Checkpoint materialization deliberately ignores this read overlay;
    /// detach checkpoint remapping is a separate format-governed seam.
    pub fn set_endpoint_tombstones(&mut self, tombstones: RelEndpointTombstones) {
        self.endpoint_tombstones = tombstones;
    }

    /// Validates and durably appends one relationship edge.
    pub fn insert(
        &mut self,
        wal: &mut WalWriter,
        from_offset: u64,
        to_offset: u64,
        values: Vec<Value>,
    ) -> DevonResult<()> {
        validate_values(&self.schema, &values)?;
        let payload = encode_wal_record(self.schema.name(), from_offset, to_offset, &values)?;
        wal.append(&payload)?;
        wal.sync()?;
        self.buffered_edges.push(Edge {
            from: from_offset,
            to: to_offset,
            values,
        });
        Ok(())
    }

    /// Validates and buffers one relationship edge recovered from the WAL.
    pub fn recover_edge(
        &mut self,
        from_offset: u64,
        to_offset: u64,
        values: Vec<Value>,
    ) -> DevonResult<()> {
        validate_values(&self.schema, &values)?;
        self.buffered_edges.push(Edge {
            from: from_offset,
            to: to_offset,
            values,
        });
        Ok(())
    }

    /// Returns whether this table has edges awaiting checkpoint.
    #[must_use]
    pub fn has_buffered_edges(&self) -> bool {
        !self.buffered_edges.is_empty()
    }

    /// Merges every buffered edge into forward and backward CSR groups.
    pub fn checkpoint(&mut self, pager: &Pager, catalog: &mut Catalog) -> DevonResult<()> {
        if self.buffered_edges.is_empty() {
            return Ok(());
        }
        ensure_catalog_schema(&self.schema, catalog)?;
        let from_layout = endpoint_layout(pager, catalog, self.schema.from())?;
        let to_layout = endpoint_layout(pager, catalog, self.schema.to())?;
        validate_buffered_offsets(&self.buffered_edges, &from_layout, &to_layout)?;

        let mut storage = catalog
            .rel_storage(self.schema.name())
            .cloned()
            .unwrap_or_default();
        validate_storage_lengths(&storage, &from_layout, &to_layout)?;
        let types = schema_types(&self.schema);
        merge_direction(
            pager,
            &mut storage.fwd,
            &from_layout,
            to_layout.total_rows,
            &types,
            &self.buffered_edges,
            Direction::Out,
        )?;
        merge_direction(
            pager,
            &mut storage.bwd,
            &to_layout,
            from_layout.total_rows,
            &types,
            &self.buffered_edges,
            Direction::In,
        )?;
        catalog.set_rel_storage(self.schema.name(), storage)?;
        self.buffered_edges.clear();
        Ok(())
    }

    /// Rebuilds affected forward and backward CSR groups for one node-offset
    /// epoch change.
    ///
    /// Reads always come from `old_catalog`; replacement entries accumulate
    /// only in the unpublished `new_catalog`. Each replacement group is
    /// charged, built, compared for byte-identical logical content, and
    /// released before the next group. Empty groups use a zero/short catalog
    /// entry and no `RCSR` page.
    ///
    /// A source group is affected when:
    ///
    /// - its covered slot layout changes because an endpoint row before or
    ///   inside it was compacted;
    /// - it contains an incident edge removed by an endpoint tombstone;
    /// - it contains a surviving neighbor whose offset changes under the
    ///   other endpoint's remap; or
    /// - it receives a committed overlay edge insert.
    ///
    /// The implementation inspects every replacement group and reuses an old
    /// page only after complete logical equality proves all five persisted
    /// components (slots, neighbors, properties, order, and row count).
    pub fn checkpoint_with_remap(
        &mut self,
        pager: &Pager,
        old_catalog: &Catalog,
        new_catalog: &mut Catalog,
        from_remap: &OffsetRemap,
        to_remap: &OffsetRemap,
        budget: &MemoryBudget,
    ) -> DevonResult<()> {
        if self.buffered_edges.is_empty()
            && self.endpoint_tombstones.is_empty()
            && from_remap.is_identity()
            && to_remap.is_identity()
        {
            return Ok(());
        }
        ensure_catalog_schema(&self.schema, old_catalog)?;
        ensure_catalog_schema(&self.schema, new_catalog)?;
        let old_from = endpoint_layout(pager, old_catalog, self.schema.from())?;
        let old_to = endpoint_layout(pager, old_catalog, self.schema.to())?;
        let new_from = endpoint_layout(pager, new_catalog, self.schema.from())?;
        let new_to = endpoint_layout(pager, new_catalog, self.schema.to())?;
        validate_remap_layout(from_remap, &old_from, &new_from, self.schema.from())?;
        validate_remap_layout(to_remap, &old_to, &new_to, self.schema.to())?;
        validate_buffered_remap_offsets(&self.buffered_edges, from_remap, to_remap)?;

        let mut storage = old_catalog
            .rel_storage(self.schema.name())
            .cloned()
            .unwrap_or_default();
        validate_storage_lengths(&storage, &old_from, &old_to)?;
        let types = schema_types(&self.schema);
        rewrite_remapped_direction(
            self,
            pager,
            new_catalog,
            &mut storage,
            &old_from,
            &new_from,
            &old_to,
            from_remap,
            to_remap,
            &types,
            Direction::Out,
            budget,
        )?;
        rewrite_remapped_direction(
            self,
            pager,
            new_catalog,
            &mut storage,
            &old_to,
            &new_to,
            &old_from,
            to_remap,
            from_remap,
            &types,
            Direction::In,
            budget,
        )?;
        new_catalog.set_rel_storage(self.schema.name(), storage)?;
        self.buffered_edges.clear();
        self.endpoint_tombstones = RelEndpointTombstones::default();
        Ok(())
    }

    /// Returns neighbors from persisted CSR plus the buffered delta overlay.
    pub fn neighbors(
        &self,
        pager: &Pager,
        catalog: &Catalog,
        direction: Direction,
        from: u64,
    ) -> DevonResult<Vec<u64>> {
        ensure_catalog_schema(&self.schema, catalog)?;
        validate_direction(&self.schema, direction)?;
        let from_layout = endpoint_layout(pager, catalog, self.schema.from())?;
        let to_layout = endpoint_layout(pager, catalog, self.schema.to())?;
        let storage = catalog.rel_storage(self.schema.name());
        if let Some(storage) = storage {
            validate_storage_lengths(storage, &from_layout, &to_layout)?;
        }

        match direction {
            Direction::Out => self.neighbors_one(
                pager,
                storage.map_or(&[][..], |state| state.fwd.as_slice()),
                &from_layout,
                to_layout.total_rows,
                Direction::Out,
                from,
            ),
            Direction::In => self.neighbors_one(
                pager,
                storage.map_or(&[][..], |state| state.bwd.as_slice()),
                &to_layout,
                from_layout.total_rows,
                Direction::In,
                from,
            ),
            Direction::Both => {
                let mut neighbors = self.neighbors_one(
                    pager,
                    storage.map_or(&[][..], |state| state.fwd.as_slice()),
                    &from_layout,
                    to_layout.total_rows,
                    Direction::Out,
                    from,
                )?;
                let mut incoming = self.neighbors_one(
                    pager,
                    storage.map_or(&[][..], |state| state.bwd.as_slice()),
                    &to_layout,
                    from_layout.total_rows,
                    Direction::In,
                    from,
                )?;
                if fold(self.schema.from()) == fold(self.schema.to()) {
                    incoming.retain(|neighbor| *neighbor != from);
                }
                neighbors.extend(incoming);
                Ok(neighbors)
            }
        }
    }

    /// Returns every neighbor read into resident adjacency before endpoint
    /// filtering and self-loop de-duplication.
    ///
    /// This view exists for memory-budget sizing. Query results must use
    /// [`Self::neighbors`]. In `Both`, the two physical CSR copies are both
    /// included because both are resident before logical de-duplication.
    pub fn neighbors_before_filtering(
        &self,
        pager: &Pager,
        catalog: &Catalog,
        direction: Direction,
        from: u64,
    ) -> DevonResult<Vec<u64>> {
        ensure_catalog_schema(&self.schema, catalog)?;
        validate_direction(&self.schema, direction)?;
        let from_layout = endpoint_layout(pager, catalog, self.schema.from())?;
        let to_layout = endpoint_layout(pager, catalog, self.schema.to())?;
        let storage = catalog.rel_storage(self.schema.name());
        if let Some(storage) = storage {
            validate_storage_lengths(storage, &from_layout, &to_layout)?;
        }
        match direction {
            Direction::Out => self.neighbors_one_unfiltered(
                pager,
                storage.map_or(&[][..], |state| state.fwd.as_slice()),
                &from_layout,
                to_layout.total_rows,
                Direction::Out,
                from,
            ),
            Direction::In => self.neighbors_one_unfiltered(
                pager,
                storage.map_or(&[][..], |state| state.bwd.as_slice()),
                &to_layout,
                from_layout.total_rows,
                Direction::In,
                from,
            ),
            Direction::Both => {
                let mut neighbors = self.neighbors_one_unfiltered(
                    pager,
                    storage.map_or(&[][..], |state| state.fwd.as_slice()),
                    &from_layout,
                    to_layout.total_rows,
                    Direction::Out,
                    from,
                )?;
                neighbors.extend(self.neighbors_one_unfiltered(
                    pager,
                    storage.map_or(&[][..], |state| state.bwd.as_slice()),
                    &to_layout,
                    from_layout.total_rows,
                    Direction::In,
                    from,
                )?);
                Ok(neighbors)
            }
        }
    }

    /// Reads checkpointed forward CSR edges, then the buffered edge tail.
    pub fn scan(
        &self,
        pager: &Pager,
        catalog: &Catalog,
    ) -> DevonResult<Vec<(u64, u64, Vec<Value>)>> {
        self.scan_cursor(pager, catalog)?
            .map(|edge| edge.map(|edge| (edge.from, edge.to, edge.values)))
            .collect()
    }

    /// Streams checkpointed forward CSR and the buffered edge tail without
    /// materializing all relationship rows.
    pub fn scan_cursor<'a>(
        &'a self,
        pager: &'a Pager,
        catalog: &'a Catalog,
    ) -> DevonResult<RelScanCursor<'a>> {
        RelScanCursor::new(self, pager, catalog)
    }

    fn neighbors_one(
        &self,
        pager: &Pager,
        group_ids: &[u64],
        grouped_layout: &EndpointLayout,
        neighbor_row_count: u64,
        direction: Direction,
        from: u64,
    ) -> DevonResult<Vec<u64>> {
        if self.grouped_endpoint_is_tombstoned(direction, from) {
            return Ok(Vec::new());
        }
        let mut neighbors = self.neighbors_one_unfiltered(
            pager,
            group_ids,
            grouped_layout,
            neighbor_row_count,
            direction,
            from,
        )?;
        neighbors.retain(|neighbor| !self.neighbor_is_tombstoned(direction, *neighbor));
        Ok(neighbors)
    }

    fn neighbors_one_unfiltered(
        &self,
        pager: &Pager,
        group_ids: &[u64],
        grouped_layout: &EndpointLayout,
        neighbor_row_count: u64,
        direction: Direction,
        from: u64,
    ) -> DevonResult<Vec<u64>> {
        let mut neighbors = Vec::new();
        if let Some((group_index, slot)) = grouped_layout.locate(from)
            && let Some(group_id) = stored_group_id(group_ids, group_index)
        {
            let types = schema_types(&self.schema);
            let group = CsrGroup::read_checked(pager, group_id, &types, neighbor_row_count)?;
            validate_csr_row_count(&group, grouped_layout.row_counts[group_index], group_index)?;
            if let Some(range) = group.edge_range(slot) {
                for edge_index in range {
                    neighbors.push(csr_neighbor(&group, edge_index)?);
                }
            }
        }
        neighbors.extend(
            self.buffered_edges
                .iter()
                .filter_map(|edge| match direction {
                    Direction::Out if edge.from == from => Some(edge.to),
                    Direction::In if edge.to == from => Some(edge.from),
                    Direction::Out | Direction::In | Direction::Both => None,
                }),
        );
        Ok(neighbors)
    }

    fn grouped_endpoint_is_tombstoned(&self, direction: Direction, offset: u64) -> bool {
        match direction {
            Direction::Out => self.endpoint_tombstones.from_offsets.contains(&offset),
            Direction::In => self.endpoint_tombstones.to_offsets.contains(&offset),
            Direction::Both => false,
        }
    }

    fn neighbor_is_tombstoned(&self, direction: Direction, offset: u64) -> bool {
        match direction {
            Direction::Out => self.endpoint_tombstones.to_offsets.contains(&offset),
            Direction::In => self.endpoint_tombstones.from_offsets.contains(&offset),
            Direction::Both => false,
        }
    }

    fn edge_is_tombstoned(&self, from: u64, to: u64) -> bool {
        self.endpoint_tombstones.from_offsets.contains(&from)
            || self.endpoint_tombstones.to_offsets.contains(&to)
    }

    fn remapped_edge_is_tombstoned(
        &self,
        direction: Direction,
        grouped: u64,
        neighbor: u64,
    ) -> bool {
        match direction {
            Direction::Out => self.edge_is_tombstoned(grouped, neighbor),
            Direction::In => self.edge_is_tombstoned(neighbor, grouped),
            Direction::Both => false,
        }
    }

    fn overlay_group_estimate(
        &self,
        direction: Direction,
        grouped_remap: &OffsetRemap,
        neighbor_remap: &OffsetRemap,
        new_start: u64,
        new_end: u64,
    ) -> DevonResult<(usize, usize)> {
        let mut edges = 0_usize;
        let mut properties = 0_usize;
        for edge in &self.buffered_edges {
            if self.edge_is_tombstoned(edge.from, edge.to) {
                continue;
            }
            let (grouped, neighbor) = edge_endpoints(edge, direction)?;
            let Some(grouped) = grouped_remap.map(grouped)? else {
                continue;
            };
            if grouped < new_start || grouped >= new_end || neighbor_remap.map(neighbor)?.is_none()
            {
                continue;
            }
            edges = edges
                .checked_add(1)
                .ok_or_else(|| corrupt("overlay CSR edge estimate overflows usize"))?;
            properties = properties
                .checked_add(CsrGroup::checkpoint_property_bytes(&edge.values)?)
                .ok_or_else(|| corrupt("overlay CSR property estimate overflows usize"))?;
        }
        Ok((edges, properties))
    }

    fn append_remapped_overlay_group(
        &self,
        replacement: &mut CsrGroup,
        direction: Direction,
        grouped_remap: &OffsetRemap,
        neighbor_remap: &OffsetRemap,
        new_start: u64,
        new_end: u64,
    ) -> DevonResult<()> {
        for edge in &self.buffered_edges {
            if self.edge_is_tombstoned(edge.from, edge.to) {
                continue;
            }
            let (grouped, neighbor) = edge_endpoints(edge, direction)?;
            let Some(new_grouped) = grouped_remap.map(grouped)? else {
                continue;
            };
            if new_grouped < new_start || new_grouped >= new_end {
                continue;
            }
            let Some(new_neighbor) = neighbor_remap.map(neighbor)? else {
                continue;
            };
            let slot = usize::try_from(new_grouped - new_start)
                .map_err(|_| corrupt("replacement overlay CSR slot exceeds usize"))?;
            replacement.push_edge(slot, new_neighbor, edge.values.clone())?;
        }
        Ok(())
    }
}

/// Decodes a relationship-insert WAL payload.
///
/// A valid JSON payload without the `rel` discriminator is a foreign WAL
/// record and returns [`DevonError::NotFound`]. Once `rel` is present, an
/// invalid relationship-record shape returns [`DevonError::Corrupt`]. JSON
/// that cannot be parsed is also corrupt because its record kind cannot be
/// established safely.
pub fn decode_wal_record(payload: &[u8]) -> DevonResult<(String, u64, u64, Vec<Value>)> {
    let value: serde_json::Value = serde_json::from_slice(payload)
        .map_err(|error| corrupt(format!("WAL record is not valid JSON: {error}")))?;
    let is_relationship = value
        .as_object()
        .is_some_and(|object| object.contains_key("rel"));
    if !is_relationship {
        return Err(DevonError::NotFound {
            what: "relationship-insert WAL record".to_owned(),
        });
    }
    let record: WalRecord = serde_json::from_value(value).map_err(|error| {
        corrupt(format!(
            "relationship-insert WAL record is malformed: {error}"
        ))
    })?;
    Ok((record.rel, record.from, record.to, record.values))
}

fn encode_wal_record(rel: &str, from: u64, to: u64, values: &[Value]) -> DevonResult<Vec<u8>> {
    serde_json::to_vec(&WalRecord {
        rel: rel.to_owned(),
        from,
        to,
        values: values.to_vec(),
    })
    .map_err(|error| invalid_argument(format!("relationship insert cannot be encoded: {error}")))
}

fn validate_values(schema: &RelTableSchema, values: &[Value]) -> DevonResult<()> {
    if values.len() != schema.columns().len() {
        return Err(invalid_argument(format!(
            "relationship table `{}` expects {} property columns but edge has {} values",
            schema.name(),
            schema.columns().len(),
            values.len()
        )));
    }
    for (column, value) in schema.columns().iter().zip(values) {
        if !value.matches_type(&column.ty) {
            return Err(invalid_argument(format!(
                "column `{}` in relationship table `{}` expects {} but received {value}",
                column.name,
                schema.name(),
                column.ty
            )));
        }
    }
    Ok(())
}

fn validate_direction(schema: &RelTableSchema, direction: Direction) -> DevonResult<()> {
    if direction == Direction::Both && fold(schema.from()) != fold(schema.to()) {
        return Err(invalid_argument(format!(
            "relationship table `{}` cannot use Both direction across node tables `{}` and `{}`",
            schema.name(),
            schema.from(),
            schema.to()
        )));
    }
    Ok(())
}

fn ensure_catalog_schema(schema: &RelTableSchema, catalog: &Catalog) -> DevonResult<()> {
    let catalog_schema = catalog
        .rel_table(schema.name())
        .ok_or_else(|| DevonError::NotFound {
            what: format!("relationship table `{}`", schema.name()),
        })?;
    if catalog_schema != schema {
        return Err(invalid_argument(format!(
            "relationship table `{}` schema differs from the catalog",
            schema.name()
        )));
    }
    Ok(())
}

fn endpoint_layout(
    pager: &Pager,
    catalog: &Catalog,
    table_name: &str,
) -> DevonResult<EndpointLayout> {
    let schema = catalog
        .node_table(table_name)
        .ok_or_else(|| DevonError::NotFound {
            what: format!("node table `{table_name}`"),
        })?;
    let group_ids = catalog
        .table_storage(table_name)
        .map_or(&[][..], |storage| storage.groups.as_slice());
    let mut row_counts = Vec::with_capacity(group_ids.len());
    let mut starts = Vec::with_capacity(group_ids.len());
    let mut total_rows = 0_u64;
    for (index, group_id) in group_ids.iter().copied().enumerate() {
        starts.push(total_rows);
        let row_count = read_node_group_row_count(pager, group_id, schema.columns().len(), index)?;
        total_rows = total_rows
            .checked_add(row_count as u64)
            .ok_or_else(|| corrupt(format!("node table `{table_name}` row count overflows u64")))?;
        row_counts.push(row_count);
    }
    Ok(EndpointLayout {
        row_counts,
        starts,
        total_rows,
    })
}

fn read_node_group_row_count(
    pager: &Pager,
    page_id: u64,
    expected_columns: usize,
    group_index: usize,
) -> DevonResult<usize> {
    // Delegates to the canonical metadata reader owned by the node-group
    // format. The old local copy validated directory padding with an
    // entry count that predated derived b1 rescore entries and would
    // reject valid groups; full structural validation belongs to
    // `NodeGroup::read`, not this count path.
    NodeGroup::read_row_count(pager, page_id, expected_columns).map_err(|error| match error {
        DevonError::Corrupt { context } => corrupt(format!("node group {group_index}: {context}")),
        DevonError::InvalidArgument { context } => corrupt(format!(
            "node group {group_index} references invalid directory page {page_id}: {context}"
        )),
        other => other,
    })
}

fn validate_buffered_offsets(
    edges: &[Edge],
    from_layout: &EndpointLayout,
    to_layout: &EndpointLayout,
) -> DevonResult<()> {
    for (index, edge) in edges.iter().enumerate() {
        if edge.from >= from_layout.total_rows {
            return Err(invalid_argument(format!(
                "edge {index} from offset {} is outside checkpointed row count {}",
                edge.from, from_layout.total_rows
            )));
        }
        if edge.to >= to_layout.total_rows {
            return Err(invalid_argument(format!(
                "edge {index} to offset {} is outside checkpointed row count {}",
                edge.to, to_layout.total_rows
            )));
        }
    }
    Ok(())
}

fn validate_buffered_remap_offsets(
    edges: &[Edge],
    from_remap: &OffsetRemap,
    to_remap: &OffsetRemap,
) -> DevonResult<()> {
    for edge in edges {
        let _ = from_remap.map(edge.from)?;
        let _ = to_remap.map(edge.to)?;
    }
    Ok(())
}

fn validate_remap_layout(
    remap: &OffsetRemap,
    old_layout: &EndpointLayout,
    new_layout: &EndpointLayout,
    table: &str,
) -> DevonResult<()> {
    if old_layout.total_rows > remap.old_domain() {
        return Err(corrupt(format!(
            "node table `{table}` checkpointed rows {} exceed old remap domain {}",
            old_layout.total_rows,
            remap.old_domain()
        )));
    }
    let expected = remap.new_domain()?;
    if new_layout.total_rows != expected {
        return Err(corrupt(format!(
            "node table `{table}` remap produces {expected} rows but replacement storage has {}",
            new_layout.total_rows
        )));
    }
    Ok(())
}

fn validate_storage_lengths(
    storage: &RelStorage,
    from_layout: &EndpointLayout,
    to_layout: &EndpointLayout,
) -> DevonResult<()> {
    if storage.fwd.len() > from_layout.row_counts.len() {
        return Err(corrupt(
            "relationship forward storage has more groups than its source table",
        ));
    }
    if storage.bwd.len() > to_layout.row_counts.len() {
        return Err(corrupt(
            "relationship backward storage has more groups than its destination table",
        ));
    }
    Ok(())
}

fn merge_direction(
    pager: &Pager,
    group_ids: &mut Vec<u64>,
    grouped_layout: &EndpointLayout,
    neighbor_row_count: u64,
    types: &[LogicalType],
    edges: &[Edge],
    direction: Direction,
) -> DevonResult<()> {
    let mut additions: BTreeMap<usize, Vec<PendingEdge<'_>>> = BTreeMap::new();
    for edge in edges {
        let (group_index, slot, neighbor) = match direction {
            Direction::Out => {
                let (group, slot) = grouped_layout.locate(edge.from).ok_or_else(|| {
                    invalid_argument("source offset is outside checkpointed storage")
                })?;
                (group, slot, edge.to)
            }
            Direction::In => {
                let (group, slot) = grouped_layout.locate(edge.to).ok_or_else(|| {
                    invalid_argument("destination offset is outside checkpointed storage")
                })?;
                (group, slot, edge.from)
            }
            Direction::Both => return Err(invalid_argument("cannot materialize Both direction")),
        };
        additions.entry(group_index).or_default().push(PendingEdge {
            slot,
            neighbor,
            values: &edge.values,
        });
    }

    for (group_index, group_additions) in additions {
        let row_count = grouped_layout.row_counts[group_index];
        let existing = stored_group_id(group_ids, group_index)
            .map(|page_id| CsrGroup::read_checked(pager, page_id, types, neighbor_row_count))
            .transpose()?;
        if let Some(group) = &existing {
            validate_csr_row_count(group, row_count, group_index)?;
        }
        let mut replacement = CsrGroup::new(row_count, types.to_vec())?;
        if let Some(group) = &existing {
            copy_existing_edges(group, &mut replacement)?;
        }
        for edge in group_additions {
            replacement.push_edge(edge.slot, edge.neighbor, edge.values.to_vec())?;
        }
        let page_id = replacement.write(pager)?;
        if group_ids.len() <= group_index {
            group_ids.resize(group_index + 1, 0);
        }
        group_ids[group_index] = page_id;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn rewrite_remapped_direction(
    table: &RelTable,
    pager: &Pager,
    catalog: &mut Catalog,
    storage: &mut RelStorage,
    old_grouped: &EndpointLayout,
    new_grouped: &EndpointLayout,
    old_neighbor: &EndpointLayout,
    grouped_remap: &OffsetRemap,
    neighbor_remap: &OffsetRemap,
    types: &[LogicalType],
    direction: Direction,
    budget: &MemoryBudget,
) -> DevonResult<()> {
    let old_ids = match direction {
        Direction::Out => storage.fwd.clone(),
        Direction::In => storage.bwd.clone(),
        Direction::Both => return Err(invalid_argument("cannot materialize Both direction")),
    };
    let mut new_ids = Vec::with_capacity(new_grouped.row_counts.len());
    for group_index in 0..new_grouped.row_counts.len() {
        let row_count = new_grouped.row_counts[group_index];
        let new_start = new_grouped.starts[group_index];
        let new_end = new_start
            .checked_add(row_count as u64)
            .ok_or_else(|| corrupt("replacement CSR source range overflows u64"))?;
        let relevant =
            relevant_old_groups(&old_ids, old_grouped, grouped_remap, new_start, new_end)?;
        let candidate = stored_group_id(&old_ids, group_index);
        let (edge_capacity, property_bytes, old_scratch) =
            estimate_old_groups(pager, types, &relevant, candidate)?;
        let (overlay_edges, overlay_properties) = table.overlay_group_estimate(
            direction,
            grouped_remap,
            neighbor_remap,
            new_start,
            new_end,
        )?;
        let edge_capacity = edge_capacity
            .checked_add(overlay_edges)
            .ok_or_else(|| corrupt("replacement CSR edge estimate overflows usize"))?;
        let property_bytes = property_bytes
            .checked_add(overlay_properties)
            .ok_or_else(|| corrupt("replacement CSR property estimate overflows usize"))?;
        let replacement_estimate = CsrGroup::replacement_checkpoint_estimate(
            row_count,
            edge_capacity,
            types,
            property_bytes,
        )?;
        let requested = old_scratch
            .checked_add(replacement_estimate.resident_bytes)
            .and_then(|bytes| bytes.checked_add(replacement_estimate.encoded_bytes))
            .ok_or_else(|| corrupt("detach checkpoint scratch estimate overflows usize"))?;
        let failure = DetachCheckpointBlocked {
            relationship: table.schema.name().to_owned(),
            direction: direction_name(direction).to_owned(),
            group: group_index,
            requested,
            charged: budget.charged(),
            limit: budget.limit(),
        };
        let _scratch = ScratchCharge::reserve(budget, requested, failure)?;
        let mut replacement = CsrGroup::new(row_count, types.to_vec())?;
        for (old_index, page_id) in &relevant {
            append_remapped_old_group(
                table,
                pager,
                &mut replacement,
                *page_id,
                *old_index,
                old_grouped,
                old_neighbor.total_rows,
                grouped_remap,
                neighbor_remap,
                new_start,
                direction,
                types,
            )?;
        }
        table.append_remapped_overlay_group(
            &mut replacement,
            direction,
            grouped_remap,
            neighbor_remap,
            new_start,
            new_end,
        )?;
        let page_id = replacement_page_id(
            pager,
            candidate,
            &replacement,
            old_neighbor.total_rows,
            types,
        )?;
        new_ids.push(page_id);
        set_direction_ids(storage, direction, new_ids.clone());
        catalog.set_rel_storage(table.schema.name(), storage.clone())?;
    }
    while new_ids.last() == Some(&0) {
        new_ids.pop();
    }
    set_direction_ids(storage, direction, new_ids);
    catalog.set_rel_storage(table.schema.name(), storage.clone())?;
    Ok(())
}

fn relevant_old_groups(
    old_ids: &[u64],
    old_layout: &EndpointLayout,
    remap: &OffsetRemap,
    new_start: u64,
    new_end: u64,
) -> DevonResult<Vec<(usize, u64)>> {
    let mut relevant = Vec::new();
    for (index, row_count) in old_layout.row_counts.iter().copied().enumerate() {
        let Some(page_id) = stored_group_id(old_ids, index) else {
            continue;
        };
        let old_end = old_layout.starts[index]
            .checked_add(row_count as u64)
            .ok_or_else(|| corrupt("old CSR source range overflows u64"))?;
        let Some((mapped_start, mapped_end)) =
            remap.survivor_range(old_layout.starts[index], old_end)?
        else {
            continue;
        };
        if mapped_start < new_end && new_start < mapped_end {
            relevant.push((index, page_id));
        }
    }
    Ok(relevant)
}

fn estimate_old_groups(
    pager: &Pager,
    types: &[LogicalType],
    relevant: &[(usize, u64)],
    candidate: Option<u64>,
) -> DevonResult<(usize, usize, usize)> {
    let mut edge_capacity = 0_usize;
    let mut property_bytes = 0_usize;
    let mut old_scratch = 0_usize;
    let mut inspected = BTreeSet::new();
    for page_id in relevant.iter().map(|(_, page_id)| *page_id) {
        let estimate = CsrGroup::checkpoint_estimate(pager, page_id, types)?;
        edge_capacity = edge_capacity
            .checked_add(estimate.edge_count)
            .ok_or_else(|| corrupt("old CSR edge estimate overflows usize"))?;
        property_bytes = property_bytes
            .checked_add(estimate.property_bytes)
            .ok_or_else(|| corrupt("old CSR property estimate overflows usize"))?;
        old_scratch = old_scratch.max(
            estimate
                .resident_bytes
                .checked_add(estimate.encoded_bytes)
                .ok_or_else(|| corrupt("old CSR decode estimate overflows usize"))?,
        );
        inspected.insert(page_id);
    }
    if let Some(page_id) = candidate.filter(|page_id| !inspected.contains(page_id)) {
        let estimate = CsrGroup::checkpoint_estimate(pager, page_id, types)?;
        old_scratch = old_scratch.max(
            estimate
                .resident_bytes
                .checked_add(estimate.encoded_bytes)
                .ok_or_else(|| corrupt("old CSR decode estimate overflows usize"))?,
        );
    }
    Ok((edge_capacity, property_bytes, old_scratch))
}

#[allow(clippy::too_many_arguments)]
fn append_remapped_old_group(
    table: &RelTable,
    pager: &Pager,
    replacement: &mut CsrGroup,
    page_id: u64,
    old_index: usize,
    old_grouped: &EndpointLayout,
    old_neighbor_rows: u64,
    grouped_remap: &OffsetRemap,
    neighbor_remap: &OffsetRemap,
    new_start: u64,
    direction: Direction,
    types: &[LogicalType],
) -> DevonResult<()> {
    let group = CsrGroup::read_checked(pager, page_id, types, old_neighbor_rows)?;
    validate_csr_row_count(&group, old_grouped.row_counts[old_index], old_index)?;
    for slot in 0..group.row_count() {
        let old_source = old_grouped.starts[old_index]
            .checked_add(slot as u64)
            .ok_or_else(|| corrupt("old CSR source offset overflows u64"))?;
        let Some(new_source) = grouped_remap.map(old_source)? else {
            continue;
        };
        let Some(new_slot) = replacement_slot(replacement, new_start, new_source)? else {
            continue;
        };
        let range = group
            .edge_range(slot)
            .ok_or_else(|| corrupt(format!("CSR group is missing slot {slot}")))?;
        for edge_index in range {
            let old_neighbor = csr_neighbor(&group, edge_index)?;
            if table.remapped_edge_is_tombstoned(direction, old_source, old_neighbor) {
                continue;
            }
            let Some(new_neighbor) = neighbor_remap.map(old_neighbor)? else {
                continue;
            };
            replacement.push_edge(new_slot, new_neighbor, csr_values(&group, edge_index)?)?;
        }
    }
    Ok(())
}

fn replacement_slot(
    replacement: &CsrGroup,
    new_start: u64,
    new_source: u64,
) -> DevonResult<Option<usize>> {
    let Some(relative) = new_source.checked_sub(new_start) else {
        return Ok(None);
    };
    let slot =
        usize::try_from(relative).map_err(|_| corrupt("replacement CSR slot exceeds usize"))?;
    Ok((slot < replacement.row_count()).then_some(slot))
}

fn replacement_page_id(
    pager: &Pager,
    candidate: Option<u64>,
    replacement: &CsrGroup,
    old_neighbor_rows: u64,
    types: &[LogicalType],
) -> DevonResult<u64> {
    if replacement.edge_count() == 0 {
        return Ok(0);
    }
    if let Some(page_id) = candidate {
        let old = CsrGroup::read_checked(pager, page_id, types, old_neighbor_rows)?;
        if old == *replacement {
            return Ok(page_id);
        }
    }
    replacement.write(pager)
}

fn set_direction_ids(storage: &mut RelStorage, direction: Direction, ids: Vec<u64>) {
    match direction {
        Direction::Out => storage.fwd = ids,
        Direction::In => storage.bwd = ids,
        Direction::Both => unreachable!("Both is rejected before catalog mutation"),
    }
}

fn edge_endpoints(edge: &Edge, direction: Direction) -> DevonResult<(u64, u64)> {
    match direction {
        Direction::Out => Ok((edge.from, edge.to)),
        Direction::In => Ok((edge.to, edge.from)),
        Direction::Both => Err(invalid_argument("cannot materialize Both direction")),
    }
}

const fn direction_name(direction: Direction) -> &'static str {
    match direction {
        Direction::Out => "fwd",
        Direction::In => "bwd",
        Direction::Both => "both",
    }
}

fn copy_existing_edges(source: &CsrGroup, destination: &mut CsrGroup) -> DevonResult<()> {
    for slot in 0..source.row_count() {
        let range = source
            .edge_range(slot)
            .ok_or_else(|| corrupt(format!("CSR group is missing slot {slot}")))?;
        for edge_index in range {
            destination.push_edge(
                slot,
                csr_neighbor(source, edge_index)?,
                csr_values(source, edge_index)?,
            )?;
        }
    }
    Ok(())
}

fn csr_neighbor(group: &CsrGroup, edge_index: usize) -> DevonResult<u64> {
    group.neighbor(edge_index).ok_or_else(|| {
        corrupt(format!(
            "CSR group is missing neighbor at edge {edge_index}"
        ))
    })
}

fn csr_values(group: &CsrGroup, edge_index: usize) -> DevonResult<Vec<Value>> {
    (0..group.column_count())
        .map(|column_index| {
            group
                .value(edge_index, column_index)
                .cloned()
                .ok_or_else(|| {
                    corrupt(format!(
                        "CSR group is missing edge {edge_index}, property column {column_index}"
                    ))
                })
        })
        .collect()
}

fn validate_csr_row_count(
    group: &CsrGroup,
    endpoint_row_count: usize,
    group_index: usize,
) -> DevonResult<()> {
    if group.row_count() > endpoint_row_count {
        return Err(corrupt(format!(
            "CSR group {group_index} row_count {} exceeds endpoint node-group row_count {endpoint_row_count}",
            group.row_count()
        )));
    }
    Ok(())
}

fn stored_group_id(group_ids: &[u64], index: usize) -> Option<u64> {
    group_ids
        .get(index)
        .copied()
        .filter(|page_id| *page_id != 0)
}

fn schema_types(schema: &RelTableSchema) -> Vec<LogicalType> {
    schema.columns().iter().map(|column| column.ty).collect()
}

fn invalid_argument(context: impl Into<String>) -> DevonError {
    DevonError::InvalidArgument {
        context: context.into(),
    }
}

fn corrupt(context: impl Into<String>) -> DevonError {
    DevonError::Corrupt {
        context: context.into(),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use devondb_types::{
        DevonError,
        logical_type::LogicalType,
        schema::{Column, NodeTableSchema, RelTableSchema},
        value::Value,
    };
    use tempfile::tempdir;

    use super::{
        DetachCheckpointBlocked, Direction, RelTable, decode_wal_record, encode_wal_record,
    };
    use crate::catalog::Catalog;
    use crate::csr_group::CsrGroup;
    use crate::node_group::NODE_GROUP_CAPACITY;
    use crate::node_table::NodeTable;
    use crate::pager::Pager;
    use crate::wal::{WalWriter, replay};

    const PAGE_SIZE: u32 = 4096;
    const DB_ID: [u8; 16] = *b"rel-table-db-id!";

    fn node_schema() -> NodeTableSchema {
        NodeTableSchema::new(
            "Person".to_owned(),
            vec![Column {
                name: "id".to_owned(),
                ty: LogicalType::Int64,
                primary_key: true,
            }],
        )
        .unwrap()
    }

    fn rel_schema() -> RelTableSchema {
        RelTableSchema::new(
            "Knows".to_owned(),
            "Person".to_owned(),
            "Person".to_owned(),
            vec![Column {
                name: "since".to_owned(),
                ty: LogicalType::Int64,
                primary_key: false,
            }],
        )
        .unwrap()
    }

    fn company_schema() -> NodeTableSchema {
        NodeTableSchema::new(
            "Company".to_owned(),
            vec![Column {
                name: "id".to_owned(),
                ty: LogicalType::Int64,
                primary_key: true,
            }],
        )
        .unwrap()
    }

    fn works_at_schema() -> RelTableSchema {
        RelTableSchema::new(
            "WorksAt".to_owned(),
            "Person".to_owned(),
            "Company".to_owned(),
            Vec::new(),
        )
        .unwrap()
    }

    fn create_database(path: &Path, row_count: usize) -> (Pager, Catalog) {
        let pager = Pager::create(path, PAGE_SIZE, DB_ID).unwrap();
        let node_schema = node_schema();
        let mut catalog = Catalog::default();
        catalog.add_node_table(node_schema.clone()).unwrap();
        catalog.add_rel_table(rel_schema()).unwrap();
        append_checkpointed_nodes(&pager, &mut catalog, node_schema, 0, row_count);
        catalog.save(&pager, 1).unwrap();
        (pager, catalog)
    }

    fn append_checkpointed_nodes(
        pager: &Pager,
        catalog: &mut Catalog,
        schema: NodeTableSchema,
        start: usize,
        end: usize,
    ) {
        let mut table = NodeTable::new(schema);
        for id in start..end {
            table.recover_row(vec![Value::Int64(id as i64)]).unwrap();
        }
        table.checkpoint(pager, catalog).unwrap();
    }

    fn open_wal(pager: &Pager, path: &Path) -> WalWriter {
        WalWriter::open(path, pager.superblock().checkpoint_lsn + 1).unwrap()
    }

    #[test]
    fn buffered_edges_are_visible_in_both_directions() {
        let directory = tempdir().unwrap();
        let db_path = directory.path().join("buffered.devondb");
        let wal_path = directory.path().join("buffered.devondb-wal");
        let (pager, catalog) = create_database(&db_path, 3);
        let mut wal = open_wal(&pager, &wal_path);
        let mut table = RelTable::new(rel_schema());
        table.insert(&mut wal, 0, 1, vec![Value::Int64(1)]).unwrap();
        table.insert(&mut wal, 0, 0, vec![Value::Int64(2)]).unwrap();
        table.insert(&mut wal, 2, 0, vec![Value::Int64(3)]).unwrap();

        assert_eq!(
            table
                .neighbors(&pager, &catalog, Direction::Out, 0)
                .unwrap(),
            vec![1, 0]
        );
        assert_eq!(
            table.neighbors(&pager, &catalog, Direction::In, 0).unwrap(),
            vec![0, 2]
        );
        assert_eq!(
            table
                .neighbors(&pager, &catalog, Direction::Both, 0)
                .unwrap(),
            vec![1, 0, 2]
        );
    }

    #[test]
    fn both_rejects_relationships_between_different_node_tables() {
        let directory = tempdir().unwrap();
        let db_path = directory.path().join("heterogeneous-both.devondb");
        let pager = Pager::create(db_path, PAGE_SIZE, DB_ID).unwrap();
        let mut catalog = Catalog::default();
        catalog.add_node_table(node_schema()).unwrap();
        catalog.add_node_table(company_schema()).unwrap();
        catalog.add_rel_table(works_at_schema()).unwrap();
        let table = RelTable::new(works_at_schema());

        let error = table
            .neighbors(&pager, &catalog, Direction::Both, 0)
            .unwrap_err();

        let DevonError::InvalidArgument { context } = error else {
            panic!("expected InvalidArgument, got {error}");
        };
        assert!(context.contains("Person"));
        assert!(context.contains("Company"));
    }

    #[test]
    fn checkpoint_survives_pager_catalog_and_wal_reopen() {
        let directory = tempdir().unwrap();
        let db_path = directory.path().join("reopen.devondb");
        let wal_path = directory.path().join("reopen.devondb-wal");
        let (pager, mut catalog) = create_database(&db_path, 4);
        let mut wal = open_wal(&pager, &wal_path);
        let mut table = RelTable::new(rel_schema());
        let expected = vec![
            (0, 1, vec![Value::Int64(10)]),
            (0, 2, vec![Value::Null]),
            (3, 0, vec![Value::Int64(30)]),
        ];
        for (from, to, values) in &expected {
            table.insert(&mut wal, *from, *to, values.clone()).unwrap();
        }
        table.checkpoint(&pager, &mut catalog).unwrap();
        let publish_lsn = pager.superblock().checkpoint_lsn + 1;
        catalog.save(&pager, publish_lsn).unwrap();
        drop(table);
        drop(wal);
        drop(pager);

        let pager = Pager::open(db_path).unwrap();
        let catalog = Catalog::load(&pager).unwrap();
        let _wal = open_wal(&pager, &wal_path);
        let table = RelTable::new(rel_schema());

        assert_eq!(table.scan(&pager, &catalog).unwrap(), expected);
        assert_eq!(
            table
                .neighbors(&pager, &catalog, Direction::Out, 0)
                .unwrap(),
            vec![1, 2]
        );
        assert_eq!(
            table.neighbors(&pager, &catalog, Direction::In, 0).unwrap(),
            vec![3]
        );
        assert_eq!(
            table
                .neighbors(&pager, &catalog, Direction::Both, 0)
                .unwrap(),
            vec![1, 2, 3]
        );
    }

    #[test]
    fn edges_cross_endpoint_node_group_boundary() {
        let directory = tempdir().unwrap();
        let db_path = directory.path().join("multi-group.devondb");
        let wal_path = directory.path().join("multi-group.devondb-wal");
        let row_count = NODE_GROUP_CAPACITY + 2;
        let (pager, mut catalog) = create_database(&db_path, row_count);
        let mut wal = open_wal(&pager, &wal_path);
        let mut table = RelTable::new(rel_schema());
        let last_first_group = (NODE_GROUP_CAPACITY - 1) as u64;
        let first_second_group = NODE_GROUP_CAPACITY as u64;
        table
            .insert(
                &mut wal,
                last_first_group,
                first_second_group,
                vec![Value::Int64(1)],
            )
            .unwrap();
        table
            .insert(&mut wal, first_second_group, 0, vec![Value::Int64(2)])
            .unwrap();
        table.checkpoint(&pager, &mut catalog).unwrap();

        let storage = catalog.rel_storage("Knows").unwrap();
        assert_eq!(storage.fwd.len(), 2);
        assert_eq!(storage.bwd.len(), 2);
        assert!(storage.fwd.iter().all(|page| *page != 0));
        assert!(storage.bwd.iter().all(|page| *page != 0));
        assert_eq!(
            table
                .neighbors(&pager, &catalog, Direction::Out, last_first_group,)
                .unwrap(),
            vec![first_second_group]
        );
        assert_eq!(
            table
                .neighbors(&pager, &catalog, Direction::In, first_second_group,)
                .unwrap(),
            vec![last_first_group]
        );
    }

    #[test]
    fn endpoint_tail_growth_after_csr_write_is_readable() {
        let directory = tempdir().unwrap();
        let db_path = directory.path().join("tail-growth.devondb");
        let wal_path = directory.path().join("tail-growth.devondb-wal");
        let (pager, mut catalog) = create_database(&db_path, 2);
        let mut wal = open_wal(&pager, &wal_path);
        let mut table = RelTable::new(rel_schema());
        table.insert(&mut wal, 0, 1, vec![Value::Int64(1)]).unwrap();
        table.checkpoint(&pager, &mut catalog).unwrap();
        let old_csr = catalog.rel_storage("Knows").unwrap().fwd[0];
        assert_eq!(
            CsrGroup::read_checked(&pager, old_csr, &[LogicalType::Int64], 2)
                .unwrap()
                .row_count(),
            2
        );

        append_checkpointed_nodes(&pager, &mut catalog, node_schema(), 2, 5);
        assert!(
            table
                .neighbors(&pager, &catalog, Direction::Out, 4)
                .unwrap()
                .is_empty()
        );
        table.insert(&mut wal, 4, 0, vec![Value::Int64(2)]).unwrap();
        assert_eq!(
            table
                .neighbors(&pager, &catalog, Direction::Out, 4)
                .unwrap(),
            vec![0]
        );
        table.checkpoint(&pager, &mut catalog).unwrap();

        let new_csr = catalog.rel_storage("Knows").unwrap().fwd[0];
        assert_ne!(new_csr, old_csr);
        assert_eq!(
            CsrGroup::read_checked(&pager, new_csr, &[LogicalType::Int64], 5)
                .unwrap()
                .row_count(),
            5
        );
        assert_eq!(
            table
                .neighbors(&pager, &catalog, Direction::Out, 0)
                .unwrap(),
            vec![1]
        );
    }

    #[test]
    fn empty_endpoint_groups_use_zero_or_short_catalog_entries() {
        let directory = tempdir().unwrap();
        let db_path = directory.path().join("sparse.devondb");
        let wal_path = directory.path().join("sparse.devondb-wal");
        let (pager, mut catalog) = create_database(&db_path, NODE_GROUP_CAPACITY + 1);
        let mut wal = open_wal(&pager, &wal_path);
        let mut table = RelTable::new(rel_schema());
        table
            .insert(
                &mut wal,
                NODE_GROUP_CAPACITY as u64,
                0,
                vec![Value::Int64(1)],
            )
            .unwrap();
        table.checkpoint(&pager, &mut catalog).unwrap();

        let storage = catalog.rel_storage("Knows").unwrap();
        assert_eq!(storage.fwd.len(), 2);
        assert_eq!(storage.fwd[0], 0);
        assert_ne!(storage.fwd[1], 0);
        assert_eq!(storage.bwd.len(), 1);
        assert_ne!(storage.bwd[0], 0);
    }

    #[test]
    fn wal_replay_recovers_a_kill_nine_edge() {
        let directory = tempdir().unwrap();
        let db_path = directory.path().join("recovery.devondb");
        let wal_path = directory.path().join("recovery.devondb-wal");
        let (pager, catalog) = create_database(&db_path, 3);
        let mut wal = open_wal(&pager, &wal_path);
        let mut table = RelTable::new(rel_schema());
        table.insert(&mut wal, 1, 2, vec![Value::Int64(9)]).unwrap();
        drop(table);
        drop(wal);
        drop(catalog);
        drop(pager);

        let pager = Pager::open(db_path).unwrap();
        let catalog = Catalog::load(&pager).unwrap();
        let mut recovered = RelTable::new(rel_schema());
        for (_lsn, payload) in replay(&wal_path).unwrap() {
            let (rel, from, to, values) = decode_wal_record(&payload).unwrap();
            if rel == recovered.schema().name() {
                recovered.recover_edge(from, to, values).unwrap();
            }
        }

        assert_eq!(
            recovered
                .neighbors(&pager, &catalog, Direction::Out, 1)
                .unwrap(),
            vec![2]
        );
        assert_eq!(
            recovered.scan(&pager, &catalog).unwrap(),
            vec![(1, 2, vec![Value::Int64(9)])]
        );
    }

    #[test]
    fn type_mismatch_names_column_and_does_not_write_wal() {
        let directory = tempdir().unwrap();
        let db_path = directory.path().join("mismatch.devondb");
        let wal_path = directory.path().join("mismatch.devondb-wal");
        let (pager, _catalog) = create_database(&db_path, 2);
        let mut wal = open_wal(&pager, &wal_path);
        let mut table = RelTable::new(rel_schema());

        let error = table
            .insert(&mut wal, 0, 1, vec![Value::String("bad".to_owned())])
            .unwrap_err();

        let DevonError::InvalidArgument { context } = error else {
            panic!("expected InvalidArgument, got {error}");
        };
        assert!(context.contains("since"));
        assert_eq!(fs::metadata(wal_path).unwrap().len(), 0);
        assert!(!table.has_buffered_edges());
    }

    #[test]
    fn checkpoint_rejects_offsets_outside_checkpointed_endpoints() {
        let directory = tempdir().unwrap();
        let db_path = directory.path().join("bad-offset.devondb");
        let wal_path = directory.path().join("bad-offset.devondb-wal");
        let (pager, mut catalog) = create_database(&db_path, 2);
        let mut wal = open_wal(&pager, &wal_path);
        let mut table = RelTable::new(rel_schema());
        table.insert(&mut wal, 2, 0, vec![Value::Int64(1)]).unwrap();

        let error = table.checkpoint(&pager, &mut catalog).unwrap_err();

        assert!(matches!(error, DevonError::InvalidArgument { .. }));
        assert!(catalog.rel_storage("Knows").is_none());
        assert!(table.has_buffered_edges());
    }

    #[test]
    fn checkpoint_retry_succeeds_after_buffered_endpoint_is_checkpointed() {
        let directory = tempdir().unwrap();
        let db_path = directory.path().join("buffered-endpoint-retry.devondb");
        let wal_path = directory.path().join("buffered-endpoint-retry.devondb-wal");
        let (pager, mut catalog) = create_database(&db_path, 0);
        let mut wal = open_wal(&pager, &wal_path);
        let mut nodes = NodeTable::new(node_schema());
        let mut relationships = RelTable::new(rel_schema());
        nodes.insert(&mut wal, vec![Value::Int64(1)]).unwrap();
        relationships
            .insert(&mut wal, 0, 0, vec![Value::Int64(2026)])
            .unwrap();

        let error = relationships.checkpoint(&pager, &mut catalog).unwrap_err();
        assert!(matches!(error, DevonError::InvalidArgument { .. }));
        assert!(relationships.has_buffered_edges());

        nodes.checkpoint(&pager, &mut catalog).unwrap();
        relationships.checkpoint(&pager, &mut catalog).unwrap();

        assert!(!relationships.has_buffered_edges());
        assert_eq!(
            relationships
                .neighbors(&pager, &catalog, Direction::Out, 0)
                .unwrap(),
            vec![0]
        );
    }

    #[test]
    fn checkpoint_without_buffered_edges_is_a_no_op() {
        let directory = tempdir().unwrap();
        let db_path = directory.path().join("no-op.devondb");
        let (pager, mut catalog) = create_database(&db_path, 2);
        let mut table = RelTable::new(rel_schema());
        let lsn = pager.superblock().checkpoint_lsn;
        let file_len = fs::metadata(&db_path).unwrap().len();

        table.checkpoint(&pager, &mut catalog).unwrap();

        assert_eq!(pager.superblock().checkpoint_lsn, lsn);
        assert_eq!(fs::metadata(db_path).unwrap().len(), file_len);
        assert!(catalog.rel_storage("Knows").is_none());
    }

    #[test]
    fn existing_edges_precede_new_edges_after_merge() {
        let directory = tempdir().unwrap();
        let db_path = directory.path().join("merge.devondb");
        let wal_path = directory.path().join("merge.devondb-wal");
        let (pager, mut catalog) = create_database(&db_path, 3);
        let mut wal = open_wal(&pager, &wal_path);
        let mut table = RelTable::new(rel_schema());
        table.insert(&mut wal, 0, 1, vec![Value::Int64(1)]).unwrap();
        table.checkpoint(&pager, &mut catalog).unwrap();
        let first_page = catalog.rel_storage("Knows").unwrap().fwd[0];
        table.insert(&mut wal, 0, 2, vec![Value::Int64(2)]).unwrap();
        table.insert(&mut wal, 0, 1, vec![Value::Int64(3)]).unwrap();

        table.checkpoint(&pager, &mut catalog).unwrap();

        assert_ne!(catalog.rel_storage("Knows").unwrap().fwd[0], first_page);
        assert_eq!(
            table
                .neighbors(&pager, &catalog, Direction::Out, 0)
                .unwrap(),
            vec![1, 2, 1]
        );
        assert_eq!(
            table.scan(&pager, &catalog).unwrap(),
            vec![
                (0, 1, vec![Value::Int64(1)]),
                (0, 2, vec![Value::Int64(2)]),
                (0, 1, vec![Value::Int64(3)]),
            ]
        );
    }

    #[test]
    fn wal_payload_uses_the_binding_field_order_and_decodes() {
        let payload = encode_wal_record("Knows", 4, 9, &[Value::Int64(2026)]).unwrap();
        assert_eq!(
            String::from_utf8(payload.clone()).unwrap(),
            r#"{"rel":"Knows","from":4,"to":9,"values":[{"Int64":2026}]}"#
        );
        assert_eq!(
            decode_wal_record(&payload).unwrap(),
            ("Knows".to_owned(), 4, 9, vec![Value::Int64(2026)])
        );
    }

    #[test]
    fn wal_decoder_distinguishes_node_inserts_from_malformed_rel_inserts() {
        let node_insert = br#"{"table":"Person","row":[{"Int64":1}]}"#;
        let foreign = decode_wal_record(node_insert).unwrap_err();
        assert!(matches!(foreign, DevonError::NotFound { .. }));

        let malformed_rel = br#"{"rel":"Knows","from":"zero","to":1,"values":[]}"#;
        let malformed = decode_wal_record(malformed_rel).unwrap_err();
        assert!(matches!(malformed, DevonError::Corrupt { .. }));
    }

    #[test]
    fn blocked_checkpoint_context_round_trips_a_quoted_relationship_name() {
        let expected = DetachCheckpointBlocked {
            relationship: "Heavy Links direction=inside".to_owned(),
            direction: "bwd".to_owned(),
            group: 7,
            requested: 11,
            charged: 13,
            limit: 17,
        };
        let error = DevonError::BudgetExceeded {
            context: expected.context(),
        };

        assert_eq!(DetachCheckpointBlocked::from_error(&error), Some(expected));
    }
}
