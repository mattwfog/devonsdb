//! Facade wiring for persistent HNSW lifecycle and ANN execution.

use std::{collections::VecDeque, mem::size_of, ops::Range, sync::Arc};

use devondb_exec::{
    chunk::{CHUNK_CAPACITY, Chunk, ChunkBuilder},
    hnsw::KnnScan as AnnKnnScan,
    source::ChunkSource,
};
use devondb_plan::expr::Metric;
use devondb_storage::{
    budget::MemoryBudget,
    catalog::{Catalog, IndexEntry, IndexKind},
    hnsw::{
        index::{
            ConstructionVector, ConstructionVectorAccess, HnswNodeGroups, InsertProposalOutcome,
            build_initial_index, load_persisted_index, propose_insert, publish_checkpoint,
            publish_checkpoint_with_catch_up,
        },
        scoring::ConstructionScorer,
        types::{
            GraphAccess, HnswConfig, HnswDelta, HnswMetric, NavigationEncoding, NavigationScorer,
            supports_index,
        },
        view::{HnswGroupLayout, HnswSnapshotView},
    },
    node_group::{NODE_GROUP_CAPACITY, NodeGroup},
    overlay::{CommitDelta, CommitLink, PublishedState},
    pager::Pager,
    vector_encoding::{decode_f16, decode_i8, encode_b1, encode_f16, encode_i8},
    wal::WalWriter,
};
use devondb_types::{
    DevonError, DevonResult,
    logical_type::{B1Rescore, LogicalType, VectorEncoding},
    schema::{NodeTableSchema, fold, suggestion_suffix},
    value::Value,
};

use crate::txn::{PendingHnswIndex, Transaction};

use super::{
    checkpoint::checkpoint_locked,
    next_lsn,
    options::{CommitPipe, Shared},
    truncate_wal,
    view::ReadView,
};

const INITIAL_BUILD_BATCH_ROWS: usize = 64;
const NODE_GROUP_DIRECTORY_HEADER_LEN: usize = 16;
const NODE_GROUP_DIRECTORY_ENTRY_LEN: usize = 16;
const CONSTRUCTION_ALLOCATION_OVERHEAD: usize = 64;
const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
const DEFAULT_EF_SEARCH: u64 = 128;

pub(super) fn prepare_index_build(
    transaction: &Transaction,
    name: &str,
    table: &str,
    column: &str,
    metric: Metric,
) -> DevonResult<PendingHnswIndex> {
    if !transaction.writes.is_empty() {
        return Err(invalid_argument(
            "CREATE HNSW INDEX must be the only statement in its transaction",
        ));
    }
    let schema = require_index_column(&transaction.catalog, table, column)?.0;
    ensure_index_available(&transaction.catalog, name, table, column)?;
    let column_type = schema
        .column_index(column)
        .and_then(|index| schema.columns().get(index))
        .map(|candidate| candidate.ty)
        .ok_or_else(|| DevonError::NotFound {
            what: format!("column `{table}.{column}`"),
        })?;
    let metric = storage_metric(metric);
    if !supports_index(&column_type, metric) {
        return Err(invalid_argument(format!(
            "column `{table}.{column}` with type {column_type} does not support HNSW {metric:?}"
        )));
    }
    let navigation = NavigationEncoding::of_column(&column_type)
        .ok_or_else(|| invalid_argument("HNSW indexes require a vector column"))?;
    let config = HnswConfig::with_defaults(level_seed(transaction, name), metric, navigation);
    Ok(PendingHnswIndex {
        name: name.to_owned(),
        table: table.to_owned(),
        column: column.to_owned(),
        schema,
        config,
    })
}

pub(super) fn publish_index_build(
    shared: &Arc<Shared>,
    pipe: &mut CommitPipe,
    pending: &PendingHnswIndex,
) -> DevonResult<()> {
    validate_build_identity(&shared.current_state().catalog, pending)?;
    checkpoint_locked(shared, pipe)?;
    let current = shared.current_state();
    validate_build_identity(&current.catalog, pending)?;

    let mut catalog = (*current.catalog).clone();
    let index = build_materialized_index(shared, &catalog, pending)?;
    catalog.add_index(IndexEntry {
        name: pending.name.clone(),
        kind: IndexKind::Hnsw,
        table: pending.table.clone(),
        column: pending.column.clone(),
        root: index.root_page_id,
    })?;
    let publish_lsn = next_lsn(&shared.pager)?;
    catalog.save(&shared.pager, publish_lsn)?;
    rotate_empty_wal(shared, pipe)?;
    shared.publish(Arc::new(PublishedState {
        catalog: Arc::new(catalog),
        chain: None,
        last_commit_lsn: publish_lsn,
        catalog_generation: publish_lsn,
        recent_summaries: current.recent_summaries.clone(),
    }));
    Ok(())
}

