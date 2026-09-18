//! The committed MVCC overlay: `PublishedState`, commit-delta chains,
//! and conflict summaries (docs/MVCC.md §3).

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    iter::FusedIterator,
    mem::size_of,
    sync::{
        Arc, Mutex, OnceLock, PoisonError,
        atomic::{AtomicUsize, Ordering},
    },
};

use devondb_types::{
    DevonError, DevonResult,
    schema::{Column, NodeTableSchema, RelTableSchema},
    value::Value,
};

use crate::{
    budget::MemoryBudget,
    catalog::Catalog,
    fulltext::{FullTextError, FullTextIndex, FullTextIndexBuilder},
    node_group::NodeGroup,
    pager::Pager,
    txn_log::{CommittedGroup, DdlPayload, RelEndpoint, WalPayload},
};

/// Writer-policy estimate for a commit-link allocation and its `Arc` header.
pub const COMMIT_LINK_OVERHEAD_BYTES: usize = 128;
/// Writer-policy estimate for the resident map and vector containers in a commit delta.
pub const COMMIT_DELTA_OVERHEAD_BYTES: usize = 96;
/// Writer-policy estimate for one `BTreeMap` entry and its vector container.
pub const MAP_ENTRY_OVERHEAD_BYTES: usize = 64;
/// Writer-policy estimate for one row's vector container.
pub const ROW_OVERHEAD_BYTES: usize = 24;
/// Writer-policy estimate for an edge and its values-vector container.
pub const OVERLAY_EDGE_OVERHEAD_BYTES: usize = 40;
/// Writer-policy estimate for the two endpoint-role set containers.
pub const REL_ENDPOINT_TOMBSTONES_OVERHEAD_BYTES: usize = 48;
/// Writer-policy estimate for one endpoint offset held in a `BTreeSet`.
pub const REL_TOMBSTONE_OFFSET_OVERHEAD_BYTES: usize = 40;
/// Writer-policy estimate for a DDL enum and its schema-owned containers.
pub const DDL_OP_OVERHEAD_BYTES: usize = 96;
/// Writer-policy estimate for one schema column and its name container.
pub const SCHEMA_COLUMN_OVERHEAD_BYTES: usize = 48;
/// Writer-policy estimate for a conflict summary's map and vector containers.
pub const COMMIT_SUMMARY_OVERHEAD_BYTES: usize = 96;
/// Writer-policy estimate for one owned table name in a conflict summary.
pub const SUMMARY_TABLE_NAME_OVERHEAD_BYTES: usize = 24;
/// Writer-policy base estimate for one checkpointed primary-key index.
pub const PK_INDEX_BASE_BYTES: usize = 128;
/// Writer-policy estimate for one integer primary-key hash-map entry.
pub const PK_INDEX_INT_ENTRY_BYTES: usize = 48;
/// Writer-policy estimate for one string primary-key entry before its bytes.
pub const PK_INDEX_STRING_ENTRY_BYTES: usize = 64;
/// Writer-policy estimate for one persisted node-group location.
pub const PK_INDEX_GROUP_BYTES: usize = 32;

/// The immutable committed state of the database at one instant.
#[derive(Debug)]
pub struct PublishedState {
    /// Schemas, including committed-but-uncheckpointed DDL, and storage maps.
    pub catalog: Arc<Catalog>,
    /// Newest committed delta, or `None` immediately after a checkpoint.
    pub chain: Option<Arc<CommitLink>>,
    /// Commit LSN of the newest committed transaction.
    pub last_commit_lsn: u64,
    /// The `checkpoint_lsn` of the superblock publication that produced
    /// this state's catalog — the base-generation pin key for free-page
    /// reclamation (`docs/FREE_PAGES.md` § The pin horizon). NEVER keyed
    /// on the snapshot LSN: a snapshot's LSN is always ≥ its base
    /// generation, because substituting it would unpin pages the pinned
    /// catalog still names.
    pub catalog_generation: u64,
    /// Conflict summaries of recent commits, with the newest entry last.
    pub recent_summaries: Vec<(u64, Arc<CommitSummary>)>,
}

impl PublishedState {
    /// Iterates this state's commit links from the oldest to the newest.
    pub fn commit_links_oldest_first(&self) -> CommitChainIter<'_> {
        commit_links_oldest_first(self.chain.as_ref())
    }

    /// The committed overlay's charged bytes: chain links plus retained
    /// conflict summaries (docs/MVCC.md §7.2). Checkpoint triggers key on
    /// this sum — never on the budget's total, which the page cache
    /// legitimately dominates. A summary whose estimate failed counts as
    /// zero so trigger decisions never block on estimation.
    #[must_use]
    pub fn overlay_charged_bytes(&self) -> usize {
        let links = self
            .commit_links_oldest_first()
            .fold(0_usize, |total, link| {
                total.saturating_add(link.charged_bytes)
            });
        let summaries = self
            .recent_summaries
            .iter()
            .fold(0_usize, |total, (_, summary)| {
                total.saturating_add(summary.estimated_bytes().unwrap_or(0))
            });
        links.saturating_add(summaries)
    }

    /// Iterates one node table's effective overlay rows in insert order.
    ///
    /// Updates replace an overlay insert in its original position, deletes
    /// remove that position, and a later insert of the deleted key appends a
    /// new position. This visible-only iterator deliberately does not carry
    /// offsets; consumers resolving physical node offsets must use
    /// [`Self::node_slots`] so deleted insert positions remain holes until
    /// checkpoint (`docs/DETACH_DELETE.md` § Merged read view).
    pub fn node_rows<'a>(&'a self, table: &'a str) -> impl Iterator<Item = &'a [Value]> + 'a {
        let primary_key_column = self.catalog.node_table(table).and_then(|schema| {
            schema
                .columns()
                .iter()
                .position(|column| column.primary_key)
        });
        node_rows_oldest_first(self.chain.as_ref(), table, primary_key_column.unwrap_or(0))
    }

    /// Iterates every assigned overlay insert slot, including deleted holes.
    ///
    /// Positions are stable within the current checkpoint epoch. Updates keep
    /// their original position, deletes yield `row: None`, and a later insert
    /// of the same primary key appends after the complete physical span.
    pub fn node_slots<'a>(
        &'a self,
        table: &'a str,
    ) -> impl Iterator<Item = OverlayNodeSlot<'a>> + 'a {
        let primary_key_column = self.catalog.node_table(table).and_then(|schema| {
            schema
                .columns()
                .iter()
                .position(|column| column.primary_key)
        });
        node_slots_oldest_first(self.chain.as_ref(), table, primary_key_column.unwrap_or(0))
    }

    /// Computes this state's net update and delete effects for one node table.
    ///
    /// This is the checked entry point for merge callers that also scan
    /// persisted rows. A malformed replacement row or tombstone whose key is
    /// not an `Int64` or `String` is reported as corruption.
    pub fn node_dml_effects<'a>(&'a self, table: &'a str) -> DevonResult<NodeDmlEffects<'a>> {
        let schema = self
            .catalog
            .node_table(table)
            .ok_or_else(|| DevonError::NotFound {
                what: format!("node table `{table}`"),
            })?;
        let primary_key_column = schema
            .columns()
            .iter()
            .position(|column| column.primary_key)
            .ok_or_else(|| DevonError::Corrupt {
                context: format!("node table `{table}` has no primary-key column"),
            })?;
        node_dml_effects(self.chain.as_ref(), table, primary_key_column)
    }

    /// Iterates one relationship table's overlay edges in insertion order.
    pub fn rel_edges<'a>(&'a self, table: &'a str) -> impl Iterator<Item = &'a OverlayEdge> + 'a {
        rel_edges_oldest_first(self.chain.as_ref(), table)
    }

    /// Unions one relationship table's endpoint tombstones oldest-first.
    #[must_use]
    pub fn rel_tombstones(&self, table: &str) -> RelEndpointTombstones {
        rel_tombstones_oldest_first(self.chain.as_ref(), table)
    }

    /// Resolves a checkpointed primary key through this state's lazy index.
    ///
    /// [`PkIndexResult::Unavailable`] means the shared memory budget refused
    /// the derived index and the caller must use its correctness fallback.
    #[doc(hidden)]
    pub fn checkpointed_node_offset(
        state: &Arc<Self>,
        pager: &Pager,
        budget: &Arc<MemoryBudget>,
        table: &str,
        key: &Value,
    ) -> DevonResult<PkIndexResult<u64>> {
        let Some(schema) = state.catalog.node_table(table) else {
            return Ok(PkIndexResult::Indexed(None));
        };
        let table_cache = published_pk_caches(state).table(schema.name());
        table_cache.lookup_offset(state, pager, budget, schema, key)
    }

    /// Resolves and reads exactly one checkpointed row through the PK index.
    ///
    /// A most-recently-decoded group is retained per table, so sequential
    /// resolution decodes each group once rather than once per row.
    #[doc(hidden)]
    pub fn checkpointed_node_row(
        state: &Arc<Self>,
        pager: &Pager,
        budget: &Arc<MemoryBudget>,
        table: &str,
        key: &Value,
    ) -> DevonResult<PkIndexResult<Vec<Value>>> {
        let Some(schema) = state.catalog.node_table(table) else {
            return Ok(PkIndexResult::Indexed(None));
        };
        let table_cache = published_pk_caches(state).table(schema.name());
        table_cache.lookup_row(state, pager, budget, schema, key)
    }

    /// Reports whether this state currently owns a built table PK index.
    #[doc(hidden)]
    #[must_use]
    pub fn checkpointed_pk_index_present(state: &Arc<Self>, table: &str) -> bool {
        let canonical = state
            .catalog
            .node_table(table)
            .map_or(table, NodeTableSchema::name);
        published_pk_caches_if_present(state)
            .and_then(|caches| caches.existing_table(canonical))
            .is_some_and(|cache| cache.is_ready())
    }
}

impl PublishedState {
    /// Shares `prev`'s checkpointed primary-key caches with `next`.
    ///
    /// Called at the ordinary-commit publish ONLY: a commit changes the
    /// overlay, never checkpointed storage, so every cached map stays
    /// exactly valid for the successor state (`docs/UI.md` §12.4 offset
    /// stability; deletes/updates are seen by the DML-effects tier, which
    /// resolution consults before the map). Checkpoint and COPY publishes
    /// rewrite storage maps and must NOT adopt — fresh rebuild there is
    /// the law, and the post-checkpoint rebuild test enforces it.
    ///
    /// ABA safety: the caller holds BOTH Arcs across this call, so
    /// neither key's allocation can be reused mid-adoption; `prev`'s
    /// later `Drop` removes only its own key.
    pub fn adopt_pk_caches(prev: &Arc<Self>, next: &Arc<Self>) {
        let Some(registry) = PUBLISHED_PK_CACHES.get() else {
            return;
        };
        let mut registry = registry.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(caches) = registry.get(&(Arc::as_ptr(prev) as usize)).cloned() {
            registry.insert(Arc::as_ptr(next) as usize, caches);
        }
    }
}

