//! Side-effect-free visible-row cursors and canonical statement dumps.

use super::*;

use std::io::Write;

use devondb_plan::{
    statement::{InterfaceColumn, RelRow, StatementEnvelope},
    text::printer::print_statement,
};
use devondb_storage::{csr_group::CsrGroup, overlay::CommitLink};
use devondb_types::schema::suggestion_suffix;

/// One visible node row and its stable offset in the pinned checkpoint epoch.
#[derive(Debug, Clone, PartialEq)]
pub struct VisibleNode {
    /// Physical node offset used by relationship endpoints in this snapshot.
    pub offset: u64,
    /// Property values in catalog declaration order.
    pub values: Vec<Value>,
}

/// One visible relationship row, including every property value.
#[derive(Debug, Clone, PartialEq)]
pub struct VisibleRelationship {
    /// Source-node physical offset in this snapshot.
    pub from_offset: u64,
    /// Destination-node physical offset in this snapshot.
    pub to_offset: u64,
    /// Relationship-property values in catalog declaration order.
    pub values: Vec<Value>,
}

/// A primary-key-ordered cursor over one node table's visible rows.
///
/// The cursor pins one immutable database snapshot and retains at most one
/// decoded node group and one output row. It does not materialize the table.
pub struct VisibleNodeCursor {
    snapshot: Snapshot,
    schema: NodeTableSchema,
    primary_key_column: usize,
    checkpointed_rows: u64,
    last_key: Option<NodeKey>,
    finished: bool,
}