pub(super) fn propose_commit_deltas(
    shared: &Arc<Shared>,
    current: &Arc<PublishedState>,
    catalog: &Catalog,
    delta: &mut CommitDelta,
) -> DevonResult<()> {
    let indexes = catalog.indexes().to_vec();
    for entry in indexes {
        let Some(new_rows) = folded_map_get(&delta.nodes, &entry.table) else {
            continue;
        };
        if new_rows.is_empty() {
            continue;
        }
        let (schema, column_type) = require_index_column(catalog, &entry.table, &entry.column)?;
        let base = load_persisted_index(&shared.pager, entry.root)?;
        validate_root_config(&base.root.config, column_type, &entry.name)?;
        if table_has_mutation_history(current, Some(delta), &entry.table) {
            continue;
        }
        let proposal = propose_one_commit_index(
            shared,
            current,
            catalog,
            delta,
            &entry.name,
            &schema,
            &entry.column,
            &base,
        );
        match proposal {
            Ok(Some(index_delta)) => {
                delta.hnsw.insert(entry.name, index_delta);
            }
            Ok(None) | Err(DevonError::BudgetExceeded { .. }) => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn propose_one_commit_index(
    shared: &Arc<Shared>,
    current: &Arc<PublishedState>,
    catalog: &Catalog,
    delta: &CommitDelta,
    index_name: &str,
    schema: &NodeTableSchema,
    column: &str,
    base: &devondb_storage::hnsw::index::PersistedHnswIndex,
) -> DevonResult<Option<HnswDelta>> {
    let (vectors, first_new_row) = PageBackedConstructionVectors::from_commit(
        shared, current, catalog, delta, schema, column, index_name,
    )?;
    let end = vectors.row_count();
    if first_new_row == end {
        return Ok(None);
    }
    let deltas = visible_index_deltas(current, index_name);
    let groups = HnswGroupLayout::new(persisted_group_counts(shared, catalog, schema)?)?;
    let verified_capacity = verified_cell_capacity(&base.root);
    let graph = HnswSnapshotView::new(
        &shared.pager,
        &shared.budget,
        base.root,
        base.directory.clone(),
        &deltas,
        first_new_row,
        groups,
        verified_capacity,
    )?;
    match propose_insert(
        &graph,
        &base.root.config,
        first_new_row..end,
        &vectors,
        &shared.budget,
    )? {
        InsertProposalOutcome::Proposed(proposal) => {
            let (delta, charge) = proposal.into_parts();
            drop(charge);
            Ok(Some(delta))
        }
        InsertProposalOutcome::TailExists | InsertProposalOutcome::BudgetExceeded => Ok(None),
    }
}

pub(super) fn checkpoint_indexes(
    shared: &Shared,
    state: &PublishedState,
    catalog: &mut Catalog,
) -> DevonResult<()> {
    let indexes = catalog.indexes().to_vec();
    for entry in indexes {
        let (schema, column_type) = require_index_column(catalog, &entry.table, &entry.column)?;
        let base = load_persisted_index(&shared.pager, entry.root)?;
        validate_root_config(&base.root.config, column_type, &entry.name)?;
        if table_has_mutation_history(state, None, &entry.table) {
            rebuild_mutated_index(shared, catalog, &entry, &schema, &base.root.config)?;
            continue;
        }
        let deltas = visible_index_deltas(state, &entry.name);
        let groups = HnswNodeGroups::new(persisted_group_counts(shared, catalog, &schema)?)?;
        let publication = match PageBackedConstructionVectors::from_materialized(
            shared,
            catalog,
            &schema,
            &entry.column,
            &entry.name,
        ) {
            Ok(vectors) => publish_checkpoint_with_catch_up(
                &shared.pager,
                &shared.budget,
                &base.root.config,
                Some(entry.root),
                &deltas,
                &groups,
                groups.total_rows(),
                &vectors,
            ),
            Err(DevonError::BudgetExceeded { .. }) => publish_checkpoint(
                &shared.pager,
                &shared.budget,
                &base.root.config,
                Some(entry.root),
                &deltas,
                &groups,
            ),
            Err(error) => return Err(error),
        }?;
        catalog.set_index_root(&entry.name, publication.index.root_page_id)?;
    }
    Ok(())
}

/// Rebuilds every HNSW index on `schema`'s table against the prospective
/// catalog `C1` inside the COPY fence (`docs/INDEX_BULK_LOAD.md` steps 6-9).
/// For each affected index in catalog order a full-coverage
/// replacement root is constructed through the page-backed accessor and
/// installed into `catalog`; nothing is saved here — the caller's single
/// `catalog.save`/superblock publication is the only visibility edge.
/// Partial coverage is an error, never a publishable state (invariants
/// 5, 7, 8): a budget stop surfaces as the measured `BudgetExceeded`.
pub(super) fn copy_rebuild_indexes(
    shared: &Shared,
    state: &PublishedState,
    schema: &NodeTableSchema,
    catalog: &mut Catalog,
) -> DevonResult<()> {
    let affected = catalog
        .indexes()
        .iter()
        .filter(|entry| fold(&entry.table) == fold(schema.name()))
        .cloned()
        .collect::<Vec<_>>();
    if affected.is_empty() {
        return Ok(());
    }
    let expected_names = affected
        .iter()
        .map(|entry| entry.name.clone())
        .collect::<Vec<_>>();
    for entry in &affected {
        let (index_schema, column_type) =
            require_index_column(catalog, &entry.table, &entry.column)?;
        // Invariant 4: the existing root's persisted config (metric, M,
        // M0, ef_construction, navigation, level_seed) drives the build.
        let base = load_persisted_index(&shared.pager, entry.root)?;
        validate_root_config(&base.root.config, column_type, &entry.name)?;
        let groups = HnswNodeGroups::new(persisted_group_counts(shared, catalog, &index_schema)?)?;
        let prospective_rows = groups.total_rows();
        let vectors = PageBackedConstructionVectors::from_materialized(
            shared,
            catalog,
            &index_schema,
            &entry.column,
            &entry.name,
        )?;
        let index = if base.root.covered_rows == 0 {
            // The doc's preferred batched driver: from-scratch adaptive
            // batches are byte-equivalent to catch-up from coverage zero
            // (same seed, same offsets-ascending insertion order).
            build_initial_index_adaptive(
                &shared.pager,
                &shared.budget,
                &base.root.config,
                &groups,
                prospective_rows,
                &vectors,
                &entry.name,
            )?
        } else {
            publish_checkpoint_with_catch_up(
                &shared.pager,
                &shared.budget,
                &base.root.config,
                Some(entry.root),
                &visible_index_deltas(state, &entry.name),
                &groups,
                prospective_rows,
                &vectors,
            )?
            .index
        };
        drop(vectors);
        if index.root.covered_rows != prospective_rows {
            // Catch-up stops at its last contiguous success under budget
            // pressure; for COPY that is a refusal, never a partial result.
            let required = construction_headroom_bytes(&column_type)?;
            return Err(DevonError::BudgetExceeded {
                context: construction_budget_context(
                    &shared.budget,
                    &entry.name,
                    required,
                    prospective_rows,
                    &column_type,
                    1,
                    &format!(
                        "COPY catch-up stopped at coverage {} of {prospective_rows} prospective rows",
                        index.root.covered_rows
                    ),
                ),
            });
        }
        // Invariant 6: revalidate the BUILT root against the prospective
        // schema and encoding before it becomes installable.
        let built = load_persisted_index(&shared.pager, index.root_page_id)?;
        validate_root_config(&built.root.config, column_type, &entry.name)?;
        catalog.set_index_root(&entry.name, index.root_page_id)?;
    }
    // Step 9: the affected-index set cannot have changed under the held
    // commit pipe; a drift here is corruption, not a race to tolerate.
    let now = catalog
        .indexes()
        .iter()
        .filter(|entry| fold(&entry.table) == fold(schema.name()))
        .map(|entry| entry.name.clone())
        .collect::<Vec<_>>();
    if now != expected_names {
        return Err(corrupt(format!(
            "COPY captured HNSW indexes {expected_names:?} for table `{}` but the catalog now names {now:?}",
            schema.name()
        )));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(super) fn approximate_knn_source(
    view: &ReadView,
    schema: &NodeTableSchema,
    vector_column: usize,
    column: &str,
    query: &[f32],
    k: u64,
    metric: Metric,
    rows: Arc<Vec<Vec<Value>>>,
) -> DevonResult<Option<Box<dyn ChunkSource>>> {
    let Some(entry) = view.catalog.indexes().iter().find(|index| {
        fold(&index.table) == fold(schema.name()) && fold(&index.column) == fold(column)
    }) else {
        return Ok(None);
    };
    let base = load_persisted_index(&view.shared.pager, entry.root)?;
    let column_type = schema.columns()[vector_column].ty;
    validate_root_config(&base.root.config, column_type, &entry.name)?;
    if base.root.config.metric != storage_metric(metric)
        || column_type.vector_dim().map(|dim| dim as usize) != Some(query.len())
    {
        return Ok(None);
    }
    let row_count =
        u64::try_from(rows.len()).map_err(|_| corrupt("HNSW query row count exceeds u64::MAX"))?;
    let group_counts = persisted_group_counts(&view.shared, &view.catalog, schema)?;
    let deltas = view
        .hnsw
        .get(fold(&entry.name).as_ref())
        .map_or(&[][..], Vec::as_slice);
    let graph = HnswSnapshotView::new(
        &view.shared.pager,
        &view.shared.budget,
        base.root,
        base.directory,
        deltas,
        row_count,
        HnswGroupLayout::new(group_counts)?,
        search_verified_capacity(&base.root.config, &base.root, row_count, k)?,
    )?;
    let tail_start = usize::try_from(graph.covered_rows())
        .map_err(|_| corrupt("HNSW exact-tail offset exceeds usize"))?;
    if tail_start > rows.len() {
        return Err(corrupt("HNSW exact-tail offset exceeds visible rows"));
    }

    let scorer = ConstructionScorer::new(&base.root.config, &column_type, query)?;
    let navigation_rows = Arc::clone(&rows);
    let distance = Box::new(move |node| {
        let vector = vector_at(&navigation_rows, node, vector_column)?;
        let slot = encode_navigation_slot(vector, column_type)?;
        scorer.navigation_distance(&slot)
    });
    let rescore = if base.root.config.navigation == NavigationEncoding::B1 {
        let rescore_rows = Arc::clone(&rows);
        Some(
            Box::new(move |node| Ok(vector_at(&rescore_rows, node, vector_column)?.to_vec()))
                as Box<devondb_exec::hnsw::RescoreAccessor<'_>>,
        )
    } else {
        None
    };
    let types = scan_types(schema);
    let tail = Box::new(RowsSource::new(
        Arc::clone(&rows),
        types.clone(),
        tail_start..rows.len(),
    ));
    let full_rows = Arc::clone(&rows);
    let full_types = types.clone();
    let full_scan = Box::new(move || {
        Box::new(RowsSource::new(
            Arc::clone(&full_rows),
            full_types.clone(),
            0..full_rows.len(),
        )) as Box<dyn ChunkSource>
    });
    let mut source = AnnKnnScan::new(
        graph,
        base.root.config,
        distance,
        rescore,
        tail,
        full_scan,
        vector_column,
        query.to_vec(),
        k,
        metric,
        &view.shared.budget,
    );
    let mut chunks = VecDeque::new();
    while let Some(chunk) = source.next_chunk()? {
        chunks.push_back(chunk);
    }
    Ok(Some(Box::new(BufferedSource { chunks })))
}

fn search_verified_capacity(
    config: &HnswConfig,
    root: &devondb_storage::hnsw::format::HnswRoot,
    covered_rows: u64,
    k: u64,
) -> DevonResult<usize> {
    let mut width = k.max(DEFAULT_EF_SEARCH);
    if config.navigation == NavigationEncoding::B1 {
        width = width.max(
            k.checked_mul(3)
                .ok_or_else(|| invalid_argument("HNSW oversample width overflows u64"))?,
        );
    }
    let layer_zero = width
        .checked_mul(u64::from(config.m0) + 1)
        .ok_or_else(|| invalid_argument("HNSW search discovery bound overflows"))?;
    let upper = u64::from(root.layer_count)
        .checked_mul(u64::from(config.m) + 1)
        .ok_or_else(|| invalid_argument("HNSW search upper-layer bound overflows"))?;
    let visited = covered_rows.min(
        layer_zero
            .checked_add(upper)
            .ok_or_else(|| invalid_argument("HNSW search discovery bound overflows"))?,
    );
    let cells = u64::from(root.layer_count) * u64::from(root.group_count);
    usize::try_from(visited.min(cells))
        .map_err(|_| invalid_argument("HNSW verified-group capacity exceeds usize"))
}

fn scan_types(schema: &NodeTableSchema) -> Vec<LogicalType> {
    schema
        .columns()
        .iter()
        .map(|column| column.ty)
        .chain([LogicalType::Int64])
        .collect()
}

fn vector_at(rows: &[Vec<Value>], node: u64, column: usize) -> DevonResult<&[f32]> {
    let node = usize::try_from(node)
        .map_err(|_| invalid_argument("HNSW candidate offset exceeds usize"))?;
    match rows.get(node).and_then(|row| row.get(column)) {
        Some(Value::Vector(vector)) => Ok(vector),
        Some(Value::Null) => Err(corrupt(format!(
            "HNSW topology references null-vector row {node}"
        ))),
        Some(value) => Err(corrupt(format!(
            "HNSW candidate row {node} has unexpected vector value {value}"
        ))),
        None => Err(corrupt(format!(
            "HNSW candidate row {node} is outside the snapshot"
        ))),
    }
}

struct RowsSource {
    rows: Arc<Vec<Vec<Value>>>,
    types: Vec<LogicalType>,
    range: Range<usize>,
    next: usize,
}

impl RowsSource {
    fn new(rows: Arc<Vec<Vec<Value>>>, types: Vec<LogicalType>, range: Range<usize>) -> Self {
        let next = range.start;
        Self {
            rows,
            types,
            range,
            next,
        }
    }
}

impl ChunkSource for RowsSource {
    fn next_chunk(&mut self) -> DevonResult<Option<Chunk>> {
        if self.next >= self.range.end {
            return Ok(None);
        }
        let mut builder = ChunkBuilder::new(self.types.clone());
        let mut emitted = 0;
        while self.next < self.range.end && emitted < CHUNK_CAPACITY {
            let mut row = self
                .rows
                .get(self.next)
                .cloned()
                .ok_or_else(|| corrupt("HNSW row source range exceeds visible rows"))?;
            let offset = i64::try_from(self.next)
                .map_err(|_| corrupt("HNSW node offset cannot be represented as Int64"))?;
            row.push(Value::Int64(offset));
            builder.push_row(row)?;
            self.next += 1;
            emitted += 1;
        }
        Ok(Some(builder.finish()))
    }
}

struct BufferedSource {
    chunks: VecDeque<Chunk>,
}

impl ChunkSource for BufferedSource {
    fn next_chunk(&mut self) -> DevonResult<Option<Chunk>> {
        Ok(self.chunks.pop_front())
    }
}

fn visible_index_deltas(state: &PublishedState, index_name: &str) -> Vec<HnswDelta> {
    state
        .commit_links_oldest_first()
        .filter_map(|link| folded_map_get(&link.delta.hnsw, index_name).cloned())
        .collect()
}

fn validate_root_config(
    config: &HnswConfig,
    column_type: LogicalType,
    index_name: &str,
) -> DevonResult<()> {
    config.validate()?;
    if NavigationEncoding::of_column(&column_type) != Some(config.navigation) {
        return Err(corrupt(format!(
            "HNSW index `{index_name}` navigation encoding does not match its column"
        )));
    }
    if !supports_index(&column_type, config.metric) {
        return Err(corrupt(format!(
            "HNSW index `{index_name}` has an unsupported column/metric combination"
        )));
    }
    Ok(())
}

fn verified_cell_capacity(root: &devondb_storage::hnsw::format::HnswRoot) -> usize {
    usize::from(root.layer_count).saturating_mul(root.group_count as usize)
}

fn validate_build_identity(catalog: &Catalog, pending: &PendingHnswIndex) -> DevonResult<()> {
    let (schema, column_type) = require_index_column(catalog, &pending.table, &pending.column)?;
    if schema != pending.schema {
        return Err(DevonError::TransactionConflict {
            context: format!(
                "node table `{}` changed while HNSW index `{}` was building",
                pending.table, pending.name
            ),
        });
    }
    if NavigationEncoding::of_column(&column_type) != Some(pending.config.navigation)
        || !supports_index(&column_type, pending.config.metric)
    {
        return Err(DevonError::TransactionConflict {
            context: format!(
                "column `{}.{}` changed while HNSW index `{}` was building",
                pending.table, pending.column, pending.name
            ),
        });
    }
    if let Some(existing) = catalog.indexes().iter().find(|index| {
        fold(&index.name) == fold(&pending.name)
            || (fold(&index.table) == fold(&pending.table)
                && fold(&index.column) == fold(&pending.column))
    }) {
        return Err(DevonError::TransactionConflict {
            context: format!(
                "HNSW index `{}` conflicts with concurrently published index `{}`",
                pending.name, existing.name
            ),
        });
    }
    Ok(())
}

fn ensure_index_available(
    catalog: &Catalog,
    name: &str,
    table: &str,
    column: &str,
) -> DevonResult<()> {
    if catalog
        .indexes()
        .iter()
        .any(|index| fold(&index.name) == fold(name))
    {
        return Err(invalid_argument(format!(
            "index name `{name}` is already in the catalog"
        )));
    }
    if let Some(existing) = catalog
        .indexes()
        .iter()
        .find(|index| fold(&index.table) == fold(table) && fold(&index.column) == fold(column))
    {
        return Err(invalid_argument(format!(
            "column `{table}.{column}` is already indexed by `{}`",
            existing.name
        )));
    }
    Ok(())
}

fn require_index_column(
    catalog: &Catalog,
    table: &str,
    column: &str,
) -> DevonResult<(NodeTableSchema, LogicalType)> {
    let schema = catalog
        .node_table(table)
        .cloned()
        .ok_or_else(|| DevonError::NotFound {
            what: format!(
                "node table `{table}`{}",
                suggestion_suffix(
                    table,
                    catalog.node_tables().iter().map(|schema| schema.name())
                )
            ),
        })?;
    let column_type = schema
        .column_index(column)
        .and_then(|index| schema.columns().get(index))
        .map(|candidate| candidate.ty)
        .ok_or_else(|| DevonError::NotFound {
            what: format!(
                "column `{table}.{column}`{}",
                suggestion_suffix(
                    column,
                    schema
                        .columns()
                        .iter()
                        .map(|candidate| candidate.name.as_str())
                )
            ),
        })?;
    Ok((schema, column_type))
}

fn rotate_empty_wal(shared: &Shared, pipe: &mut CommitPipe) -> DevonResult<()> {
    pipe.wal = None;
    truncate_wal(&pipe.wal_path)?;
    pipe.wal = Some(WalWriter::open(&pipe.wal_path, next_lsn(&shared.pager)?)?);
    Ok(())
}

fn persisted_group_counts(
    shared: &Shared,
    catalog: &Catalog,
    schema: &NodeTableSchema,
) -> DevonResult<Vec<u64>> {
    catalog
        .table_storage(schema.name())
        .map_or(&[][..], |storage| storage.groups.as_slice())
        .iter()
        .map(|page_id| {
            NodeGroup::read_row_count(&shared.pager, *page_id, schema.columns().len()).and_then(
                |count| {
                    u64::try_from(count)
                        .map_err(|_| corrupt("node-group row count exceeds u64::MAX"))
                },
            )
        })
        .collect()
}

fn build_initial_index_adaptive<V>(
    pager: &Pager,
    budget: &MemoryBudget,
    config: &HnswConfig,
    groups: &HnswNodeGroups,
    row_count: u64,
    vectors: &V,
    index_name: &str,
) -> DevonResult<devondb_storage::hnsw::index::PersistedHnswIndex>
where
    V: ConstructionVectorAccess,
{
    let mut batch_rows = INITIAL_BUILD_BATCH_ROWS;
    loop {
        reclaim_construction_cache(pager, budget, vectors.column_type())?;
        match build_initial_index(
            pager, budget, config, groups, row_count, batch_rows, vectors,
        ) {
            Ok(index) => return Ok(index),
            Err(DevonError::BudgetExceeded { context: _ }) if batch_rows > 1 => {
                pager.shed_cache(budget.limit());
                batch_rows = (batch_rows / 2).max(1);
            }
            Err(DevonError::BudgetExceeded { context }) => {
                let required = construction_headroom_bytes(vectors.column_type())?;
                return Err(DevonError::BudgetExceeded {
                    context: construction_budget_context(
                        budget,
                        index_name,
                        required,
                        row_count,
                        vectors.column_type(),
                        batch_rows,
                        &context,
                    ),
                });
            }
            Err(error) => return Err(error),
        }
    }
}

/// Shared page-backed construction source for CREATE INDEX and bulk loaders.
///
/// Persisted vectors are decoded one row at a time. Overlay vectors remain
/// borrowed, and the accessor deliberately keeps no interior cache: the
/// storage trait takes `&self`, but caching would pin frames past one callback.
pub(super) struct PageBackedConstructionVectors<'source> {
    pager: &'source Pager,
    budget: &'source MemoryBudget,
    index_name: &'source str,
    table: &'source str,
    column_type: LogicalType,
    column_index: usize,
    primary_key_index: usize,
    groups: Vec<PersistedVectorGroup>,
    persisted_rows: u64,
    links: Vec<&'source CommitLink>,
    overlay_rows: u64,
    delta_rows: Option<&'source [Vec<Value>]>,
    row_count: u64,
    _metadata_charge: ConstructionCharge<'source>,
}

#[derive(Debug, Clone, Copy)]
struct PersistedVectorGroup {
    start: u64,
    row_count: usize,
    main: VectorPayloadEntry,
    rescore: Option<VectorPayloadEntry>,
}

#[derive(Debug, Clone, Copy)]
struct VectorPayloadEntry {
    first_page: u64,
    byte_len: usize,
    checksum: u32,
    slot_len: usize,
    row_count: usize,
}

struct ConstructionCharge<'budget> {
    budget: &'budget MemoryBudget,
    bytes: usize,
}

impl<'budget> ConstructionCharge<'budget> {
    fn new(
        budget: &'budget MemoryBudget,
        bytes: usize,
        context: impl FnOnce() -> String,
    ) -> DevonResult<Self> {
        if !budget.charge_or_reclaim(bytes) {
            return Err(DevonError::BudgetExceeded { context: context() });
        }
        Ok(Self { budget, bytes })
    }
}

impl Drop for ConstructionCharge<'_> {
    fn drop(&mut self) {
        self.budget.release(self.bytes);
    }
}

impl<'source> PageBackedConstructionVectors<'source> {
    fn from_commit(
        shared: &'source Shared,
        state: &'source PublishedState,
        catalog: &Catalog,
        delta: &'source CommitDelta,
        schema: &'source NodeTableSchema,
        column: &str,
        index_name: &'source str,
    ) -> DevonResult<(Self, u64)> {
        let vectors = Self::new(
            shared,
            Some(state),
            catalog,
            Some(delta),
            schema,
            column,
            index_name,
        )?;
        let first_new_row = vectors
            .persisted_rows
            .checked_add(vectors.overlay_rows)
            .ok_or_else(|| corrupt("HNSW table row count exceeds u64::MAX"))?;
        Ok((vectors, first_new_row))
    }

    pub(super) fn from_materialized(
        shared: &'source Shared,
        catalog: &Catalog,
        schema: &'source NodeTableSchema,
        column: &str,
        index_name: &'source str,
    ) -> DevonResult<Self> {
        Self::new(shared, None, catalog, None, schema, column, index_name)
    }

    #[allow(clippy::too_many_arguments)]
    fn new(
        shared: &'source Shared,
        state: Option<&'source PublishedState>,
        catalog: &Catalog,
        delta: Option<&'source CommitDelta>,
        schema: &'source NodeTableSchema,
        column: &str,
        index_name: &'source str,
    ) -> DevonResult<Self> {
        let column_index = schema
            .column_index(column)
            .ok_or_else(|| DevonError::NotFound {
                what: format!("column `{}.{column}`", schema.name()),
            })?;
        let primary_key_index = schema
            .columns()
            .iter()
            .position(|candidate| candidate.primary_key)
            .ok_or_else(|| corrupt(format!("node table `{}` has no primary key", schema.name())))?;
        let column_type = schema.columns()[column_index].ty;
        let group_ids = catalog
            .table_storage(schema.name())
            .map_or(&[][..], |storage| storage.groups.as_slice());
        let persisted_rows = persisted_rows(&shared.pager, group_ids, schema.columns().len())?;
        let link_count = state.map_or(Ok(0), count_commit_links)?;
        let raw_overlay_rows =
            state.map_or(Ok(0), |state| raw_overlay_row_count(state, schema.name()))?;
        let delta_rows = delta.and_then(|delta| folded_map_get(&delta.nodes, schema.name()));
        let raw_delta_rows = delta_rows.map_or(0, Vec::len);
        let raw_overlay_rows = u64::try_from(raw_overlay_rows)
            .map_err(|_| corrupt("HNSW overlay row count exceeds u64::MAX"))?;
        let raw_delta_rows = u64::try_from(raw_delta_rows)
            .map_err(|_| corrupt("HNSW delta row count exceeds u64::MAX"))?;
        let prospective_rows = persisted_rows
            .checked_add(raw_overlay_rows)
            .and_then(|rows| rows.checked_add(raw_delta_rows))
            .ok_or_else(|| corrupt("HNSW table row count exceeds u64::MAX"))?;
        let metadata_bytes =
            construction_metadata_bytes(group_ids.len(), link_count, schema.columns().len())?;
        let metadata_charge = ConstructionCharge::new(&shared.budget, metadata_bytes, || {
            construction_budget_context(
                &shared.budget,
                index_name,
                metadata_bytes,
                prospective_rows,
                &column_type,
                INITIAL_BUILD_BATCH_ROWS,
                "page-backed accessor metadata",
            )
        })?;

        let mut types = Vec::new();
        reserve_exact(&mut types, schema.columns().len(), "HNSW column types")?;
        types.extend(schema.columns().iter().map(|candidate| candidate.ty));
        let links = collect_commit_links(state, link_count)?;
        let groups =
            load_vector_groups(&shared.pager, group_ids, &types, column_index, column_type)?;
        let mut vectors = Self {
            pager: &shared.pager,
            budget: &shared.budget,
            index_name,
            table: schema.name(),
            column_type,
            column_index,
            primary_key_index,
            groups,
            persisted_rows,
            links,
            overlay_rows: 0,
            delta_rows: delta_rows.map(Vec::as_slice),
            row_count: 0,
            _metadata_charge: metadata_charge,
        };
        vectors.overlay_rows = vectors.effective_overlay_row_count()?;
        let delta_count = vectors.delta_rows.map_or(0, <[Vec<Value>]>::len);
        vectors.row_count = vectors
            .persisted_rows
            .checked_add(vectors.overlay_rows)
            .and_then(|rows| rows.checked_add(u64::try_from(delta_count).ok()?))
            .ok_or_else(|| corrupt("HNSW table row count exceeds u64::MAX"))?;
        Ok(vectors)
    }

    pub(super) const fn row_count(&self) -> u64 {
        self.row_count
    }

    fn effective_overlay_row_count(&self) -> DevonResult<u64> {
        if !self.has_overlay_dml() {
            return self.links.iter().try_fold(0_u64, |total, link| {
                let count = folded_map_get(&link.delta.nodes, self.table).map_or(0, Vec::len);
                total
                    .checked_add(
                        u64::try_from(count)
                            .map_err(|_| corrupt("HNSW overlay row count exceeds u64::MAX"))?,
                    )
                    .ok_or_else(|| corrupt("HNSW overlay row count exceeds u64::MAX"))
            });
        }
        let mut count = 0_u64;
        for (link_index, link) in self.links.iter().enumerate() {
            let Some(rows) = folded_map_get(&link.delta.nodes, self.table) else {
                continue;
            };
            for row_index in 0..rows.len() {
                if self
                    .resolve_overlay_insert(link_index, row_index)?
                    .is_some()
                {
                    count = count
                        .checked_add(1)
                        .ok_or_else(|| corrupt("HNSW overlay row count exceeds u64::MAX"))?;
                }
            }
        }
        Ok(count)
    }

    fn has_overlay_dml(&self) -> bool {
        self.links.iter().any(|link| {
            folded_map_get(&link.delta.node_updates, self.table)
                .is_some_and(|rows| !rows.is_empty())
                || folded_map_get(&link.delta.node_deletes, self.table)
                    .is_some_and(|rows| !rows.is_empty())
        })
    }

    fn overlay_row(&self, target: u64) -> DevonResult<&[Value]> {
        if !self.has_overlay_dml() {
            let mut remaining = target;
            for link in &self.links {
                let Some(rows) = folded_map_get(&link.delta.nodes, self.table) else {
                    continue;
                };
                let count = u64::try_from(rows.len())
                    .map_err(|_| corrupt("HNSW overlay row count exceeds u64::MAX"))?;
                if remaining < count {
                    let index = usize::try_from(remaining)
                        .map_err(|_| corrupt("HNSW overlay row index exceeds usize"))?;
                    return rows
                        .get(index)
                        .map(Vec::as_slice)
                        .ok_or_else(|| corrupt("HNSW overlay row index is out of bounds"));
                }
                remaining -= count;
            }
            return Err(corrupt("HNSW overlay row index is out of bounds"));
        }

        let mut position = 0_u64;
        for (link_index, link) in self.links.iter().enumerate() {
            let Some(rows) = folded_map_get(&link.delta.nodes, self.table) else {
                continue;
            };
            for row_index in 0..rows.len() {
                if let Some(row) = self.resolve_overlay_insert(link_index, row_index)? {
                    if position == target {
                        return Ok(row);
                    }
                    position = position
                        .checked_add(1)
                        .ok_or_else(|| corrupt("HNSW overlay row position exceeds u64::MAX"))?;
                }
            }
        }
        Err(corrupt("HNSW overlay row index is out of bounds"))
    }

    fn resolve_overlay_insert(
        &self,
        link_index: usize,
        row_index: usize,
    ) -> DevonResult<Option<&[Value]>> {
        let rows = folded_map_get(&self.links[link_index].delta.nodes, self.table)
            .ok_or_else(|| corrupt("HNSW overlay insert table disappeared"))?;
        let inserted = rows
            .get(row_index)
            .map(Vec::as_slice)
            .ok_or_else(|| corrupt("HNSW overlay insert row is out of bounds"))?;
        let key = inserted
            .get(self.primary_key_index)
            .ok_or_else(|| corrupt("HNSW overlay row is missing its primary key"))?;
        let mut visible = inserted;
        for (later_index, link) in self.links[link_index..].iter().enumerate() {
            if let Some(inserts) = folded_map_get(&link.delta.nodes, self.table) {
                let start = if later_index == 0 { row_index + 1 } else { 0 };
                if inserts[start..]
                    .iter()
                    .any(|row| row.get(self.primary_key_index) == Some(key))
                {
                    return Ok(None);
                }
            }
            if let Some(updates) = folded_map_get(&link.delta.node_updates, self.table)
                && let Some(update) = updates
                    .iter()
                    .rev()
                    .find(|row| row.get(self.primary_key_index) == Some(key))
            {
                visible = update;
            }
            if folded_map_get(&link.delta.node_deletes, self.table)
                .is_some_and(|deletes| deletes.iter().any(|candidate| candidate == key))
            {
                return Ok(None);
            }
        }
        Ok(Some(visible))
    }

    fn value_at(&self, node: u64) -> DevonResult<ConstructionRow<'_>> {
        if node < self.persisted_rows {
            return Ok(ConstructionRow::Persisted(node));
        }
        let overlay_offset = node - self.persisted_rows;
        if overlay_offset < self.overlay_rows {
            return self
                .overlay_row(overlay_offset)
                .map(ConstructionRow::Borrowed);
        }
        let delta_offset = overlay_offset - self.overlay_rows;
        let delta_offset = usize::try_from(delta_offset)
            .map_err(|_| corrupt("HNSW delta row index exceeds usize"))?;
        self.delta_rows
            .and_then(|rows| rows.get(delta_offset))
            .map(Vec::as_slice)
            .map(ConstructionRow::Borrowed)
            .ok_or_else(|| corrupt(format!("HNSW vector row {node} is out of bounds")))
    }

    fn with_persisted_vector<R, F>(&self, node: u64, read: F) -> DevonResult<R>
    where
        F: FnOnce(Option<ConstructionVector<'_>>) -> DevonResult<R>,
    {
        let group_index = self.groups.partition_point(|group| group.start <= node) - 1;
        let group = self
            .groups
            .get(group_index)
            .ok_or_else(|| corrupt(format!("HNSW vector row {node} is out of bounds")))?;
        let row = usize::try_from(node - group.start)
            .map_err(|_| corrupt("HNSW node-group row index exceeds usize"))?;
        if row >= group.row_count {
            return Err(corrupt(format!("HNSW vector row {node} is out of bounds")));
        }
        if !read_validity(self.pager, group.main, row)? {
            ensure_null_slot_zero(self.pager, group.main, row)?;
            if let Some(rescore) = group.rescore {
                ensure_null_slot_zero(self.pager, rescore, row)?;
            }
            return read(None);
        }
        if let Some(rescore) = group.rescore
            && !read_validity(self.pager, rescore, row)?
        {
            return Err(corrupt(format!(
                "HNSW vector row {node} main/rescore validity disagrees"
            )));
        }

        let required = construction_access_bytes(&self.column_type)?;
        reclaim_construction_cache(self.pager, self.budget, &self.column_type)?;
        let _charge = ConstructionCharge::new(self.budget, required, || {
            construction_budget_context(
                self.budget,
                self.index_name,
                required,
                self.row_count,
                &self.column_type,
                INITIAL_BUILD_BATCH_ROWS,
                "one persisted construction vector",
            )
        })?;
        let navigation = read_slot(self.pager, group.main, row)?;
        let decoded = decode_construction_vector(
            self.pager,
            self.column_type,
            group.rescore,
            row,
            &navigation,
            self.column_index,
        )?;
        read(Some(ConstructionVector {
            decoded: &decoded,
            navigation_slot: &navigation,
        }))
    }

    fn with_borrowed_vector<R, F>(&self, row: &[Value], read: F) -> DevonResult<R>
    where
        F: FnOnce(Option<ConstructionVector<'_>>) -> DevonResult<R>,
    {
        let value = row
            .get(self.column_index)
            .ok_or_else(|| corrupt("HNSW row is missing its indexed column"))?;
        let Value::Vector(decoded) = value else {
            return match value {
                Value::Null => read(None),
                other => Err(corrupt(format!(
                    "HNSW vector column decoded as unexpected value {other}"
                ))),
            };
        };
        let required = navigation_slot_len(self.column_type)?
            .checked_add(CONSTRUCTION_ALLOCATION_OVERHEAD)
            .ok_or_else(|| corrupt("HNSW construction vector charge overflows"))?;
        reclaim_construction_cache(self.pager, self.budget, &self.column_type)?;
        let _charge = ConstructionCharge::new(self.budget, required, || {
            construction_budget_context(
                self.budget,
                self.index_name,
                required,
                self.row_count,
                &self.column_type,
                INITIAL_BUILD_BATCH_ROWS,
                "one overlay construction vector",
            )
        })?;
        let navigation = encode_navigation_slot(decoded, self.column_type)?;
        read(Some(ConstructionVector {
            decoded,
            navigation_slot: &navigation,
        }))
    }
}

enum ConstructionRow<'row> {
    Persisted(u64),
    Borrowed(&'row [Value]),
}

impl ConstructionVectorAccess for PageBackedConstructionVectors<'_> {
    fn column_type(&self) -> &LogicalType {
        &self.column_type
    }

    fn with_vector<R, F>(&self, node: u64, read: F) -> DevonResult<R>
    where
        F: FnOnce(Option<ConstructionVector<'_>>) -> DevonResult<R>,
    {
        match self.value_at(node)? {
            ConstructionRow::Persisted(node) => self.with_persisted_vector(node, read),
            ConstructionRow::Borrowed(row) => self.with_borrowed_vector(row, read),
        }
    }
}

fn persisted_rows(pager: &Pager, group_ids: &[u64], column_count: usize) -> DevonResult<u64> {
    group_ids.iter().try_fold(0_u64, |total, page_id| {
        let rows = NodeGroup::read_row_count(pager, *page_id, column_count)?;
        total
            .checked_add(
                u64::try_from(rows)
                    .map_err(|_| corrupt("node-group row count exceeds u64::MAX"))?,
            )
            .ok_or_else(|| corrupt("node-group row count exceeds u64::MAX"))
    })
}

fn count_commit_links(state: &PublishedState) -> DevonResult<usize> {
    let mut count = 0_usize;
    let mut current = state.chain.as_deref();
    while let Some(link) = current {
        count = count
            .checked_add(1)
            .ok_or_else(|| corrupt("HNSW commit-link count exceeds usize"))?;
        current = link.prev.as_deref();
    }
    Ok(count)
}

fn collect_commit_links(
    state: Option<&PublishedState>,
    capacity: usize,
) -> DevonResult<Vec<&CommitLink>> {
    let mut links = Vec::new();
    reserve_exact(&mut links, capacity, "HNSW commit-link index")?;
    let mut current = state.and_then(|state| state.chain.as_deref());
    while let Some(link) = current {
        links.push(link);
        current = link.prev.as_deref();
    }
    links.reverse();
    Ok(links)
}

fn raw_overlay_row_count(state: &PublishedState, table: &str) -> DevonResult<usize> {
    let mut count = 0_usize;
    let mut current = state.chain.as_deref();
    while let Some(link) = current {
        count = count
            .checked_add(folded_map_get(&link.delta.nodes, table).map_or(0, Vec::len))
            .ok_or_else(|| corrupt("HNSW overlay row count exceeds usize"))?;
        current = link.prev.as_deref();
    }
    Ok(count)
}

fn load_vector_groups(
    pager: &Pager,
    group_ids: &[u64],
    types: &[LogicalType],
    column_index: usize,
    column_type: LogicalType,
) -> DevonResult<Vec<PersistedVectorGroup>> {
    let mut groups = Vec::new();
    reserve_exact(&mut groups, group_ids.len(), "HNSW node-group index")?;
    let mut start = 0_u64;
    for (group_index, page_id) in group_ids.iter().enumerate() {
        let directory = NodeGroup::read_directory(pager, *page_id, types)?;
        if group_index + 1 < group_ids.len() && directory.row_count() < NODE_GROUP_CAPACITY {
            return Err(corrupt(format!(
                "node group {group_index} is partial but is not the table's last group"
            )));
        }
        let page = referenced_page(pager, *page_id, "node-group directory")?;
        let main = vector_entry(&page, column_index, directory.row_count(), column_type)?;
        let rescore = rescore_entry(
            &page,
            types,
            column_index,
            directory.row_count(),
            column_type,
        )?;
        let non_null = verify_vector_payload(pager, main, directory.row_count())?;
        if let Some(rescore) = rescore {
            verify_vector_payload(pager, rescore, directory.row_count())?;
            validate_rescore_validity(pager, main, rescore, directory.row_count())?;
        }
        if let Some(zone_maps) = directory.zone_maps() {
            let expected_nulls = directory.row_count().saturating_sub(non_null);
            if zone_maps[column_index].null_count as usize != expected_nulls {
                return Err(corrupt(format!(
                    "HNSW vector column {column_index} null count disagrees with zone map"
                )));
            }
        }
        groups.push(PersistedVectorGroup {
            start,
            row_count: directory.row_count(),
            main,
            rescore,
        });
        start = start
            .checked_add(
                u64::try_from(directory.row_count())
                    .map_err(|_| corrupt("node-group row count exceeds u64::MAX"))?,
            )
            .ok_or_else(|| corrupt("node-group row count exceeds u64::MAX"))?;
    }
    Ok(groups)
}

fn vector_entry(
    page: &[u8],
    entry_index: usize,
    row_count: usize,
    column_type: LogicalType,
) -> DevonResult<VectorPayloadEntry> {
    decode_vector_entry(
        page,
        entry_index,
        row_count,
        navigation_slot_len(column_type)?,
    )
}

fn rescore_entry(
    page: &[u8],
    types: &[LogicalType],
    column_index: usize,
    row_count: usize,
    column_type: LogicalType,
) -> DevonResult<Option<VectorPayloadEntry>> {
    let LogicalType::VectorEncoded {
        dim,
        encoding: VectorEncoding::B1 { rescore, .. },
    } = column_type
    else {
        return Ok(None);
    };
    let Some(slot_len) = rescore_slot_len(dim, rescore)? else {
        return Ok(None);
    };
    let preceding = types[..column_index]
        .iter()
        .filter(|logical_type| has_b1_rescore(**logical_type))
        .count();
    decode_vector_entry(page, types.len() + preceding, row_count, slot_len).map(Some)
}

fn decode_vector_entry(
    page: &[u8],
    entry_index: usize,
    row_count: usize,
    slot_len: usize,
) -> DevonResult<VectorPayloadEntry> {
    let offset = NODE_GROUP_DIRECTORY_HEADER_LEN
        .checked_add(
            entry_index
                .checked_mul(NODE_GROUP_DIRECTORY_ENTRY_LEN)
                .ok_or_else(|| corrupt("node-group entry offset overflows"))?,
        )
        .ok_or_else(|| corrupt("node-group entry offset overflows"))?;
    let end = offset
        .checked_add(NODE_GROUP_DIRECTORY_ENTRY_LEN)
        .ok_or_else(|| corrupt("node-group entry end overflows"))?;
    let entry = page
        .get(offset..end)
        .ok_or_else(|| corrupt("node-group vector entry exceeds directory page"))?;
    let byte_len = read_u32_at(entry, 8) as usize;
    let validity_len = row_count.div_ceil(8);
    let expected = row_count
        .checked_mul(slot_len)
        .and_then(|bytes| bytes.checked_add(validity_len))
        .ok_or_else(|| corrupt("node-group vector payload length overflows"))?;
    if byte_len != expected {
        return Err(corrupt(format!(
            "node-group vector payload length is {byte_len}, expected {expected}"
        )));
    }
    Ok(VectorPayloadEntry {
        first_page: read_u64_at(entry, 0),
        byte_len,
        checksum: read_u32_at(entry, 12),
        slot_len,
        row_count,
    })
}

fn verify_vector_payload(
    pager: &Pager,
    entry: VectorPayloadEntry,
    row_count: usize,
) -> DevonResult<usize> {
    let page_size = pager.superblock().page_size as usize;
    let page_count = entry.byte_len.div_ceil(page_size);
    let mut crc = !0_u32;
    let mut remaining = entry.byte_len;
    let validity_len = row_count.div_ceil(8);
    let mut validity_seen = 0_usize;
    let mut non_null = 0_usize;
    let mut last_validity = 0_u8;
    for page_index in 0..page_count {
        let page_id = entry
            .first_page
            .checked_add(
                u64::try_from(page_index)
                    .map_err(|_| corrupt("vector payload page index exceeds u64"))?,
            )
            .ok_or_else(|| corrupt("vector payload page run overflows"))?;
        let page = referenced_page(pager, page_id, "vector payload")?;
        let take = remaining.min(page_size);
        crc = crc32c_update(crc, &page[..take]);
        let validity_take = (validity_len - validity_seen).min(take);
        if validity_take > 0 {
            let validity = &page[..validity_take];
            non_null = non_null
                .checked_add(
                    validity
                        .iter()
                        .map(|byte| byte.count_ones() as usize)
                        .sum::<usize>(),
                )
                .ok_or_else(|| corrupt("vector validity count overflows"))?;
            last_validity = *validity.last().unwrap_or(&0);
            validity_seen += validity_take;
        }
        if take < page_size && page[take..].iter().any(|byte| *byte != 0) {
            return Err(corrupt("vector payload page padding is not zero"));
        }
        remaining -= take;
    }
    if !crc != entry.checksum {
        return Err(corrupt("vector payload CRC-32C does not match"));
    }
    if !row_count.is_multiple_of(8) {
        let used = row_count % 8;
        let mask = !((1_u16 << used) - 1) as u8;
        if last_validity & mask != 0 {
            return Err(corrupt("vector validity bitmap trailing bits are not zero"));
        }
    }
    Ok(non_null)
}

fn validate_rescore_validity(
    pager: &Pager,
    main: VectorPayloadEntry,
    rescore: VectorPayloadEntry,
    row_count: usize,
) -> DevonResult<()> {
    let validity_len = row_count.div_ceil(8);
    for offset in 0..validity_len {
        if read_payload_byte(pager, main, offset)? != read_payload_byte(pager, rescore, offset)? {
            return Err(corrupt("b1 main and rescore null positions disagree"));
        }
    }
    Ok(())
}

fn read_validity(pager: &Pager, entry: VectorPayloadEntry, row: usize) -> DevonResult<bool> {
    let byte = read_payload_byte(pager, entry, row / 8)?;
    Ok(byte & (1 << (row % 8)) != 0)
}

fn read_slot(pager: &Pager, entry: VectorPayloadEntry, row: usize) -> DevonResult<Vec<u8>> {
    read_payload_range(pager, entry, slot_offset(entry, row)?, entry.slot_len)
}

fn slot_offset(entry: VectorPayloadEntry, row: usize) -> DevonResult<usize> {
    if row >= entry.row_count {
        return Err(corrupt("vector payload row index is out of bounds"));
    }
    let offset = entry
        .byte_len
        .checked_sub(
            entry
                .slot_len
                .checked_mul(entry.row_count)
                .ok_or_else(|| corrupt("vector payload values length overflows"))?,
        )
        .and_then(|validity| {
            row.checked_mul(entry.slot_len)
                .and_then(|slot| validity.checked_add(slot))
        })
        .ok_or_else(|| corrupt("vector payload slot offset overflows"))?;
    Ok(offset)
}

fn ensure_null_slot_zero(pager: &Pager, entry: VectorPayloadEntry, row: usize) -> DevonResult<()> {
    let offset = slot_offset(entry, row)?;
    if !payload_range_is_zero(pager, entry, offset, entry.slot_len)? {
        return Err(corrupt(format!("vector row {row} null slot is not zero")));
    }
    Ok(())
}

fn read_payload_byte(pager: &Pager, entry: VectorPayloadEntry, offset: usize) -> DevonResult<u8> {
    if offset >= entry.byte_len {
        return Err(corrupt("vector payload byte exceeds its byte length"));
    }
    let page_size = pager.superblock().page_size as usize;
    let page_index = offset / page_size;
    let page_id = entry
        .first_page
        .checked_add(
            u64::try_from(page_index)
                .map_err(|_| corrupt("vector payload page index exceeds u64"))?,
        )
        .ok_or_else(|| corrupt("vector payload page run overflows"))?;
    let page = referenced_page(pager, page_id, "vector payload")?;
    Ok(page[offset % page_size])
}

fn payload_range_is_zero(
    pager: &Pager,
    entry: VectorPayloadEntry,
    offset: usize,
    len: usize,
) -> DevonResult<bool> {
    let end = payload_range_end(entry, offset, len)?;
    let page_size = pager.superblock().page_size as usize;
    let mut cursor = offset;
    while cursor < end {
        let page_index = cursor / page_size;
        let in_page = cursor % page_size;
        let page_id = entry
            .first_page
            .checked_add(
                u64::try_from(page_index)
                    .map_err(|_| corrupt("vector payload page index exceeds u64"))?,
            )
            .ok_or_else(|| corrupt("vector payload page run overflows"))?;
        let page = referenced_page(pager, page_id, "vector payload")?;
        let take = (end - cursor).min(page_size - in_page);
        if page[in_page..in_page + take].iter().any(|byte| *byte != 0) {
            return Ok(false);
        }
        cursor += take;
    }
    Ok(true)
}

fn read_payload_range(
    pager: &Pager,
    entry: VectorPayloadEntry,
    offset: usize,
    len: usize,
) -> DevonResult<Vec<u8>> {
    let end = payload_range_end(entry, offset, len)?;
    let page_size = pager.superblock().page_size as usize;
    let mut bytes = Vec::new();
    reserve_exact(&mut bytes, len, "HNSW vector slot")?;
    let mut cursor = offset;
    while cursor < end {
        let page_index = cursor / page_size;
        let in_page = cursor % page_size;
        let page_id = entry
            .first_page
            .checked_add(
                u64::try_from(page_index)
                    .map_err(|_| corrupt("vector payload page index exceeds u64"))?,
            )
            .ok_or_else(|| corrupt("vector payload page run overflows"))?;
        let page = referenced_page(pager, page_id, "vector payload")?;
        let take = (end - cursor).min(page_size - in_page);
        bytes.extend_from_slice(&page[in_page..in_page + take]);
        cursor += take;
    }
    Ok(bytes)
}

fn payload_range_end(entry: VectorPayloadEntry, offset: usize, len: usize) -> DevonResult<usize> {
    let end = offset
        .checked_add(len)
        .ok_or_else(|| corrupt("vector payload range overflows"))?;
    if end > entry.byte_len {
        return Err(corrupt("vector payload range exceeds its byte length"));
    }
    Ok(end)
}

fn decode_construction_vector(
    pager: &Pager,
    column_type: LogicalType,
    rescore: Option<VectorPayloadEntry>,
    row: usize,
    navigation: &[u8],
    column_index: usize,
) -> DevonResult<Vec<f32>> {
    match column_type {
        LogicalType::Vector { .. } => decode_f32_slot(navigation),
        LogicalType::VectorEncoded {
            encoding: VectorEncoding::F16,
            ..
        } => decode_f16(navigation).map_err(|error| {
            corrupt(format!(
                "column {column_index} row {row} f16 vector is invalid: {error}"
            ))
        }),
        LogicalType::VectorEncoded {
            encoding: VectorEncoding::I8,
            ..
        } => decode_i8(navigation).map_err(|error| {
            corrupt(format!(
                "column {column_index} row {row} i8 vector is invalid: {error}"
            ))
        }),
        LogicalType::VectorEncoded {
            encoding: VectorEncoding::B1 { rescore: kind, .. },
            ..
        } => {
            let entry = rescore.ok_or_else(|| {
                corrupt("b1 HNSW construction requires a persisted rescore payload")
            })?;
            let slot = read_slot(pager, entry, row)?;
            match kind {
                B1Rescore::F16 => decode_f16(&slot),
                B1Rescore::I8 => decode_i8(&slot),
                B1Rescore::F32 => return decode_f32_slot(&slot),
                B1Rescore::None => {
                    return Err(corrupt("b1 HNSW construction has no rescore payload"));
                }
            }
            .map_err(|error| {
                corrupt(format!(
                    "column {column_index} row {row} b1 rescore vector is invalid: {error}"
                ))
            })
        }
        other => Err(invalid_argument(format!(
            "HNSW construction requires a vector column, got {other}"
        ))),
    }
}

fn decode_f32_slot(slot: &[u8]) -> DevonResult<Vec<f32>> {
    if !slot.len().is_multiple_of(4) {
        return Err(corrupt("f32 vector slot length is not divisible by four"));
    }
    Ok(slot
        .as_chunks::<4>()
        .0
        .iter()
        .map(|bytes| f32::from_le_bytes(*bytes))
        .collect())
}

fn navigation_slot_len(column_type: LogicalType) -> DevonResult<usize> {
    let dimension = usize::try_from(
        column_type
            .vector_dim()
            .ok_or_else(|| invalid_argument("HNSW construction requires a vector column"))?,
    )
    .map_err(|_| invalid_argument("HNSW vector dimension exceeds usize"))?;
    match column_type {
        LogicalType::Vector { .. } => dimension.checked_mul(4),
        LogicalType::VectorEncoded {
            encoding: VectorEncoding::F16,
            ..
        } => dimension.checked_mul(2),
        LogicalType::VectorEncoded {
            encoding: VectorEncoding::I8,
            ..
        } => dimension.checked_add(8),
        LogicalType::VectorEncoded {
            encoding: VectorEncoding::B1 { .. },
            ..
        } => dimension.checked_add(7).map(|bits| bits / 8),
        _ => None,
    }
    .ok_or_else(|| invalid_argument("HNSW navigation slot length overflows"))
}

fn rescore_slot_len(dimension: u32, rescore: B1Rescore) -> DevonResult<Option<usize>> {
    let dimension = usize::try_from(dimension)
        .map_err(|_| invalid_argument("HNSW vector dimension exceeds usize"))?;
    let slot = match rescore {
        B1Rescore::None => return Ok(None),
        B1Rescore::F16 => dimension.checked_mul(2),
        B1Rescore::I8 => dimension.checked_add(8),
        B1Rescore::F32 => dimension.checked_mul(4),
    }
    .ok_or_else(|| invalid_argument("HNSW rescore slot length overflows"))?;
    Ok(Some(slot))
}

fn has_b1_rescore(column_type: LogicalType) -> bool {
    matches!(
        column_type,
        LogicalType::VectorEncoded {
            encoding: VectorEncoding::B1 {
                rescore: B1Rescore::F16 | B1Rescore::I8 | B1Rescore::F32,
                ..
            },
            ..
        }
    )
}

fn construction_access_bytes(column_type: &LogicalType) -> DevonResult<usize> {
    let dimension = usize::try_from(
        column_type
            .vector_dim()
            .ok_or_else(|| invalid_argument("HNSW construction requires a vector column"))?,
    )
    .map_err(|_| invalid_argument("HNSW vector dimension exceeds usize"))?;
    let navigation = navigation_slot_len(*column_type)?;
    let decoded = dimension
        .checked_mul(size_of::<f32>())
        .ok_or_else(|| invalid_argument("HNSW decoded vector size overflows"))?;
    let rescore = match *column_type {
        LogicalType::VectorEncoded {
            dim,
            encoding: VectorEncoding::B1 { rescore, .. },
        } => rescore_slot_len(dim, rescore)?.unwrap_or(0),
        _ => 0,
    };
    navigation
        .checked_add(decoded)
        .and_then(|bytes| bytes.checked_add(rescore))
        .and_then(|bytes| bytes.checked_add(3 * CONSTRUCTION_ALLOCATION_OVERHEAD))
        .ok_or_else(|| invalid_argument("HNSW construction access size overflows"))
}

fn construction_headroom_bytes(column_type: &LogicalType) -> DevonResult<usize> {
    let access = construction_access_bytes(column_type)?;
    let dimension = usize::try_from(column_type.vector_dim().unwrap_or(0))
        .map_err(|_| invalid_argument("HNSW vector dimension exceeds usize"))?;
    let scorer = dimension
        .checked_mul(64)
        .and_then(|bytes| bytes.checked_add(8 * CONSTRUCTION_ALLOCATION_OVERHEAD))
        .ok_or_else(|| invalid_argument("HNSW construction scorer size overflows"))?;
    access
        .checked_add(scorer)
        .and_then(|bytes| bytes.checked_add(128 * 1024))
        .ok_or_else(|| invalid_argument("HNSW construction headroom overflows"))
}

fn reclaim_construction_cache(
    pager: &Pager,
    budget: &MemoryBudget,
    column_type: &LogicalType,
) -> DevonResult<()> {
    let target = construction_headroom_bytes(column_type)?;
    let available = budget.limit().saturating_sub(budget.charged());
    if available < target {
        pager.shed_cache(target - available);
    }
    Ok(())
}

fn construction_metadata_bytes(
    group_count: usize,
    link_count: usize,
    type_count: usize,
) -> DevonResult<usize> {
    group_count
        .checked_mul(size_of::<PersistedVectorGroup>())
        .and_then(|bytes| bytes.checked_add(link_count.checked_mul(size_of::<&CommitLink>())?))
        .and_then(|bytes| bytes.checked_add(type_count.checked_mul(size_of::<LogicalType>())?))
        .and_then(|bytes| bytes.checked_add(3 * CONSTRUCTION_ALLOCATION_OVERHEAD))
        .ok_or_else(|| invalid_argument("HNSW construction metadata size overflows"))
}

fn construction_budget_context(
    budget: &MemoryBudget,
    index_name: &str,
    required: usize,
    rows: u64,
    column_type: &LogicalType,
    batch_rows: usize,
    dominant: &str,
) -> String {
    let charged = budget.charged();
    let available = budget.limit().saturating_sub(charged);
    format!(
        "HNSW index `{index_name}` construction required={required} available={available} charged={charged} limit={} rows={rows} dimension={} encoding={column_type} batch_size={batch_rows} dominant={dominant}",
        budget.limit(),
        column_type.vector_dim().unwrap_or(0),
    )
}

fn reserve_exact<T>(values: &mut Vec<T>, capacity: usize, category: &str) -> DevonResult<()> {
    values
        .try_reserve_exact(capacity)
        .map_err(|error| DevonError::BudgetExceeded {
            context: format!("{category} allocation of {capacity} elements failed: {error}"),
        })
}

fn referenced_page(
    pager: &Pager,
    page_id: u64,
    category: &str,
) -> DevonResult<devondb_storage::pager::PageRef> {
    match pager.read_page_ref(page_id) {
        Ok(page) => Ok(page),
        Err(DevonError::InvalidArgument { context }) => Err(corrupt(format!(
            "{category} references invalid page {page_id}: {context}"
        ))),
        Err(error) => Err(error),
    }
}

fn read_u32_at(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap_or([0; 4]))
}

fn read_u64_at(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap_or([0; 8]))
}

fn crc32c_update(mut crc: u32, bytes: &[u8]) -> u32 {
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0x82f6_3b78 & 0_u32.wrapping_sub(crc & 1));
        }
    }
    crc
}