impl Drop for PublishedState {
    fn drop(&mut self) {
        let key = std::ptr::from_ref(self) as usize;
        if let Some(registry) = PUBLISHED_PK_CACHES.get() {
            registry
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(&key);
        }
        if let Some(registry) = PUBLISHED_FULLTEXT_CACHES.get() {
            registry
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(&key);
        }
    }
}

/// Sheds every idle primary-key resolution index and decoded-group cache
/// across all live published states.
///
/// The caches are derived data, so dropping one is always correct: the
/// next lookup rebuilds it, or degrades to the scan fallback through
/// [`PkIndexResult::Unavailable`]. They are also the one charge class no
/// other pressure valve reaches — proportional to table size rather than
/// any fraction of the limit, invisible to pager eviction, and pinned
/// through mid-transaction checkpoints by the transaction's own snapshot.
/// Without reclamation, these caches can exhaust the shared memory budget
/// while a small write set attempts to grow. Entries whose lock is currently
/// held are skipped via `try_lock`, which makes this safe to call from the budget reclaimer
/// even when a cache build's own `grow_charge` re-enters it.
///
/// Returns whether any charge was released.
pub fn shed_pk_caches() -> bool {
    let Some(registry) = PUBLISHED_PK_CACHES.get() else {
        return false;
    };
    let registry = registry.lock().unwrap_or_else(PoisonError::into_inner);
    let mut shed_any = false;
    for caches in registry.values() {
        let Ok(tables) = caches.tables.try_lock() else {
            continue;
        };
        for table in tables.values() {
            if let Ok(mut index) = table.index.try_lock()
                && matches!(*index, PkIndexState::Ready(_))
            {
                // Unbuilt, not Refused: the next lookup retries the
                // build under whatever headroom then exists.
                *index = PkIndexState::Unbuilt;
                shed_any = true;
            }
            if let Ok(mut cached) = table.decoded_group.try_lock()
                && cached.take().is_some()
            {
                shed_any = true;
            }
        }
    }
    shed_any
}

/// An immutable full-text index whose budget charge follows its last holder.
///
/// Like `CommitLink` (MVCC §3 rule 4), the charge belongs to the Arc-owned
/// allocation: removing a registry reference cannot release a reader's charge.
#[derive(Debug)]
pub struct ChargedFullTextIndex {
    index: FullTextIndex,
    _charge: FullTextCharge,
}

impl std::ops::Deref for ChargedFullTextIndex {
    type Target = FullTextIndex;

    fn deref(&self) -> &Self::Target {
        &self.index
    }
}

/// Result of consulting the checkpointed full-text registry.
#[derive(Debug, Clone)]
pub enum FullTextResult {
    /// A pinned, budget-charged index over checkpointed rows only.
    Indexed(Arc<ChargedFullTextIndex>),
    /// Pressure refused the index; the caller must use a bounded fallback.
    Unavailable,
}

#[derive(Debug, Default)]
enum FullTextState {
    #[default]
    Unbuilt,
    Building,
    Ready(Arc<ChargedFullTextIndex>),
    Refused,
}

type FullTextEntries = HashMap<(String, String), Arc<Mutex<FullTextState>>>;

#[derive(Debug, Default)]
struct PublishedFullTextCaches {
    // Interior mutability is restricted to derived, disposable cache state.
    entries: Mutex<FullTextEntries>,
}

static PUBLISHED_FULLTEXT_CACHES: OnceLock<Mutex<HashMap<usize, Arc<PublishedFullTextCaches>>>> =
    OnceLock::new();

fn published_fulltext_caches(state: &Arc<PublishedState>) -> Arc<PublishedFullTextCaches> {
    let mut registry = PUBLISHED_FULLTEXT_CACHES
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    Arc::clone(registry.entry(Arc::as_ptr(state) as usize).or_default())
}

impl PublishedState {
    /// Lazily indexes one String column's CHECKPOINTED groups only.
    ///
    /// Callers must merge snapshot-visible overlay inserts, updates and deletes
    /// before applying their top-k. No overlay value is ever cached here.
    pub fn fulltext_index(
        state: &Arc<Self>,
        pager: &Pager,
        budget: &Arc<MemoryBudget>,
        table: &str,
        column: &str,
    ) -> DevonResult<FullTextResult> {
        let schema = state
            .catalog
            .node_table(table)
            .ok_or_else(|| DevonError::NotFound {
                what: format!("node table `{table}`"),
            })?;
        let position = schema
            .columns()
            .iter()
            .position(|entry| entry.name.eq_ignore_ascii_case(column))
            .ok_or_else(|| DevonError::InvalidArgument {
                context: format!("unknown column `{table}.{column}`"),
            })?;
        let definition = &schema.columns()[position];
        if definition.ty != devondb_types::logical_type::LogicalType::String {
            return Err(DevonError::InvalidArgument {
                context: format!(
                    "full-text column `{table}.{column}` must be String, got {:?}",
                    definition.ty
                ),
            });
        }
        let caches = published_fulltext_caches(state);
        let entry = {
            let mut entries = caches
                .entries
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            Arc::clone(
                entries
                    .entry((schema.name().to_owned(), definition.name.clone()))
                    .or_default(),
            )
        };
        let mut index = entry.lock().unwrap_or_else(PoisonError::into_inner);
        if matches!(*index, FullTextState::Unbuilt) {
            *index = FullTextState::Building;
            match build_fulltext_index(state, pager, budget, schema, position) {
                Ok(built) => *index = FullTextState::Ready(Arc::new(built)),
                Err(DevonError::BudgetExceeded { .. }) => *index = FullTextState::Refused,
                Err(error) => {
                    *index = FullTextState::Unbuilt;
                    return Err(error);
                }
            }
        }
        Ok(match &*index {
            FullTextState::Ready(index) => FullTextResult::Indexed(Arc::clone(index)),
            _ => FullTextResult::Unavailable,
        })
    }

    /// Reports whether this publication currently has a ready full-text index.
    #[doc(hidden)]
    #[must_use]
    pub fn fulltext_index_present(state: &Arc<Self>, table: &str, column: &str) -> bool {
        let caches = published_fulltext_caches(state);
        let entries = caches
            .entries
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        entries.iter().any(|((t, c), entry)| {
            t.eq_ignore_ascii_case(table)
                && c.eq_ignore_ascii_case(column)
                && matches!(
                    *entry.lock().unwrap_or_else(PoisonError::into_inner),
                    FullTextState::Ready(_)
                )
        })
    }

    /// Adopts valid derived indexes at ordinary commit; never at checkpoint.
    /// Entries are shared only when both catalogs preserve the column's type.
    pub fn adopt_fulltext_caches(prev: &Arc<Self>, next: &Arc<Self>) {
        let Some(registry) = PUBLISHED_FULLTEXT_CACHES.get() else {
            return;
        };
        let mut registry = registry.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(caches) = registry.get(&(Arc::as_ptr(prev) as usize)) else {
            return;
        };
        let adopted = {
            let entries = caches
                .entries
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            entries
                .iter()
                .filter(|((table, column), _)| {
                    fulltext_identity_matches(&prev.catalog, &next.catalog, table, column)
                })
                .map(|(key, value)| (key.clone(), Arc::clone(value)))
                .collect()
        };
        registry.insert(
            Arc::as_ptr(next) as usize,
            Arc::new(PublishedFullTextCaches {
                entries: Mutex::new(adopted),
            }),
        );
    }
}

fn fulltext_identity_matches(prev: &Catalog, next: &Catalog, table: &str, column: &str) -> bool {
    let column_type = |catalog: &Catalog| {
        catalog.node_table(table).and_then(|schema| {
            schema
                .columns()
                .iter()
                .find(|entry| entry.name.eq_ignore_ascii_case(column))
                .map(|entry| entry.ty)
        })
    };
    column_type(prev).is_some_and(|ty| column_type(next) == Some(ty))
}

/// Sheds idle full-text entries, retaining charges pinned by active readers.
/// Uses try-locks so a build's own reclaimer may safely reenter this function.
pub fn shed_fulltext_caches() -> bool {
    let Some(registry) = PUBLISHED_FULLTEXT_CACHES.get() else {
        return false;
    };
    let Ok(registry) = registry.try_lock() else {
        return false;
    };
    let mut shed = false;
    for caches in registry.values() {
        let Ok(entries) = caches.entries.try_lock() else {
            continue;
        };
        for entry in entries.values() {
            if let Ok(mut index) = entry.try_lock()
                && matches!(*index, FullTextState::Ready(_) | FullTextState::Refused)
            {
                *index = FullTextState::Unbuilt;
                shed = true;
            }
        }
    }
    shed
}

#[derive(Debug)]
struct FullTextCharge {
    budget: Arc<MemoryBudget>,
    bytes: usize,
}

impl FullTextCharge {
    fn new(budget: &Arc<MemoryBudget>) -> Self {
        Self {
            budget: Arc::clone(budget),
            bytes: 0,
        }
    }

