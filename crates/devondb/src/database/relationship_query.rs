//! Snapshot construction and binding layout for relationship properties.

use super::*;
use devondb_exec::{
    expand_rel::ExpandRel,
    source::{EdgeNeighbor, EdgeNeighborSource},
};

struct PropertyGraph {
    edges: Vec<Arc<[EdgeNeighbor]>>,
    rows: Vec<Option<Vec<Value>>>,
}

impl EdgeNeighborSource for PropertyGraph {
    fn edges(&self, from: u64) -> DevonResult<Arc<[EdgeNeighbor]>> {
        let from =
            usize::try_from(from).map_err(|_| corrupt("relationship source offset overflows"))?;
        self.edges
            .get(from)
            .cloned()
            .ok_or_else(|| corrupt("relationship source offset is out of bounds"))
    }

    fn node_row(&self, offset: u64) -> DevonResult<&[Value]> {
        let offset = usize::try_from(offset)
            .map_err(|_| corrupt("relationship neighbor offset overflows"))?;
        self.rows
            .get(offset)
            .and_then(Option::as_deref)
            .ok_or_else(|| corrupt("visible relationship points to a missing node"))
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn expand_rel_pipeline<'budget>(
    view: &'budget ReadView,
    input: Pipeline,
    rel: &str,
    direction: Direction,
    from_binding: &str,
    binding: &str,
    rel_binding: &str,
    charges: &mut Vec<ChargedBytes<'budget>>,
) -> DevonResult<Pipeline> {
    let schema = view
        .catalog
        .rel_table(rel)
        .cloned()
        .ok_or_else(|| corrupt(format!("relationship table `{rel}` is missing")))?;
    let (traversal, neighbor_table) =
        traversal_tables(&schema, direction, input.binding_table(from_binding)?)?;
    let neighbor_schema = view
        .catalog
        .node_table(neighbor_table)
        .cloned()
        .ok_or_else(|| corrupt("relationship neighbor table is missing"))?;
    let source_rows = visible_node_count(view, input.binding_table(from_binding)?)?;
    let peak = node_materialization_peak(view, &neighbor_schema)?;
    let preflight = charge_working_set(&view.shared.budget, &view.shared.pager, peak, || {
        "ExpandRel node decode peak".into()
    })?;
    let rows = node_rows_for_view(view, neighbor_table, charges)?;
    // node_rows_for_view now owns its reconciled retained-row charge.
    drop(preflight);
    let _overlay_charge = charge_working_set(
        &view.shared.budget,
        &view.shared.pager,
        rel_overlay_bytes(view, schema.name())?,
        || "ExpandRel overlay".into(),
    )?;
    let mut table = RelTable::new(schema.clone());
    for edge in view.state.rel_edges(schema.name()) {
        table.recover_edge(edge.from, edge.to, edge.values.clone())?;
    }
    if let Some(own) = &view.own
        && let Some(edges) = own.edges.get(schema.name())
    {
        for edge in edges {
            table.recover_edge(edge.from, edge.to, edge.values.clone())?;
        }
    }
    table.set_endpoint_tombstones(effective_rel_tombstones(view, schema.name()));
    let bytes = table.property_neighbors_working_set_bytes(
        &view.shared.pager,
        &view.catalog,
        source_rows,
    )?;
    charges.push(charge_working_set(
        &view.shared.budget,
        &view.shared.pager,
        bytes,
        || "ExpandRel property adjacency".into(),
    )?);
    let edges = property_adjacency(&table, view, traversal, source_rows)?;
    input.with_expand_rel(
        PropertyGraph { edges, rows },
        from_binding,
        binding,
        rel_binding,
        &neighbor_schema,
        &schema,
        Arc::clone(&view.shared.budget),
    )
}

fn property_adjacency(
    table: &RelTable,
    view: &ReadView,
    traversal: Direction,
    count: usize,
) -> DevonResult<Vec<Arc<[EdgeNeighbor]>>> {
    let direction = match traversal {
        Direction::Out => devondb_storage::rel_table::Direction::Out,
        Direction::In => devondb_storage::rel_table::Direction::In,
        Direction::Both => devondb_storage::rel_table::Direction::Both,
    };
    let mut adjacency = Vec::with_capacity(count);
    for from in 0..count {
        let from = u64::try_from(from).map_err(|_| corrupt("relationship offset overflows"))?;
        let edges =
            table.neighbors_with_properties(&view.shared.pager, &view.catalog, direction, from)?;
        let edges = edges
            .into_iter()
            .map(|edge| EdgeNeighbor {
                offset: if traversal == Direction::In
                    || (traversal == Direction::Both && edge.from != from)
                {
                    edge.from
                } else {
                    edge.to
                },
                values: edge.values,
            })
            .collect::<Vec<_>>();
        adjacency.push(Arc::from(edges));
    }
    Ok(adjacency)
}

impl Pipeline {
    #[allow(clippy::too_many_arguments)]
    fn with_expand_rel(
        mut self,
        graph: PropertyGraph,
        from: &str,
        binding: &str,
        rel_binding: &str,
        node: &NodeTableSchema,
        rel: &RelTableSchema,
        budget: Arc<MemoryBudget>,
    ) -> DevonResult<Self> {
        let offset_column = self
            .offsets
            .get(fold(from).as_ref())
            .map(|(_, index)| *index)
            .ok_or_else(|| invalid_argument("ExpandRel source binding has no node identity"))?;
        let mut appended = vec![LogicalType::Int64];
        let offset_key = unique_offset_key(binding, &self.columns);
        self.columns.insert(offset_key.clone(), self.width);
        self.types.insert(offset_key.clone(), LogicalType::Int64);
        self.offsets
            .insert(fold(binding).into_owned(), (offset_key, self.width));
        self.binding_tables
            .insert(fold(binding).into_owned(), node.name().to_owned());
        let mut next = self.width + 1;
        for (name, columns) in [(binding, node.columns()), (rel_binding, rel.columns())] {
            for column in columns {
                let key = format!("{}.{}", fold(name), fold(&column.name));
                self.columns.insert(key.clone(), next);
                self.types.insert(key, column.ty);
                appended.push(column.ty);
                next += 1;
            }
        }
        self.source = Box::new(ExpandRel::new(
            self.source,
            Box::new(graph),
            offset_column,
            appended,
            budget,
        ));
        self.width = next;
        Ok(self)
    }
}

fn node_materialization_peak(view: &ReadView, schema: &NodeTableSchema) -> DevonResult<usize> {
    let types = schema
        .columns()
        .iter()
        .map(|column| column.ty)
        .collect::<Vec<_>>();
    let mut bytes = 0_usize;
    if let Some(storage) = view.catalog.table_storage(schema.name()) {
        for page in &storage.groups {
            let directory = NodeGroup::read_directory(&view.shared.pager, *page, &types)?;
            for column in 0..types.len() {
                checked_working_set_add(
                    &mut bytes,
                    directory.column_decode_peak_bytes(column, &types)?,
                )?;
            }
            checked_working_set_add(
                &mut bytes,
                directory
                    .row_count()
                    .checked_mul(256)
                    .ok_or_else(|| corrupt("ExpandRel node row slots overflow"))?,
            )?;
        }
    }
    let mut link = view.state.chain.as_deref();
    while let Some(current) = link {
        add_delta_node_bytes(&mut bytes, &current.delta, schema.name())?;
        link = current.prev.as_deref();
    }
    if let Some(own) = &view.own {
        add_delta_node_bytes(&mut bytes, own, schema.name())?;
    }
    // A decoded group, scan rows, visible replacements and final slots may
    // coexist while the inherited node materializer reconciles its charge.
    bytes
        .checked_mul(4)
        .ok_or_else(|| corrupt("ExpandRel node peak overflows"))
}

fn add_delta_node_bytes(bytes: &mut usize, delta: &CommitDelta, table: &str) -> DevonResult<()> {
    for rows in [delta.nodes.get(table), delta.node_updates.get(table)]
        .into_iter()
        .flatten()
    {
        for row in rows {
            checked_working_set_add(bytes, 256)?;
            add_value_bytes(bytes, row)?;
        }
    }
    if let Some(keys) = delta.node_deletes.get(table) {
        for key in keys {
            checked_working_set_add(bytes, key.approx_bytes() + 128)?;
        }
    }
    Ok(())
}

/// Reserves decode peaks before constructing or pulling the upstream scans.
/// Legacy scans stay unchanged; only an ExpandRel input opts into this bound.
pub(super) fn precharge_input<'a>(
    view: &'a ReadView,
    input: &Operator,
    root: &Operator,
    charges: &mut Vec<ChargedBytes<'a>>,
) -> DevonResult<()> {
    match input {
        Operator::ScanNodes { table, binding } => {
            let schema = view
                .catalog
                .node_table(table)
                .ok_or_else(|| corrupt("ExpandRel source table is missing"))?;
            let (key, _) = primary_key(schema)?;
            let selected =
                with_required_columns(referenced_columns(root, binding, schema), schema, [key]);
            let bytes = source_decode_peak(view, schema, selected.as_ref())?;
            charges.push(charge_working_set(
                &view.shared.budget,
                &view.shared.pager,
                bytes,
                || "ExpandRel upstream decode peak".into(),
            )?);
        }
        Operator::TextScan { .. } | Operator::ExpandRel { .. } => {} // Its own constructor precharges its input.
        Operator::Expand {
            binding,
            input: upstream,
            ..
        } => {
            let table = binding_node_table(input, binding, &view.catalog)?
                .ok_or_else(|| corrupt("upstream expansion has no node table"))?;
            let schema = view
                .catalog
                .node_table(&table)
                .ok_or_else(|| corrupt("upstream neighbor table is missing"))?;
            // The narrowed Expand constructor owns its decode/retained/output
            // reservations. Only the legacy full-width path needs this guard.
            if expand_query::projection(root, binding, schema)?.is_none() {
                charges.push(charge_working_set(
                    &view.shared.budget,
                    &view.shared.pager,
                    node_materialization_peak(view, schema)?,
                    || "ExpandRel upstream neighbor peak".into(),
                )?);
            }
            precharge_input(view, upstream, root, charges)?;
        }
        Operator::Filter { input, .. }
        | Operator::Project { input, .. }
        | Operator::Sort { input, .. }
        | Operator::Limit { input, .. }
        | Operator::Aggregate { input, .. } => precharge_input(view, input, root, charges)?,
        Operator::HashJoin { left, right, .. } => {
            precharge_input(view, left, root, charges)?;
            precharge_input(view, right, root, charges)?;
        }
        Operator::KnnScan { table, .. } | Operator::WithinScan { table, .. } => {
            let schema = view
                .catalog
                .node_table(table)
                .ok_or_else(|| corrupt("ExpandRel source table is missing"))?;
            charges.push(charge_working_set(
                &view.shared.budget,
                &view.shared.pager,
                node_materialization_peak(view, schema)?,
                || "ExpandRel ranked input peak".into(),
            )?);
        }
        Operator::ScanInterface { interface, .. } => {
            for schema in view.catalog.node_tables() {
                if table_implements(view, schema.name(), interface) {
                    charges.push(charge_working_set(
                        &view.shared.budget,
                        &view.shared.pager,
                        source_decode_peak(view, schema, None)?,
                        || "ExpandRel interface input peak".into(),
                    )?);
                }
            }
        }
    }
    Ok(())
}