fn encode_navigation_slot(vector: &[f32], column_type: LogicalType) -> DevonResult<Vec<u8>> {
    match column_type {
        LogicalType::Vector { .. } => Ok(vector
            .iter()
            .flat_map(|value| value.to_le_bytes())
            .collect()),
        LogicalType::VectorEncoded {
            encoding: VectorEncoding::F16,
            ..
        } => Ok(encode_f16(vector)),
        LogicalType::VectorEncoded {
            encoding: VectorEncoding::I8,
            ..
        } => Ok(encode_i8(vector)),
        LogicalType::VectorEncoded {
            encoding: VectorEncoding::B1 { rotation_seed, .. },
            ..
        } => Ok(encode_b1(vector, rotation_seed)),
        other => Err(invalid_argument(format!(
            "HNSW navigation requires a vector column, got {other}"
        ))),
    }
}

fn storage_metric(metric: Metric) -> HnswMetric {
    match metric {
        Metric::L2 => HnswMetric::L2,
        Metric::Cosine => HnswMetric::Cosine,
    }
}

fn level_seed(transaction: &Transaction, name: &str) -> u64 {
    let db_id = transaction.shared.pager.superblock().db_id;
    let low = u64::from_le_bytes(db_id[..8].try_into().unwrap_or([0; 8]));
    let high = u64::from_le_bytes(db_id[8..].try_into().unwrap_or([0; 8]));
    let name_hash = name.as_bytes().iter().fold(FNV_OFFSET_BASIS, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(FNV_PRIME)
    });
    low ^ high ^ name_hash
}