    fn resize(&mut self, bytes: usize) -> DevonResult<()> {
        if bytes > self.bytes && !self.budget.charge_or_reclaim(bytes - self.bytes) {
            return Err(DevonError::BudgetExceeded {
                context: format!(
                    "full-text index requested {} bytes with {} charged and limit {}",
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

impl Drop for FullTextCharge {
    fn drop(&mut self) {
        self.budget.release(self.bytes);
    }
}

fn build_fulltext_index(
    state: &PublishedState,
    pager: &Pager,
    budget: &Arc<MemoryBudget>,
    schema: &NodeTableSchema,
    column: usize,
) -> DevonResult<ChargedFullTextIndex> {
    let mut builder = FullTextIndexBuilder::default();
    let mut charge = FullTextCharge::new(budget);
    let types = schema
        .columns()
        .iter()
        .map(|column| column.ty)
        .collect::<Vec<_>>();
    let groups = state
        .catalog
        .table_storage(schema.name())
        .map_or(&[][..], |storage| storage.groups.as_slice());
    let mut ordinal = 0_u64;
    for group in groups {
        let directory = NodeGroup::read_directory(pager, *group, &types)?;
        let mut scratch = FullTextCharge::new(budget);
        scratch.resize(directory.string_column_decode_peak_bytes(column)?)?;
        let (decoded, _) = NodeGroup::read_column_typed(pager, *group, &types, column)?;
        let devondb_types::column::Column::Boxed(values) = decoded else {
            return Err(pk_corrupt("full-text String column was not boxed"));
        };
        scratch.resize(
            values.capacity() * size_of::<Value>()
                + values
                    .iter()
                    .map(|value| match value {
                        Value::String(text) => text.capacity(),
                        _ => 0,
                    })
                    .sum::<usize>(),
        )?;
        for value in &values {
            match value {
                Value::String(text) => builder
                    .push_with_reservation(ordinal, text, |bytes| charge.resize(bytes).is_ok())
                    .map_err(|error| match error {
                        FullTextError::MemoryRefused => DevonError::BudgetExceeded {
                            context: "full-text builder working memory refused".into(),
                        },
                        error => pk_corrupt(error.to_string()),
                    })?,
                Value::Null => {}
                _ => {
                    return Err(pk_corrupt(
                        "full-text String column contained a non-string value",
                    ));
                }
            }
            ordinal = ordinal
                .checked_add(1)
                .ok_or_else(|| pk_corrupt("full-text row ordinal exceeds u64::MAX"))?;
        }
        // Charge the actual capacity delta after every group, before reading another.
        charge.resize(builder.resident_size_bytes())?;
    }
    charge.resize(builder.finish_peak_bytes())?;
    let index = builder.finish();
    charge.resize(index.heap_bytes())?;
    Ok(ChargedFullTextIndex {
        index,
        _charge: charge,
    })
}

/// Result of consulting a checkpointed primary-key index.
#[derive(Debug, Clone, PartialEq)]
pub enum PkIndexResult<T> {
    /// The index was built; the key may or may not have been present.
    Indexed(Option<T>),
    /// Budget pressure refused the index, so the scan fallback is required.
    Unavailable,
}

#[derive(Debug, Default)]
struct PublishedPkCaches {
    tables: Mutex<HashMap<String, Arc<TablePkCache>>>,
}

impl PublishedPkCaches {
    fn table(&self, table: &str) -> Arc<TablePkCache> {
        let mut tables = self.tables.lock().unwrap_or_else(PoisonError::into_inner);
        Arc::clone(
            tables
                .entry(table.to_owned())
                .or_insert_with(|| Arc::new(TablePkCache::default())),
        )
    }

    fn existing_table(&self, table: &str) -> Option<Arc<TablePkCache>> {
        self.tables
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(table)
            .cloned()
    }
}

static PUBLISHED_PK_CACHES: OnceLock<Mutex<HashMap<usize, Arc<PublishedPkCaches>>>> =
    OnceLock::new();

fn published_pk_caches(state: &Arc<PublishedState>) -> Arc<PublishedPkCaches> {
    let mut registry = PUBLISHED_PK_CACHES
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    Arc::clone(
        registry
            .entry(Arc::as_ptr(state) as usize)
            .or_insert_with(|| Arc::new(PublishedPkCaches::default())),
    )
}

fn published_pk_caches_if_present(state: &Arc<PublishedState>) -> Option<Arc<PublishedPkCaches>> {
    PUBLISHED_PK_CACHES.get().and_then(|registry| {
        registry
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&(Arc::as_ptr(state) as usize))
            .cloned()
    })
}

#[derive(Debug, Default)]
struct TablePkCache {
    index: Mutex<PkIndexState>,
    decoded_group: Mutex<Option<CachedNodeGroup>>,
}

#[derive(Debug, Default)]
enum PkIndexState {
    #[default]
    Unbuilt,
    Ready(PkResolutionIndex),
    Refused,
}

impl TablePkCache {
    fn lookup_offset(
        &self,
        state: &PublishedState,
        pager: &Pager,
        budget: &Arc<MemoryBudget>,
        schema: &NodeTableSchema,
        key: &Value,
    ) -> DevonResult<PkIndexResult<u64>> {
        let mut index = self.index.lock().unwrap_or_else(PoisonError::into_inner);
        ensure_pk_index(&mut index, state, pager, budget, schema)?;
        match &*index {
            PkIndexState::Ready(index) => Ok(PkIndexResult::Indexed(index.offset(key))),
            PkIndexState::Refused => Ok(PkIndexResult::Unavailable),
            PkIndexState::Unbuilt => Err(pk_corrupt("PK index build left its state unbuilt")),
        }
    }

    fn lookup_row(
        &self,
        state: &PublishedState,
        pager: &Pager,
        budget: &Arc<MemoryBudget>,
        schema: &NodeTableSchema,
        key: &Value,
    ) -> DevonResult<PkIndexResult<Vec<Value>>> {
        let mut index = self.index.lock().unwrap_or_else(PoisonError::into_inner);
        ensure_pk_index(&mut index, state, pager, budget, schema)?;
        let (offset, location) = match &*index {
            PkIndexState::Ready(index) => match index.locate(key)? {
                Some(found) => found,
                None => return Ok(PkIndexResult::Indexed(None)),
            },
            PkIndexState::Refused => return Ok(PkIndexResult::Unavailable),
            PkIndexState::Unbuilt => {
                return Err(pk_corrupt("PK index build left its state unbuilt"));
            }
        };
        drop(index);
        let row = self.read_row(pager, budget, schema, offset, location)?;
        Ok(PkIndexResult::Indexed(Some(row)))
    }

    fn read_row(
        &self,
        pager: &Pager,
        budget: &Arc<MemoryBudget>,
        schema: &NodeTableSchema,
        offset: u64,
        location: GroupLocation,
    ) -> DevonResult<Vec<Value>> {
        let row = location.row_index(offset)?;
        let mut cached = self
            .decoded_group
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(group) = cached
            .as_ref()
            .filter(|group| group.page_id == location.page_id)
        {
            return node_group_row(&group.group, row);
        }
        *cached = None;
        let types = schema
            .columns()
            .iter()
            .map(|column| column.ty)
            .collect::<Vec<_>>();
        let group = NodeGroup::read(pager, location.page_id, &types)?;
        let result = node_group_row(&group, row)?;
        cache_decoded_group(&mut cached, budget, location.page_id, group);
        Ok(result)
    }

    fn is_ready(&self) -> bool {
        matches!(
            *self.index.lock().unwrap_or_else(PoisonError::into_inner),
            PkIndexState::Ready(_)
        )
    }
}

fn ensure_pk_index(
    state: &mut PkIndexState,
    published: &PublishedState,
    pager: &Pager,
    budget: &Arc<MemoryBudget>,
    schema: &NodeTableSchema,
) -> DevonResult<()> {
    if !matches!(state, PkIndexState::Unbuilt) {
        return Ok(());
    }
    match build_pk_index(published, pager, budget, schema) {
        Ok(Some(index)) => *state = PkIndexState::Ready(index),
        Ok(None) | Err(DevonError::BudgetExceeded { .. }) => *state = PkIndexState::Refused,
        Err(error) => return Err(error),
    }
    Ok(())
}

#[derive(Debug)]
struct PkResolutionIndex {
    integer_offsets: HashMap<i64, u64>,
    string_offsets: HashMap<String, u64>,
    groups: Vec<GroupLocation>,
    charged_bytes: usize,
    budget: Arc<MemoryBudget>,
}

impl PkResolutionIndex {
    fn new(budget: &Arc<MemoryBudget>) -> Self {
        Self {
            integer_offsets: HashMap::new(),
            string_offsets: HashMap::new(),
            groups: Vec::new(),
            charged_bytes: 0,
            budget: Arc::clone(budget),
        }
    }

    fn offset(&self, key: &Value) -> Option<u64> {
        match key {
            Value::Int64(key) => self.integer_offsets.get(key).copied(),
            Value::String(key) => self.string_offsets.get(key.as_str()).copied(),
            _ => None,
        }
    }

    fn locate(&self, key: &Value) -> DevonResult<Option<(u64, GroupLocation)>> {
        let Some(offset) = self.offset(key) else {
            return Ok(None);
        };
        let position = self
            .groups
            .partition_point(|location| location.start_offset <= offset);
        let location = position
            .checked_sub(1)
            .and_then(|index| self.groups.get(index))
            .copied()
            .ok_or_else(|| pk_corrupt("indexed node offset has no containing group"))?;
        location.row_index(offset)?;
        Ok(Some((offset, location)))
    }

    fn push_group(&mut self, location: GroupLocation) -> bool {
        if !self.grow_charge(PK_INDEX_GROUP_BYTES) || self.groups.try_reserve(1).is_err() {
            return false;
        }
        self.groups.push(location);
        true
    }

    fn insert(&mut self, value: &Value, offset: u64) -> bool {
        match value {
            Value::Int64(key) => self.insert_integer(*key, offset),
            Value::String(key) => self.insert_string(key, offset),
            Value::Null => true,
            _ => false,
        }
    }

    fn insert_integer(&mut self, key: i64, offset: u64) -> bool {
        if self.integer_offsets.contains_key(&key) {
            return true;
        }
        if !self.grow_charge(PK_INDEX_INT_ENTRY_BYTES)
            || self.integer_offsets.try_reserve(1).is_err()
        {
            return false;
        }
        self.integer_offsets.insert(key, offset);
        true
    }

    fn insert_string(&mut self, key: &str, offset: u64) -> bool {
        if self.string_offsets.contains_key(key) {
            return true;
        }
        let Some(bytes) = PK_INDEX_STRING_ENTRY_BYTES.checked_add(key.len()) else {
            return false;
        };
        if !self.grow_charge(bytes) || self.string_offsets.try_reserve(1).is_err() {
            return false;
        }
        self.string_offsets.insert(key.to_owned(), offset);
        true
    }

    fn grow_charge(&mut self, bytes: usize) -> bool {
        let requested = if self.charged_bytes == 0 {
            let Some(requested) = PK_INDEX_BASE_BYTES.checked_add(bytes) else {
                return false;
            };
            requested
        } else {
            bytes
        };
        let Some(charged) = self.charged_bytes.checked_add(requested) else {
            return false;
        };
        if !self.budget.charge_or_reclaim(requested) {
            return false;
        }
        self.charged_bytes = charged;
        true
    }
}

impl Drop for PkResolutionIndex {
    fn drop(&mut self) {
        self.budget.release(self.charged_bytes);
        self.charged_bytes = 0;
    }
}

#[derive(Debug, Clone, Copy)]
struct GroupLocation {
    page_id: u64,
    start_offset: u64,
    row_count: u64,
}

impl GroupLocation {
    fn row_index(self, offset: u64) -> DevonResult<usize> {
        let row = offset
            .checked_sub(self.start_offset)
            .ok_or_else(|| pk_corrupt("indexed node offset precedes its containing group"))?;
        if row >= self.row_count {
            return Err(pk_corrupt(
                "indexed node offset exceeds its containing group",
            ));
        }
        usize::try_from(row).map_err(|_| pk_corrupt("node-group row index exceeds usize::MAX"))
    }
}

fn build_pk_index(
    state: &PublishedState,
    pager: &Pager,
    budget: &Arc<MemoryBudget>,
    schema: &NodeTableSchema,
) -> DevonResult<Option<PkResolutionIndex>> {
    let mut index = PkResolutionIndex::new(budget);
    let key_column = schema
        .columns()
        .iter()
        .position(|column| column.primary_key)
        .ok_or_else(|| pk_corrupt(format!("node table `{}` has no primary key", schema.name())))?;
    let types = schema
        .columns()
        .iter()
        .map(|column| column.ty)
        .collect::<Vec<_>>();
    let group_ids = state
        .catalog
        .table_storage(schema.name())
        .map_or(&[][..], |storage| storage.groups.as_slice());
    let mut start_offset = 0_u64;
    for group_id in group_ids {
        // Decode only the primary-key column. A full-row read
        // decodes every fat column (content, embeddings) of every row just
        // to map pk→offset, which dominated live map builds.
        let (keys, group_rows) = NodeGroup::read_column(pager, *group_id, &types, key_column)?;
        let row_count = u64::try_from(group_rows)
            .map_err(|_| pk_corrupt("node-group row count exceeds u64::MAX"))?;
        if !index.push_group(GroupLocation {
            page_id: *group_id,
            start_offset,
            row_count,
        }) || !index_group_keys(&mut index, &keys, start_offset)?
        {
            return Ok(None);
        }
        start_offset = start_offset
            .checked_add(row_count)
            .ok_or_else(|| pk_corrupt("checkpointed node offset exceeds u64::MAX"))?;
    }
    Ok(Some(index))
}

fn index_group_keys(
    index: &mut PkResolutionIndex,
    keys: &[Value],
    start_offset: u64,
) -> DevonResult<bool> {
    for (row, key) in keys.iter().enumerate() {
        let row_offset =
            u64::try_from(row).map_err(|_| pk_corrupt("node-group row index exceeds u64::MAX"))?;
        let offset = start_offset
            .checked_add(row_offset)
            .ok_or_else(|| pk_corrupt("checkpointed node offset exceeds u64::MAX"))?;
        if !index.insert(key, offset) {
            return Ok(false);
        }
    }
    Ok(true)
}

#[derive(Debug)]
struct CachedNodeGroup {
    page_id: u64,
    group: NodeGroup,
    charged_bytes: usize,
    budget: Arc<MemoryBudget>,
}

impl Drop for CachedNodeGroup {
    fn drop(&mut self) {
        self.budget.release(self.charged_bytes);
        self.charged_bytes = 0;
    }
}

fn cache_decoded_group(
    cached: &mut Option<CachedNodeGroup>,
    budget: &Arc<MemoryBudget>,
    page_id: u64,
    group: NodeGroup,
) {
    let Some(charged_bytes) = node_group_estimated_bytes(&group) else {
        return;
    };
    if budget.charge_or_reclaim(charged_bytes) {
        *cached = Some(CachedNodeGroup {
            page_id,
            group,
            charged_bytes,
            budget: Arc::clone(budget),
        });
    }
}

fn node_group_estimated_bytes(group: &NodeGroup) -> Option<usize> {
    let mut bytes = size_of::<NodeGroup>()
        .checked_add(group.column_count().checked_mul(size_of::<Vec<Value>>())?)?;
    for row in 0..group.row_count() {
        for column in 0..group.column_count() {
            bytes = bytes.checked_add(group.value(row, column)?.approx_bytes())?;
        }
    }
    Some(bytes)
}

fn node_group_row(group: &NodeGroup, row: usize) -> DevonResult<Vec<Value>> {
    (0..group.column_count())
        .map(|column| {
            group
                .value(row, column)
                .cloned()
                .ok_or_else(|| pk_corrupt("node group is missing an indexed row value"))
        })
        .collect()
}

fn pk_corrupt(context: impl Into<String>) -> DevonError {
    DevonError::Corrupt {
        context: context.into(),
    }
}

/// One immutable committed delta linked to the next-older commit.
#[derive(Debug)]
pub struct CommitLink {
    /// Next-older commit.
    pub prev: Option<Arc<CommitLink>>,
    /// LSN of this commit's terminating WAL record.
    pub commit_lsn: u64,
    /// Rows, edges, and DDL made visible by this commit.
    pub delta: CommitDelta,
    /// Bytes charged to the shared memory budget for this link.
    pub charged_bytes: usize,
    budget: Arc<MemoryBudget>,
}

impl CommitLink {
    /// Constructs and charges one link against `budget`.
    ///
    /// The charge covers [`COMMIT_LINK_OVERHEAD_BYTES`] plus the delta's
    /// writer-policy estimate. It is released exactly once when the link is
    /// dropped, including when a whole uniquely owned chain is dropped.
    pub fn new(
        prev: Option<Arc<Self>>,
        commit_lsn: u64,
        delta: CommitDelta,
        budget: Arc<MemoryBudget>,
    ) -> DevonResult<Self> {
        let delta_bytes = delta.estimated_bytes()?;
        let charged_bytes = checked_estimate_add(COMMIT_LINK_OVERHEAD_BYTES, delta_bytes)?;
        if !budget.try_charge(charged_bytes) {
            return Err(DevonError::BudgetExceeded {
                context: format!(
                    "committed overlay requested {charged_bytes} bytes with {} bytes charged and limit {}",
                    budget.charged(),
                    budget.limit()
                ),
            });
        }
        Ok(Self {
            prev,
            commit_lsn,
            delta,
            charged_bytes,
            budget,
        })
    }

    /// Constructs, charges, and wraps one link in its publication `Arc`.
    pub fn new_arc(
        prev: Option<Arc<Self>>,
        commit_lsn: u64,
        delta: CommitDelta,
        budget: Arc<MemoryBudget>,
    ) -> DevonResult<Arc<Self>> {
        Self::new(prev, commit_lsn, delta, budget).map(Arc::new)
    }

    /// Wraps one link whose bytes were already charged by the caller.
    ///
    /// The commit pipeline reserves the link's charge BEFORE its WAL fsync
    /// so that no post-fsync step can fail on budget (a post-fsync
    /// `BudgetExceeded` would misreport a durably committed transaction as
    /// failed). `reserved_bytes` must equal this link's exact estimate —
    /// a mismatch is a bookkeeping bug and is rejected before the
    /// reservation is consumed, leaving the caller to release it.
    pub fn new_arc_reserved(
        prev: Option<Arc<Self>>,
        commit_lsn: u64,
        delta: CommitDelta,
        budget: Arc<MemoryBudget>,
        reserved_bytes: usize,
    ) -> DevonResult<Arc<Self>> {
        let delta_bytes = delta.estimated_bytes()?;
        let charged_bytes = checked_estimate_add(COMMIT_LINK_OVERHEAD_BYTES, delta_bytes)?;
        if charged_bytes != reserved_bytes {
            return Err(DevonError::InvalidArgument {
                context: format!(
                    "commit reservation of {reserved_bytes} bytes does not match the link estimate of {charged_bytes} bytes"
                ),
            });
        }
        Ok(Arc::new(Self {
            prev,
            commit_lsn,
            delta,
            charged_bytes,
            budget,
        }))
    }
}

impl Drop for CommitLink {
    fn drop(&mut self) {
        release_link_charge(self);
        let mut previous = self.prev.take();
        while let Some(link) = previous {
            match Arc::try_unwrap(link) {
                Ok(mut owned) => {
                    previous = owned.prev.take();
                    release_link_charge(&mut owned);
                }
                Err(shared) => {
                    drop(shared);
                    break;
                }
            }
        }
    }
}

fn release_link_charge(link: &mut CommitLink) {
    link.budget.release(link.charged_bytes);
    link.charged_bytes = 0;
}

/// Endpoint-role relationship tombstones for one relationship table.
///
/// An edge is hidden when its source occurs in [`Self::from_offsets`] or its
/// destination occurs in [`Self::to_offsets`]. The predicate intentionally
/// carries no per-edge identity, so duplicate edges are removed together.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RelEndpointTombstones {
    /// Source-node offsets whose outgoing edges are hidden.
    pub from_offsets: BTreeSet<u64>,
    /// Destination-node offsets whose incoming edges are hidden.
    pub to_offsets: BTreeSet<u64>,
}

impl RelEndpointTombstones {
    /// Unions `other` into this endpoint-role predicate.
    pub fn union_with(&mut self, other: &Self) {
        self.from_offsets.extend(other.from_offsets.iter().copied());
        self.to_offsets.extend(other.to_offsets.iter().copied());
    }

    /// Returns whether neither endpoint role contains an offset.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.from_offsets.is_empty() && self.to_offsets.is_empty()
    }

    /// Estimates the two set containers and every resident offset entry.
    pub fn estimated_bytes(&self) -> DevonResult<usize> {
        let entries = self
            .from_offsets
            .len()
            .checked_add(self.to_offsets.len())
            .ok_or_else(estimate_overflow)?;
        checked_estimate_add(
            REL_ENDPOINT_TOMBSTONES_OVERHEAD_BYTES,
            entries
                .checked_mul(REL_TOMBSTONE_OFFSET_OVERHEAD_BYTES)
                .ok_or_else(estimate_overflow)?,
        )
    }
}

/// The rows, edges, tombstones, DDL, and derived index deltas published by one commit.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CommitDelta {
    /// Per node table, rows in statement order.
    pub nodes: BTreeMap<String, Vec<Vec<Value>>>,
    /// Per node table, full replacement rows in statement order, keyed
    /// implicitly by the primary-key value each row carries
    /// (`docs/UI.md` §12.2).
    pub node_updates: BTreeMap<String, Vec<Vec<Value>>>,
    /// Per node table, tombstoned primary-key values in statement order
    /// (`docs/UI.md` §12.2).
    pub node_deletes: BTreeMap<String, Vec<Value>>,
    /// Per relationship table, edges in statement order with resolved offsets.
    pub edges: BTreeMap<String, Vec<OverlayEdge>>,
    /// Per relationship table, endpoint-role predicates hiding incident edges.
    pub rel_tombstones: BTreeMap<String, RelEndpointTombstones>,
    /// DDL operations in statement order.
    pub ddl: Vec<DdlOp>,
    /// Per index name, the committed HNSW overlay delta (`docs/HNSW.md`
    /// §5.1). Derived state: never carried in the WAL, so recovered
    /// deltas leave this empty and recovered rows join the exact tail
    /// (`docs/HNSW.md` §5.4).
    pub hnsw: BTreeMap<String, crate::hnsw::types::HnswDelta>,
}

impl CommitDelta {
    /// Builds a delta from one recovered committed WAL group.
    ///
    /// Every record must carry the enclosing group's commit LSN. A mismatch
    /// indicates recovery framing corruption rather than a visibility filter.
    pub fn from_committed_group(group: CommittedGroup) -> DevonResult<Self> {
        let CommittedGroup {
            commit_lsn,
            records,
        } = group;
        if let Some(record) = records.iter().find(|record| record.begin_lsn != commit_lsn) {
            return Err(DevonError::Corrupt {
                context: format!(
                    "WAL group committed at LSN {commit_lsn} contains record stamped with begin LSN {}",
                    record.begin_lsn
                ),
            });
        }
        Self::from_wal_payloads(records.into_iter().map(|record| record.payload))
    }

    /// Builds a delta from non-commit WAL payloads in canonical WAL order.
    pub fn from_wal_payloads<I>(payloads: I) -> DevonResult<Self>
    where
        I: IntoIterator<Item = WalPayload>,
    {
        let mut delta = Self::default();
        for payload in payloads {
            match payload {
                WalPayload::NodeInsert { table, row } => {
                    delta.nodes.entry(table).or_default().push(row);
                }
                WalPayload::RelInsert {
                    rel,
                    from,
                    to,
                    values,
                } => delta
                    .edges
                    .entry(rel)
                    .or_default()
                    .push(OverlayEdge { from, to, values }),
                WalPayload::NodeUpdate { table, row } => {
                    delta.node_updates.entry(table).or_default().push(row);
                }
                WalPayload::RelDelete {
                    rel,
                    endpoint,
                    offset,
                } => {
                    let tombstones = delta.rel_tombstones.entry(rel).or_default();
                    match endpoint {
                        RelEndpoint::From => {
                            tombstones.from_offsets.insert(offset);
                        }
                        RelEndpoint::To => {
                            tombstones.to_offsets.insert(offset);
                        }
                    }
                }
                WalPayload::NodeDelete { table, key } => {
                    delta.node_deletes.entry(table).or_default().push(key);
                }
                WalPayload::Ddl(ddl) => delta.ddl.push(ddl.into()),
                WalPayload::Commit { .. } => {
                    return Err(DevonError::Corrupt {
                        context: "commit delta contains a WAL commit payload".to_owned(),
                    });
                }
            }
        }
        Ok(delta)
    }

    /// Estimates this delta's charged in-memory bytes using writer policy.
    pub fn estimated_bytes(&self) -> DevonResult<usize> {
        let mut total = COMMIT_DELTA_OVERHEAD_BYTES;
        estimate_node_rows(&mut total, &self.nodes)?;
        estimate_node_rows(&mut total, &self.node_updates)?;
        for (table, keys) in &self.node_deletes {
            checked_estimate_accumulate(&mut total, MAP_ENTRY_OVERHEAD_BYTES)?;
            checked_estimate_accumulate(&mut total, table.len())?;
            estimate_values(&mut total, keys)?;
        }
        estimate_edges(&mut total, &self.edges)?;
        estimate_rel_tombstones(&mut total, &self.rel_tombstones)?;
        for ddl in &self.ddl {
            checked_estimate_accumulate(&mut total, DDL_OP_OVERHEAD_BYTES)?;
            estimate_ddl(&mut total, ddl)?;
        }
        Ok(total)
    }
}

impl TryFrom<CommittedGroup> for CommitDelta {
    type Error = DevonError;

    fn try_from(group: CommittedGroup) -> Result<Self, Self::Error> {
        Self::from_committed_group(group)
    }
}

/// One committed relationship edge with globally resolved endpoint offsets.
#[derive(Debug, Clone, PartialEq)]
pub struct OverlayEdge {
    /// Source-node global offset.
    pub from: u64,
    /// Destination-node global offset.
    pub to: u64,
    /// Property values in catalog declaration order.
    pub values: Vec<Value>,
}

/// One assigned node-insert slot in the committed overlay.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OverlayNodeSlot<'a> {
    /// Zero-based physical position after the checkpointed node prefix.
    pub position: usize,
    /// Visible row at this position, or `None` for an uncheckpointed hole.
    pub row: Option<&'a [Value]>,
}

/// One committed, not-necessarily-checkpointed schema change.
#[derive(Debug, Clone, PartialEq)]
pub enum DdlOp {
    /// Creates a node table.
    CreateNodeTable(NodeTableSchema),
    /// Creates a relationship table.
    CreateRelTable(RelTableSchema),
}

impl From<DdlPayload> for DdlOp {
    fn from(value: DdlPayload) -> Self {
        match value {
            DdlPayload::CreateNodeTable(schema) => Self::CreateNodeTable(schema),
            DdlPayload::CreateRelTable(schema) => Self::CreateRelTable(schema),
        }
    }
}

/// Everything conflict detection needs from one committed transaction.
#[derive(Debug)]
pub struct CommitSummary {
    /// Node table to inserted primary-key values.
    pub inserted_pks: BTreeMap<String, Vec<Value>>,
    /// Node table to primary-key values written by update OR delete
    /// (`docs/UI.md` §12.2 — conflict granularity does not distinguish).
    pub dml_pks: BTreeMap<String, Vec<Value>>,
    /// PKs whose node delete also claims incident relationship removal.
    pub detach_pks: BTreeMap<String, Vec<Value>>,
    /// Endpoint keys used by committed relationship inserts, grouped by
    /// endpoint node table.
    pub edge_endpoint_pks: BTreeMap<String, Vec<Value>>,
    /// Names of node and relationship tables created by this commit.
    pub created_tables: Vec<String>,
    charged_bytes: AtomicUsize,
    budget: Option<Arc<MemoryBudget>>,
}

impl CommitSummary {
    /// Constructs an uncharged summary for estimation or later reservation.
    #[must_use]
    pub fn new(inserted_pks: BTreeMap<String, Vec<Value>>, created_tables: Vec<String>) -> Self {
        Self::new_with_dml(inserted_pks, BTreeMap::new(), created_tables)
    }

