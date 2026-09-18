#[cfg(feature = "fts")]
#[path = "fulltext.rs"]
mod fulltext;

#[path = "expand_query.rs"]
mod expand_query;

#[path = "relationship_query.rs"]
mod relationship_query;

use super::*;

use std::{
    collections::{BTreeMap, BTreeSet},
    mem::size_of,
    sync::{
        Mutex, PoisonError, Weak,
        atomic::{AtomicU64, Ordering},
    },
};

use devondb_exec::column::Column;
use devondb_exec::join::{HashJoin, JoinLayout};
use devondb_exec::source::{
    NeighborSource, OuterBindings, ScalarSubqueryExecutor, ScalarSubqueryResult,
};
use devondb_plan::ops::{JoinKey, JoinType, KnnVectorSource};
use devondb_storage::{
    budget::ChargedBytes,
    hnsw::types::HnswDelta,
    node_group::{NodeGroupDirectory, ZoneMapStats, ZoneMapValue},
    overlay::{
        PkIndexResult, PkKey, REL_ENDPOINT_TOMBSTONES_OVERHEAD_BYTES,
        REL_TOMBSTONE_OFFSET_OVERHEAD_BYTES, RelEndpointTombstones,
    },
    superblock::ZONE_MAPS_FLAG,
};
use devondb_types::logical_type::{B1Rescore, VectorEncoding};
use devondb_types::schema::fold;

use super::projection::{position_in, referenced_columns, referenced_names, with_required_columns};
use super::typing::{
    aggregate_metadata, aggregate_types, canonical_expression, expression_type, projected_metadata,
    projection_types, scan_binding, validate_plan,
};

pub(crate) fn run_snapshot(snapshot: &Snapshot, plan: &Plan) -> DevonResult<QueryResult> {
    let view = ReadView::new(
        Arc::clone(&snapshot.shared),
        Arc::clone(&snapshot.state),
        Arc::clone(&snapshot.state.catalog),
        None,
    );
    run_view(&view, plan)
}

impl Database {
    /// Diagnostic count of cached-postings row scoring on this calling thread.
    #[cfg(feature = "fts")]
    #[doc(hidden)]
    pub fn fulltext_cached_rows_scored(&self) -> u64 {
        fulltext::cached_rows()
    }

    /// Returns data-page read requests made by this database handle's shared
    /// pager since open or the last reset. Cache hits count as requests.
    ///
    /// NOT part of the supported API surface. It exists so integration tests —
    /// which can only reach public items — can assert work that was NOT done,
    /// the only way to prove zone-map pruning actually skipped a group
    /// (`crates/devondb/tests/zone_maps.rs`). The counter is a diagnostic with
    /// no stability promise: its units, reset semantics, and existence may
    /// change at any time, and it is deliberately excluded from the docs.
    #[doc(hidden)]
    #[must_use]
    pub fn page_read_count(&self) -> u64 {
        self.shared.pager.page_read_count()
    }

    /// Resets this database handle's shared pager read counter.
    ///
    /// NOT part of the supported API surface — see [`Self::page_read_count`].
    #[doc(hidden)]
    pub fn reset_page_read_count(&self) {
        self.shared.pager.reset_page_read_count();
    }

    /// Returns how many node groups this process has decoded through the
    /// typed column scan path (`docs/SCALE.md` §6.5) since the last reset.
    ///
    /// NOT part of the supported API surface. It exists so integration tests
    /// can prove the typed path — and its boxed fallback for overlay-shadowed
    /// groups — are actually taken (`crates/devondb/tests/typed_scan_e2e.rs`);
    /// disabling the typed path leaves results identical and this counter at zero.
    /// Process-global, unlike the per-handle [`Self::page_read_count`]; no
    /// stability promise.
    #[doc(hidden)]
    #[must_use]
    pub fn typed_group_scan_count(&self) -> u64 {
        TYPED_GROUP_SCANS.load(Ordering::Relaxed)
    }

    /// Resets the typed group scan counter.
    ///
    /// NOT part of the supported API surface — see
    /// [`Self::typed_group_scan_count`].
    #[doc(hidden)]
    pub fn reset_typed_group_scan_count(&self) {
        TYPED_GROUP_SCANS.store(0, Ordering::Relaxed);
    }

    /// Reports whether the current published state has built `table`'s PK index.
    ///
    /// NOT part of the supported API surface. This is a diagnostic for the
    /// PK-resolution integration tests and carries no stability promise.
    #[doc(hidden)]
    #[must_use]
    pub fn pk_resolution_index_present(&self, table: &str) -> bool {
        let state = self.shared.current_state();
        PublishedState::checkpointed_pk_index_present(&state, table)
    }

    /// Free-page diagnostics — `None` when the file carries no ledger
    /// (`docs/FREE_PAGES.md` gates 1, 2, and 5).
    ///
    /// NOT part of the supported API surface; no stability promise.
    #[doc(hidden)]
    pub fn free_pages_stats(&self) -> DevonResult<Option<FreePagesStats>> {
        let pager = &self.shared.pager;
        let Some(extension) = pager.free_pages_extension() else {
            if pager.free_pages_degraded() {
                return Ok(Some(FreePagesStats {
                    retired_total: 0,
                    live_entries: 0,
                    session_reused: pager.free_pages_session_reused(),
                    min_pin: pager.min_pin(),
                    degraded: true,
                }));
            }
            return Ok(None);
        };
        let mut live_entries = 0_u64;
        for (_page_id, ledger) in pager.free_pages_ledger_pages()? {
            live_entries +=
                (ledger.entries.len() as u64).saturating_sub(u64::from(ledger.consumed_count));
        }
        Ok(Some(FreePagesStats {
            retired_total: extension.retired_total,
            live_entries,
            session_reused: pager.free_pages_session_reused(),
            min_pin: pager.min_pin(),
            degraded: pager.free_pages_degraded(),
        }))
    }
}

/// Free-page diagnostic snapshot returned by [`Database::free_pages_stats`].
///
/// NOT part of the supported API surface; no stability promise.
#[doc(hidden)]
#[derive(Clone, Copy, Debug)]
pub struct FreePagesStats {
    /// Cumulative retirement entries ever appended.
    pub retired_total: u64,
    /// Unconsumed ledger entries in the durable chain.
    pub live_entries: u64,
    /// Pages handed out by ledger reuse this session.
    pub session_reused: u64,
    /// The current pin horizon.
    pub min_pin: u64,
    /// Whether this session abandoned the ledger after a validation failure.
    pub degraded: bool,
}

#[derive(Clone)]
pub(super) struct ReadView {
    pub(super) shared: Arc<Shared>,
    pub(super) state: Arc<PublishedState>,
    pub(super) catalog: Arc<Catalog>,
    pub(super) own: Option<Arc<CommitDelta>>,
    pub(super) hnsw: Arc<BTreeMap<String, Vec<HnswDelta>>>,
}

impl ReadView {
    pub(super) fn new(
        shared: Arc<Shared>,
        state: Arc<PublishedState>,
        catalog: Arc<Catalog>,
        own: Option<Arc<CommitDelta>>,
    ) -> Self {
        let mut hnsw = BTreeMap::<String, Vec<HnswDelta>>::new();
        for link in state.commit_links_oldest_first() {
            for (name, delta) in &link.delta.hnsw {
                hnsw.entry(fold(name).into_owned())
                    .or_default()
                    .push(delta.clone());
            }
        }
        if let Some(delta) = &own {
            for (name, delta) in &delta.hnsw {
                hnsw.entry(fold(name).into_owned())
                    .or_default()
                    .push(delta.clone());
            }
        }
        Self {
            shared,
            state,
            catalog,
            own,
            hnsw: Arc::new(hnsw),
        }
    }
}

#[derive(Clone, Debug)]
enum VisibleRowEffect {
    Update(Vec<Value>),
    Delete,
}

fn visible_dml_effects(
    view: &ReadView,
    schema: &NodeTableSchema,
) -> DevonResult<BTreeMap<OverlayKey, VisibleRowEffect>> {
    let (key_index, _) = primary_key(schema)?;
    let mut visible = BTreeMap::new();
    if view.state.catalog.node_table(schema.name()).is_some() {
        let committed = view.state.node_dml_effects(schema.name())?;
        for row in committed.updates.values() {
            visible.insert(
                OverlayKey::from_row(schema.name(), row, key_index)?,
                VisibleRowEffect::Update(row.to_vec()),
            );
        }
        for key in committed.tombstones {
            visible.insert(OverlayKey::from_pk_key(key), VisibleRowEffect::Delete);
        }
    }
    if let Some(own) = &view.own {
        apply_own_dml_effects(&mut visible, own, schema.name(), key_index)?;
    }
    Ok(visible)
}

fn apply_own_dml_effects(
    visible: &mut BTreeMap<OverlayKey, VisibleRowEffect>,
    own: &CommitDelta,
    table: &str,
    key_index: usize,
) -> DevonResult<()> {
    if let Some(rows) = own.node_updates.get(table) {
        for row in rows {
            visible.insert(
                OverlayKey::from_row(table, row, key_index)?,
                VisibleRowEffect::Update(row.clone()),
            );
        }
    }
    if let Some(keys) = own.node_deletes.get(table) {
        for key in keys {
            visible.insert(OverlayKey::from_value(key)?, VisibleRowEffect::Delete);
        }
    }
    // Surviving inserts replace earlier tombstones; updates of own inserts
    // are already folded into own.nodes by the write set.
    if let Some(rows) = own.nodes.get(table) {
        for row in rows {
            visible.remove(&OverlayKey::from_row(table, row, key_index)?);
        }
    }
    Ok(())
}

pub(super) fn run_view(view: &ReadView, plan: &Plan) -> DevonResult<QueryResult> {
    validate_plan(plan, &view.catalog)?;
    let columns = result_columns(&plan.plan, &view.catalog)?;
    let mut working_set_charges = Vec::new();
    let mut pipeline = build_pipeline(view, &plan.plan, &plan.plan, &mut working_set_charges)?;
    let hidden_columns = pipeline.internal_columns();
    let rows = collect_rows(
        pipeline.source.as_mut(),
        &hidden_columns,
        &view.shared.budget,
        &view.shared.pager,
    )?;
    drop(pipeline);
    drop(working_set_charges);
    Ok(QueryResult { columns, rows })
}

fn build_pipeline<'budget>(
    view: &'budget ReadView,
    operator: &Operator,
    root: &Operator,
    charges: &mut Vec<ChargedBytes<'budget>>,
) -> DevonResult<Pipeline> {
    match operator {
        Operator::TextScan {
            table,
            column,
            query,
            k,
            binding,
        } => {
            #[cfg(feature = "fts")]
            {
                fulltext::text_pipeline(view, root, table, column, query, *k, binding)
            }
            #[cfg(not(feature = "fts"))]
            {
                let _ = (table, column, query, k, binding);
                Err(invalid_argument(
                    "TextScan execution requires the fts feature",
                ))
            }
        }
        Operator::ScanNodes { table, binding } => scan_pipeline(view, root, table, binding, None),
        Operator::ScanInterface { interface, binding } => {
            interface_pipeline(view, root, interface, binding, None)
        }
        Operator::Filter { predicate, input } => {
            let input = match input.as_ref() {
                Operator::ScanNodes { table, binding } => {
                    scan_pipeline(view, root, table, binding, Some(predicate))?
                }
                Operator::ScanInterface { interface, binding } => {
                    interface_pipeline(view, root, interface, binding, Some(predicate))?
                }
                _ => build_pipeline(view, input, root, charges)?,
            };
            input.with_filter(predicate.clone(), scalar_executor(view))
        }
        Operator::Project { exprs, input } => build_pipeline(view, input, root, charges)?
            .with_project(exprs, &view.catalog, scalar_executor(view)),
        Operator::Limit {
            count,
            offset,
            input,
        } => {
            Ok(build_pipeline(view, input, root, charges)?.with_limit(*count, offset.unwrap_or(0)))
        }
        Operator::ExpandRel {
            rel,
            direction,
            from_binding,
            binding,
            rel_binding,
            input,
        } => {
            relationship_query::precharge_input(view, input, root, charges)?;
            let input = build_pipeline(view, input, root, charges)?;
            relationship_query::expand_rel_pipeline(
                view,
                input,
                rel,
                *direction,
                from_binding,
                binding,
                rel_binding,
                charges,
            )
        }
        Operator::Expand {
            rel,
            direction,
            from_binding,
            binding,
            input,
        } => {
            let input = build_pipeline(view, input, root, charges)?;
            expand_pipeline(
                view,
                root,
                input,
                rel,
                *direction,
                from_binding,
                binding,
                charges,
            )
        }
        Operator::Sort { keys, input } => build_pipeline(view, input, root, charges)?.with_sort(
            keys,
            view.shared.spill_config.clone(),
            scalar_executor(view),
        ),
        Operator::Aggregate {
            group_by,
            aggs,
            input,
        } => build_pipeline(view, input, root, charges)?.with_aggregate(
            group_by,
            aggs,
            view.shared.spill_config.clone(),
            &view.catalog,
            scalar_executor(view),
        ),
        Operator::KnnScan {
            table,
            column,
            query,
            k,
            metric,
            mode,
        } => match query {
            KnnVectorSource::Literal(vector) => knn_pipeline(
                view, root, table, column, vector, *k, *metric, *mode, charges,
            ),
            KnnVectorSource::Scalar { plan } => {
                let result = scalar_executor(view).execute(plan, &OuterBindings::new())?;
                let value = match result.values.as_slice() {
                    [] => Value::Null,
                    [value] => value.clone(),
                    _ => {
                        return Err(invalid_argument(
                            "scalar subquery returned more than one row",
                        ));
                    }
                };
                let vector = match value {
                    Value::Null => {
                        return Err(invalid_argument("knn query vector is null"));
                    }
                    Value::Vector(vector) => vector,
                    other => {
                        return Err(invalid_argument(format!(
                            "knn query vector has type {}; expected {}",
                            other
                                .logical_type()
                                .map_or_else(|| "Null".to_owned(), |ty| ty.to_string()),
                            result.output_type
                        )));
                    }
                };
                let declared_dim = view
                    .catalog
                    .node_table(table)
                    .and_then(|schema| schema.column_index(column))
                    .and_then(|index| {
                        view.catalog
                            .node_table(table)
                            .map(|schema| schema.columns()[index].ty.value_type().vector_dim())
                            .unwrap_or(None)
                    })
                    .ok_or_else(|| DevonError::NotFound {
                        what: format!("vector column `{table}.{column}`"),
                    })?;
                if vector.len() != declared_dim as usize {
                    return Err(invalid_argument(format!(
                        "knn query vector has dimension {}; column `{table}.{column}` has type Vector({declared_dim})",
                        vector.len()
                    )));
                }
                knn_pipeline(
                    view, root, table, column, &vector, *k, *metric, *mode, charges,
                )
            }
        },
        Operator::WithinScan {
            table,
            column,
            center,
            meters,
        } => within_pipeline(view, root, table, column, *center, *meters),
        Operator::HashJoin {
            join,
            on,
            left,
            right,
        } => {
            let left = build_pipeline(view, left, root, charges)?;
            let right = build_pipeline(view, right, root, charges)?;
            left.with_hash_join(right, on, *join, Arc::clone(&view.shared.budget))
        }
    }
}

fn scan_pipeline(
    view: &ReadView,
    root: &Operator,
    table: &str,
    binding: &str,
    pruning_predicate: Option<&Expr>,
) -> DevonResult<Pipeline> {
    let schema = view
        .catalog
        .node_table(table)
        .cloned()
        .ok_or_else(|| DevonError::NotFound {
            what: format!("node table `{table}`"),
        })?;
    // The pushdown set: what the plan reads for this binding, plus the
    // primary key (overlay shadow checks and PK resolution need it). The
    // hidden offset column is synthesized, never decoded.
    let (key_index, _) = primary_key(&schema)?;
    let projection = with_required_columns(
        referenced_columns(root, binding, &schema),
        &schema,
        [key_index],
    );
    let zone_map_filter = pruning_predicate
        .and_then(|predicate| ZoneMapFilter::from_expression(predicate, &schema, binding));
    let source = ScanSource::new(view, schema.clone(), zone_map_filter, projection.clone())?;
    Pipeline::scan(Box::new(source), &schema, binding, projection.as_ref())
}