fn folded_map_get<'map, V>(
    map: &'map std::collections::BTreeMap<String, V>,
    name: &str,
) -> Option<&'map V> {
    let folded = fold(name);
    map.iter()
        .find(|(stored, _)| fold(stored) == folded)
        .map(|(_, value)| value)
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

fn build_materialized_index(
    shared: &Shared,
    catalog: &Catalog,
    pending: &PendingHnswIndex,
) -> DevonResult<devondb_storage::hnsw::index::PersistedHnswIndex> {
    let (schema, _) = require_index_column(catalog, &pending.table, &pending.column)?;
    let groups = HnswNodeGroups::new(persisted_group_counts(shared, catalog, &schema)?)?;
    let vectors = PageBackedConstructionVectors::from_materialized(
        shared,
        catalog,
        &schema,
        &pending.column,
        &pending.name,
    )?;
    build_initial_index_adaptive(
        &shared.pager,
        &shared.budget,
        &pending.config,
        &groups,
        groups.total_rows(),
        &vectors,
        &pending.name,
    )
}

/// Raw mutation history remains dirty even when delete/reinsert erases net effects.
pub(super) fn table_has_mutation_history(
    state: &PublishedState,
    own: Option<&CommitDelta>,
    table: &str,
) -> bool {
    if own.is_some_and(|delta| delta_mutates_table(delta, table)) {
        return true;
    }
    let mut link = state.chain.as_deref();
    while let Some(current) = link {
        if delta_mutates_table(&current.delta, table) {
            return true;
        }
        link = current.prev.as_deref();
    }
    false
}