    /// Constructs an uncharged summary carrying update/delete PKs.
    #[must_use]
    pub fn new_with_dml(
        inserted_pks: BTreeMap<String, Vec<Value>>,
        dml_pks: BTreeMap<String, Vec<Value>>,
        created_tables: Vec<String>,
    ) -> Self {
        Self::new_with_detach_claims(
            inserted_pks,
            dml_pks,
            BTreeMap::new(),
            BTreeMap::new(),
            created_tables,
        )
    }

    /// Constructs an uncharged summary with asymmetric detach/edge claims.
    #[must_use]
    pub fn new_with_detach_claims(
        inserted_pks: BTreeMap<String, Vec<Value>>,
        dml_pks: BTreeMap<String, Vec<Value>>,
        detach_pks: BTreeMap<String, Vec<Value>>,
        edge_endpoint_pks: BTreeMap<String, Vec<Value>>,
        created_tables: Vec<String>,
    ) -> Self {
        Self {
            inserted_pks,
            dml_pks,
            detach_pks,
            edge_endpoint_pks,
            created_tables,
            charged_bytes: AtomicUsize::new(0),
            budget: None,
        }
    }

    /// Estimates this summary's charged in-memory bytes using writer policy.
    pub fn estimated_bytes(&self) -> DevonResult<usize> {
        self.estimated_bytes_with_base(COMMIT_SUMMARY_OVERHEAD_BYTES)
    }