fn interface_pipeline(
    view: &ReadView,
    root: &Operator,
    interface: &str,
    binding: &str,
    pruning_predicate: Option<&Expr>,
) -> DevonResult<Pipeline> {
    let interface =
        view.catalog
            .interface(interface)
            .cloned()
            .ok_or_else(|| DevonError::NotFound {
                what: format!("interface `{interface}`"),
            })?;
    // Pushdown over the interface declaration: the emitted
    // interface columns narrow to those the plan references, and each
    // implementing table's scan decodes only the table columns feeding
    // them (plus its primary key, for overlay shadow checks).
    let emit: Option<Vec<usize>> = referenced_names(root, binding).map(|names| {
        interface
            .columns
            .iter()
            .enumerate()
            .filter(|(_, column)| names.contains(fold(&column.name).as_ref()))
            .map(|(index, _)| index)
            .collect()
    });
    let emit = emit
        .filter(|emit| emit.len() < interface.columns.len())
        .unwrap_or_else(|| (0..interface.columns.len()).collect());
    let interface_types = emit
        .iter()
        .map(|&index| interface.columns[index].ty)
        .collect::<Vec<_>>();
    let mut inputs = Vec::new();
    for schema in view.catalog.node_tables() {
        if !table_implements(view, schema.name(), &interface.name) {
            continue;
        }
        let table_columns = emit
            .iter()
            .map(|&index| {
                schema
                    .column_index(&interface.columns[index].name)
                    .ok_or_else(|| {
                        corrupt(format!(
                            "node class `{}` lost interface `{}` column `{}`",
                            schema.name(),
                            interface.name,
                            interface.columns[index].name
                        ))
                    })
            })
            .collect::<DevonResult<Vec<_>>>()?;
        let (key_index, _) = primary_key(schema)?;
        let scan_set = with_required_columns(
            Some(table_columns.iter().copied().collect()),
            schema,
            [key_index],
        );
        // The scan chunk carries the set in table order; the interface
        // projection remaps declaration order into those chunk positions.
        let chunk_projection = match &scan_set {
            Some(set) => table_columns
                .iter()
                .map(|&index| {
                    position_in(set, index).ok_or_else(|| {
                        corrupt(format!(
                            "scan projection for `{}` lost interface column {index}",
                            schema.name()
                        ))
                    })
                })
                .collect::<DevonResult<Vec<_>>>()?,
            None => table_columns,
        };
        let zone_map_filter = pruning_predicate
            .and_then(|predicate| ZoneMapFilter::from_expression(predicate, schema, binding));
        let scan_view = view.clone();
        let scan_schema = schema.clone();
        inputs.push(InterfaceScanInput::deferred(
            move || {
                ScanSource::new(&scan_view, scan_schema, zone_map_filter, scan_set)
                    .map(|source| Box::new(source) as Box<dyn ChunkSource>)
            },
            chunk_projection,
            schema.name().to_owned(),
        ));
    }
    Pipeline::interface_scan(
        Box::new(ExecScanInterface::new(inputs, interface_types)),
        &interface,
        binding,
        &emit,
    )
}

fn table_implements(view: &ReadView, table: &str, interface: &str) -> bool {
    view.catalog.node_class(table).is_some_and(|class| {
        class
            .implements
            .iter()
            .any(|implemented| fold(implemented) == fold(interface))
    })
}

#[allow(clippy::too_many_arguments)]
fn knn_pipeline<'budget>(
    view: &'budget ReadView,
    root: &Operator,
    table: &str,
    column: &str,
    query: &[f32],
    k: u64,
    metric: Metric,
    mode: KnnMode,
    charges: &mut Vec<ChargedBytes<'budget>>,
) -> DevonResult<Pipeline> {
    let schema = view
        .catalog
        .node_table(table)
        .cloned()
        .ok_or_else(|| DevonError::NotFound {
            what: format!("node table `{table}`"),
        })?;
    let vector_column = schema
        .column_index(column)
        .ok_or_else(|| DevonError::NotFound {
            what: format!("column `{table}.{column}`"),
        })?;
    let matching_index = view
        .catalog
        .indexes()
        .iter()
        .any(|index| fold(&index.table) == fold(table) && fold(&index.column) == fold(column));
    let dirty = matching_index
        && super::hnsw::table_has_mutation_history(&view.state, view.own.as_deref(), table);
    if mode == KnnMode::Approximate && matching_index {
        super::hnsw::validate_query_index(view, &schema, column)?;
    }
    if mode == KnnMode::Approximate && matching_index && !dirty {
        let charge_count = charges.len();
        let rows = match node_rows_for_view(view, table, charges) {
            Ok(rows) if rows.iter().all(Option::is_some) => Some(Arc::new(
                rows.into_iter().flatten().collect::<Vec<Vec<Value>>>(),
            )),
            Ok(_) => None,
            Err(DevonError::BudgetExceeded { .. }) => None,
            Err(error) => return Err(error),
        };
        if let Some(rows) = rows {
            match super::hnsw::approximate_knn_source(
                view,
                &schema,
                vector_column,
                column,
                query,
                k,
                metric,
                rows,
            ) {
                Ok(Some(source)) => return Pipeline::knn(source, &schema, table, None),
                Ok(None) | Err(DevonError::BudgetExceeded { .. }) => {}
                Err(error) => return Err(error),
            }
        }
        charges.truncate(charge_count);
    }
    // The exact path scans the table: narrow its decode to the columns the
    // plan reads plus the vector column the scan ranks by and the primary
    // key.
    let (key_index, _) = primary_key(&schema)?;
    let projection = with_required_columns(
        referenced_columns(root, table, &schema),
        &schema,
        [key_index, vector_column],
    );
    let source: Box<dyn ChunkSource> = if dirty {
        Box::new(StrictKnnSource::new(
            view,
            schema.clone(),
            projection.clone(),
        )?)
    } else {
        Box::new(ScanSource::new(
            view,
            schema.clone(),
            None,
            projection.clone(),
        )?)
    };
    let remapped_vector_column = remap_column(&projection, vector_column)?;
    // Retained candidate rows charge the statement-wide budget, released on
    // eviction and at drop.
    let knn = ExactKnnScan::new(source, remapped_vector_column, query.to_vec(), k, metric)
        .with_budget(Arc::clone(&view.shared.budget));
    Pipeline::knn(Box::new(knn), &schema, table, projection.as_ref())
}

/// Dirty HNSW scans reserve decode peaks before constructing any overlay or
/// group rows, and emit tight one-row chunks rather than reserving 2048 slots.
struct StrictKnnSource {
    source: ScanSource,
    _decode: OwnedWorkingSetCharge,
}
impl StrictKnnSource {
    fn new(
        view: &ReadView,
        schema: NodeTableSchema,
        projection: Option<BTreeSet<usize>>,
    ) -> DevonResult<Self> {
        let decode = relationship_query::source_decode_peak(view, &schema, projection.as_ref())?;
        // ExactKnnScan's output builder reserves a full chunk per column even
        // for k=1. Conversion consumes columns sequentially, so one extra
        // column covers overlapping Value/typed slots (plus offset/distance).
        let columns = projection
            .as_ref()
            .map_or(schema.columns().len(), BTreeSet::len);
        let output = columns
            .checked_add(3)
            .and_then(|n| n.checked_mul(devondb_exec::chunk::CHUNK_CAPACITY))
            .and_then(|n| n.checked_mul(std::mem::size_of::<Value>()))
            .ok_or_else(|| corrupt("dirty KNN output capacity overflow"))?;
        let bytes = decode
            .checked_add(output)
            .ok_or_else(|| corrupt("dirty KNN working set overflow"))?;
        if !view.shared.budget.charge_or_reclaim(bytes) {
            return Err(DevonError::BudgetExceeded {
                context: "dirty HNSW exact scan decode reservation".into(),
            });
        }
        let decode = OwnedWorkingSetCharge {
            budget: Arc::clone(&view.shared.budget),
            bytes,
        };
        let source = ScanSource::new(view, schema, None, projection)?;
        Ok(Self {
            source,
            _decode: decode,
        })
    }
}
impl ChunkSource for StrictKnnSource {
    fn next_chunk(&mut self) -> DevonResult<Option<Chunk>> {
        let row = match self.source.next_persisted_row()? {
            Some(row) => row,
            None => match self.source.overlay_rows.next() {
                Some(row) => self.source.overlay_row(row)?,
                None => return Ok(None),
            },
        };
        let columns = self
            .source
            .types
            .iter()
            .zip(row)
            .map(|(ty, value)| Column::from_values(ty, vec![value]))
            .collect();
        Chunk::from_columns(self.source.types.clone(), columns).map(Some)
    }
}

/// Maps a table column index into a narrowed scan chunk; `None` projection
/// is the identity full-width layout.
fn remap_column(projection: &Option<BTreeSet<usize>>, column: usize) -> DevonResult<usize> {
    match projection {
        None => Ok(column),
        Some(set) => position_in(set, column)
            .ok_or_else(|| corrupt(format!("scan projection lost required column {column}"))),
    }
}

fn within_pipeline(
    view: &ReadView,
    root: &Operator,
    table: &str,
    column: &str,
    center: devondb_types::GeoPoint,
    meters: f64,
) -> DevonResult<Pipeline> {
    let schema = view
        .catalog
        .node_table(table)
        .cloned()
        .ok_or_else(|| DevonError::NotFound {
            what: format!("node table `{table}`"),
        })?;
    let point_column = schema
        .column_index(column)
        .ok_or_else(|| DevonError::NotFound {
            what: format!("column `{table}.{column}`"),
        })?;
    let covering_ranges = devondb_exec::within::WithinScan::covering_ranges(center, meters)?;
    let zone_map_filter = ZoneMapFilter::for_geo_point(point_column, covering_ranges);
    // Pushdown decodes what the plan reads plus the GeoPoint
    // column the fence filters on and the primary key.
    let (key_index, _) = primary_key(&schema)?;
    let projection = with_required_columns(
        referenced_columns(root, table, &schema),
        &schema,
        [key_index, point_column],
    );
    let source = ScanSource::new(
        view,
        schema.clone(),
        Some(zone_map_filter),
        projection.clone(),
    )?;
    let within = devondb_exec::within::WithinScan::new(
        Box::new(source),
        remap_column(&projection, point_column)?,
        center,
        meters,
    )?;
    Pipeline::scan(Box::new(within), &schema, table, projection.as_ref())
}

#[allow(clippy::too_many_arguments)]
fn expand_pipeline<'budget>(
    view: &'budget ReadView,
    root: &Operator,
    input: Pipeline,
    rel: &str,
    direction: Direction,
    from_binding: &str,
    binding: &str,
    charges: &mut Vec<ChargedBytes<'budget>>,
) -> DevonResult<Pipeline> {
    let rel_schema = view
        .catalog
        .rel_table(rel)
        .cloned()
        .ok_or_else(|| DevonError::NotFound {
            what: format!("relationship table `{rel}`"),
        })?;
    let rel_name = rel_schema.name().to_owned();
    let source_table = input.binding_table(from_binding)?.to_owned();
    let (storage_direction, neighbor_table) =
        traversal_tables(&rel_schema, direction, &source_table)?;
    let neighbor_table = neighbor_table.to_owned();
    let source_row_count = visible_node_count(view, &source_table)?;
    let neighbor_schema = view
        .catalog
        .node_table(&neighbor_table)
        .cloned()
        .ok_or_else(|| DevonError::NotFound {
            what: format!("node table `{neighbor_table}`"),
        })?;
    let projection = expand_query::projection(root, binding, &neighbor_schema)?;
    // The projected reservation is a per-group scan worst case (decode peak x4).
    // On small or narrow destinations it exceeds what the full-width path ever
    // holds, so refusing there would reject queries the full-width path serves.
    // A budget refusal from the narrow path therefore falls back to the wide one
    // rather than failing the query; any other error still propagates.
    let projected_rows = match &projection {
        Some(selected) => {
            match expand_query::materialize(view, &neighbor_schema, selected, charges) {
                Ok(materialized) => Some(materialized),
                Err(DevonError::BudgetExceeded { .. }) => None,
                Err(error) => return Err(error),
            }
        }
        None => None,
    };
    let projected = projected_rows.is_some();
    let (neighbor_rows, output_schema) = match projected_rows {
        Some(materialized) => materialized,
        None => (
            node_rows_for_view(view, &neighbor_table, charges)?,
            neighbor_schema.clone(),
        ),
    };
    if projected {
        expand_query::charge_output(
            view,
            input.width + output_schema.columns().len() + 1,
            &neighbor_rows,
            charges,
        )?;
    }
    let rel_bytes = rel_overlay_bytes(view, &rel_name)?;
    let _rel_charge =
        charge_working_set(&view.shared.budget, &view.shared.pager, rel_bytes, || {
            format!("expand relationship overlay for `{rel}`")
        })?;
    let mut rel_table = RelTable::new(rel_schema);
    for edge in view.state.rel_edges(&rel_name) {
        rel_table.recover_edge(edge.from, edge.to, edge.values.clone())?;
    }
    if let Some(own) = &view.own
        && let Some(edges) = own.edges.get(&rel_name)
    {
        for edge in edges {
            rel_table.recover_edge(edge.from, edge.to, edge.values.clone())?;
        }
    }
    rel_table.set_endpoint_tombstones(effective_rel_tombstones(view, &rel_name));
    let adjacency_bytes = adjacency_working_set_bytes(
        &rel_table,
        &view.shared.pager,
        &view.catalog,
        storage_direction,
        source_row_count,
        &rel_name,
    )?;
    let adjacency_charge = charge_working_set(
        &view.shared.budget,
        &view.shared.pager,
        adjacency_bytes,
        || format!("scan/expand adjacency working set for relationship `{rel}`"),
    )?;
    let dense_legacy_view = !projected
        && neighbor_rows.iter().all(Option::is_some)
        && visible_dml_effects(view, &neighbor_schema)?.is_empty();
    let _validation_charge = if dense_legacy_view {
        let checkpointed_neighbors =
            usize::try_from(checkpointed_row_count(view, &neighbor_schema)?)
                .map_err(|_| corrupt("checkpointed node count cannot index a working set"))?;
        Some(charge_working_set(
            &view.shared.budget,
            &view.shared.pager,
            rows_prefix_bytes(&neighbor_rows, checkpointed_neighbors)?,
            || format!("expand destination validation working set for `{neighbor_table}`"),
        )?)
    } else {
        None
    };
    let neighbors = if dense_legacy_view {
        ExpandGraphSnapshot::Dense(GraphSnapshot::materialize(
            &rel_table,
            &view.shared.pager,
            &view.catalog,
            direction,
            &source_table,
            source_row_count,
            neighbor_rows.into_iter().flatten().collect(),
        )?)
    } else {
        ExpandGraphSnapshot::Slots(SlotGraphSnapshot::materialize(
            &rel_table,
            &view.shared.pager,
            &view.catalog,
            direction,
            &source_table,
            source_row_count,
            neighbor_rows,
            Some(&output_schema),
        )?)
    };
    charges.push(adjacency_charge);
    input.with_expand(
        neighbors,
        rel,
        direction,
        from_binding,
        binding,
        &output_schema,
    )
}

fn visible_node_count(view: &ReadView, table: &str) -> DevonResult<usize> {
    let schema = view
        .catalog
        .node_table(table)
        .ok_or_else(|| DevonError::NotFound {
            what: format!("node table `{table}`"),
        })?;
    let table_name = schema.name();
    let checkpointed = usize::try_from(checkpointed_row_count(view, schema)?)
        .map_err(|_| corrupt("checkpointed node count cannot index a working set"))?;
    let overlay = view.state.node_slots(table_name).count();
    let own = view
        .own
        .as_ref()
        .and_then(|delta| delta.nodes.get(table_name))
        .map_or(0, Vec::len);
    checkpointed
        .checked_add(overlay)
        .and_then(|count| count.checked_add(own))
        .ok_or_else(|| corrupt(format!("node table `{table}` row count exceeds usize::MAX")))
}

fn node_rows_for_view<'budget>(
    view: &'budget ReadView,
    table: &str,
    charges: &mut Vec<ChargedBytes<'budget>>,
) -> DevonResult<Vec<Option<Vec<Value>>>> {
    let schema = view
        .catalog
        .node_table(table)
        .cloned()
        .ok_or_else(|| DevonError::NotFound {
            what: format!("node table `{table}`"),
        })?;
    let table_name = schema.name().to_owned();
    let base_count = usize::try_from(checkpointed_row_count(view, &schema)?)
        .map_err(|_| corrupt("checkpointed node count cannot index a working set"))?;
    let mut requested = minimum_rows_bytes(&schema, base_count)?;
    checked_working_set_add(
        &mut requested,
        base_count
            .checked_mul(PHYSICAL_SLOT_INDEX_ENTRY_BYTES)
            .ok_or_else(|| corrupt("node physical-slot estimate exceeds usize::MAX"))?,
    )?;
    for slot in view.state.node_slots(&table_name) {
        checked_working_set_add(&mut requested, size_of::<Option<Vec<Value>>>())?;
        checked_working_set_add(&mut requested, PHYSICAL_SLOT_INDEX_ENTRY_BYTES)?;
        if let Some(row) = slot.row {
            add_value_bytes(&mut requested, row)?;
        }
    }
    if let Some(own) = &view.own
        && let Some(own_rows) = own.nodes.get(&table_name)
    {
        for row in own_rows {
            add_row_bytes(&mut requested, row)?;
            checked_working_set_add(&mut requested, PHYSICAL_SLOT_INDEX_ENTRY_BYTES)?;
        }
    }
    let mut charge =
        charge_working_set(&view.shared.budget, &view.shared.pager, requested, || {
            format!("scan/expand node-row working set for table `{table}`")
        })?;
    let effects = visible_dml_effects(view, &schema)?;
    let mut rows = NodeTable::new(schema.clone())
        .scan(&view.shared.pager, &view.catalog)?
        .into_iter()
        .map(Some)
        .collect::<Vec<_>>();
    rows.extend(
        view.state
            .node_slots(&table_name)
            .map(|slot| slot.row.map(<[Value]>::to_vec)),
    );
    if let Some(own) = &view.own
        && let Some(own_rows) = own.nodes.get(&table_name)
    {
        rows.extend(own_rows.iter().cloned().map(Some));
    }
    rows = apply_slot_effects(&schema, rows, &effects)?;
    let actual = slot_rows_bytes(&rows, rows.capacity())?;
    if actual > charge.bytes() {
        let additional = actual - charge.bytes();
        grow_working_set_charge(
            &mut charge,
            &view.shared.budget,
            &view.shared.pager,
            additional,
            || format!("scan/expand node-row working set for table `{table}`"),
        )?;
    }
    charges.push(charge);
    Ok(rows)
}