fn delta_mutates_table(delta: &CommitDelta, table: &str) -> bool {
    folded_map_get(&delta.node_updates, table).is_some_and(|rows| !rows.is_empty())
        || folded_map_get(&delta.node_deletes, table).is_some_and(|rows| !rows.is_empty())
}

pub(super) fn delta_mutates_indexed_table(catalog: &Catalog, delta: &CommitDelta) -> bool {
    catalog
        .indexes()
        .iter()
        .any(|entry| delta_mutates_table(delta, &entry.table))
}

/// Validate matching derived metadata before any dirty/budget exact fallback.
pub(super) fn validate_query_index(
    view: &ReadView,
    schema: &NodeTableSchema,
    column: &str,
) -> DevonResult<()> {
    if let Some(entry) = view.catalog.indexes().iter().find(|entry| {
        fold(&entry.table) == fold(schema.name()) && fold(&entry.column) == fold(column)
    }) {
        let base = load_persisted_index(&view.shared.pager, entry.root)?;
        let (_, ty) = require_index_column(&view.catalog, schema.name(), column)?;
        validate_root_config(&base.root.config, ty, &entry.name)?;
        let rows =
            HnswNodeGroups::new(persisted_group_counts(&view.shared, &view.catalog, schema)?)?
                .total_rows();
        if base.root.covered_rows > rows {
            return Err(corrupt("HNSW base coverage exceeds checkpointed rows"));
        }
    }
    Ok(())
}