    /// Constructs, charges, and wraps one summary in its retention `Arc`.
    pub fn new_arc(summary: Self, budget: Arc<MemoryBudget>) -> DevonResult<Arc<Self>> {
        let charged_bytes = summary.estimated_bytes()?;
        if !budget.try_charge(charged_bytes) {
            return Err(DevonError::BudgetExceeded {
                context: format!(
                    "commit summary requested {charged_bytes} bytes with {} bytes charged and limit {}",
                    budget.charged(),
                    budget.limit()
                ),
            });
        }
        Self::new_arc_reserved(summary, Arc::clone(&budget), charged_bytes)
            .inspect_err(|_| budget.release(charged_bytes))
    }

    /// Wraps a summary whose exact bytes were already charged by the caller.
    pub fn new_arc_reserved(
        mut summary: Self,
        budget: Arc<MemoryBudget>,
        reserved_bytes: usize,
    ) -> DevonResult<Arc<Self>> {
        let estimated_bytes = summary.estimated_bytes()?;
        if estimated_bytes != reserved_bytes {
            return Err(DevonError::InvalidArgument {
                context: format!(
                    "commit summary reservation of {reserved_bytes} bytes does not match the estimate of {estimated_bytes} bytes"
                ),
            });
        }
        summary.charged_bytes = AtomicUsize::new(reserved_bytes);
        summary.budget = Some(budget);
        Ok(Arc::new(summary))
    }

    /// Returns the summary bytes that remain charged to the shared budget.
    #[must_use]
    pub fn charged_bytes(&self) -> usize {
        self.charged_bytes.load(Ordering::Relaxed)
    }

    fn estimated_bytes_with_base(&self, base: usize) -> DevonResult<usize> {
        let mut total = base;
        for (table, keys) in self
            .inserted_pks
            .iter()
            .chain(&self.dml_pks)
            .chain(&self.detach_pks)
            .chain(&self.edge_endpoint_pks)
        {
            checked_estimate_accumulate(&mut total, MAP_ENTRY_OVERHEAD_BYTES)?;
            checked_estimate_accumulate(&mut total, table.len())?;
            estimate_values(&mut total, keys)?;
        }
        for table in &self.created_tables {
            checked_estimate_accumulate(&mut total, SUMMARY_TABLE_NAME_OVERHEAD_BYTES)?;
            checked_estimate_accumulate(&mut total, table.len())?;
        }
        Ok(total)
    }

    fn release_charge(&self) {
        let charged_bytes = self.charged_bytes.swap(0, Ordering::Relaxed);
        if charged_bytes > 0
            && let Some(budget) = &self.budget
        {
            budget.release(charged_bytes);
        }
    }
}

impl Default for CommitSummary {
    fn default() -> Self {
        Self::new(BTreeMap::new(), Vec::new())
    }
}

impl PartialEq for CommitSummary {
    fn eq(&self, other: &Self) -> bool {
        self.inserted_pks == other.inserted_pks
            && self.dml_pks == other.dml_pks
            && self.detach_pks == other.detach_pks
            && self.edge_endpoint_pks == other.edge_endpoint_pks
            && self.created_tables == other.created_tables
    }
}

/// An ordered borrowed node primary key used while merging overlay effects.
///
/// Node-table schemas admit only `Int64` and `String` primary keys. Borrowing
/// the string spelling keeps a chain walk from allocating one copy per key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum PkKey<'a> {
    /// A signed 64-bit integer primary key.
    Int64(i64),
    /// A UTF-8 string primary key.
    String(&'a str),
}

impl<'a> TryFrom<&'a Value> for PkKey<'a> {
    type Error = DevonError;