pub(super) fn source_decode_peak(
    view: &ReadView,
    schema: &NodeTableSchema,
    selected: Option<&BTreeSet<usize>>,
) -> DevonResult<usize> {
    let types = schema
        .columns()
        .iter()
        .map(|column| column.ty)
        .collect::<Vec<_>>();
    let mut maximum = 0;
    // DML may trigger a full boxed-group fallback even for a narrow scan.
    let full = selected.is_none()
        || types.iter().any(|ty| carries_b1_rescore(*ty))
        || view.state.chain.is_some()
        || view.own.is_some();
    if let Some(storage) = view.catalog.table_storage(schema.name()) {
        for page in &storage.groups {
            let directory = NodeGroup::read_directory(&view.shared.pager, *page, &types)?;
            let mut group = directory
                .row_count()
                .checked_mul(256)
                .ok_or_else(|| corrupt("source slots overflow"))?;
            for column in 0..types.len() {
                if full || selected.is_some_and(|set| set.contains(&column)) {
                    checked_working_set_add(
                        &mut group,
                        directory.column_decode_peak_bytes(column, &types)?,
                    )?;
                }
            }
            maximum = maximum.max(group);
        }
    }
    let mut link = view.state.chain.as_deref();
    while let Some(current) = link {
        add_delta_node_bytes(&mut maximum, &current.delta, schema.name())?;
        link = current.prev.as_deref();
    }
    if let Some(own) = &view.own {
        add_delta_node_bytes(&mut maximum, own, schema.name())?;
    }
    maximum
        .checked_mul(4)
        .ok_or_else(|| corrupt("source decode peak overflows"))
}