fn apply_slot_effects(
    schema: &NodeTableSchema,
    rows: Vec<Option<Vec<Value>>>,
    effects: &BTreeMap<OverlayKey, VisibleRowEffect>,
) -> DevonResult<Vec<Option<Vec<Value>>>> {
    let (key_index, _) = primary_key(schema)?;
    let mut visible = Vec::with_capacity(rows.len());
    let mut positions = BTreeMap::new();
    for row in rows {
        let Some(row) = row else {
            visible.push(None);
            continue;
        };
        let key = OverlayKey::from_row(schema.name(), &row, key_index)?;
        let replacement = match effects.get(&key) {
            Some(VisibleRowEffect::Update(replacement)) => Some(replacement.clone()),
            Some(VisibleRowEffect::Delete) => None,
            None => Some(row),
        };
        if let Some(previous) = positions.insert(key.clone(), visible.len()) {
            visible[previous] = None;
        }
        if replacement.is_none() {
            positions.remove(&key);
        }
        visible.push(replacement);
    }
    Ok(visible)
}

/// Expand's hole-aware snapshot. Node rows stay indexed by physical offset;
/// a relationship surviving tombstone filtering into a hole is corruption.
struct SlotGraphSnapshot {
    rel: String,
    direction: Direction,
    adjacency: Vec<Vec<u64>>,
    node_rows: Vec<Option<Vec<Value>>>,
}

enum ExpandGraphSnapshot {
    Dense(GraphSnapshot),
    Slots(SlotGraphSnapshot),
}

impl NeighborSource for ExpandGraphSnapshot {
    fn neighbors(&mut self, rel: &str, direction: Direction, from: u64) -> DevonResult<Vec<u64>> {
        match self {
            Self::Dense(snapshot) => snapshot.neighbors(rel, direction, from),
            Self::Slots(snapshot) => snapshot.neighbors(rel, direction, from),
        }
    }

    fn node_row(
        &mut self,
        rel: &str,
        direction: Direction,
        offset: u64,
    ) -> DevonResult<Vec<Value>> {
        match self {
            Self::Dense(snapshot) => snapshot.node_row(rel, direction, offset),
            Self::Slots(snapshot) => snapshot.node_row(rel, direction, offset),
        }
    }
}

impl SlotGraphSnapshot {
    #[allow(clippy::too_many_arguments)]
    fn materialize(
        table: &RelTable,
        pager: &Pager,
        catalog: &Catalog,
        direction: Direction,
        source_table: &str,
        source_row_count: usize,
        node_rows: Vec<Option<Vec<Value>>>,
        projected_schema: Option<&NodeTableSchema>,
    ) -> DevonResult<Self> {
        let traversal = crate::graph::traversal_direction(table.schema(), direction, source_table)?;
        let neighbor_table = match traversal {
            Direction::Out | Direction::Both => table.schema().to(),
            Direction::In => table.schema().from(),
        };
        let neighbor_schema =
            catalog
                .node_table(neighbor_table)
                .ok_or_else(|| DevonError::NotFound {
                    what: format!("node table `{neighbor_table}`"),
                })?;
        validate_slot_rows(projected_schema.unwrap_or(neighbor_schema), &node_rows)?;
        let storage_direction = match traversal {
            Direction::Out => devondb_storage::rel_table::Direction::Out,
            Direction::In => devondb_storage::rel_table::Direction::In,
            Direction::Both => devondb_storage::rel_table::Direction::Both,
        };
        let mut adjacency = Vec::with_capacity(source_row_count);
        for offset in 0..source_row_count {
            let offset = u64::try_from(offset)
                .map_err(|_| corrupt("node offset cannot be represented as u64"))?;
            adjacency.push(table.neighbors(pager, catalog, storage_direction, offset)?);
        }
        Ok(Self {
            rel: table.schema().name().to_owned(),
            direction,
            adjacency,
            node_rows,
        })
    }

    fn validate_request(&self, rel: &str, direction: Direction) -> DevonResult<()> {
        if fold(rel) != fold(&self.rel) || direction != self.direction {
            return Err(corrupt(format!(
                "Expand requested relationship `{rel}` direction {direction:?} from snapshot of `{}` direction {:?}",
                self.rel, self.direction
            )));
        }
        Ok(())
    }
}

impl NeighborSource for SlotGraphSnapshot {
    fn neighbors(&mut self, rel: &str, direction: Direction, from: u64) -> DevonResult<Vec<u64>> {
        self.validate_request(rel, direction)?;
        let index = usize::try_from(from)
            .map_err(|_| corrupt(format!("node offset {from} cannot index adjacency")))?;
        self.adjacency.get(index).cloned().ok_or_else(|| {
            corrupt(format!(
                "node offset {from} is outside relationship `{rel}` adjacency"
            ))
        })
    }

    fn node_row(
        &mut self,
        rel: &str,
        direction: Direction,
        offset: u64,
    ) -> DevonResult<Vec<Value>> {
        self.validate_request(rel, direction)?;
        let index = usize::try_from(offset)
            .map_err(|_| corrupt(format!("neighbor offset {offset} cannot index node rows")))?;
        self.node_rows
            .get(index)
            .ok_or_else(|| {
                corrupt(format!(
                    "neighbor offset {offset} from relationship `{rel}` is outside node rows"
                ))
            })?
            .clone()
            .ok_or_else(|| {
                corrupt(format!(
                    "neighbor offset {offset} from relationship `{rel}` resolves to a deleted hole"
                ))
            })
    }
}

fn validate_slot_rows(schema: &NodeTableSchema, rows: &[Option<Vec<Value>>]) -> DevonResult<()> {
    for (offset, row) in rows.iter().enumerate() {
        let Some(row) = row else {
            continue;
        };
        if row.len() != schema.columns().len() {
            return Err(corrupt(format!(
                "node offset {offset} for `{}` has {} columns, expected {}",
                schema.name(),
                row.len(),
                schema.columns().len()
            )));
        }
        for (column, value) in schema.columns().iter().zip(row) {
            if !value.matches_type(&column.ty) {
                return Err(corrupt(format!(
                    "node offset {offset} for `{}` column `{}` expects {} but found {value}",
                    schema.name(),
                    column.name,
                    column.ty
                )));
            }
        }
    }
    Ok(())
}

pub(super) fn resolve_node_row(
    view: &ReadView,
    table: &str,
    key: &Value,
) -> DevonResult<Vec<Value>> {
    let schema = view
        .catalog
        .node_table(table)
        .cloned()
        .ok_or_else(|| DevonError::NotFound {
            what: format!("node table `{table}`"),
        })?;
    let table_name = schema.name().to_owned();
    let (key_index, key_column) = primary_key(&schema)?;
    let cache_key = OverlayKey::from_query(&schema, key_index, key_column, key)?;
    match visible_dml_effects(view, &schema)?.get(&cache_key) {
        Some(VisibleRowEffect::Update(row)) => return Ok(row.clone()),
        Some(VisibleRowEffect::Delete) => return Err(missing_node_key(table, key)),
        None => {}
    }
    if let Some(row) = own_node_row(view, &table_name, key_index, key) {
        return Ok(row);
    }
    if let Some(row) = view
        .state
        .node_rows(&table_name)
        .find(|row| row.get(key_index) == Some(key))
    {
        return Ok(row.to_vec());
    }
    match PublishedState::checkpointed_node_row(
        &view.state,
        &view.shared.pager,
        &view.shared.budget,
        &table_name,
        key,
    )? {
        PkIndexResult::Indexed(Some(row)) => return Ok(row),
        PkIndexResult::Indexed(None) => return Err(missing_node_key(table, key)),
        PkIndexResult::Unavailable => {}
    }
    let rows = NodeTable::new(schema).scan(&view.shared.pager, &view.catalog)?;
    rows.into_iter()
        .find(|row| row.get(key_index) == Some(key))
        .ok_or_else(|| missing_node_key(table, key))
}

fn own_node_row(view: &ReadView, table: &str, key_index: usize, key: &Value) -> Option<Vec<Value>> {
    view.own
        .as_ref()
        .and_then(|delta| delta.nodes.get(table))
        .and_then(|rows| rows.iter().find(|row| row.get(key_index) == Some(key)))
        .cloned()
}

fn missing_node_key(table: &str, key: &Value) -> DevonError {
    DevonError::NotFound {
        what: format!("node table `{table}` primary key {key}"),
    }
}

pub(super) fn resolve_node_key(view: &ReadView, table: &str, key: &Value) -> DevonResult<u64> {
    let schema = view
        .catalog
        .node_table(table)
        .cloned()
        .ok_or_else(|| DevonError::NotFound {
            what: format!("node table `{table}`"),
        })?;
    let table_name = schema.name().to_owned();
    let (key_index, key_column) = primary_key(&schema)?;
    let cache_key = OverlayKey::from_query(&schema, key_index, key_column, key)?;
    let (overlay_position, overlay_count) =
        overlay_key_position(view, &table_name, key_index, &cache_key)?;
    if let Some(own) = &view.own
        && let Some(rows) = own.nodes.get(&table_name)
    {
        for (position, row) in rows.iter().enumerate() {
            if row.get(key_index) == Some(key) {
                let overlay_position = overlay_count.checked_add(position).ok_or_else(|| {
                    corrupt(format!(
                        "node table `{table}` overlay position exceeds usize::MAX"
                    ))
                })?;
                return node_offset(checkpointed_row_count(view, &schema)?, overlay_position);
            }
        }
    }
    if view_has_node_deletes(view, &table_name)
        && matches!(
            visible_dml_effects(view, &schema)?.get(&cache_key),
            Some(VisibleRowEffect::Delete)
        )
    {
        return Err(missing_node_key(table, key));
    }
    if let Some(position) = overlay_position {
        return node_offset(checkpointed_row_count(view, &schema)?, position);
    }
    if let Some(offset) = resolve_checkpointed_node_key(view, &schema, key_index, key)? {
        return Ok(offset);
    }
    Err(missing_node_key(table, key))
}

fn view_has_node_deletes(view: &ReadView, table: &str) -> bool {
    if view
        .own
        .as_ref()
        .is_some_and(|delta| delta.node_deletes.contains_key(table))
    {
        return true;
    }
    let mut link = view.state.chain.as_ref();
    while let Some(current) = link {
        if current.delta.node_deletes.contains_key(table) {
            return true;
        }
        link = current.prev.as_ref();
    }
    false
}

fn checkpointed_row_count(view: &ReadView, schema: &NodeTableSchema) -> DevonResult<u64> {
    let mut count = 0_u64;
    let group_ids = view
        .catalog
        .table_storage(schema.name())
        .map_or(&[][..], |storage| storage.groups.as_slice());
    for (group_index, group_id) in group_ids.iter().copied().enumerate() {
        let group_rows =
            NodeGroup::read_row_count(&view.shared.pager, group_id, schema.columns().len())
                .map_err(|error| {
                    corrupt(format!(
                        "node table `{}` group {group_index} has invalid metadata: {error}",
                        schema.name()
                    ))
                })?;
        count = count
            .checked_add(u64::try_from(group_rows).map_err(|_| {
                corrupt(format!(
                    "node table `{}` group row count exceeds u64::MAX",
                    schema.name()
                ))
            })?)
            .ok_or_else(|| {
                corrupt(format!(
                    "checkpointed row count for node table `{}` exceeds u64::MAX",
                    schema.name()
                ))
            })?;
    }
    Ok(count)
}

fn resolve_checkpointed_node_key(
    view: &ReadView,
    schema: &NodeTableSchema,
    key_index: usize,
    key: &Value,
) -> DevonResult<Option<u64>> {
    match PublishedState::checkpointed_node_offset(
        &view.state,
        &view.shared.pager,
        &view.shared.budget,
        schema.name(),
        key,
    )? {
        PkIndexResult::Indexed(offset) => return Ok(offset),
        PkIndexResult::Unavailable => {}
    }
    scan_checkpointed_node_key(view, schema, key_index, key)
}