    fn try_from(value: &'a Value) -> Result<Self, Self::Error> {
        match value {
            Value::Int64(value) => Ok(Self::Int64(*value)),
            Value::String(value) => Ok(Self::String(value)),
            other => Err(DevonError::Corrupt {
                context: format!(
                    "overlay primary key must be Int64 or String, found {}",
                    value_kind(other)
                ),
            }),
        }
    }
}

/// The net node-DML effect of one table's visible commit chain.
#[derive(Debug, Default, PartialEq)]
pub struct NodeDmlEffects<'a> {
    /// Newest full replacement row for each updated primary key.
    pub updates: BTreeMap<PkKey<'a>, &'a [Value]>,
    /// Primary keys whose newest operation is a delete.
    pub tombstones: BTreeSet<PkKey<'a>>,
}

impl Drop for CommitSummary {
    fn drop(&mut self) {
        self.release_charge();
    }
}

/// An iterator over commit links in oldest-first overlay read order.
pub struct CommitChainIter<'a> {
    newest_to_oldest: Vec<&'a CommitLink>,
}

impl<'a> Iterator for CommitChainIter<'a> {
    type Item = &'a CommitLink;

    fn next(&mut self) -> Option<Self::Item> {
        self.newest_to_oldest.pop()
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = self.newest_to_oldest.len();
        (remaining, Some(remaining))
    }
}

impl ExactSizeIterator for CommitChainIter<'_> {}
impl FusedIterator for CommitChainIter<'_> {}

/// Collects a chain once and iterates its links from oldest to newest.
pub fn commit_links_oldest_first(head: Option<&Arc<CommitLink>>) -> CommitChainIter<'_> {
    let mut newest_to_oldest = Vec::new();
    let mut current = head.map(Arc::as_ref);
    while let Some(link) = current {
        newest_to_oldest.push(link);
        current = link.prev.as_ref().map(Arc::as_ref);
    }
    CommitChainIter { newest_to_oldest }
}

/// Computes one table's net DML effects with commit links applied oldest-first.
///
/// Inserts clear an older tombstone or replacement, updates replace the prior
/// update for their key, and deletes remove a replacement and leave a
/// tombstone. The chain is walked exactly once.
pub fn node_dml_effects<'a>(
    head: Option<&'a Arc<CommitLink>>,
    table: &'a str,
    primary_key_column: usize,
) -> DevonResult<NodeDmlEffects<'a>> {
    let mut effects = NodeDmlEffects::default();
    for link in commit_links_oldest_first(head) {
        for row in table_rows(&link.delta.node_updates, table) {
            let key = row_pk(row, primary_key_column, table)?;
            effects.tombstones.remove(&key);
            effects.updates.insert(key, row);
        }
        for value in table_values(&link.delta.node_deletes, table) {
            let key = PkKey::try_from(value)?;
            effects.updates.remove(&key);
            effects.tombstones.insert(key);
        }
        for row in table_rows(&link.delta.nodes, table) {
            let key = row_pk(row, primary_key_column, table)?;
            effects.updates.remove(&key);
            effects.tombstones.remove(&key);
        }
    }
    Ok(effects)
}

/// Iterates one node table's effective overlay rows in visible insert order.
///
/// An update of an overlay row preserves its insert position; a delete drops
/// that position; and a later insert appends a newly revived position. Thus
/// This compatibility view does not expose physical positions. Offset-sensitive
/// consumers must use [`node_slots_oldest_first`].
pub fn node_rows_oldest_first<'a>(
    head: Option<&'a Arc<CommitLink>>,
    table: &'a str,
    primary_key_column: usize,
) -> impl Iterator<Item = &'a [Value]> + 'a {
    node_slots_oldest_first(head, table, primary_key_column).filter_map(|slot| slot.row)
}

/// Iterates every assigned overlay insert slot in physical position order.
///
/// Deleted rows remain `None` holes, while reinserts append after the full
/// span. This is the binding pre-checkpoint offset view for relationship
/// endpoint resolution (`docs/DETACH_DELETE.md` § Merged read view).
pub fn node_slots_oldest_first<'a>(
    head: Option<&'a Arc<CommitLink>>,
    table: &'a str,
    primary_key_column: usize,
) -> impl Iterator<Item = OverlayNodeSlot<'a>> + 'a {
    effective_node_rows(head, table, primary_key_column)
        .into_iter()
        .enumerate()
        .map(|(position, row)| OverlayNodeSlot { position, row })
}

fn effective_node_rows<'a>(
    head: Option<&'a Arc<CommitLink>>,
    table: &'a str,
    primary_key_column: usize,
) -> Vec<Option<&'a [Value]>> {
    let mut rows = Vec::new();
    let mut active = BTreeMap::new();
    for link in commit_links_oldest_first(head) {
        apply_node_updates(
            &mut rows,
            &active,
            table_rows(&link.delta.node_updates, table),
            primary_key_column,
        );
        apply_node_deletes(
            &mut rows,
            &mut active,
            table_values(&link.delta.node_deletes, table),
        );
        // Writes retain the delete of the old physical slot and surviving
        // reinserts separately; the new slot must be appended last.
        apply_node_inserts(
            &mut rows,
            &mut active,
            table_rows(&link.delta.nodes, table),
            primary_key_column,
        );
    }
    rows
}

fn apply_node_inserts<'a, I>(
    rows: &mut Vec<Option<&'a [Value]>>,
    active: &mut BTreeMap<PkKey<'a>, usize>,
    inserts: I,
    primary_key_column: usize,
) where
    I: Iterator<Item = &'a [Value]>,
{
    for row in inserts {
        if let Some(key) = optional_row_pk(row, primary_key_column)
            && let Some(previous) = active.insert(key, rows.len())
        {
            rows[previous] = None;
        }
        rows.push(Some(row));
    }
}

fn apply_node_updates<'a, I>(
    rows: &mut [Option<&'a [Value]>],
    active: &BTreeMap<PkKey<'a>, usize>,
    updates: I,
    primary_key_column: usize,
) where
    I: Iterator<Item = &'a [Value]>,
{
    for row in updates {
        let Some(key) = optional_row_pk(row, primary_key_column) else {
            continue;
        };
        if let Some(position) = active.get(&key) {
            rows[*position] = Some(row);
        }
    }
}

fn apply_node_deletes<'a, I>(
    rows: &mut [Option<&'a [Value]>],
    active: &mut BTreeMap<PkKey<'a>, usize>,
    deletes: I,
) where
    I: Iterator<Item = &'a Value>,
{
    for value in deletes {
        let Ok(key) = PkKey::try_from(value) else {
            continue;
        };
        if let Some(position) = active.remove(&key) {
            rows[position] = None;
        }
    }
}

fn optional_row_pk(row: &[Value], primary_key_column: usize) -> Option<PkKey<'_>> {
    row.get(primary_key_column)
        .and_then(|value| PkKey::try_from(value).ok())
}

fn row_pk<'a>(row: &'a [Value], primary_key_column: usize, table: &str) -> DevonResult<PkKey<'a>> {
    let value = row
        .get(primary_key_column)
        .ok_or_else(|| DevonError::Corrupt {
            context: format!(
                "overlay row for node table `{table}` has no primary-key column at index {primary_key_column}"
            ),
        })?;
    PkKey::try_from(value)
}

fn table_rows<'a>(
    rows: &'a BTreeMap<String, Vec<Vec<Value>>>,
    table: &str,
) -> impl Iterator<Item = &'a [Value]> {
    rows.get(table).into_iter().flatten().map(Vec::as_slice)
}

fn table_values<'a>(
    values: &'a BTreeMap<String, Vec<Value>>,
    table: &str,
) -> impl Iterator<Item = &'a Value> {
    values.get(table).into_iter().flatten()
}

fn value_kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "Null",
        Value::Bool(_) => "Bool",
        Value::Int64(_) => "Int64",
        Value::Float64(_) => "Float64",
        Value::String(_) => "String",
        Value::Vector(_) => "Vector",
        Value::GeoPoint(_) => "GeoPoint",
        Value::Timestamp(_) => "Timestamp",
        Value::Bytes(_) => "Bytes",
        Value::Decimal(_) => "Decimal",
        Value::Json(_) => "Json",
    }
}

/// Iterates one relationship table's edges across a chain in insertion order.
pub fn rel_edges_oldest_first<'a>(
    head: Option<&'a Arc<CommitLink>>,
    table: &'a str,
) -> impl Iterator<Item = &'a OverlayEdge> + 'a {
    commit_links_oldest_first(head)
        .flat_map(move |link| link.delta.edges.get(table).into_iter().flatten())
}

/// Unions one relationship table's tombstones across a chain oldest-first.
#[must_use]
pub fn rel_tombstones_oldest_first(
    head: Option<&Arc<CommitLink>>,
    table: &str,
) -> RelEndpointTombstones {
    let mut tombstones = RelEndpointTombstones::default();
    for link in commit_links_oldest_first(head) {
        if let Some(delta) = link.delta.rel_tombstones.get(table) {
            tombstones.union_with(delta);
        }
    }
    tombstones
}

/// Computes a row's stable global offset from its checkpoint and overlay positions.
pub fn node_offset(checkpointed_total: u64, overlay_position: usize) -> DevonResult<u64> {
    let overlay_position =
        u64::try_from(overlay_position).map_err(|_| DevonError::InvalidArgument {
            context: "overlay row position exceeds u64::MAX".to_owned(),
        })?;
    checkpointed_total
        .checked_add(overlay_position)
        .ok_or_else(|| DevonError::InvalidArgument {
            context: format!(
                "checkpointed row total {checkpointed_total} plus overlay position {overlay_position} exceeds u64::MAX"
            ),
        })
}

/// Returns summaries whose commit LSN is strictly newer than `minimum_snapshot_lsn`.
///
/// The input order is preserved and retained summaries share their existing
/// `Arc`s. Charges for pruned summaries are released explicitly and exactly
/// once, even when an older published state still shares their `Arc`s.
pub fn prune_recent_summaries(
    recent_summaries: &[(u64, Arc<CommitSummary>)],
    minimum_snapshot_lsn: u64,
) -> Vec<(u64, Arc<CommitSummary>)> {
    let mut retained = Vec::new();
    for (commit_lsn, summary) in recent_summaries {
        if *commit_lsn > minimum_snapshot_lsn {
            retained.push((*commit_lsn, Arc::clone(summary)));
        } else {
            summary.release_charge();
        }
    }
    retained
}