/// A cursor over one relationship table's visible forward adjacency.
///
/// Checkpointed CSR is decoded one group at a time and the committed overlay
/// follows in commit/insertion order. Endpoint tombstones are tested directly
/// against the pinned commit chain, so deleted edges are never returned.
pub struct VisibleRelationshipCursor {
    snapshot: Snapshot,
    schema: RelTableSchema,
    types: Vec<LogicalType>,
    neighbor_rows: u64,
    group_index: usize,
    next_group_start: u64,
    group_start: u64,
    group: Option<CsrGroup>,
    slot: usize,
    edge_index: usize,
    base_finished: bool,
    overlay_lsn: u64,
    overlay_link: Option<Arc<CommitLink>>,
    overlay_index: usize,
    finished: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum NodeKey {
    Int64(i64),
    String(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct OverlayOrigin {
    commit_lsn: u64,
    row_index: usize,
}

enum NodeVisibility<'a> {
    Base(Option<&'a [Value]>),
    Overlay {
        origin: OverlayOrigin,
        row: &'a [Value],
    },
    Deleted,
}

impl Database {
    /// Returns a streaming, primary-key-ordered cursor for one node table.
    pub fn visible_nodes(&self, table: &str) -> DevonResult<VisibleNodeCursor> {
        VisibleNodeCursor::new(self.snapshot(), table)
    }

    /// Returns a streaming cursor for one relationship table, including
    /// relationship property values.
    pub fn visible_relationships(&self, table: &str) -> DevonResult<VisibleRelationshipCursor> {
        VisibleRelationshipCursor::new(self.snapshot(), table)
    }

    /// Writes a deterministic, parser-valid statement stream representing
    /// the pinned visible database state.
    ///
    /// Table schemas appear in catalog order, followed by ontology
    /// declarations, primary-key-ordered node inserts, and visible
    /// relationship inserts. Deleted rows and incident edges are absent.
    pub fn dump(&self, output: &mut impl Write) -> DevonResult<()> {
        dump_snapshot(&self.snapshot(), output)
    }
}

impl VisibleNodeCursor {
    fn new(snapshot: Snapshot, table: &str) -> DevonResult<Self> {
        let schema = snapshot
            .state
            .catalog
            .node_table(table)
            .cloned()
            .ok_or_else(|| missing_node_table(&snapshot, table))?;
        let primary_key_column = schema
            .columns()
            .iter()
            .position(|column| column.primary_key)
            .ok_or_else(|| corrupt(format!("node table `{}` has no primary key", schema.name())))?;
        let checkpointed_rows = checkpointed_node_rows(&snapshot, &schema)?;
        Ok(Self {
            snapshot,
            schema,
            primary_key_column,
            checkpointed_rows,
            last_key: None,
            finished: false,
        })
    }

    fn next_visible(&self) -> DevonResult<Option<(NodeKey, VisibleNode)>> {
        let mut best = None;
        self.scan_checkpointed(&mut best)?;
        self.scan_overlay(&mut best)?;
        Ok(best)
    }

    fn scan_checkpointed(&self, best: &mut Option<(NodeKey, VisibleNode)>) -> DevonResult<()> {
        let types = node_types(&self.schema);
        let group_ids = node_group_ids(&self.snapshot, self.schema.name());
        let mut group_start = 0_u64;
        for group_id in group_ids {
            let group = NodeGroup::read(&self.snapshot.shared.pager, *group_id, &types)?;
            for row_index in 0..group.row_count() {
                self.consider_checkpointed(&group, row_index, group_start, best)?;
            }
            group_start = add_rows(group_start, group.row_count(), self.schema.name())?;
        }
        Ok(())
    }

    fn consider_checkpointed(
        &self,
        group: &NodeGroup,
        row_index: usize,
        group_start: u64,
        best: &mut Option<(NodeKey, VisibleNode)>,
    ) -> DevonResult<()> {
        let key_value = group
            .value(row_index, self.primary_key_column)
            .ok_or_else(|| corrupt("node group is missing a primary-key value"))?;
        let key = NodeKey::try_from_value(key_value)?;
        if !self.key_is_candidate(&key, best) {
            return Ok(());
        }
        let values = match resolve_node(&self.snapshot.state, &self.schema, &key)? {
            NodeVisibility::Base(Some(replacement)) => replacement.to_vec(),
            NodeVisibility::Base(None) => group_row(group, row_index)?,
            NodeVisibility::Overlay { .. } | NodeVisibility::Deleted => return Ok(()),
        };
        let row_offset = u64::try_from(row_index)
            .map_err(|_| corrupt("node-group row index exceeds u64::MAX"))?;
        let offset = group_start
            .checked_add(row_offset)
            .ok_or_else(|| corrupt("node offset exceeds u64::MAX"))?;
        *best = Some((key, VisibleNode { offset, values }));
        Ok(())
    }

    fn scan_overlay(&self, best: &mut Option<(NodeKey, VisibleNode)>) -> DevonResult<()> {
        let mut remaining = overlay_insert_count(&self.snapshot.state, self.schema.name())?;
        let mut link = self.snapshot.state.chain.as_deref();
        while let Some(current) = link {
            let rows = delta_node_rows(current, self.schema.name());
            remaining = remaining
                .checked_sub(rows.len())
                .ok_or_else(|| corrupt("overlay insert position underflow"))?;
            for (row_index, row) in rows.iter().enumerate() {
                self.consider_overlay(current, row_index, row, remaining, best)?;
            }
            link = current.prev.as_deref();
        }
        Ok(())
    }

    fn consider_overlay(
        &self,
        link: &CommitLink,
        row_index: usize,
        row: &[Value],
        link_start: usize,
        best: &mut Option<(NodeKey, VisibleNode)>,
    ) -> DevonResult<()> {
        let key_value = row
            .get(self.primary_key_column)
            .ok_or_else(|| corrupt("overlay row is missing a primary-key value"))?;
        let key = NodeKey::try_from_value(key_value)?;
        if !self.key_is_candidate(&key, best) {
            return Ok(());
        }
        let origin = OverlayOrigin {
            commit_lsn: link.commit_lsn,
            row_index,
        };
        let NodeVisibility::Overlay {
            origin: visible_origin,
            row: visible,
        } = resolve_node(&self.snapshot.state, &self.schema, &key)?
        else {
            return Ok(());
        };
        if origin != visible_origin {
            return Ok(());
        }
        let position = link_start
            .checked_add(row_index)
            .ok_or_else(|| corrupt("overlay insert position exceeds usize::MAX"))?;
        let offset = node_offset(self.checkpointed_rows, position)?;
        *best = Some((
            key,
            VisibleNode {
                offset,
                values: visible.to_vec(),
            },
        ));
        Ok(())
    }

    fn key_is_candidate(&self, key: &NodeKey, best: &Option<(NodeKey, VisibleNode)>) -> bool {
        self.last_key.as_ref().is_none_or(|last| key > last)
            && best.as_ref().is_none_or(|(current, _)| key < current)
    }
}

impl Iterator for VisibleNodeCursor {
    type Item = DevonResult<VisibleNode>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.finished {
            return None;
        }
        match self.next_visible() {
            Ok(Some((key, row))) => {
                self.last_key = Some(key);
                Some(Ok(row))
            }
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

impl VisibleRelationshipCursor {
    fn new(snapshot: Snapshot, table: &str) -> DevonResult<Self> {
        let schema = snapshot
            .state
            .catalog
            .rel_table(table)
            .cloned()
            .ok_or_else(|| missing_rel_table(&snapshot, table))?;
        let source_groups = node_group_ids(&snapshot, schema.from()).len();
        let fwd_groups = snapshot
            .state
            .catalog
            .rel_storage(schema.name())
            .map_or(0, |storage| storage.fwd.len());
        if fwd_groups > source_groups {
            return Err(corrupt(
                "relationship forward storage has more groups than its source table",
            ));
        }
        let neighbor_schema = snapshot
            .state
            .catalog
            .node_table(schema.to())
            .ok_or_else(|| corrupt("relationship destination table is missing"))?;
        let neighbor_rows = checkpointed_node_rows(&snapshot, neighbor_schema)?;
        let types = schema.columns().iter().map(|column| column.ty).collect();
        Ok(Self {
            snapshot,
            schema,
            types,
            neighbor_rows,
            group_index: 0,
            next_group_start: 0,
            group_start: 0,
            group: None,
            slot: 0,
            edge_index: 0,
            base_finished: false,
            overlay_lsn: 0,
            overlay_link: None,
            overlay_index: 0,
            finished: false,
        })
    }

    fn next_visible(&mut self) -> DevonResult<Option<VisibleRelationship>> {
        loop {
            let edge = if self.base_finished {
                self.next_overlay()?
            } else if let Some(edge) = self.next_checkpointed()? {
                Some(edge)
            } else {
                self.base_finished = true;
                self.next_overlay()?
            };
            let Some(edge) = edge else {
                return Ok(None);
            };
            if !edge_is_tombstoned(
                &self.snapshot.state,
                self.schema.name(),
                edge.from_offset,
                edge.to_offset,
            ) {
                return Ok(Some(edge));
            }
        }
    }

    fn next_checkpointed(&mut self) -> DevonResult<Option<VisibleRelationship>> {
        loop {
            if let Some(edge) = self.next_group_edge()? {
                return Ok(Some(edge));
            }
            if !self.load_next_group()? {
                return Ok(None);
            }
        }
    }

    fn next_group_edge(&mut self) -> DevonResult<Option<VisibleRelationship>> {
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
                let from_offset = self
                    .group_start
                    .checked_add(slot)
                    .ok_or_else(|| corrupt("relationship source offset exceeds u64::MAX"))?;
                return Ok(Some(VisibleRelationship {
                    from_offset,
                    to_offset: csr_neighbor(group, edge_index)?,
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
        let source = self
            .snapshot
            .state
            .catalog
            .node_table(self.schema.from())
            .ok_or_else(|| corrupt("relationship source table is missing"))?;
        let source_groups = node_group_ids(&self.snapshot, source.name());
        while let Some(group_id) = source_groups.get(self.group_index) {
            let index = self.group_index;
            self.group_index += 1;
            let row_count = NodeGroup::read_row_count(
                &self.snapshot.shared.pager,
                *group_id,
                source.columns().len(),
            )?;
            self.group_start = self.next_group_start;
            self.next_group_start = add_rows(self.next_group_start, row_count, source.name())?;
            let csr_id = self
                .snapshot
                .state
                .catalog
                .rel_storage(self.schema.name())
                .and_then(|storage| storage.fwd.get(index))
                .copied()
                .filter(|page_id| *page_id != 0);
            let Some(csr_id) = csr_id else {
                continue;
            };
            let group = CsrGroup::read_checked(
                &self.snapshot.shared.pager,
                csr_id,
                &self.types,
                self.neighbor_rows,
            )?;
            if group.row_count() > row_count {
                return Err(corrupt(format!(
                    "CSR group {index} row_count {} exceeds endpoint node-group row_count {row_count}",
                    group.row_count()
                )));
            }
            self.group = Some(group);
            self.slot = 0;
            self.edge_index = 0;
            return Ok(true);
        }
        Ok(false)
    }

    fn next_overlay(&mut self) -> DevonResult<Option<VisibleRelationship>> {
        loop {
            if let Some(link) = &self.overlay_link {
                let edges = delta_rel_edges(link, self.schema.name());
                if let Some(edge) = edges.get(self.overlay_index) {
                    self.overlay_index += 1;
                    return Ok(Some(VisibleRelationship {
                        from_offset: edge.from,
                        to_offset: edge.to,
                        values: edge.values.clone(),
                    }));
                }
                self.overlay_lsn = link.commit_lsn;
            }
            self.overlay_link =
                next_rel_link(&self.snapshot.state, self.schema.name(), self.overlay_lsn);
            self.overlay_index = 0;
            if self.overlay_link.is_none() {
                return Ok(None);
            }
        }
    }
}

impl Iterator for VisibleRelationshipCursor {
    type Item = DevonResult<VisibleRelationship>;

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

impl NodeKey {
    fn try_from_value(value: &Value) -> DevonResult<Self> {
        match value {
            Value::Int64(value) => Ok(Self::Int64(*value)),
            Value::String(value) => Ok(Self::String(value.clone())),
            other => Err(corrupt(format!(
                "node primary key must be Int64 or String, found {other}"
            ))),
        }
    }

    fn matches(&self, value: &Value) -> bool {
        match (self, value) {
            (Self::Int64(left), Value::Int64(right)) => left == right,
            (Self::String(left), Value::String(right)) => left == right,
            _ => false,
        }
    }
}

fn dump_snapshot(snapshot: &Snapshot, output: &mut impl Write) -> DevonResult<()> {
    write_schema(snapshot, output)?;
    write_ontology(snapshot, output)?;
    write_nodes(snapshot, output)?;
    write_relationships(snapshot, output)
}

fn write_schema(snapshot: &Snapshot, output: &mut impl Write) -> DevonResult<()> {
    for schema in snapshot.state.catalog.node_tables() {
        write_statement(
            output,
            Statement::CreateNodeTable {
                name: schema.name().to_owned(),
                columns: schema.columns().to_vec(),
            },
        )?;
    }
    for schema in snapshot.state.catalog.rel_tables() {
        write_statement(
            output,
            Statement::CreateRelTable {
                name: schema.name().to_owned(),
                from: schema.from().to_owned(),
                to: schema.to().to_owned(),
                columns: schema.columns().to_vec(),
            },
        )?;
    }
    Ok(())
}

fn write_ontology(snapshot: &Snapshot, output: &mut impl Write) -> DevonResult<()> {
    let Some(ontology) = snapshot.state.catalog.ontology() else {
        return Ok(());
    };
    for interface in &ontology.interfaces {
        write_statement(
            output,
            Statement::CreateInterface {
                name: interface.name.clone(),
                columns: interface
                    .columns
                    .iter()
                    .map(|column| InterfaceColumn {
                        name: column.name.clone(),
                        ty: column.ty,
                    })
                    .collect(),
            },
        )?;
    }
    for class in &ontology.node_classes {
        write_statement(
            output,
            Statement::CreateClass {
                table: class.table.clone(),
                display: class.display.clone(),
                plural: class.plural.clone(),
                label: class.label.clone(),
                summary: class.summary.clone(),
                color: class.color.clone(),
                description: class.description.clone(),
                verb: None,
                inverse: None,
                implements: class.implements.clone(),
            },
        )?;
    }
    for class in &ontology.rel_classes {
        write_statement(
            output,
            Statement::CreateClass {
                table: class.table.clone(),
                display: None,
                plural: None,
                label: None,
                summary: Vec::new(),
                color: None,
                description: None,
                verb: class.verb.clone(),
                inverse: class.inverse.clone(),
                implements: Vec::new(),
            },
        )?;
    }
    Ok(())
}

fn write_nodes(snapshot: &Snapshot, output: &mut impl Write) -> DevonResult<()> {
    for schema in snapshot.state.catalog.node_tables() {
        let mut cursor = VisibleNodeCursor::new(clone_snapshot(snapshot), schema.name())?;
        for row in &mut cursor {
            write_statement(
                output,
                Statement::InsertNode {
                    table: schema.name().to_owned(),
                    rows: vec![row?.values],
                },
            )?;
        }
    }
    Ok(())
}

fn write_relationships(snapshot: &Snapshot, output: &mut impl Write) -> DevonResult<()> {
    for schema in snapshot.state.catalog.rel_tables() {
        let mut cursor = VisibleRelationshipCursor::new(clone_snapshot(snapshot), schema.name())?;
        for edge in &mut cursor {
            let edge = edge?;
            let from_key = node_key_at_offset(snapshot, schema.from(), edge.from_offset)?;
            let to_key = node_key_at_offset(snapshot, schema.to(), edge.to_offset)?;
            write_statement(
                output,
                Statement::InsertRel {
                    table: schema.name().to_owned(),
                    rows: vec![RelRow {
                        from_key,
                        to_key,
                        values: edge.values,
                    }],
                },
            )?;
        }
    }
    Ok(())
}

fn write_statement(output: &mut impl Write, statement: Statement) -> DevonResult<()> {
    let text = print_statement(&StatementEnvelope {
        v: PLAN_VERSION,
        stmt: statement,
    })?;
    writeln!(output, "{text}")?;
    Ok(())
}

fn clone_snapshot(snapshot: &Snapshot) -> Snapshot {
    Snapshot::new(Arc::clone(&snapshot.shared), Arc::clone(&snapshot.state))
}

fn resolve_node<'a>(
    state: &'a PublishedState,
    schema: &NodeTableSchema,
    key: &NodeKey,
) -> DevonResult<NodeVisibility<'a>> {
    let key_column = schema
        .columns()
        .iter()
        .position(|column| column.primary_key)
        .ok_or_else(|| corrupt(format!("node table `{}` has no primary key", schema.name())))?;
    let mut replacement = None;
    let mut link = state.chain.as_deref();
    while let Some(current) = link {
        if replacement.is_none() {
            if delta_node_deletes(current, schema.name())
                .iter()
                .rev()
                .any(|value| key.matches(value))
            {
                return Ok(NodeVisibility::Deleted);
            }
            replacement = delta_node_updates(current, schema.name())
                .iter()
                .rev()
                .find(|row| row.get(key_column).is_some_and(|value| key.matches(value)))
                .map(Vec::as_slice);
        }
        if let Some((row_index, row)) = delta_node_rows(current, schema.name())
            .iter()
            .enumerate()
            .rev()
            .find(|(_, row)| row.get(key_column).is_some_and(|value| key.matches(value)))
        {
            return Ok(NodeVisibility::Overlay {
                origin: OverlayOrigin {
                    commit_lsn: current.commit_lsn,
                    row_index,
                },
                row: replacement.unwrap_or(row),
            });
        }
        link = current.prev.as_deref();
    }
    Ok(NodeVisibility::Base(replacement))
}

fn node_key_at_offset(snapshot: &Snapshot, table: &str, offset: u64) -> DevonResult<Value> {
    let schema = snapshot
        .state
        .catalog
        .node_table(table)
        .ok_or_else(|| corrupt(format!("relationship endpoint table `{table}` is missing")))?;
    let key_column = schema
        .columns()
        .iter()
        .position(|column| column.primary_key)
        .ok_or_else(|| corrupt(format!("node table `{table}` has no primary key")))?;
    let checkpointed = checkpointed_node_rows(snapshot, schema)?;
    if offset < checkpointed {
        return checkpointed_key_at_offset(snapshot, schema, key_column, offset);
    }
    overlay_key_at_offset(snapshot, schema, key_column, offset - checkpointed)
}

fn checkpointed_key_at_offset(
    snapshot: &Snapshot,
    schema: &NodeTableSchema,
    key_column: usize,
    offset: u64,
) -> DevonResult<Value> {
    let types = node_types(schema);
    let mut start = 0_u64;
    for group_id in node_group_ids(snapshot, schema.name()) {
        let rows =
            NodeGroup::read_row_count(&snapshot.shared.pager, *group_id, schema.columns().len())?;
        let end = add_rows(start, rows, schema.name())?;
        if offset < end {
            let (keys, _) =
                NodeGroup::read_column(&snapshot.shared.pager, *group_id, &types, key_column)?;
            let row = usize::try_from(offset - start)
                .map_err(|_| corrupt("node offset row index exceeds usize::MAX"))?;
            return keys
                .get(row)
                .cloned()
                .ok_or_else(|| corrupt("node group is missing a relationship endpoint key"));
        }
        start = end;
    }
    Err(corrupt(format!(
        "node offset {offset} is outside table `{}`",
        schema.name()
    )))
}

fn overlay_key_at_offset(
    snapshot: &Snapshot,
    schema: &NodeTableSchema,
    key_column: usize,
    position: u64,
) -> DevonResult<Value> {
    let total = overlay_insert_count(&snapshot.state, schema.name())?;
    let position =
        usize::try_from(position).map_err(|_| corrupt("overlay node offset exceeds usize::MAX"))?;
    if position >= total {
        return Err(corrupt(format!(
            "overlay node offset {position} is outside table `{}`",
            schema.name()
        )));
    }
    let mut remaining = total;
    let mut link = snapshot.state.chain.as_deref();
    while let Some(current) = link {
        let rows = delta_node_rows(current, schema.name());
        remaining -= rows.len();
        if position >= remaining && position < remaining + rows.len() {
            return rows[position - remaining]
                .get(key_column)
                .cloned()
                .ok_or_else(|| corrupt("overlay endpoint row has no primary key"));
        }
        link = current.prev.as_deref();
    }
    Err(corrupt("overlay endpoint position has no assigned row"))
}

fn checkpointed_node_rows(snapshot: &Snapshot, schema: &NodeTableSchema) -> DevonResult<u64> {
    let mut total = 0_u64;
    for group_id in node_group_ids(snapshot, schema.name()) {
        let rows =
            NodeGroup::read_row_count(&snapshot.shared.pager, *group_id, schema.columns().len())?;
        total = add_rows(total, rows, schema.name())?;
    }
    Ok(total)
}

fn add_rows(total: u64, rows: usize, table: &str) -> DevonResult<u64> {
    let rows = u64::try_from(rows)
        .map_err(|_| corrupt(format!("node table `{table}` row count exceeds u64::MAX")))?;
    total
        .checked_add(rows)
        .ok_or_else(|| corrupt(format!("node table `{table}` row count exceeds u64::MAX")))
}

fn overlay_insert_count(state: &PublishedState, table: &str) -> DevonResult<usize> {
    let mut total = 0_usize;
    let mut link = state.chain.as_deref();
    while let Some(current) = link {
        total = total
            .checked_add(delta_node_rows(current, table).len())
            .ok_or_else(|| corrupt("overlay node insert count exceeds usize::MAX"))?;
        link = current.prev.as_deref();
    }
    Ok(total)
}

fn edge_is_tombstoned(state: &PublishedState, table: &str, from: u64, to: u64) -> bool {
    let mut link = state.chain.as_deref();
    while let Some(current) = link {
        if current
            .delta
            .rel_tombstones
            .get(table)
            .is_some_and(|tombstones| {
                tombstones.from_offsets.contains(&from) || tombstones.to_offsets.contains(&to)
            })
        {
            return true;
        }
        link = current.prev.as_deref();
    }
    false
}

fn next_rel_link(state: &PublishedState, table: &str, after: u64) -> Option<Arc<CommitLink>> {
    let mut best: Option<Arc<CommitLink>> = None;
    let mut link = state.chain.clone();
    while let Some(current) = link {
        let eligible = current.commit_lsn > after && !delta_rel_edges(&current, table).is_empty();
        let earlier = best
            .as_ref()
            .is_none_or(|candidate| current.commit_lsn < candidate.commit_lsn);
        if eligible && earlier {
            best = Some(Arc::clone(&current));
        }
        link = current.prev.clone();
    }
    best
}

fn node_group_ids<'a>(snapshot: &'a Snapshot, table: &str) -> &'a [u64] {
    snapshot
        .state
        .catalog
        .table_storage(table)
        .map_or(&[], |storage| storage.groups.as_slice())
}

fn node_types(schema: &NodeTableSchema) -> Vec<LogicalType> {
    schema.columns().iter().map(|column| column.ty).collect()
}

fn group_row(group: &NodeGroup, row: usize) -> DevonResult<Vec<Value>> {
    (0..group.column_count())
        .map(|column| {
            group
                .value(row, column)
                .cloned()
                .ok_or_else(|| corrupt("node group is missing a row value"))
        })
        .collect()
}

fn csr_neighbor(group: &CsrGroup, edge: usize) -> DevonResult<u64> {
    group
        .neighbor(edge)
        .ok_or_else(|| corrupt("CSR group is missing an edge neighbor"))
}

fn csr_values(group: &CsrGroup, edge: usize) -> DevonResult<Vec<Value>> {
    (0..group.column_count())
        .map(|column| {
            group
                .value(edge, column)
                .cloned()
                .ok_or_else(|| corrupt("CSR group is missing an edge property value"))
        })
        .collect()
}

fn delta_node_rows<'a>(link: &'a CommitLink, table: &str) -> &'a [Vec<Value>] {
    link.delta.nodes.get(table).map_or(&[], Vec::as_slice)
}

fn delta_node_updates<'a>(link: &'a CommitLink, table: &str) -> &'a [Vec<Value>] {
    link.delta
        .node_updates
        .get(table)
        .map_or(&[], Vec::as_slice)
}

fn delta_node_deletes<'a>(link: &'a CommitLink, table: &str) -> &'a [Value] {
    link.delta
        .node_deletes
        .get(table)
        .map_or(&[], Vec::as_slice)
}

fn delta_rel_edges<'a>(link: &'a CommitLink, table: &str) -> &'a [OverlayEdge] {
    link.delta.edges.get(table).map_or(&[], Vec::as_slice)
}

fn missing_node_table(snapshot: &Snapshot, table: &str) -> DevonError {
    DevonError::NotFound {
        what: format!(
            "node table `{table}`{}",
            suggestion_suffix(
                table,
                snapshot
                    .state
                    .catalog
                    .node_tables()
                    .iter()
                    .map(NodeTableSchema::name)
            )
        ),
    }
}

fn missing_rel_table(snapshot: &Snapshot, table: &str) -> DevonError {
    DevonError::NotFound {
        what: format!(
            "relationship table `{table}`{}",
            suggestion_suffix(
                table,
                snapshot
                    .state
                    .catalog
                    .rel_tables()
                    .iter()
                    .map(RelTableSchema::name)
            )
        ),
    }
}