fn scan_checkpointed_node_key(
    view: &ReadView,
    schema: &NodeTableSchema,
    key_index: usize,
    key: &Value,
) -> DevonResult<Option<u64>> {
    let property_types = schema
        .columns()
        .iter()
        .map(|column| column.ty)
        .collect::<Vec<_>>();
    let group_ids = view
        .catalog
        .table_storage(schema.name())
        .map_or(&[][..], |storage| storage.groups.as_slice());
    let mut group_start = 0_u64;
    for group_id in group_ids {
        let group = NodeGroup::read(&view.shared.pager, *group_id, &property_types)?;
        for row in 0..group.row_count() {
            if group.value(row, key_index) == Some(key) {
                let row = u64::try_from(row)
                    .map_err(|_| corrupt("node-group row index exceeds u64::MAX"))?;
                return group_start
                    .checked_add(row)
                    .map(Some)
                    .ok_or_else(|| corrupt("checkpointed node offset exceeds u64::MAX"));
            }
        }
        group_start = group_start
            .checked_add(
                u64::try_from(group.row_count())
                    .map_err(|_| corrupt("node-group row count cannot be represented as u64"))?,
            )
            .ok_or_else(|| corrupt("checkpointed node offset exceeds u64::MAX"))?;
    }
    Ok(None)
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum OverlayKey {
    Int64(i64),
    String(String),
}

impl OverlayKey {
    fn from_value(value: &Value) -> DevonResult<Self> {
        match value {
            Value::Int64(value) => Ok(Self::Int64(*value)),
            Value::String(value) => Ok(Self::String(value.clone())),
            other => Err(corrupt(format!(
                "node DML primary key must be Int64 or String, found {other}"
            ))),
        }
    }

    fn from_pk_key(value: PkKey<'_>) -> Self {
        match value {
            PkKey::Int64(value) => Self::Int64(value),
            PkKey::String(value) => Self::String(value.to_owned()),
        }
    }

    fn to_value(&self) -> Value {
        match self {
            Self::Int64(value) => Value::Int64(*value),
            Self::String(value) => Value::String(value.clone()),
        }
    }

    fn from_query(
        schema: &NodeTableSchema,
        key_index: usize,
        key_column: &str,
        key: &Value,
    ) -> DevonResult<Self> {
        match (schema.columns()[key_index].ty, key) {
            (LogicalType::Int64, Value::Int64(value)) => Ok(Self::Int64(*value)),
            (LogicalType::String, Value::String(value)) => Ok(Self::String(value.clone())),
            (expected, _) => Err(invalid_argument(format!(
                "primary key `{key_column}` in node table `{}` expects {expected} but received {key}",
                schema.name()
            ))),
        }
    }

    fn from_row(table: &str, row: &[Value], key_index: usize) -> DevonResult<Self> {
        match row.get(key_index) {
            Some(Value::Int64(value)) => Ok(Self::Int64(*value)),
            Some(Value::String(value)) => Ok(Self::String(value.clone())),
            Some(value) => Err(corrupt(format!(
                "node table `{table}` overlay has invalid primary key {value}"
            ))),
            None => Err(corrupt(format!(
                "node table `{table}` overlay row is missing its primary key"
            ))),
        }
    }
}

struct OverlayKeyIndex {
    state: Weak<PublishedState>,
    table: String,
    positions: BTreeMap<OverlayKey, usize>,
    row_count: usize,
}

// Endpoint resolution is a scalar API invoked repeatedly by the commit
// pipeline. This small process-wide cache avoids rebuilding the same immutable
// state's index per endpoint; the mutex never participates in snapshot reads,
// and the fixed entry cap bounds stale-state retention.
static OVERLAY_KEY_INDEXES: Mutex<Vec<OverlayKeyIndex>> = Mutex::new(Vec::new());
const OVERLAY_KEY_INDEX_CACHE_ENTRIES: usize = 8;

fn overlay_key_position(
    view: &ReadView,
    table: &str,
    key_index: usize,
    key: &OverlayKey,
) -> DevonResult<(Option<usize>, usize)> {
    let mut indexes = OVERLAY_KEY_INDEXES
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    indexes.retain(|index| index.state.strong_count() > 0);
    let state = Arc::downgrade(&view.state);
    let position = indexes
        .iter()
        .position(|index| Weak::ptr_eq(&index.state, &state) && index.table == table);
    let index = match position {
        Some(position) => &indexes[position],
        None => {
            if indexes.len() == OVERLAY_KEY_INDEX_CACHE_ENTRIES {
                indexes.remove(0);
            }
            indexes.push(build_overlay_key_index(view, table, key_index)?);
            indexes
                .last()
                .ok_or_else(|| corrupt("overlay key index disappeared after insertion"))?
        }
    };
    Ok((index.positions.get(key).copied(), index.row_count))
}

fn build_overlay_key_index(
    view: &ReadView,
    table: &str,
    key_index: usize,
) -> DevonResult<OverlayKeyIndex> {
    let mut index = OverlayKeyIndex {
        state: Arc::downgrade(&view.state),
        table: table.to_owned(),
        positions: BTreeMap::new(),
        row_count: 0,
    };
    for slot in view.state.node_slots(table) {
        if let Some(row) = slot.row {
            let key = OverlayKey::from_row(table, row, key_index)?;
            index.positions.entry(key).or_insert(slot.position);
        }
        index.row_count = slot.position.checked_add(1).ok_or_else(|| {
            corrupt(format!(
                "node table `{table}` overlay row count exceeds usize::MAX"
            ))
        })?;
    }
    Ok(index)
}

pub(super) fn primary_key(schema: &NodeTableSchema) -> DevonResult<(usize, &str)> {
    schema
        .columns()
        .iter()
        .enumerate()
        .find(|(_, column)| column.primary_key)
        .map(|(index, column)| (index, column.name.as_str()))
        .ok_or_else(|| corrupt(format!("node table `{}` has no primary key", schema.name())))
}

struct Pipeline {
    source: Box<dyn ChunkSource>,
    columns: HashMap<String, usize>,
    types: HashMap<String, LogicalType>,
    offsets: HashMap<String, (String, usize)>,
    binding_tables: HashMap<String, String>,
    classes: HashMap<String, (String, usize)>,
    scores: HashMap<String, (String, usize)>,
    width: usize,
}

impl Pipeline {
    fn scan(
        source: Box<dyn ChunkSource>,
        schema: &NodeTableSchema,
        binding: &str,
        projection: Option<&BTreeSet<usize>>,
    ) -> DevonResult<Self> {
        let binding_key = fold(binding).into_owned();
        let scan = scan_binding(schema, binding, projection)?;
        let mut columns = scan.columns;
        let mut types = scan.types;
        let offset_key = unique_offset_key(&binding_key, &columns);
        let offset_index = scan.indices.len();
        columns.insert(offset_key.clone(), offset_index);
        types.insert(offset_key.clone(), LogicalType::Int64);
        Ok(Self {
            source,
            columns,
            types,
            offsets: HashMap::from([(binding_key, (offset_key, offset_index))]),
            binding_tables: HashMap::from([(fold(binding).into_owned(), schema.name().to_owned())]),
            classes: HashMap::new(),
            scores: HashMap::new(),
            width: scan.indices.len() + 1,
        })
    }

    fn interface_scan(
        source: Box<dyn ChunkSource>,
        interface: &devondb_storage::catalog::InterfaceEntry,
        binding: &str,
        emit: &[usize],
    ) -> DevonResult<Self> {
        let binding_key = fold(binding).into_owned();
        let mut columns = emit
            .iter()
            .enumerate()
            .map(|(position, index)| {
                (
                    format!("{binding_key}.{}", fold(&interface.columns[*index].name)),
                    position,
                )
            })
            .collect::<HashMap<_, _>>();
        let mut types = emit
            .iter()
            .map(|&index| {
                (
                    format!("{binding_key}.{}", fold(&interface.columns[index].name)),
                    interface.columns[index].ty,
                )
            })
            .collect::<HashMap<_, _>>();
        let class_key = classof_column_key(&binding_key);
        let class_index = emit.len();
        columns.insert(class_key.clone(), class_index);
        types.insert(class_key.clone(), LogicalType::String);
        Ok(Self {
            source,
            columns,
            types,
            offsets: HashMap::new(),
            binding_tables: HashMap::new(),
            classes: HashMap::from([(binding_key, (class_key, class_index))]),
            scores: HashMap::new(),
            width: emit.len() + 1,
        })
    }

    /// Builds a KnnScan pipeline: the table's scan layout plus one trailing
    /// output-only `distance` column (PLAN_IR.md § knn semantics — the name
    /// contains no `.` so no expression column reference can ever shadow it).
    fn knn(
        source: Box<dyn ChunkSource>,
        schema: &NodeTableSchema,
        binding: &str,
        projection: Option<&BTreeSet<usize>>,
    ) -> DevonResult<Self> {
        let scan = Self::scan(source, schema, binding, projection)?;
        let columns = scan
            .columns
            .into_iter()
            .chain([(DISTANCE_COLUMN_NAME.to_owned(), scan.width)])
            .collect();
        let types = scan
            .types
            .into_iter()
            .chain([(DISTANCE_COLUMN_NAME.to_owned(), LogicalType::Float64)])
            .collect();
        Ok(Self {
            source: scan.source,
            columns,
            types,
            offsets: scan.offsets,
            binding_tables: scan.binding_tables,
            classes: scan.classes,
            scores: scan.scores,
            width: scan.width + 1,
        })
    }

    fn with_filter(
        self,
        predicate: Expr,
        executor: Arc<dyn ScalarSubqueryExecutor>,
    ) -> DevonResult<Self> {
        let resolved_columns = resolved_expression_map(&predicate, &self.columns)?;
        Ok(Self {
            source: Box::new(
                Filter::new(self.source, predicate, resolved_columns)
                    .with_scalar_executor(executor),
            ),
            columns: self.columns,
            types: self.types,
            offsets: self.offsets,
            binding_tables: self.binding_tables,
            classes: self.classes,
            scores: self.scores,
            width: self.width,
        })
    }

    fn with_project(
        self,
        exprs: &[ProjectionItem],
        catalog: &Catalog,
        executor: Arc<dyn ScalarSubqueryExecutor>,
    ) -> DevonResult<Self> {
        let mut resolved_types = self.types.clone();
        let mut resolved_columns = self.columns.clone();
        for item in exprs {
            add_resolved_expression(&item.expr, &self.types, &mut resolved_types)?;
            add_resolved_expression(&item.expr, &self.columns, &mut resolved_columns)?;
        }
        let mut output_types = projection_types(exprs, &resolved_types, catalog)?;
        let projected_width = output_types.len();
        let (columns, types) = projected_metadata(exprs, &output_types);
        let mut columns = fold_map_keys(columns);
        let mut types = fold_map_keys(types);
        let internal_offsets = self.internal_offsets();
        let internal_classes = self.internal_classes();
        let mut internal_scores: Vec<_> = self
            .scores
            .iter()
            .map(|(binding, (key, index))| (binding.clone(), key.clone(), *index))
            .collect();
        internal_scores.sort_unstable_by_key(|(_, _, index)| *index);
        let mut expressions = exprs
            .iter()
            .map(|item| (item.expr.clone(), item.alias.clone()))
            .collect::<Vec<_>>();
        let mut offsets = HashMap::with_capacity(internal_offsets.len());
        for (index, (binding, old_key, _)) in internal_offsets.iter().enumerate() {
            let key = unique_offset_key(binding, &columns);
            expressions.push((Expr::Col(old_key.clone()), key.clone()));
            output_types.push(LogicalType::Int64);
            columns.insert(key.clone(), projected_width + index);
            types.insert(key.clone(), LogicalType::Int64);
            offsets.insert(binding.clone(), (key, projected_width + index));
        }
        let class_start = projected_width + internal_offsets.len();
        let mut classes = HashMap::with_capacity(internal_classes.len());
        for (index, (binding, key, _)) in internal_classes.iter().enumerate() {
            expressions.push((Expr::Col(key.clone()), key.clone()));
            output_types.push(LogicalType::String);
            columns.insert(key.clone(), class_start + index);
            types.insert(key.clone(), LogicalType::String);
            classes.insert(binding.clone(), (key.clone(), class_start + index));
        }
        let score_start = class_start + internal_classes.len();
        let mut scores = HashMap::with_capacity(internal_scores.len());
        for (index, (binding, key, _)) in internal_scores.iter().enumerate() {
            expressions.push((Expr::Col(key.clone()), key.clone()));
            output_types.push(LogicalType::Float64);
            columns.insert(key.clone(), score_start + index);
            types.insert(key.clone(), LogicalType::Float64);
            scores.insert(binding.clone(), (key.clone(), score_start + index));
        }
        Ok(Self {
            source: Box::new(
                Project::new(self.source, expressions, resolved_columns, output_types)?
                    .with_scalar_executor(executor),
            ),
            columns,
            types,
            offsets,
            binding_tables: self.binding_tables,
            classes,
            scores,
            width: projected_width
                + internal_offsets.len()
                + internal_classes.len()
                + internal_scores.len(),
        })
    }

    fn with_limit(self, count: u64, offset: u64) -> Self {
        Self {
            source: Box::new(Limit::new(self.source, count, offset)),
            columns: self.columns,
            types: self.types,
            offsets: self.offsets,
            binding_tables: self.binding_tables,
            classes: self.classes,
            scores: self.scores,
            width: self.width,
        }
    }

    fn with_sort(
        self,
        keys: &[SortKey],
        spill_config: SpillConfig,
        executor: Arc<dyn ScalarSubqueryExecutor>,
    ) -> DevonResult<Self> {
        let sort_keys = keys
            .iter()
            .map(|key| (key.expr.clone(), key.order))
            .collect();
        let mut columns = self.columns;
        for key in keys {
            let canonical = columns.clone();
            add_resolved_expression(&key.expr, &canonical, &mut columns)?;
        }
        Ok(Self {
            source: Box::new(
                Sort::new(self.source, sort_keys, columns.clone(), spill_config)
                    .with_scalar_executor(executor),
            ),
            columns,
            types: self.types,
            offsets: self.offsets,
            binding_tables: self.binding_tables,
            classes: self.classes,
            scores: self.scores,
            width: self.width,
        })
    }

    fn with_aggregate(
        self,
        group_by: &[Expr],
        aggs: &[AggregateItem],
        spill_config: SpillConfig,
        catalog: &Catalog,
        executor: Arc<dyn ScalarSubqueryExecutor>,
    ) -> DevonResult<Self> {
        let mut resolved_types = self.types.clone();
        let mut resolved_columns = self.columns.clone();
        for expression in group_by
            .iter()
            .chain(aggs.iter().map(|aggregate| &aggregate.expr))
        {
            add_resolved_expression(expression, &self.types, &mut resolved_types)?;
            add_resolved_expression(expression, &self.columns, &mut resolved_columns)?;
        }
        let output_types = aggregate_types(group_by, aggs, &resolved_types, catalog)?;
        let (columns, types) = folded_aggregate_metadata(group_by, aggs, &output_types)?;
        let aggregates = aggs
            .iter()
            .map(|aggregate| (aggregate.function, aggregate.expr.clone()))
            .collect();
        let width = output_types.len();
        Ok(Self {
            source: Box::new(
                Aggregate::new(
                    self.source,
                    group_by.to_vec(),
                    aggregates,
                    resolved_columns,
                    output_types,
                    spill_config,
                )
                .with_scalar_executor(executor),
            ),
            columns,
            types,
            offsets: HashMap::new(),
            binding_tables: HashMap::new(),
            classes: HashMap::new(),
            scores: HashMap::new(),
            width,
        })
    }

    fn with_hash_join(
        self,
        right: Self,
        on: &[JoinKey],
        join: JoinType,
        budget: Arc<MemoryBudget>,
    ) -> DevonResult<Self> {
        let mut left_layout_columns = self.columns.clone();
        let mut right_layout_columns = right.columns.clone();
        for key in on {
            add_resolved_expression(&key.left, &self.columns, &mut left_layout_columns)?;
            add_resolved_expression(&key.right, &right.columns, &mut right_layout_columns)?;
        }
        let left_layout = JoinLayout::new(left_layout_columns, self.ordered_types()?);
        let right_layout = JoinLayout::new(right_layout_columns, right.ordered_types()?);

        let right_offset = self.width;
        let width = self
            .width
            .checked_add(right.width)
            .ok_or_else(|| corrupt("join output width exceeds usize::MAX"))?;
        let columns = self
            .columns
            .into_iter()
            .chain(
                right
                    .columns
                    .into_iter()
                    .map(|(name, index)| (name, index + right_offset)),
            )
            .collect();
        let types = self.types.into_iter().chain(right.types).collect();
        let offsets = self
            .offsets
            .into_iter()
            .chain(
                right
                    .offsets
                    .into_iter()
                    .map(|(binding, (name, index))| (binding, (name, index + right_offset))),
            )
            .collect();
        let binding_tables = self
            .binding_tables
            .into_iter()
            .chain(right.binding_tables)
            .collect();
        let classes = self
            .classes
            .into_iter()
            .chain(
                right
                    .classes
                    .into_iter()
                    .map(|(binding, (name, index))| (binding, (name, index + right_offset))),
            )
            .collect();
        let scores = self
            .scores
            .into_iter()
            .chain(
                right
                    .scores
                    .into_iter()
                    .map(|(binding, (name, index))| (binding, (name, index + right_offset))),
            )
            .collect();
        Ok(Self {
            source: Box::new(HashJoin::new(
                self.source,
                right.source,
                on.to_vec(),
                join,
                left_layout,
                right_layout,
                budget,
            )),
            columns,
            types,
            offsets,
            binding_tables,
            classes,
            scores,
            width,
        })
    }

    fn ordered_types(&self) -> DevonResult<Vec<LogicalType>> {
        let mut ordered = vec![None; self.width];
        for (name, index) in &self.columns {
            let ty =
                self.types.get(name).copied().ok_or_else(|| {
                    corrupt(format!("pipeline column `{name}` has no physical type"))
                })?;
            let slot = ordered.get_mut(*index).ok_or_else(|| {
                corrupt(format!(
                    "pipeline column `{name}` has out-of-range physical index {index}"
                ))
            })?;
            if slot.is_some_and(|existing| existing != ty) {
                return Err(corrupt(format!(
                    "pipeline physical column {index} has conflicting types"
                )));
            }
            *slot = Some(ty);
        }
        ordered
            .into_iter()
            .enumerate()
            .map(|(index, ty)| {
                ty.ok_or_else(|| corrupt(format!("pipeline physical column {index} has no type")))
            })
            .collect()
    }

    fn with_expand(
        self,
        neighbors: ExpandGraphSnapshot,
        rel: &str,
        direction: Direction,
        from_binding: &str,
        binding: &str,
        neighbor_schema: &NodeTableSchema,
    ) -> DevonResult<Self> {
        let offset_column = self
            .offsets
            .get(fold(from_binding).as_ref())
            .map(|(_, index)| *index)
            .ok_or_else(|| {
                invalid_argument(format!(
                    "Expand from_binding `{from_binding}` has no node offset in its input"
                ))
            })?;
        let neighbor_types = neighbor_schema
            .columns()
            .iter()
            .map(|column| column.ty)
            .collect::<Vec<_>>();
        let mut columns = self.columns;
        let mut types = self.types;
        let binding_key = fold(binding).into_owned();
        for (index, column) in neighbor_schema.columns().iter().enumerate() {
            columns.insert(
                format!("{binding_key}.{}", fold(&column.name)),
                self.width + index + 1,
            );
            types.insert(format!("{binding_key}.{}", fold(&column.name)), column.ty);
        }
        let offset_key = unique_offset_key(&binding_key, &columns);
        columns.insert(offset_key.clone(), self.width);
        types.insert(offset_key.clone(), LogicalType::Int64);
        let mut offsets = self.offsets;
        offsets.insert(binding_key.clone(), (offset_key, self.width));
        let mut binding_tables = self.binding_tables;
        binding_tables.insert(binding_key, neighbor_schema.name().to_owned());
        Ok(Self {
            source: Box::new(Expand::new(
                self.source,
                Box::new(neighbors),
                rel.to_owned(),
                direction,
                offset_column,
                neighbor_types,
            )),
            columns,
            types,
            offsets,
            binding_tables,
            classes: self.classes,
            scores: self.scores,
            width: self.width + neighbor_schema.columns().len() + 1,
        })
    }

    fn binding_table(&self, binding: &str) -> DevonResult<&str> {
        self.binding_tables
            .get(fold(binding).as_ref())
            .map(String::as_str)
            .ok_or_else(|| {
                invalid_argument(format!(
                    "Expand from_binding `{binding}` has no node table in its input"
                ))
            })
    }

    fn internal_columns(&self) -> Vec<usize> {
        let mut columns = self
            .internal_offsets()
            .into_iter()
            .map(|(_, _, index)| index)
            .collect::<Vec<_>>();
        columns.extend(
            self.internal_classes()
                .into_iter()
                .map(|(_, _, index)| index),
        );
        columns.extend(self.scores.values().map(|(_, index)| *index));
        columns.sort_unstable();
        columns.dedup();
        columns
    }

    fn internal_offsets(&self) -> Vec<(String, String, usize)> {
        // Offset columns are tracked by provenance, not by a forgeable name
        // suffix: project aliases are arbitrary strings in DevonPlan. Each
        // private key is also collision-checked against the current columns.
        let mut offsets = self
            .offsets
            .iter()
            .map(|(binding, (key, index))| (binding.clone(), key.clone(), *index))
            .collect::<Vec<_>>();
        offsets.sort_unstable_by_key(|(_, _, index)| *index);
        offsets
    }

    fn internal_classes(&self) -> Vec<(String, String, usize)> {
        let mut classes = self
            .classes
            .iter()
            .map(|(binding, (key, index))| (binding.clone(), key.clone(), *index))
            .collect::<Vec<_>>();
        classes.sort_unstable_by_key(|(_, _, index)| *index);
        classes
    }
}

fn resolved_expression_map<V: Copy>(
    expression: &Expr,
    columns: &HashMap<String, V>,
) -> DevonResult<HashMap<String, V>> {
    let mut resolved = columns.clone();
    add_resolved_expression(expression, columns, &mut resolved)?;
    Ok(resolved)
}

fn add_resolved_expression<V: Copy>(
    expression: &Expr,
    canonical: &HashMap<String, V>,
    resolved: &mut HashMap<String, V>,
) -> DevonResult<()> {
    match expression {
        Expr::Col(reference) => {
            if let Some(value) = canonical.get(fold(reference).as_ref()) {
                resolved.insert(reference.clone(), *value);
            }
            Ok(())
        }
        // classof names a binding, never a column reference — leaf here.
        Expr::ClassOf(_) | Expr::ScoreOf(_) => Ok(()),
        Expr::Binary { left, right, .. } | Expr::Distance { left, right, .. } => {
            add_resolved_expression(left, canonical, resolved)?;
            add_resolved_expression(right, canonical, resolved)
        }
        Expr::If {
            cond,
            then_expr,
            else_expr,
        } => {
            add_resolved_expression(cond, canonical, resolved)?;
            add_resolved_expression(then_expr, canonical, resolved)?;
            add_resolved_expression(else_expr, canonical, resolved)
        }
        Expr::Coalesce(expressions) | Expr::Least(expressions) | Expr::Greatest(expressions) => {
            for expression in expressions {
                add_resolved_expression(expression, canonical, resolved)?;
            }
            Ok(())
        }
        Expr::DateTrunc { value, .. } | Expr::Round { value, .. } => {
            add_resolved_expression(value, canonical, resolved)
        }
        Expr::DateAdd { value, amount, .. } => {
            add_resolved_expression(value, canonical, resolved)?;
            add_resolved_expression(amount, canonical, resolved)
        }
        Expr::RoundDiv {
            numerator,
            denominator,
            ..
        } => {
            add_resolved_expression(numerator, canonical, resolved)?;
            add_resolved_expression(denominator, canonical, resolved)
        }
        // Correlated references resolve against the outer map; the embedded
        // tree's own scope never folds to an outer binding (PLAN_IR
        // correlation law), and unresolvable names are skipped by the Col
        // arm, so a blanket walk over-registers nothing.
        Expr::Scalar { plan } => add_resolved_operator_expressions(plan, canonical, resolved),
        Expr::Not(operand) => add_resolved_expression(operand, canonical, resolved),
        Expr::Lit(_) => Ok(()),
    }
}

fn add_resolved_operator_expressions<V: Copy>(
    operator: &Operator,
    canonical: &HashMap<String, V>,
    resolved: &mut HashMap<String, V>,
) -> DevonResult<()> {
    match operator {
        Operator::ScanNodes { .. }
        | Operator::ScanInterface { .. }
        | Operator::KnnScan { .. }
        | Operator::TextScan { .. }
        | Operator::WithinScan { .. } => Ok(()),
        Operator::ExpandRel { input, .. }
        | Operator::Expand { input, .. }
        | Operator::Limit { input, .. } => {
            add_resolved_operator_expressions(input, canonical, resolved)
        }
        Operator::Filter { predicate, input } => {
            add_resolved_expression(predicate, canonical, resolved)?;
            add_resolved_operator_expressions(input, canonical, resolved)
        }
        Operator::Project { exprs, input } => {
            for item in exprs {
                add_resolved_expression(&item.expr, canonical, resolved)?;
            }
            add_resolved_operator_expressions(input, canonical, resolved)
        }
        Operator::Sort { keys, input } => {
            for key in keys {
                add_resolved_expression(&key.expr, canonical, resolved)?;
            }
            add_resolved_operator_expressions(input, canonical, resolved)
        }
        Operator::Aggregate {
            group_by,
            aggs,
            input,
        } => {
            for expression in group_by {
                add_resolved_expression(expression, canonical, resolved)?;
            }
            for aggregate in aggs {
                add_resolved_expression(&aggregate.expr, canonical, resolved)?;
            }
            add_resolved_operator_expressions(input, canonical, resolved)
        }
        Operator::HashJoin {
            on, left, right, ..
        } => {
            for key in on {
                add_resolved_expression(&key.left, canonical, resolved)?;
                add_resolved_expression(&key.right, canonical, resolved)?;
            }
            add_resolved_operator_expressions(left, canonical, resolved)?;
            add_resolved_operator_expressions(right, canonical, resolved)
        }
    }
}

fn fold_map_keys<V>(map: HashMap<String, V>) -> HashMap<String, V> {
    map.into_iter()
        .map(|(name, value)| (fold(&name).into_owned(), value))
        .collect()
}

fn folded_aggregate_metadata(
    group_by: &[Expr],
    aggs: &[AggregateItem],
    output_types: &[LogicalType],
) -> DevonResult<(HashMap<String, usize>, HashMap<String, LogicalType>)> {
    let (base_columns, base_types) = aggregate_metadata(group_by, aggs, output_types)?;
    let mut display_names = group_by
        .iter()
        .map(canonical_expression)
        .collect::<DevonResult<Vec<_>>>()?;
    display_names.extend(aggs.iter().map(|aggregate| aggregate.alias.clone()));
    let mut lookup_names = group_by
        .iter()
        .map(|expression| {
            fold_expression_identifiers(expression).and_then(|folded| canonical_expression(&folded))
        })
        .collect::<DevonResult<Vec<_>>>()?;
    lookup_names.extend(
        aggs.iter()
            .map(|aggregate| fold(&aggregate.alias).into_owned()),
    );
    let mut columns = HashMap::with_capacity(lookup_names.len());
    let mut types = HashMap::with_capacity(lookup_names.len());
    for (display_name, lookup_name) in display_names.into_iter().zip(lookup_names) {
        let index = base_columns
            .get(&display_name)
            .copied()
            .ok_or_else(|| corrupt("aggregate output metadata lost a column"))?;
        let ty = base_types
            .get(&display_name)
            .copied()
            .ok_or_else(|| corrupt("aggregate output metadata lost a type"))?;
        columns.insert(lookup_name.clone(), index);
        types.insert(lookup_name, ty);
    }
    Ok((columns, types))
}

fn fold_expression_identifiers(expression: &Expr) -> DevonResult<Expr> {
    match expression {
        Expr::Col(reference) => Ok(Expr::Col(fold(reference).into_owned())),
        Expr::ClassOf(binding) => Ok(Expr::ClassOf(fold(binding).into_owned())),
        Expr::ScoreOf(binding) => Ok(Expr::ScoreOf(fold(binding).into_owned())),
        Expr::Lit(value) => Ok(Expr::Lit(value.clone())),
        Expr::Binary { op, left, right } => Ok(Expr::Binary {
            op: *op,
            left: Box::new(fold_expression_identifiers(left)?),
            right: Box::new(fold_expression_identifiers(right)?),
        }),
        Expr::Not(operand) => Ok(Expr::Not(Box::new(fold_expression_identifiers(operand)?))),
        Expr::Distance {
            left,
            right,
            metric,
        } => Ok(Expr::Distance {
            left: Box::new(fold_expression_identifiers(left)?),
            right: Box::new(fold_expression_identifiers(right)?),
            metric: *metric,
        }),
        Expr::If {
            cond,
            then_expr,
            else_expr,
        } => Ok(Expr::If {
            cond: Box::new(fold_expression_identifiers(cond)?),
            then_expr: Box::new(fold_expression_identifiers(then_expr)?),
            else_expr: Box::new(fold_expression_identifiers(else_expr)?),
        }),
        Expr::Coalesce(expressions) => Ok(Expr::Coalesce(fold_expressions(expressions)?)),
        Expr::Least(expressions) => Ok(Expr::Least(fold_expressions(expressions)?)),
        Expr::Greatest(expressions) => Ok(Expr::Greatest(fold_expressions(expressions)?)),
        Expr::DateTrunc { unit, value } => Ok(Expr::DateTrunc {
            unit: *unit,
            value: Box::new(fold_expression_identifiers(value)?),
        }),
        Expr::DateAdd {
            unit,
            value,
            amount,
        } => Ok(Expr::DateAdd {
            unit: *unit,
            value: Box::new(fold_expression_identifiers(value)?),
            amount: Box::new(fold_expression_identifiers(amount)?),
        }),
        Expr::Round { value, places } => Ok(Expr::Round {
            value: Box::new(fold_expression_identifiers(value)?),
            places: *places,
        }),
        Expr::RoundDiv {
            numerator,
            denominator,
            places,
        } => Ok(Expr::RoundDiv {
            numerator: Box::new(fold_expression_identifiers(numerator)?),
            denominator: Box::new(fold_expression_identifiers(denominator)?),
            places: *places,
        }),
        Expr::Scalar { plan } => Ok(Expr::Scalar {
            plan: Box::new(fold_operator_expression_identifiers(plan)?),
        }),
    }
}

fn fold_operator_expression_identifiers(operator: &Operator) -> DevonResult<Operator> {
    let mut folded = operator.clone();
    fold_operator_expressions_in_place(&mut folded)?;
    Ok(folded)
}

fn fold_operator_expressions_in_place(operator: &mut Operator) -> DevonResult<()> {
    match operator {
        Operator::ScanNodes { .. }
        | Operator::ScanInterface { .. }
        | Operator::KnnScan { .. }
        | Operator::TextScan { .. }
        | Operator::WithinScan { .. } => Ok(()),
        Operator::ExpandRel { input, .. }
        | Operator::Expand { input, .. }
        | Operator::Limit { input, .. } => fold_operator_expressions_in_place(input),
        Operator::Filter { predicate, input } => {
            *predicate = fold_expression_identifiers(predicate)?;
            fold_operator_expressions_in_place(input)
        }
        Operator::Project { exprs, input } => {
            for item in exprs.iter_mut() {
                item.expr = fold_expression_identifiers(&item.expr)?;
            }
            fold_operator_expressions_in_place(input)
        }
        Operator::Sort { keys, input } => {
            for key in keys.iter_mut() {
                key.expr = fold_expression_identifiers(&key.expr)?;
            }
            fold_operator_expressions_in_place(input)
        }
        Operator::Aggregate {
            group_by,
            aggs,
            input,
        } => {
            for expression in group_by.iter_mut() {
                *expression = fold_expression_identifiers(expression)?;
            }
            for aggregate in aggs.iter_mut() {
                aggregate.expr = fold_expression_identifiers(&aggregate.expr)?;
            }
            fold_operator_expressions_in_place(input)
        }
        Operator::HashJoin {
            on, left, right, ..
        } => {
            for key in on.iter_mut() {
                key.left = fold_expression_identifiers(&key.left)?;
                key.right = fold_expression_identifiers(&key.right)?;
            }
            fold_operator_expressions_in_place(left)?;
            fold_operator_expressions_in_place(right)
        }
    }
}

fn fold_expressions(expressions: &[Expr]) -> DevonResult<Vec<Expr>> {
    expressions
        .iter()
        .map(fold_expression_identifiers)
        .collect()
}

/// The facade's same-snapshot scalar-subquery executor.
///
/// Holds a full copy of the containing query's [`ReadView`] (all shared
/// state is `Arc`-backed), so every embedded run sees exactly the outer
/// query's snapshot, own-transaction delta, and budget — the PLAN_IR
/// same-snapshot law.
struct ViewScalarExecutor {
    view: ReadView,
}

fn scalar_executor(view: &ReadView) -> Arc<dyn ScalarSubqueryExecutor> {
    Arc::new(ViewScalarExecutor { view: view.clone() })
}

impl ScalarSubqueryExecutor for ViewScalarExecutor {
    fn memory_budget(&self) -> Arc<MemoryBudget> {
        Arc::clone(&self.view.shared.budget)
    }

    fn execute(&self, plan: &Operator, outer: &OuterBindings) -> DevonResult<ScalarSubqueryResult> {
        let substituted = substitute_outer_bindings(plan, outer)?;
        let output_type = expression_type(
            &Expr::Scalar {
                plan: Box::new(substituted.clone()),
            },
            &HashMap::new(),
            &self.view.catalog,
        )?
        .output_type();
        let embedded = Plan {
            v: PLAN_VERSION,
            plan: Operator::Limit {
                count: 2,
                offset: None,
                input: Box::new(substituted),
            },
        };
        let result = run_view(&self.view, &embedded)?;
        let values = result
            .rows
            .into_iter()
            .take(2)
            .map(|mut row| {
                if row.len() == 1 {
                    Ok(row.remove(0))
                } else {
                    Err(invalid_argument(format!(
                        "scalar subquery must expose exactly one output column; got {}",
                        row.len()
                    )))
                }
            })
            .collect::<DevonResult<Vec<_>>>()?;
        Ok(ScalarSubqueryResult {
            output_type,
            values,
        })
    }
}

/// Rewrites correlated outer references to literals for one outer row.
///
/// `outer` keys are the containing pipeline's folded canonical
/// `binding.column` references, so matching folds the embedded spelling
/// (exact matching after canonical folding — never fuzzy).
fn substitute_outer_bindings(operator: &Operator, outer: &OuterBindings) -> DevonResult<Operator> {
    let mut substituted = operator.clone();
    substitute_operator_expressions(&mut substituted, outer)?;
    Ok(substituted)
}

fn substitute_operator_expressions(
    operator: &mut Operator,
    outer: &OuterBindings,
) -> DevonResult<()> {
    match operator {
        Operator::ScanNodes { .. }
        | Operator::ScanInterface { .. }
        | Operator::KnnScan { .. }
        | Operator::TextScan { .. }
        | Operator::WithinScan { .. } => Ok(()),
        Operator::ExpandRel { input, .. }
        | Operator::Expand { input, .. }
        | Operator::Limit { input, .. } => substitute_operator_expressions(input, outer),
        Operator::Filter { predicate, input } => {
            *predicate = substitute_expression(predicate, outer)?;
            substitute_operator_expressions(input, outer)
        }
        Operator::Project { exprs, input } => {
            for item in exprs.iter_mut() {
                item.expr = substitute_expression(&item.expr, outer)?;
            }
            substitute_operator_expressions(input, outer)
        }
        Operator::Sort { keys, input } => {
            for key in keys.iter_mut() {
                key.expr = substitute_expression(&key.expr, outer)?;
            }
            substitute_operator_expressions(input, outer)
        }
        Operator::Aggregate {
            group_by,
            aggs,
            input,
        } => {
            for expression in group_by.iter_mut() {
                *expression = substitute_expression(expression, outer)?;
            }
            for aggregate in aggs.iter_mut() {
                aggregate.expr = substitute_expression(&aggregate.expr, outer)?;
            }
            substitute_operator_expressions(input, outer)
        }
        Operator::HashJoin {
            on, left, right, ..
        } => {
            for key in on.iter_mut() {
                key.left = substitute_expression(&key.left, outer)?;
                key.right = substitute_expression(&key.right, outer)?;
            }
            substitute_operator_expressions(left, outer)?;
            substitute_operator_expressions(right, outer)
        }
    }
}

fn substitute_expression(expression: &Expr, outer: &OuterBindings) -> DevonResult<Expr> {
    match expression {
        Expr::Col(reference) => Ok(match outer.get(fold(reference).as_ref()) {
            Some(value) => Expr::Lit(value.clone()),
            None => expression.clone(),
        }),
        Expr::ScoreOf(binding) => Ok(match outer.get(&scoreof_column_key(binding)) {
            Some(value) => Expr::Lit(value.clone()),
            None => expression.clone(),
        }),
        Expr::ClassOf(binding) => Ok(match outer.get(&classof_column_key(binding)) {
            Some(value) => Expr::Lit(value.clone()),
            None => expression.clone(),
        }),
        Expr::Lit(_) => Ok(expression.clone()),
        Expr::Binary { op, left, right } => Ok(Expr::Binary {
            op: *op,
            left: Box::new(substitute_expression(left, outer)?),
            right: Box::new(substitute_expression(right, outer)?),
        }),
        Expr::Not(operand) => Ok(Expr::Not(Box::new(substitute_expression(operand, outer)?))),
        Expr::Distance {
            left,
            right,
            metric,
        } => Ok(Expr::Distance {
            left: Box::new(substitute_expression(left, outer)?),
            right: Box::new(substitute_expression(right, outer)?),
            metric: *metric,
        }),
        Expr::If {
            cond,
            then_expr,
            else_expr,
        } => Ok(Expr::If {
            cond: Box::new(substitute_expression(cond, outer)?),
            then_expr: Box::new(substitute_expression(then_expr, outer)?),
            else_expr: Box::new(substitute_expression(else_expr, outer)?),
        }),
        Expr::Coalesce(expressions) => {
            Ok(Expr::Coalesce(substitute_expressions(expressions, outer)?))
        }
        Expr::Least(expressions) => Ok(Expr::Least(substitute_expressions(expressions, outer)?)),
        Expr::Greatest(expressions) => {
            Ok(Expr::Greatest(substitute_expressions(expressions, outer)?))
        }
        Expr::DateTrunc { unit, value } => Ok(Expr::DateTrunc {
            unit: *unit,
            value: Box::new(substitute_expression(value, outer)?),
        }),
        Expr::DateAdd {
            unit,
            value,
            amount,
        } => Ok(Expr::DateAdd {
            unit: *unit,
            value: Box::new(substitute_expression(value, outer)?),
            amount: Box::new(substitute_expression(amount, outer)?),
        }),
        Expr::Round { value, places } => Ok(Expr::Round {
            value: Box::new(substitute_expression(value, outer)?),
            places: *places,
        }),
        Expr::RoundDiv {
            numerator,
            denominator,
            places,
        } => Ok(Expr::RoundDiv {
            numerator: Box::new(substitute_expression(numerator, outer)?),
            denominator: Box::new(substitute_expression(denominator, outer)?),
            places: *places,
        }),
        Expr::Scalar { plan } => Ok(Expr::Scalar {
            plan: Box::new(substitute_outer_bindings(plan, outer)?),
        }),
    }
}

fn substitute_expressions(expressions: &[Expr], outer: &OuterBindings) -> DevonResult<Vec<Expr>> {
    expressions
        .iter()
        .map(|expression| substitute_expression(expression, outer))
        .collect()
}

#[derive(Clone)]
enum ZoneMapFilter {
    Scalar {
        column: usize,
        op: BinaryOp,
        literal: Value,
    },
    GeoPoint {
        column: usize,
        covering_ranges: Vec<(u64, u64)>,
    },
}

impl ZoneMapFilter {
    fn from_expression(expression: &Expr, schema: &NodeTableSchema, binding: &str) -> Option<Self> {
        let Expr::Binary { op, left, right } = expression else {
            return None;
        };
        if !is_comparison(*op) {
            return None;
        }
        let (reference, literal, op) = match (left.as_ref(), right.as_ref()) {
            (Expr::Col(reference), Expr::Lit(literal)) => (reference, literal, *op),
            (Expr::Lit(literal), Expr::Col(reference)) => {
                (reference, literal, reverse_comparison(*op)?)
            }
            _ => return None,
        };
        let binding = fold(binding);
        let column = schema
            .columns()
            .iter()
            .position(|column| fold(reference) == format!("{binding}.{}", fold(&column.name)))?;
        let supported_literal = matches!(
            (schema.columns()[column].ty, literal),
            (LogicalType::Int64, Value::Int64(_)) | (LogicalType::Float64, Value::Float64(_))
        );
        supported_literal.then(|| Self::Scalar {
            column,
            op,
            literal: literal.clone(),
        })
    }

    fn for_geo_point(column: usize, covering_ranges: Vec<(u64, u64)>) -> Self {
        Self::GeoPoint {
            column,
            covering_ranges,
        }
    }

    fn excludes_group(&self, stats: &[ZoneMapStats]) -> bool {
        match self {
            Self::Scalar {
                column,
                op,
                literal,
            } => scalar_filter_excludes_group(stats, *column, *op, literal),
            Self::GeoPoint {
                column,
                covering_ranges,
            } => geo_filter_excludes_group(stats, *column, covering_ranges),
        }
    }
}

fn scalar_filter_excludes_group(
    stats: &[ZoneMapStats],
    column: usize,
    op: BinaryOp,
    literal: &Value,
) -> bool {
    let Some(stats) = stats.get(column) else {
        return false;
    };
    match (stats.min, stats.max, literal) {
        (Some(ZoneMapValue::Int64(min)), Some(ZoneMapValue::Int64(max)), Value::Int64(literal)) => {
            range_excludes(op, min, max, *literal)
        }
        (
            Some(ZoneMapValue::Float64(min)),
            Some(ZoneMapValue::Float64(max)),
            Value::Float64(literal),
        ) => float_range_excludes(op, min, max, *literal),
        _ => false,
    }
}

fn geo_filter_excludes_group(
    stats: &[ZoneMapStats],
    column: usize,
    covering_ranges: &[(u64, u64)],
) -> bool {
    let Some(stats) = stats.get(column) else {
        return false;
    };
    let (Some(ZoneMapValue::GeoPoint(min)), Some(ZoneMapValue::GeoPoint(max))) =
        (stats.min, stats.max)
    else {
        return false;
    };
    !covering_ranges
        .iter()
        .any(|(range_min, range_max)| min <= *range_max && *range_min <= max)
}

const fn is_comparison(op: BinaryOp) -> bool {
    matches!(
        op,
        BinaryOp::Eq | BinaryOp::Ne | BinaryOp::Lt | BinaryOp::Le | BinaryOp::Gt | BinaryOp::Ge
    )
}

const fn reverse_comparison(op: BinaryOp) -> Option<BinaryOp> {
    match op {
        BinaryOp::Eq => Some(BinaryOp::Eq),
        BinaryOp::Ne => Some(BinaryOp::Ne),
        BinaryOp::Lt => Some(BinaryOp::Gt),
        BinaryOp::Le => Some(BinaryOp::Ge),
        BinaryOp::Gt => Some(BinaryOp::Lt),
        BinaryOp::Ge => Some(BinaryOp::Le),
        BinaryOp::And
        | BinaryOp::Or
        | BinaryOp::Add
        | BinaryOp::Sub
        | BinaryOp::Mul
        | BinaryOp::Div => None,
    }
}

fn range_excludes<T>(op: BinaryOp, min: T, max: T, literal: T) -> bool
where
    T: Copy + PartialEq + PartialOrd,
{
    match op {
        BinaryOp::Eq => literal < min || literal > max,
        BinaryOp::Ne => min == literal && max == literal,
        BinaryOp::Lt => min >= literal,
        BinaryOp::Le => min > literal,
        BinaryOp::Gt => max <= literal,
        BinaryOp::Ge => max < literal,
        BinaryOp::And
        | BinaryOp::Or
        | BinaryOp::Add
        | BinaryOp::Sub
        | BinaryOp::Mul
        | BinaryOp::Div => false,
    }
}

fn float_range_excludes(op: BinaryOp, min: f64, max: f64, literal: f64) -> bool {
    if literal.is_nan() {
        return op != BinaryOp::Ne;
    }
    range_excludes(op, min, max, literal)
}

/// Process-global count of node groups decoded through the typed column scan
/// path ([`Database::typed_group_scan_count`]).
static TYPED_GROUP_SCANS: AtomicU64 = AtomicU64::new(0);

/// Whether a per-column typed decode must refuse this type: b1-rescore
/// sidecars pair with their main column only in the full-group decode
/// (`NodeGroup::read_column_typed` refuses them, as `read_column` does).
fn carries_b1_rescore(logical_type: LogicalType) -> bool {
    matches!(
        logical_type,
        LogicalType::VectorEncoded {
            encoding: VectorEncoding::B1 { rescore, .. },
            ..
        } if rescore != B1Rescore::None
    )
}

struct ScanSource {
    view: ReadView,
    schema: NodeTableSchema,
    group_ids: Vec<u64>,
    next_group: usize,
    current_group: Option<GroupScan>,
    overlay_rows: std::vec::IntoIter<OverlayScanRow>,
    next_offset: u64,
    /// Chunk column types: the decoded table columns (table order) plus the
    /// trailing hidden offset.
    types: Vec<LogicalType>,
    /// Full schema types, for storage reads (directory validation law).
    property_types: Vec<LogicalType>,
    /// Decoded table column indices in chunk order (ascending table order);
    /// the full range when pushdown keeps every column.
    columns: Vec<usize>,
    /// Position of the primary key column within `columns`.
    key_position: usize,
    /// Primary key table column index (full-width boxed/overlay rows).
    key_index: usize,
    zone_map_filter: Option<ZoneMapFilter>,
    effects: BTreeMap<OverlayKey, VisibleRowEffect>,
    shadowed_keys: BTreeSet<OverlayKey>,
    _charge: OwnedWorkingSetCharge,
}

/// One persisted node group mid-scan (`docs/SCALE.md` §6.5).
enum GroupScan {
    /// No overlay effect shadows any row of this group: its columns were
    /// decoded straight from payload bytes into typed storage, and an
    /// unconsumed group becomes exactly one chunk (a group never exceeds
    /// `CHUNK_CAPACITY` = `NODE_GROUP_CAPACITY` rows).
    ///
    /// Budget note (§6.6): the typed columns carry the same accounting rule
    /// the boxed path uses — `Column::approx_bytes` is `len × width` +
    /// bitmap words for fixed-width storage and per-value approx bytes for
    /// `Boxed` — applied at the existing scan working-set sites, which
    /// charge materialized rows. The decoded group itself is NOT charged,
    /// exactly as the boxed `NodeGroup` never was: a scan must stream a
    /// table larger than memory through the budgeted pager cache with only
    /// one transient group in flight (spill_reclaim's 262k-rows-in-1-MiB
    /// ladder test; pk_cache_shed's fat-group test).
    Typed {
        columns: Vec<Column>,
        row_count: usize,
        next_row: usize,
    },
    /// Overlay effects shadow rows of this group (MVCC deltas applied on
    /// top): fall back to the boxed row path for this group. End-to-end tests
    /// verify result equality between the two paths.
    Boxed { group: NodeGroup, next_row: usize },
}

struct OwnedWorkingSetCharge {
    budget: Arc<MemoryBudget>,
    bytes: usize,
}

const PHYSICAL_SLOT_INDEX_ENTRY_BYTES: usize = 64;

impl Drop for OwnedWorkingSetCharge {
    fn drop(&mut self) {
        self.budget.release(self.bytes);
    }
}

struct OverlayScanRow {
    values: Vec<Value>,
    offset: u64,
}

fn build_overlay_scan_rows(
    view: &ReadView,
    schema: &NodeTableSchema,
    checkpointed: u64,
    rows: Vec<Option<Vec<Value>>>,
    effects: &BTreeMap<OverlayKey, VisibleRowEffect>,
) -> DevonResult<(Vec<OverlayScanRow>, BTreeSet<OverlayKey>)> {
    let (key_index, _) = primary_key(schema)?;
    let mut seen = BTreeSet::new();
    let mut visible = Vec::with_capacity(rows.len() + effects.len());
    for (position, row) in rows.into_iter().enumerate().rev() {
        let Some(row) = row else {
            continue;
        };
        let key = OverlayKey::from_row(schema.name(), &row, key_index)?;
        if !seen.insert(key.clone()) {
            continue;
        }
        let offset = node_offset_value(checkpointed, position)?;
        match effects.get(&key) {
            Some(VisibleRowEffect::Update(replacement)) => visible.push(OverlayScanRow {
                values: replacement.clone(),
                offset,
            }),
            Some(VisibleRowEffect::Delete) => {}
            None => visible.push(OverlayScanRow {
                values: row,
                offset,
            }),
        }
    }
    visible.reverse();
    append_persisted_replacements(view, schema, effects, &seen, &mut visible)?;
    Ok((visible, seen))
}

fn append_persisted_replacements(
    view: &ReadView,
    schema: &NodeTableSchema,
    effects: &BTreeMap<OverlayKey, VisibleRowEffect>,
    overlay_keys: &BTreeSet<OverlayKey>,
    rows: &mut Vec<OverlayScanRow>,
) -> DevonResult<()> {
    let (key_index, _) = primary_key(schema)?;
    for (key, effect) in effects {
        let VisibleRowEffect::Update(replacement) = effect else {
            continue;
        };
        if overlay_keys.contains(key) {
            continue;
        }
        let value = key.to_value();
        let offset =
            resolve_checkpointed_node_key(view, schema, key_index, &value)?.ok_or_else(|| {
                corrupt(format!(
                    "update names missing primary key {value} in `{}`",
                    schema.name()
                ))
            })?;
        rows.push(OverlayScanRow {
            values: replacement.clone(),
            offset,
        });
    }
    Ok(())
}

fn node_offset_value(checkpointed: u64, position: usize) -> DevonResult<u64> {
    let position = u64::try_from(position)
        .map_err(|_| corrupt("node overlay position cannot be represented as u64"))?;
    checkpointed
        .checked_add(position)
        .ok_or_else(|| corrupt("node offset exceeds u64::MAX"))
}

fn group_row(group: &NodeGroup, row_index: usize) -> DevonResult<Vec<Value>> {
    let mut row = Vec::with_capacity(group.column_count() + 1);
    for column in 0..group.column_count() {
        row.push(
            group
                .value(row_index, column)
                .cloned()
                .ok_or_else(|| corrupt("node group is missing a scanned value"))?,
        );
    }
    Ok(row)
}

impl ScanSource {
    fn new(
        view: &ReadView,
        schema: NodeTableSchema,
        zone_map_filter: Option<ZoneMapFilter>,
        projection: Option<BTreeSet<usize>>,
    ) -> DevonResult<Self> {
        let stored_groups = view
            .catalog
            .table_storage(schema.name())
            .map_or(&[][..], |storage| storage.groups.as_slice());
        let checkpointed = checkpointed_row_count(view, &schema)?;
        let effects = visible_dml_effects(view, &schema)?;
        let mut overlay_count = 0_usize;
        let mut requested = stored_groups
            .len()
            .checked_mul(size_of::<u64>())
            .ok_or_else(|| corrupt("scan group-id memory estimate exceeds usize::MAX"))?;
        for slot in view.state.node_slots(schema.name()) {
            overlay_count = overlay_count
                .checked_add(1)
                .ok_or_else(|| corrupt("scan overlay row count exceeds usize::MAX"))?;
            checked_working_set_add(&mut requested, size_of::<Option<Vec<Value>>>())?;
            checked_working_set_add(&mut requested, PHYSICAL_SLOT_INDEX_ENTRY_BYTES)?;
            if let Some(row) = slot.row {
                add_value_bytes(&mut requested, row)?;
            }
        }
        if let Some(own) = &view.own
            && let Some(rows) = own.nodes.get(schema.name())
        {
            for row in rows {
                overlay_count = overlay_count
                    .checked_add(1)
                    .ok_or_else(|| corrupt("scan overlay row count exceeds usize::MAX"))?;
                add_row_bytes(&mut requested, row)?;
                checked_working_set_add(&mut requested, PHYSICAL_SLOT_INDEX_ENTRY_BYTES)?;
            }
        }
        for effect in effects.values() {
            if let VisibleRowEffect::Update(row) = effect {
                add_row_bytes(&mut requested, row)?;
            }
        }
        let charge = owned_working_set_charge(
            Arc::clone(&view.shared.budget),
            &view.shared.pager,
            requested,
            || format!("scan working set for node table `{}`", schema.name()),
        )?;
        let group_ids = stored_groups.to_vec();
        let mut base_overlay_rows = Vec::with_capacity(overlay_count);
        base_overlay_rows.extend(
            view.state
                .node_slots(schema.name())
                .map(|slot| slot.row.map(<[Value]>::to_vec)),
        );
        if let Some(own) = &view.own
            && let Some(rows) = own.nodes.get(schema.name())
        {
            base_overlay_rows.extend(rows.iter().cloned().map(Some));
        }
        let (overlay_rows, shadowed_keys) =
            build_overlay_scan_rows(view, &schema, checkpointed, base_overlay_rows, &effects)?;
        let (key_index, _) = primary_key(&schema)?;
        let columns: Vec<usize> = match &projection {
            Some(set) => set.iter().copied().collect(),
            None => (0..schema.columns().len()).collect(),
        };
        let key_position = columns
            .iter()
            .position(|index| *index == key_index)
            .ok_or_else(|| corrupt("scan projection lost the primary key column"))?;
        let mut types = columns
            .iter()
            .map(|&index| {
                schema
                    .columns()
                    .get(index)
                    .map(|column| column.ty)
                    .ok_or_else(|| {
                        corrupt(format!(
                            "scan projection for `{}` names out-of-range column {index}",
                            schema.name()
                        ))
                    })
            })
            .collect::<DevonResult<Vec<_>>>()?;
        types.push(LogicalType::Int64);
        let property_types = schema
            .columns()
            .iter()
            .map(|column| column.ty)
            .collect::<Vec<_>>();
        let source = Self {
            view: view.clone(),
            schema,
            group_ids,
            next_group: 0,
            current_group: None,
            overlay_rows: overlay_rows.into_iter(),
            next_offset: 0,
            types,
            property_types,
            columns,
            key_position,
            key_index,
            zone_map_filter: (view.shared.pager.superblock().feature_flags & ZONE_MAPS_FLAG != 0)
                .then_some(zone_map_filter)
                .flatten(),
            effects,
            shadowed_keys,
            _charge: charge,
        };
        Ok(source)
    }

    fn read_next_group(&mut self) -> DevonResult<bool> {
        loop {
            let Some(group_id) = self.group_ids.get(self.next_group).copied() else {
                return Ok(false);
            };
            let directory =
                NodeGroup::read_directory(&self.view.shared.pager, group_id, &self.property_types)?;
            let row_count = directory.row_count();
            if self.next_group + 1 < self.group_ids.len() && row_count < NODE_GROUP_CAPACITY {
                return Err(corrupt(format!(
                    "non-tail node group {} in table `{}` has only {row_count} rows",
                    self.next_group,
                    self.schema.name(),
                )));
            }
            self.next_group += 1;
            if self
                .zone_map_filter
                .as_ref()
                .zip(directory.zone_maps())
                .is_some_and(|(filter, stats)| filter.excludes_group(stats))
            {
                self.advance_offset(row_count)?;
                continue;
            }
            let group = self.decode_group(group_id, directory)?;
            self.current_group = Some(group);
            return Ok(true);
        }
    }

    /// Decodes one pruned-in group (docs/SCALE.md §6.5): columns go straight
    /// from payload bytes into typed [`Column`] storage. Under projection
    /// pushdown only the referenced columns are decoded, one
    /// `read_column_typed` per column — un-referenced payload pages are
    /// never read. When MVCC overlay effects shadow a row of the group, the
    /// whole group falls back to the boxed row path for correctness.
    fn decode_group(&self, group_id: u64, directory: NodeGroupDirectory) -> DevonResult<GroupScan> {
        let row_count = directory.row_count();
        let columns = self.decode_typed_columns(group_id, directory)?;
        if self.group_has_shadowed_rows(&columns)? {
            let pager = &self.view.shared.pager;
            let directory = NodeGroup::read_directory(pager, group_id, &self.property_types)?;
            let group = NodeGroup::read_from_directory(pager, directory, &self.property_types)?;
            return Ok(GroupScan::Boxed { group, next_row: 0 });
        }
        TYPED_GROUP_SCANS.fetch_add(1, Ordering::Relaxed);
        Ok(GroupScan::Typed {
            columns,
            row_count,
            next_row: 0,
        })
    }

    /// Typed decode of the scan's column set for one group whose directory
    /// was already read and validated by `read_next_group`.
    fn decode_typed_columns(
        &self,
        group_id: u64,
        directory: NodeGroupDirectory,
    ) -> DevonResult<Vec<Column>> {
        let pager = &self.view.shared.pager;
        if self.columns.len() == self.property_types.len() {
            return NodeGroup::read_columns_typed_from_directory(
                pager,
                directory,
                &self.property_types,
            );
        }
        if self
            .columns
            .iter()
            .any(|&index| carries_b1_rescore(self.property_types[index]))
        {
            // b1-rescore sidecars pair with their main column only in the
            // full-column decode (`read_column_typed` refuses them, as
            // `read_column` does): decode everything, keep the subset.
            let all = NodeGroup::read_columns_typed_from_directory(
                pager,
                directory,
                &self.property_types,
            )?;
            return self
                .columns
                .iter()
                .map(|&index| {
                    all.get(index)
                        .cloned()
                        .ok_or_else(|| corrupt("node-group typed decode lost a column"))
                })
                .collect();
        }
        // Subset decode: one read per referenced column. The directory page
        // is re-validated per column (a pager cache hit after the read
        // above); un-referenced payload pages are never touched — the
        // pushdown win. Storage exposes no directory-based subset reader.
        let row_count = directory.row_count();
        let mut columns = Vec::with_capacity(self.columns.len());
        for &index in &self.columns {
            let (column, rows) =
                NodeGroup::read_column_typed(pager, group_id, &self.property_types, index)?;
            if rows != row_count {
                return Err(corrupt(format!(
                    "node group {group_id} column {index} decoded {rows} rows, expected {row_count}"
                )));
            }
            columns.push(column);
        }
        Ok(columns)
    }

    /// Whether any row of the decoded group is shadowed by an MVCC effect —
    /// decided on the primary-key column alone. The hot path (no visible
    /// DML effects at all) costs a map emptiness check.
    fn group_has_shadowed_rows(&self, columns: &[Column]) -> DevonResult<bool> {
        if self.effects.is_empty() && self.shadowed_keys.is_empty() {
            return Ok(false);
        }
        let keys = &columns[self.key_position];
        for row in 0..keys.len() {
            let key = OverlayKey::from_value(&keys.value_at(row))?;
            if self.effects.contains_key(&key) || self.shadowed_keys.contains(&key) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Narrows a full-width row (boxed fallback group or overlay row) to
    /// the decoded column set; full-width scans pass rows through.
    fn project_row(&self, row: Vec<Value>) -> DevonResult<Vec<Value>> {
        if self.columns.len() == self.property_types.len() {
            return Ok(row);
        }
        self.columns
            .iter()
            .map(|&index| {
                row.get(index)
                    .cloned()
                    .ok_or_else(|| corrupt(format!("scan row is missing column {index}")))
            })
            .collect()
    }

    fn advance_offset(&mut self, rows: usize) -> DevonResult<()> {
        let rows = u64::try_from(rows)
            .map_err(|_| corrupt("node-group row count cannot be represented as u64"))?;
        self.next_offset = self
            .next_offset
            .checked_add(rows)
            .ok_or_else(|| corrupt("node offset exceeds u64::MAX"))?;
        Ok(())
    }

    fn next_group_chunk(&mut self) -> DevonResult<Option<Chunk>> {
        if self.current_group.is_none() && !self.read_next_group()? {
            return Ok(None);
        }
        // Whole-group fast path: an unconsumed typed group becomes exactly
        // one chunk, built straight from its typed columns (a group never
        // exceeds CHUNK_CAPACITY = NODE_GROUP_CAPACITY rows).
        if matches!(
            &self.current_group,
            Some(GroupScan::Typed { next_row: 0, .. })
        ) {
            let Some(GroupScan::Typed {
                columns, row_count, ..
            }) = self.current_group.take()
            else {
                return Err(corrupt("scan group disappeared while reading"));
            };
            return self.typed_group_chunk(columns, row_count).map(Some);
        }
        // Row path: boxed fallback groups (and a typed group reached
        // mid-fill after a boxed group) stream row by row, skipping
        // shadowed keys — the boxed-row shape, unchanged.
        let Some(first) = self.next_persisted_row()? else {
            return Ok(None);
        };
        let mut builder = ChunkBuilder::new(self.types.clone());
        builder.push_row(first)?;
        for _ in 1..CHUNK_CAPACITY {
            let Some(row) = self.next_persisted_row()? else {
                break;
            };
            builder.push_row(row)?;
        }
        Ok(Some(builder.finish()))
    }

    /// Builds one chunk from a clean group's typed columns plus the
    /// synthesized contiguous offset column (no row is shadowed, so offsets
    /// are the group's ordinal range).
    fn typed_group_chunk(
        &mut self,
        mut columns: Vec<Column>,
        row_count: usize,
    ) -> DevonResult<Chunk> {
        let offsets = self.offset_column(row_count)?;
        columns.push(offsets);
        Chunk::from_columns(self.types.clone(), columns)
    }

    /// The internal offset column for a clean group: `next_offset ..=
    /// next_offset + row_count - 1` as a typed Int64 column.
    fn offset_column(&mut self, row_count: usize) -> DevonResult<Column> {
        let rows = u64::try_from(row_count)
            .map_err(|_| corrupt("node-group row count cannot be represented as u64"))?;
        let end = self
            .next_offset
            .checked_add(rows)
            .ok_or_else(|| corrupt("node offset exceeds u64::MAX"))?;
        let values = (self.next_offset..end)
            .map(|offset| {
                i64::try_from(offset)
                    .map_err(|_| corrupt("node offset cannot be represented as Int64"))
            })
            .collect::<DevonResult<Vec<_>>>()?;
        self.next_offset = end;
        Ok(Column::Int64 {
            values,
            validity: None,
        })
    }

    fn next_persisted_row(&mut self) -> DevonResult<Option<Vec<Value>>> {
        loop {
            if self.current_group.is_none() && !self.read_next_group()? {
                return Ok(None);
            }
            match self.current_group.as_mut() {
                Some(GroupScan::Boxed { group, next_row }) => {
                    if *next_row == group.row_count() {
                        self.current_group = None;
                        continue;
                    }
                    let row = group_row(group, *next_row)?;
                    *next_row += 1;
                    let offset = self.next_offset_value()?;
                    let key = OverlayKey::from_row(self.schema.name(), &row, self.key_index)?;
                    if self.effects.contains_key(&key) || self.shadowed_keys.contains(&key) {
                        continue;
                    }
                    let mut row = self.project_row(row)?;
                    row.push(offset);
                    return Ok(Some(row));
                }
                Some(GroupScan::Typed {
                    columns,
                    row_count,
                    next_row,
                    ..
                }) => {
                    // Reached only when a boxed (shadowed) group's chunk
                    // spills into a clean group mid-fill: materialize
                    // row-wise through `value_at`. Cold path by
                    // construction — the group was proven shadow-free.
                    if *next_row == *row_count {
                        self.current_group = None;
                        continue;
                    }
                    let row_index = *next_row;
                    *next_row += 1;
                    let mut row = columns
                        .iter()
                        .map(|column| column.value_at(row_index))
                        .collect::<Vec<_>>();
                    let offset = self.next_offset_value()?;
                    row.push(offset);
                    return Ok(Some(row));
                }
                None => return Err(corrupt("scan group disappeared while reading")),
            }
        }
    }

    fn next_offset_value(&mut self) -> DevonResult<Value> {
        let value = i64::try_from(self.next_offset)
            .map_err(|_| corrupt("node offset cannot be represented as Int64"))?;
        self.next_offset = self
            .next_offset
            .checked_add(1)
            .ok_or_else(|| corrupt("node offset exceeds u64::MAX"))?;
        Ok(Value::Int64(value))
    }

    /// One overlay row narrowed to the decoded column set, with its
    /// precomputed hidden offset appended.
    fn overlay_row(&self, row: OverlayScanRow) -> DevonResult<Vec<Value>> {
        let offset = i64::try_from(row.offset)
            .map_err(|_| corrupt("node offset cannot be represented as Int64"))?;
        let mut values = self.project_row(row.values)?;
        values.push(Value::Int64(offset));
        Ok(values)
    }
}

impl ChunkSource for ScanSource {
    fn next_chunk(&mut self) -> DevonResult<Option<Chunk>> {
        if (self.current_group.is_some() || self.next_group < self.group_ids.len())
            && let Some(chunk) = self.next_group_chunk()?
        {
            return Ok(Some(chunk));
        }
        let Some(first) = self.overlay_rows.next() else {
            return Ok(None);
        };
        let mut builder = ChunkBuilder::new(self.types.clone());
        builder.push_row(self.overlay_row(first)?)?;
        for _ in 0..CHUNK_CAPACITY - 1 {
            let Some(row) = self.overlay_rows.next() else {
                break;
            };
            builder.push_row(self.overlay_row(row)?)?;
        }
        Ok(Some(builder.finish()))
    }
}

fn result_columns(operator: &Operator, catalog: &Catalog) -> DevonResult<Vec<String>> {
    match operator {
        Operator::ScanInterface { interface, binding } => {
            let interface = catalog
                .interface(interface)
                .ok_or_else(|| DevonError::NotFound {
                    what: format!("interface `{interface}`"),
                })?;
            Ok(interface
                .columns
                .iter()
                .map(|column| format!("{binding}.{}", column.name))
                .collect())
        }
        Operator::TextScan { table, binding, .. } | Operator::ScanNodes { table, binding } => {
            let schema = catalog
                .node_table(table)
                .ok_or_else(|| DevonError::NotFound {
                    what: format!("node table `{table}`"),
                })?;
            Ok(schema
                .columns()
                .iter()
                .map(|column| format!("{binding}.{}", column.name))
                .collect())
        }
        Operator::ExpandRel {
            rel,
            direction,
            from_binding,
            binding,
            input,
            ..
        }
        | Operator::Expand {
            rel,
            direction,
            from_binding,
            binding,
            input,
        } => {
            let mut columns = result_columns(input, catalog)?;
            let rel_schema = catalog.rel_table(rel).ok_or_else(|| DevonError::NotFound {
                what: format!("relationship table `{rel}`"),
            })?;
            let source_table =
                binding_node_table(input, from_binding, catalog)?.ok_or_else(|| {
                    corrupt(format!(
                        "validated Expand binding `{from_binding}` has no source node table"
                    ))
                })?;
            let (_, neighbor_table) = traversal_tables(rel_schema, *direction, &source_table)?;
            let neighbor_schema =
                catalog
                    .node_table(neighbor_table)
                    .ok_or_else(|| DevonError::NotFound {
                        what: format!("node table `{neighbor_table}`"),
                    })?;
            columns.extend(
                neighbor_schema
                    .columns()
                    .iter()
                    .map(|column| format!("{binding}.{}", column.name)),
            );
            if let Operator::ExpandRel { rel_binding, .. } = operator {
                columns.extend(
                    rel_schema
                        .columns()
                        .iter()
                        .map(|column| format!("{rel_binding}.{}", column.name)),
                );
            }
            Ok(columns)
        }
        Operator::Project { exprs, .. } => {
            Ok(exprs.iter().map(|item| item.alias.clone()).collect())
        }
        Operator::Aggregate { group_by, aggs, .. } => {
            let mut columns = group_by
                .iter()
                .map(canonical_expression)
                .collect::<DevonResult<Vec<_>>>()?;
            columns.extend(aggs.iter().map(|aggregate| aggregate.alias.clone()));
            Ok(columns)
        }
        Operator::Filter { input, .. }
        | Operator::Sort { input, .. }
        | Operator::Limit { input, .. } => result_columns(input, catalog),
        Operator::KnnScan { table, .. } => {
            let schema = catalog
                .node_table(table)
                .ok_or_else(|| DevonError::NotFound {
                    what: format!("node table `{table}`"),
                })?;
            Ok(schema
                .columns()
                .iter()
                .map(|column| format!("{table}.{}", column.name))
                .chain([DISTANCE_COLUMN_NAME.to_owned()])
                .collect())
        }
        // WithinScan binds its table's columns under the table name and
        // appends no output-only column (PLAN_IR.md § within semantics).
        Operator::WithinScan { table, .. } => {
            let schema = catalog
                .node_table(table)
                .ok_or_else(|| DevonError::NotFound {
                    what: format!("node table `{table}`"),
                })?;
            Ok(schema
                .columns()
                .iter()
                .map(|column| format!("{table}.{}", column.name))
                .collect())
        }
        // Join output is the union of both inputs' bindings: left's columns
        // then right's (PLAN_IR.md § join semantics).
        Operator::HashJoin { left, right, .. } => {
            let mut columns = result_columns(left, catalog)?;
            columns.extend(result_columns(right, catalog)?);
            Ok(columns)
        }
    }
}

fn binding_node_table(
    operator: &Operator,
    wanted: &str,
    catalog: &Catalog,
) -> DevonResult<Option<String>> {
    match operator {
        Operator::TextScan { table, binding, .. } | Operator::ScanNodes { table, binding } => {
            Ok((fold(binding) == fold(wanted)).then(|| table.clone()))
        }
        Operator::KnnScan { table, .. } | Operator::WithinScan { table, .. } => {
            Ok((fold(table) == fold(wanted)).then(|| table.clone()))
        }
        Operator::ScanInterface { .. } => Ok(None),
        Operator::ExpandRel {
            rel,
            direction,
            from_binding,
            binding,
            input,
            ..
        }
        | Operator::Expand {
            rel,
            direction,
            from_binding,
            binding,
            input,
        } => {
            if fold(binding) != fold(wanted) {
                return binding_node_table(input, wanted, catalog);
            }
            let source = binding_node_table(input, from_binding, catalog)?.ok_or_else(|| {
                corrupt(format!(
                    "validated Expand binding `{from_binding}` has no source node table"
                ))
            })?;
            let schema = catalog.rel_table(rel).ok_or_else(|| DevonError::NotFound {
                what: format!("relationship table `{rel}`"),
            })?;
            let (_, neighbor) = traversal_tables(schema, *direction, &source)?;
            Ok(Some(neighbor.to_owned()))
        }
        Operator::Filter { input, .. }
        | Operator::Project { input, .. }
        | Operator::Sort { input, .. }
        | Operator::Limit { input, .. }
        | Operator::Aggregate { input, .. } => binding_node_table(input, wanted, catalog),
        Operator::HashJoin { left, right, .. } => {
            if let Some(table) = binding_node_table(left, wanted, catalog)? {
                Ok(Some(table))
            } else {
                binding_node_table(right, wanted, catalog)
            }
        }
    }
}

fn collect_rows(
    source: &mut dyn ChunkSource,
    internal_columns: &[usize],
    budget: &MemoryBudget,
    pager: &Pager,
) -> DevonResult<Vec<Vec<Value>>> {
    let mut rows = Vec::new();
    let mut charge = charge_working_set(budget, pager, 0, || "scan result working set".to_owned())?;
    while let Some(chunk) = source.next_chunk()? {
        let desired_len = rows
            .len()
            .checked_add(chunk.row_count())
            .ok_or_else(|| corrupt("scan result row count exceeds usize::MAX"))?;
        let capacity_before = rows.capacity();
        let planned_slots = desired_len.saturating_sub(capacity_before);
        let planned_capacity_bytes = planned_slots
            .checked_mul(size_of::<Vec<Value>>())
            .ok_or_else(|| corrupt("scan result capacity estimate exceeds usize::MAX"))?;
        let mut value_bytes = 0_usize;
        for row in chunk.rows() {
            for (index, value) in row.into_iter().enumerate() {
                if internal_columns.binary_search(&index).is_err() {
                    checked_working_set_add(&mut value_bytes, value.approx_bytes())?;
                }
            }
        }
        let requested = planned_capacity_bytes
            .checked_add(value_bytes)
            .ok_or_else(|| corrupt("scan result memory estimate exceeds usize::MAX"))?;
        grow_working_set_charge(&mut charge, budget, pager, requested, || {
            "scan result working set".to_owned()
        })?;
        rows.try_reserve_exact(chunk.row_count()).map_err(|error| {
            DevonError::BudgetExceeded {
                context: format!(
                    "scan result working set allocation failed after reserving {requested} bytes: {error}"
                ),
            }
        })?;
        let extra_slots = rows
            .capacity()
            .saturating_sub(capacity_before.saturating_add(planned_slots));
        if extra_slots > 0 {
            let extra_bytes = extra_slots
                .checked_mul(size_of::<Vec<Value>>())
                .ok_or_else(|| corrupt("scan result capacity estimate exceeds usize::MAX"))?;
            grow_working_set_charge(&mut charge, budget, pager, extra_bytes, || {
                "scan result working set".to_owned()
            })?;
        }
        for row in chunk.rows() {
            let values = row
                .into_iter()
                .enumerate()
                .filter(|(index, _)| internal_columns.binary_search(index).is_err())
                .map(|(_, value)| value.clone())
                .collect::<Vec<_>>();
            rows.push(values);
        }
    }
    Ok(rows)
}

fn charge_working_set<'budget>(
    budget: &'budget MemoryBudget,
    pager: &Pager,
    requested: usize,
    category: impl Fn() -> String,
) -> DevonResult<ChargedBytes<'budget>> {
    let context = || working_set_context(budget, requested, &category());
    match ChargedBytes::try_new(budget, requested, context) {
        Ok(charge) => Ok(charge),
        Err(DevonError::BudgetExceeded { .. }) => {
            // Ladder step 1 (docs/MVCC.md §7.2): evict clean cache frames
            // before the single retry — the cache admits pages up to the
            // whole budget and yields only to eviction. No PK-cache rung
            // here: chunk production already charges frames through
            // charge_or_reclaim, whose second rung sheds the caches before
            // this ladder can ever see cache pressure.
            pager.shed_cache(requested);
            ChargedBytes::try_new(budget, requested, context)
        }
        Err(error) => Err(error),
    }
}

fn owned_working_set_charge(
    budget: Arc<MemoryBudget>,
    pager: &Pager,
    requested: usize,
    category: impl Fn() -> String,
) -> DevonResult<OwnedWorkingSetCharge> {
    if !budget.try_charge(requested) {
        pager.shed_cache(requested);
        if !budget.try_charge(requested) {
            return Err(DevonError::BudgetExceeded {
                context: working_set_context(&budget, requested, &category()),
            });
        }
    }
    Ok(OwnedWorkingSetCharge {
        budget,
        bytes: requested,
    })
}

fn grow_working_set_charge(
    charge: &mut ChargedBytes<'_>,
    budget: &MemoryBudget,
    pager: &Pager,
    requested: usize,
    category: impl Fn() -> String,
) -> DevonResult<()> {
    let context = || working_set_context(budget, requested, &category());
    match charge.grow(requested, context) {
        Ok(()) => Ok(()),
        Err(DevonError::BudgetExceeded { .. }) => {
            // Ladder step 1 (docs/MVCC.md §7.2), as in charge_working_set.
            pager.shed_cache(requested);
            charge.grow(requested, context)
        }
        Err(error) => Err(error),
    }
}

fn working_set_context(budget: &MemoryBudget, requested: usize, category: &str) -> String {
    format!(
        "{category} requested {requested} bytes with {} bytes charged and limit {}",
        budget.charged(),
        budget.limit()
    )
}

fn slot_rows_bytes(rows: &[Option<Vec<Value>>], capacity: usize) -> DevonResult<usize> {
    let mut total = capacity
        .checked_mul(size_of::<Option<Vec<Value>>>())
        .ok_or_else(|| corrupt("node-row working-set estimate exceeds usize::MAX"))?;
    for row in rows.iter().flatten() {
        for value in row {
            checked_working_set_add(&mut total, value.approx_bytes())?;
        }
    }
    Ok(total)
}

fn rows_prefix_bytes(rows: &[Option<Vec<Value>>], count: usize) -> DevonResult<usize> {
    if count > rows.len() {
        return Err(corrupt("checkpointed node count exceeds materialized rows"));
    }
    let mut total = count
        .checked_mul(size_of::<Option<Vec<Value>>>())
        .ok_or_else(|| corrupt("node-row working-set estimate exceeds usize::MAX"))?;
    for row in rows[..count].iter().flatten() {
        for value in row {
            checked_working_set_add(&mut total, value.approx_bytes())?;
        }
    }
    Ok(total)
}

fn minimum_rows_bytes(schema: &NodeTableSchema, count: usize) -> DevonResult<usize> {
    let row_values = schema.columns().iter().try_fold(0_usize, |total, column| {
        total
            .checked_add(minimum_value_bytes(column.ty))
            .ok_or_else(|| corrupt("node-row working-set estimate exceeds usize::MAX"))
    })?;
    let row_bytes = size_of::<Vec<Value>>()
        .checked_add(row_values)
        .ok_or_else(|| corrupt("node-row working-set estimate exceeds usize::MAX"))?;
    count
        .checked_mul(row_bytes)
        .ok_or_else(|| corrupt("node-row working-set estimate exceeds usize::MAX"))
}

const fn minimum_value_bytes(logical_type: LogicalType) -> usize {
    match logical_type {
        LogicalType::String => 32,
        LogicalType::Vector { dim } | LogicalType::VectorEncoded { dim, .. } => {
            32 + 4 * dim as usize
        }
        LogicalType::Bool | LogicalType::Int64 | LogicalType::Float64 => 16,
        // Two f64 components; matches Value::approx_bytes (MVCC.md §7.2).
        LogicalType::GeoPoint => 24,
        // Additional scalar types; constants match Value::approx_bytes.
        LogicalType::Timestamp => 16,
        LogicalType::Decimal { .. } => 24,
        LogicalType::Bytes | LogicalType::Json => 32,
    }
}

fn add_row_bytes(total: &mut usize, row: &[Value]) -> DevonResult<()> {
    checked_working_set_add(total, size_of::<Vec<Value>>())?;
    add_value_bytes(total, row)
}

fn add_value_bytes(total: &mut usize, row: &[Value]) -> DevonResult<()> {
    for value in row {
        checked_working_set_add(total, value.approx_bytes())?;
    }
    Ok(())
}

fn checked_working_set_add(total: &mut usize, bytes: usize) -> DevonResult<()> {
    *total = total
        .checked_add(bytes)
        .ok_or_else(|| corrupt("scan working-set estimate exceeds usize::MAX"))?;
    Ok(())
}

fn rel_overlay_bytes(view: &ReadView, rel: &str) -> DevonResult<usize> {
    let mut total = 0_usize;
    for edge in view.state.rel_edges(rel) {
        checked_working_set_add(&mut total, 40)?;
        for value in &edge.values {
            checked_working_set_add(&mut total, value.approx_bytes())?;
        }
    }
    if let Some(own) = &view.own
        && let Some(edges) = own.edges.get(rel)
    {
        for edge in edges {
            checked_working_set_add(&mut total, 40)?;
            for value in &edge.values {
                checked_working_set_add(&mut total, value.approx_bytes())?;
            }
        }
    }
    let mut tombstone_entries = 0_usize;
    for link in view.state.commit_links_oldest_first() {
        if let Some(tombstones) = link.delta.rel_tombstones.get(rel) {
            add_tombstone_entries(&mut tombstone_entries, tombstones)?;
        }
    }
    if let Some(own) = &view.own
        && let Some(tombstones) = own.rel_tombstones.get(rel)
    {
        add_tombstone_entries(&mut tombstone_entries, tombstones)?;
    }
    if tombstone_entries > 0 {
        checked_working_set_add(&mut total, REL_ENDPOINT_TOMBSTONES_OVERHEAD_BYTES)?;
        checked_working_set_add(
            &mut total,
            tombstone_entries
                .checked_mul(REL_TOMBSTONE_OFFSET_OVERHEAD_BYTES)
                .ok_or_else(|| corrupt("relationship tombstone estimate exceeds usize::MAX"))?,
        )?;
    }
    Ok(total)
}

fn add_tombstone_entries(total: &mut usize, tombstones: &RelEndpointTombstones) -> DevonResult<()> {
    *total = total
        .checked_add(tombstones.from_offsets.len())
        .and_then(|value| value.checked_add(tombstones.to_offsets.len()))
        .ok_or_else(|| corrupt("relationship tombstone count exceeds usize::MAX"))?;
    Ok(())
}

fn effective_rel_tombstones(view: &ReadView, rel: &str) -> RelEndpointTombstones {
    let mut tombstones = view.state.rel_tombstones(rel);
    if let Some(own) = &view.own
        && let Some(own_tombstones) = own.rel_tombstones.get(rel)
    {
        tombstones.union_with(own_tombstones);
    }
    tombstones
}

fn adjacency_working_set_bytes(
    table: &RelTable,
    pager: &Pager,
    catalog: &Catalog,
    direction: Direction,
    source_row_count: usize,
    rel: &str,
) -> DevonResult<usize> {
    let mut total = source_row_count
        .checked_mul(size_of::<Vec<u64>>())
        .ok_or_else(|| corrupt("expand adjacency estimate exceeds usize::MAX"))?;
    let storage_direction = match direction {
        Direction::Out => devondb_storage::rel_table::Direction::Out,
        Direction::In => devondb_storage::rel_table::Direction::In,
        Direction::Both => devondb_storage::rel_table::Direction::Both,
    };
    for offset in 0..source_row_count {
        let offset =
            u64::try_from(offset).map_err(|_| corrupt("expand source offset exceeds u64::MAX"))?;
        let neighbors = table
            .neighbors_before_filtering(pager, catalog, storage_direction, offset)
            .map_err(|error| {
                corrupt(format!(
                    "failed to size adjacency working set for relationship `{rel}` at offset {offset}: {error}"
                ))
            })?;
        let bytes = neighbors
            .len()
            .checked_mul(size_of::<u64>())
            .ok_or_else(|| corrupt("expand adjacency estimate exceeds usize::MAX"))?;
        checked_working_set_add(&mut total, bytes)?;
    }
    Ok(total)
}

fn traversal_tables<'schema>(
    schema: &'schema RelTableSchema,
    direction: Direction,
    source_table: &str,
) -> DevonResult<(Direction, &'schema str)> {
    let traversal = crate::graph::traversal_direction(schema, direction, source_table)?;
    match traversal {
        Direction::Out => Ok((traversal, schema.to())),
        Direction::In => Ok((traversal, schema.from())),
        Direction::Both => Ok((traversal, schema.to())),
    }
}