/// Releases every summary charge when no write transaction needs retention.
pub fn clear_recent_summaries(recent_summaries: &[(u64, Arc<CommitSummary>)]) {
    for (_, summary) in recent_summaries {
        summary.release_charge();
    }
}

fn estimate_node_rows(
    total: &mut usize,
    nodes: &BTreeMap<String, Vec<Vec<Value>>>,
) -> DevonResult<()> {
    for (table, rows) in nodes {
        checked_estimate_accumulate(total, MAP_ENTRY_OVERHEAD_BYTES)?;
        checked_estimate_accumulate(total, table.len())?;
        for row in rows {
            checked_estimate_accumulate(total, ROW_OVERHEAD_BYTES)?;
            estimate_values(total, row)?;
        }
    }
    Ok(())
}

fn estimate_edges(
    total: &mut usize,
    edges: &BTreeMap<String, Vec<OverlayEdge>>,
) -> DevonResult<()> {
    for (table, table_edges) in edges {
        checked_estimate_accumulate(total, MAP_ENTRY_OVERHEAD_BYTES)?;
        checked_estimate_accumulate(total, table.len())?;
        for edge in table_edges {
            checked_estimate_accumulate(total, OVERLAY_EDGE_OVERHEAD_BYTES)?;
            estimate_values(total, &edge.values)?;
        }
    }
    Ok(())
}

fn estimate_rel_tombstones(
    total: &mut usize,
    tombstones: &BTreeMap<String, RelEndpointTombstones>,
) -> DevonResult<()> {
    for (table, table_tombstones) in tombstones {
        checked_estimate_accumulate(total, MAP_ENTRY_OVERHEAD_BYTES)?;
        checked_estimate_accumulate(total, table.len())?;
        checked_estimate_accumulate(total, table_tombstones.estimated_bytes()?)?;
    }
    Ok(())
}

fn estimate_values(total: &mut usize, values: &[Value]) -> DevonResult<()> {
    for value in values {
        checked_estimate_accumulate(total, value.approx_bytes())?;
    }
    Ok(())
}

fn estimate_ddl(total: &mut usize, ddl: &DdlOp) -> DevonResult<()> {
    match ddl {
        DdlOp::CreateNodeTable(schema) => {
            checked_estimate_accumulate(total, schema.name().len())?;
            estimate_columns(total, schema.columns())
        }
        DdlOp::CreateRelTable(schema) => {
            checked_estimate_accumulate(total, schema.name().len())?;
            checked_estimate_accumulate(total, schema.from().len())?;
            checked_estimate_accumulate(total, schema.to().len())?;
            estimate_columns(total, schema.columns())
        }
    }
}

fn estimate_columns(total: &mut usize, columns: &[Column]) -> DevonResult<()> {
    for column in columns {
        checked_estimate_accumulate(total, SCHEMA_COLUMN_OVERHEAD_BYTES)?;
        checked_estimate_accumulate(total, column.name.len())?;
    }
    Ok(())
}

fn checked_estimate_accumulate(total: &mut usize, bytes: usize) -> DevonResult<()> {
    *total = checked_estimate_add(*total, bytes)?;
    Ok(())
}

fn checked_estimate_add(left: usize, right: usize) -> DevonResult<usize> {
    left.checked_add(right)
        .ok_or_else(|| DevonError::BudgetExceeded {
            context: "committed overlay byte estimate exceeds usize::MAX".to_owned(),
        })
}