fn rebuild_mutated_index(
    shared: &Shared,
    catalog: &mut Catalog,
    entry: &IndexEntry,
    schema: &NodeTableSchema,
    config: &HnswConfig,
) -> DevonResult<()> {
    let groups = HnswNodeGroups::new(persisted_group_counts(shared, catalog, schema)?)?;
    let rebuilt = match PageBackedConstructionVectors::from_materialized(
        shared,
        catalog,
        schema,
        &entry.column,
        &entry.name,
    ) {
        Ok(vectors) => devondb_storage::hnsw::index::build_initial_index_partial(
            &shared.pager,
            &shared.budget,
            config,
            &groups,
            groups.total_rows(),
            INITIAL_BUILD_BATCH_ROWS,
            &vectors,
        ),
        Err(DevonError::BudgetExceeded { .. }) => {
            publish_checkpoint(&shared.pager, &shared.budget, config, None, &[], &groups)
                .map(|publication| publication.index)
        }
        Err(error) => return Err(error),
    }?;
    catalog.set_index_root(&entry.name, rebuilt.root_page_id)
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
        time::{SystemTime, UNIX_EPOCH},
    };

    use devondb_plan::{expr::Metric, statement::Statement};
    use devondb_types::{logical_type::LogicalType, schema::Column};

    use crate::database::Database;

    const PAGE_SIZE: u32 = 4096;
    static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

    struct TestDirectory {
        path: PathBuf,
    }

    impl TestDirectory {
        fn new() -> Self {
            let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let timestamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "devondb-hnsw-suggestion-test-{}-{timestamp}-{sequence}",
                std::process::id()
            ));
            fs::create_dir(&path).unwrap();
            Self { path }
        }

        fn database(&self, name: &str) -> PathBuf {
            self.path.join(name)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn open_database(directory: &TestDirectory, name: &str) -> Database {
        let mut database = Database::create(directory.database(name), PAGE_SIZE).unwrap();
        database
            .execute(&Statement::CreateNodeTable {
                name: "Document".to_owned(),
                columns: vec![
                    Column {
                        name: "id".to_owned(),
                        ty: LogicalType::Int64,
                        primary_key: true,
                    },
                    Column {
                        name: "embedding".to_owned(),
                        ty: LogicalType::Vector { dim: 2 },
                        primary_key: false,
                    },
                ],
            })
            .unwrap();
        database
    }

    #[test]
    fn hnsw_table_name_did_you_mean() {
        let directory = TestDirectory::new();
        let mut database = open_database(&directory, "table.devondb");

        let error = database
            .execute(&Statement::CreateHnswIndex {
                name: "document_embedding".to_owned(),
                table: "Documnt".to_owned(),
                column: "embedding".to_owned(),
                metric: Metric::L2,
            })
            .unwrap_err();

        assert!(error.to_string().ends_with(" (did you mean `Document`?)"));
    }

    #[test]
    fn hnsw_column_name_did_you_mean() {
        let directory = TestDirectory::new();
        let mut database = open_database(&directory, "column.devondb");

        let error = database
            .execute(&Statement::CreateHnswIndex {
                name: "document_embedding".to_owned(),
                table: "Document".to_owned(),
                column: "embeding".to_owned(),
                metric: Metric::L2,
            })
            .unwrap_err();

        assert!(error.to_string().ends_with(" (did you mean `embedding`?)"));
    }
}