fn unique_offset_key(binding: &str, columns: &HashMap<String, usize>) -> String {
    let mut key = format!("\0devondb.offset:{binding}");
    while columns.contains_key(&key) {
        key.push('\0');
    }
    key
}

// `resolve_node_offset` remains part of the facade's graph module, but this
// read path deliberately uses the group-at-a-time resolver above so a miss
// never materializes a whole checkpointed table.
const _: fn(&NodeTable, &Pager, &Catalog, &Value) -> DevonResult<u64> = resolve_node_offset;

#[cfg(test)]
mod detach_read_tests {
    use std::sync::Arc;

    use devondb_plan::statement::RelRow;
    use devondb_storage::overlay::RelEndpointTombstones;
    use devondb_types::schema::Column as SchemaColumn;
    use tempfile::tempdir;

    use super::*;

    const PAGE_SIZE: u32 = 4096;

    fn column(name: &str, primary_key: bool) -> SchemaColumn {
        SchemaColumn {
            name: name.to_owned(),
            ty: LogicalType::Int64,
            primary_key,
        }
    }

    fn seeded_database() -> (tempfile::TempDir, Database) {
        let directory = tempdir().unwrap();
        let path = directory.path().join("detach-view.devondb");
        let mut database = Database::create(path, PAGE_SIZE).unwrap();
        database
            .execute(&Statement::CreateNodeTable {
                name: "Person".to_owned(),
                columns: vec![column("id", true)],
            })
            .unwrap();
        database
            .execute(&Statement::CreateRelTable {
                name: "Knows".to_owned(),
                from: "Person".to_owned(),
                to: "Person".to_owned(),
                columns: Vec::new(),
            })
            .unwrap();
        database
            .execute(&Statement::InsertNode {
                table: "Person".to_owned(),
                rows: [1, 2, 3]
                    .into_iter()
                    .map(|id| vec![Value::Int64(id)])
                    .collect(),
            })
            .unwrap();
        database
            .execute(&Statement::InsertRel {
                table: "Knows".to_owned(),
                rows: [(1, 2), (2, 1), (1, 1), (2, 3), (2, 3)]
                    .into_iter()
                    .map(|(from, to)| RelRow {
                        from_key: Value::Int64(from),
                        to_key: Value::Int64(to),
                        values: Vec::new(),
                    })
                    .collect(),
            })
            .unwrap();
        (directory, database)
    }