fn estimate_overflow() -> DevonError {
    DevonError::BudgetExceeded {
        context: "committed overlay byte estimate exceeds usize::MAX".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, sync::Arc};

    use devondb_types::{
        DevonError,
        logical_type::LogicalType,
        schema::{Column, NodeTableSchema},
        value::Value,
    };

    use super::{
        CommitDelta, CommitLink, CommitSummary, OverlayEdge, PkKey, PublishedState,
        commit_links_oldest_first, node_offset, prune_recent_summaries,
    };
    use crate::{
        budget::MemoryBudget,
        catalog::Catalog,
        txn_log::{CommittedGroup, CommittedRecord, DdlPayload, WalPayload},
    };

    fn delta(node_ids: &[i64], edges: &[(u64, u64, i64)]) -> CommitDelta {
        let nodes = BTreeMap::from([(
            "Person".to_owned(),
            node_ids.iter().map(|id| vec![Value::Int64(*id)]).collect(),
        )]);
        let edges = BTreeMap::from([(
            "Knows".to_owned(),
            edges
                .iter()
                .map(|(from, to, since)| OverlayEdge {
                    from: *from,
                    to: *to,
                    values: vec![Value::Int64(*since)],
                })
                .collect(),
        )]);
        CommitDelta {
            nodes,
            edges,
            ..CommitDelta::default()
        }
    }

    fn dml_delta(inserts: &[(i64, &str)], updates: &[(i64, &str)], deletes: &[i64]) -> CommitDelta {
        let mut delta = CommitDelta::default();
        if !inserts.is_empty() {
            delta.nodes.insert(
                "Person".to_owned(),
                inserts
                    .iter()
                    .map(|(id, name)| person_row(*id, name))
                    .collect(),
            );
        }
        if !updates.is_empty() {
            delta.node_updates.insert(
                "Person".to_owned(),
                updates
                    .iter()
                    .map(|(id, name)| person_row(*id, name))
                    .collect(),
            );
        }
        if !deletes.is_empty() {
            delta.node_deletes.insert(
                "Person".to_owned(),
                deletes.iter().copied().map(Value::Int64).collect(),
            );
        }
        delta
    }

    fn person_row(id: i64, name: &str) -> Vec<Value> {
        vec![Value::Int64(id), Value::String(name.to_owned())]
    }

    fn link(
        prev: Option<Arc<CommitLink>>,
        commit_lsn: u64,
        delta: CommitDelta,
        budget: &Arc<MemoryBudget>,
    ) -> Arc<CommitLink> {
        CommitLink::new_arc(prev, commit_lsn, delta, Arc::clone(budget)).unwrap()
    }

    fn state(chain: Option<Arc<CommitLink>>) -> PublishedState {
        let last_commit_lsn = chain.as_ref().map_or(0, |link| link.commit_lsn);
        let mut catalog = Catalog::default();
        catalog
            .add_node_table(
                NodeTableSchema::new(
                    "Person".to_owned(),
                    vec![
                        Column {
                            name: "id".to_owned(),
                            ty: LogicalType::Int64,
                            primary_key: true,
                        },
                        Column {
                            name: "name".to_owned(),
                            ty: LogicalType::String,
                            primary_key: false,
                        },
                    ],
                )
                .unwrap(),
            )
            .unwrap();
        PublishedState {
            catalog: Arc::new(catalog),
            chain,
            last_commit_lsn,
            catalog_generation: 0,
            recent_summaries: Vec::new(),
        }
    }

    #[test]
    fn overlay_read_order_matches_v0_across_three_links() {
        let budget = Arc::new(MemoryBudget::unlimited());
        let first = link(None, 10, delta(&[1, 2], &[(0, 1, 2001)]), &budget);
        let second = link(Some(first), 20, delta(&[3], &[(1, 2, 2002)]), &budget);
        let head = link(
            Some(second),
            30,
            delta(&[4, 5], &[(2, 3, 2003), (3, 4, 2004)]),
            &budget,
        );
        let state = state(Some(head));

        let lsns: Vec<_> = state
            .commit_links_oldest_first()
            .map(|link| link.commit_lsn)
            .collect();
        let rows: Vec<_> = state
            .node_rows("Person")
            .map(|row| row[0].clone())
            .collect();
        let edges: Vec<_> = state
            .rel_edges("Knows")
            .map(|edge| (edge.from, edge.to, edge.values[0].clone()))
            .collect();

        assert_eq!(lsns, [10, 20, 30]);
        assert_eq!(rows, [1, 2, 3, 4, 5].map(Value::Int64).as_slice());
        assert_eq!(
            edges,
            [
                (0, 1, Value::Int64(2001)),
                (1, 2, Value::Int64(2002)),
                (2, 3, Value::Int64(2003)),
                (3, 4, Value::Int64(2004)),
            ]
        );
    }

    #[test]
    fn overlay_insert_then_update_replaces_row_in_its_insert_position() {
        let budget = Arc::new(MemoryBudget::unlimited());
        let inserted = link(None, 10, dml_delta(&[(1, "old")], &[], &[]), &budget);
        let head = link(
            Some(inserted),
            20,
            dml_delta(&[], &[(1, "new")], &[]),
            &budget,
        );
        let state = state(Some(head));

        let rows: Vec<_> = state.node_rows("Person").map(<[Value]>::to_vec).collect();
        let effects = state.node_dml_effects("Person").unwrap();

        assert_eq!(rows, [person_row(1, "new")]);
        assert_eq!(effects.updates[&PkKey::Int64(1)], person_row(1, "new"));
        assert!(effects.tombstones.is_empty());
    }

    #[test]
    fn overlay_insert_then_delete_removes_the_effective_row() {
        let budget = Arc::new(MemoryBudget::unlimited());
        let inserted = link(None, 10, dml_delta(&[(1, "Ada")], &[], &[]), &budget);
        let head = link(Some(inserted), 20, dml_delta(&[], &[], &[1]), &budget);
        let state = state(Some(head));

        assert_eq!(state.node_rows("Person").count(), 0);
        let effects = state.node_dml_effects("Person").unwrap();
        assert!(effects.updates.is_empty());
        assert_eq!(effects.tombstones, [PkKey::Int64(1)].into_iter().collect());
    }

    #[test]
    fn overlay_persisted_pk_update_is_an_effect_not_an_overlay_row() {
        let budget = Arc::new(MemoryBudget::unlimited());
        let head = link(
            None,
            10,
            dml_delta(&[], &[(7, "replacement")], &[]),
            &budget,
        );
        let state = state(Some(head));

        assert_eq!(state.node_rows("Person").count(), 0);
        let effects = state.node_dml_effects("Person").unwrap();
        assert_eq!(
            effects.updates[&PkKey::Int64(7)],
            person_row(7, "replacement")
        );
    }

    #[test]
    fn overlay_insert_after_delete_revives_the_key_at_a_new_position() {
        let budget = Arc::new(MemoryBudget::unlimited());
        let inserted = link(None, 10, dml_delta(&[(1, "old")], &[], &[]), &budget);
        let deleted = link(Some(inserted), 20, dml_delta(&[], &[], &[1]), &budget);
        let head = link(
            Some(deleted),
            30,
            dml_delta(&[(1, "revived")], &[], &[]),
            &budget,
        );
        let state = state(Some(head));

        let rows: Vec<_> = state.node_rows("Person").map(<[Value]>::to_vec).collect();
        let effects = state.node_dml_effects("Person").unwrap();

        assert_eq!(rows, [person_row(1, "revived")]);
        assert!(effects.updates.is_empty());
        assert!(effects.tombstones.is_empty());
    }

    #[test]
    fn overlay_newest_update_wins_across_commit_links() {
        let budget = Arc::new(MemoryBudget::unlimited());
        let first = link(None, 10, dml_delta(&[], &[(7, "first")], &[]), &budget);
        let head = link(
            Some(first),
            20,
            dml_delta(&[], &[(7, "newest")], &[]),
            &budget,
        );
        let state = state(Some(head));

        let effects = state.node_dml_effects("Person").unwrap();

        assert_eq!(effects.updates.len(), 1);
        assert_eq!(effects.updates[&PkKey::Int64(7)], person_row(7, "newest"));
    }

    #[test]
    fn overlay_delete_of_updated_insert_leaves_only_a_tombstone() {
        let budget = Arc::new(MemoryBudget::unlimited());
        let inserted = link(None, 10, dml_delta(&[(1, "old")], &[], &[]), &budget);
        let updated = link(
            Some(inserted),
            20,
            dml_delta(&[], &[(1, "new")], &[]),
            &budget,
        );
        let head = link(Some(updated), 30, dml_delta(&[], &[], &[1]), &budget);
        let state = state(Some(head));

        assert_eq!(state.node_rows("Person").count(), 0);
        let effects = state.node_dml_effects("Person").unwrap();
        assert!(effects.updates.is_empty());
        assert!(effects.tombstones.contains(&PkKey::Int64(1)));
    }

    #[test]
    fn overlay_commit_delta_builds_directly_from_a_committed_wal_group() {
        let schema = NodeTableSchema::new(
            "Person".to_owned(),
            vec![Column {
                name: "id".to_owned(),
                ty: LogicalType::Int64,
                primary_key: true,
            }],
        )
        .unwrap();
        let payloads = vec![
            WalPayload::Ddl(DdlPayload::CreateNodeTable(schema.clone())),
            WalPayload::NodeInsert {
                table: "Person".to_owned(),
                row: vec![Value::Int64(7)],
            },
            WalPayload::RelInsert {
                rel: "Knows".to_owned(),
                from: 3,
                to: 7,
                values: vec![Value::Int64(2026)],
            },
        ];
        let group = CommittedGroup {
            commit_lsn: 44,
            records: payloads
                .into_iter()
                .map(|payload| CommittedRecord {
                    begin_lsn: 44,
                    payload,
                })
                .collect(),
        };

        let delta = CommitDelta::from_committed_group(group).unwrap();

        assert_eq!(delta.nodes["Person"], [vec![Value::Int64(7)]]);
        assert_eq!(
            delta.edges["Knows"],
            [OverlayEdge {
                from: 3,
                to: 7,
                values: vec![Value::Int64(2026)],
            }]
        );
        assert_eq!(delta.ddl, [super::DdlOp::CreateNodeTable(schema)]);
    }

    #[test]
    fn overlay_wal_payloads_build_mixed_insert_update_delete_maps() {
        let payloads = vec![
            WalPayload::NodeInsert {
                table: "Person".to_owned(),
                row: person_row(1, "inserted"),
            },
            WalPayload::NodeUpdate {
                table: "Company".to_owned(),
                row: vec![Value::String("devondb".to_owned())],
            },
            WalPayload::NodeUpdate {
                table: "Person".to_owned(),
                row: person_row(1, "first update"),
            },
            WalPayload::NodeUpdate {
                table: "Person".to_owned(),
                row: person_row(1, "second update"),
            },
            WalPayload::NodeDelete {
                table: "Person".to_owned(),
                key: Value::Int64(2),
            },
        ];

        let delta = CommitDelta::from_wal_payloads(payloads).unwrap();

        assert_eq!(delta.nodes["Person"], [person_row(1, "inserted")]);
        assert_eq!(
            delta
                .node_updates
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["Company", "Person"]
        );
        assert_eq!(
            delta.node_updates["Person"],
            [
                person_row(1, "first update"),
                person_row(1, "second update")
            ]
        );
        assert_eq!(delta.node_deletes["Person"], [Value::Int64(2)]);
    }

    #[test]
    fn overlay_wal_commit_payload_is_rejected_from_a_delta() {
        let error =
            CommitDelta::from_wal_payloads([WalPayload::Commit { records: 1 }]).unwrap_err();

        assert!(matches!(error, DevonError::Corrupt { .. }));
    }

    #[test]
    fn overlay_pk_key_rejects_float64_as_corrupt() {
        let error = PkKey::try_from(&Value::Float64(1.5)).unwrap_err();

        assert!(matches!(&error, DevonError::Corrupt { .. }));
        assert!(error.to_string().contains("Float64"));
    }

    #[test]
    fn overlay_offsets_are_stable_across_link_concatenation() {
        let budget = Arc::new(MemoryBudget::unlimited());
        let first = link(None, 10, delta(&[1, 2], &[]), &budget);
        let second = link(Some(first), 20, delta(&[3], &[]), &budget);
        let before = state(Some(Arc::clone(&second)));
        let before_offsets: Vec<_> = before
            .node_rows("Person")
            .enumerate()
            .map(|(position, _)| node_offset(7, position).unwrap())
            .collect();

        let third = link(Some(second), 30, delta(&[4, 5], &[]), &budget);
        let after = state(Some(third));
        let after_offsets: Vec<_> = after
            .node_rows("Person")
            .enumerate()
            .map(|(position, _)| node_offset(7, position).unwrap())
            .collect();

        assert_eq!(before_offsets, [7, 8, 9]);
        assert_eq!(&after_offsets[..before_offsets.len()], before_offsets);
        assert_eq!(after_offsets, [7, 8, 9, 10, 11]);
        assert!(node_offset(u64::MAX, 1).is_err());
    }

    #[test]
    fn overlay_summary_retention_prunes_at_strict_boundary() {
        let summary =
            |name: &str| Arc::new(CommitSummary::new(BTreeMap::new(), vec![name.to_owned()]));
        let summaries = vec![
            (10, summary("A")),
            (20, summary("B")),
            (21, summary("C")),
            (30, summary("D")),
        ];

        let retained = prune_recent_summaries(&summaries, 20);

        assert_eq!(
            retained.iter().map(|(lsn, _)| *lsn).collect::<Vec<_>>(),
            [21, 30]
        );
        assert!(Arc::ptr_eq(&retained[0].1, &summaries[2].1));
    }

    #[test]
    fn overlay_summary_estimate_covers_empty_and_pk_heavy_summaries() {
        let empty = CommitSummary::default();
        let heavy = CommitSummary::new(
            BTreeMap::from([(
                "Person".to_owned(),
                vec![Value::String("x".repeat(4096)); 8],
            )]),
            vec!["CreatedTable".to_owned()],
        );

        assert_eq!(
            empty.estimated_bytes().unwrap(),
            super::COMMIT_SUMMARY_OVERHEAD_BYTES
        );
        assert!(heavy.estimated_bytes().unwrap() > 8 * 4096);
    }

    #[test]
    fn overlay_summary_estimate_rejects_checked_arithmetic_overflow() {
        let summary = CommitSummary::new(BTreeMap::new(), vec!["T".to_owned()]);

        let error = summary.estimated_bytes_with_base(usize::MAX).unwrap_err();

        assert!(matches!(
            error,
            devondb_types::DevonError::BudgetExceeded { .. }
        ));
    }

    #[test]
    fn overlay_summary_charge_release_is_paired_and_double_release_safe() {
        let budget = Arc::new(MemoryBudget::new(16 * 1024));
        let raw = CommitSummary::new(
            BTreeMap::from([("Person".to_owned(), vec![Value::Int64(7)])]),
            Vec::new(),
        );
        let expected = raw.estimated_bytes().unwrap();
        let summary = CommitSummary::new_arc(raw, Arc::clone(&budget)).unwrap();

        assert_eq!(summary.charged_bytes(), expected);
        assert_eq!(budget.charged(), expected);
        summary.release_charge();
        assert_eq!(summary.charged_bytes(), 0);
        assert_eq!(budget.charged(), 0);
        summary.release_charge();
        drop(summary);
        assert_eq!(budget.charged(), 0);
    }

    #[test]
    fn overlay_budget_charge_and_release_are_symmetric() {
        let budget = Arc::new(MemoryBudget::new(16 * 1024));
        let delta = delta(&[1, 2], &[(0, 1, 2026)]);
        let expected = super::COMMIT_LINK_OVERHEAD_BYTES + delta.estimated_bytes().unwrap();

        let head = link(None, 10, delta, &budget);

        assert_eq!(head.charged_bytes, expected);
        assert_eq!(budget.charged(), expected);
        drop(head);
        assert_eq!(budget.charged(), 0);
    }

    #[test]
    fn overlay_dml_estimate_is_charged_and_released_symmetrically() {
        let budget = Arc::new(MemoryBudget::new(16 * 1024));
        let delta = dml_delta(&[], &[(7, "replacement")], &[8]);
        let base = CommitDelta::default().estimated_bytes().unwrap();
        let expected = super::COMMIT_LINK_OVERHEAD_BYTES + delta.estimated_bytes().unwrap();

        assert!(delta.estimated_bytes().unwrap() > base);
        let head = link(None, 10, delta, &budget);
        assert_eq!(head.charged_bytes, expected);
        assert_eq!(budget.charged(), expected);

        drop(head);
        assert_eq!(budget.charged(), 0);
    }

    #[test]
    fn overlay_budget_shared_chain_releases_only_unshared_links() {
        let budget = Arc::new(MemoryBudget::new(16 * 1024));
        let oldest = link(None, 10, delta(&[1], &[]), &budget);
        let oldest_charge = oldest.charged_bytes;
        let head = link(Some(Arc::clone(&oldest)), 20, delta(&[2], &[]), &budget);
        assert!(budget.charged() > oldest_charge);

        drop(head);

        assert_eq!(budget.charged(), oldest_charge);
        drop(oldest);
        assert_eq!(budget.charged(), 0);
    }

    #[test]
    fn overlay_deep_chain_drop_is_iterative() {
        let budget = Arc::new(MemoryBudget::unlimited());
        let mut head = None;
        for commit_lsn in 1..=100_000 {
            head = Some(link(head, commit_lsn, CommitDelta::default(), &budget));
        }
        assert_eq!(commit_links_oldest_first(head.as_ref()).len(), 100_000);

        drop(head);

        assert_eq!(budget.charged(), 0);
    }
}