    fn expand_plan(direction: Direction) -> Plan {
        Plan {
            v: PLAN_VERSION,
            plan: Operator::Expand {
                rel: "Knows".to_owned(),
                direction,
                from_binding: "person".to_owned(),
                binding: "neighbor".to_owned(),
                input: Box::new(Operator::ScanNodes {
                    table: "Person".to_owned(),
                    binding: "person".to_owned(),
                }),
            },
        }
    }

    fn detach_view(database: &Database) -> ReadView {
        let state = database.shared.current_state();
        let mut own = CommitDelta::default();
        own.node_deletes
            .insert("Person".to_owned(), vec![Value::Int64(1)]);
        own.rel_tombstones.insert(
            "Knows".to_owned(),
            RelEndpointTombstones {
                from_offsets: [0].into_iter().collect(),
                to_offsets: [0].into_iter().collect(),
            },
        );
        ReadView::new(
            Arc::clone(&database.shared),
            Arc::clone(&state),
            Arc::clone(&state.catalog),
            Some(Arc::new(own)),
        )
    }

    #[test]
    fn snapshot_with_tombstones_hides_node_and_incident_edges_without_compaction() {
        let (_directory, database) = seeded_database();
        let state = database.shared.current_state();
        let old = ReadView::new(
            Arc::clone(&database.shared),
            Arc::clone(&state),
            Arc::clone(&state.catalog),
            None,
        );
        let detached = detach_view(&database);

        let old_rows = run_view(&old, &expand_plan(Direction::Out)).unwrap().rows;
        assert_eq!(old_rows.len(), 5);

        let detached_rows = run_view(&detached, &expand_plan(Direction::Out))
            .unwrap()
            .rows;
        assert_eq!(
            detached_rows,
            vec![
                vec![Value::Int64(2), Value::Int64(3)],
                vec![Value::Int64(2), Value::Int64(3)],
            ]
        );
        let scanned = run_view(
            &detached,
            &Plan {
                v: PLAN_VERSION,
                plan: Operator::ScanNodes {
                    table: "Person".to_owned(),
                    binding: "person".to_owned(),
                },
            },
        )
        .unwrap();
        assert_eq!(
            scanned.rows,
            vec![vec![Value::Int64(2)], vec![Value::Int64(3)]]
        );
    }

    #[test]
    fn tombstone_heavy_read_charge_includes_sets_and_prefilter_edges() {
        let (_directory, database) = seeded_database();
        let state = database.shared.current_state();
        let mut own = CommitDelta::default();
        own.rel_tombstones.insert(
            "Knows".to_owned(),
            RelEndpointTombstones {
                from_offsets: (0..10_000).collect(),
                to_offsets: (10_000..20_000).collect(),
            },
        );
        let view = ReadView::new(
            Arc::clone(&database.shared),
            Arc::clone(&state),
            Arc::clone(&state.catalog),
            Some(Arc::new(own)),
        );
        let requested = rel_overlay_bytes(&view, "Knows").unwrap();
        let tombstone_floor =
            REL_ENDPOINT_TOMBSTONES_OVERHEAD_BYTES + 20_000 * REL_TOMBSTONE_OFFSET_OVERHEAD_BYTES;
        assert!(requested >= tombstone_floor);

        let before = view.shared.budget.charged();
        let charge = charge_working_set(&view.shared.budget, &view.shared.pager, requested, || {
            "detach tombstone-heavy read proof".to_owned()
        })
        .unwrap();
        assert_eq!(view.shared.budget.charged(), before + requested);
        drop(charge);
        assert_eq!(view.shared.budget.charged(), before);

        let mut table = RelTable::new(view.catalog.rel_table("Knows").unwrap().clone());
        for edge in view.state.rel_edges("Knows") {
            table
                .recover_edge(edge.from, edge.to, edge.values.clone())
                .unwrap();
        }
        table.set_endpoint_tombstones(effective_rel_tombstones(&view, "Knows"));
        let source_span = visible_node_count(&view, "Person").unwrap();
        let adjacency = adjacency_working_set_bytes(
            &table,
            &view.shared.pager,
            &view.catalog,
            Direction::Out,
            source_span,
            "Knows",
        )
        .unwrap();
        assert!(
            adjacency > source_span * size_of::<Vec<u64>>(),
            "all edges are filtered, but their prefilter resident bytes must remain charged"
        );
    }
}
