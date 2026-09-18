//! Node-table storage: WAL-durable inserts, checkpoints into node groups,
//! full scans, and WAL replay recovery.
//!
//! Model: rows are durable in the WAL the moment `insert` returns;
//! `checkpoint` materializes buffered rows into node groups (rewriting
//! the partial tail group onto fresh pages) and publishes them through the
//! catalog storage map; retired pages are reclaimed by free-page management.
//! `scan` reads groups in order, then the buffered tail.

use std::collections::{BTreeMap, BTreeSet};

use devondb_types::{
    DevonError, DevonResult, logical_type::LogicalType, schema::NodeTableSchema, value::Value,
};
use serde::Serialize;

use crate::catalog::{Catalog, TableStorage};
use crate::node_group::{NODE_GROUP_CAPACITY, NodeGroup};
use crate::overlay::{NodeDmlEffects, PkKey};
use crate::pager::Pager;
use crate::wal::WalWriter;

/// Whether a checkpoint caller has proved that deleting rows may compact offsets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteCompaction {
    /// Deletes may remove slots and shift every later node offset.
    Permitted,
    /// Deletes must fail because later node offsets may be load-bearing.
    Forbidden,
}

/// WAL-backed storage for one catalog node table.
pub struct NodeTable {
    schema: NodeTableSchema,
    buffered_rows: Vec<Vec<Value>>,
}

#[derive(Serialize)]
struct WalRecord {
    table: String,
    row: Vec<Value>,
}

struct MaterializationEffects<'a> {
    int_updates: BTreeMap<i64, &'a [Value]>,
    string_updates: BTreeMap<String, &'a [Value]>,
    int_tombstones: BTreeSet<i64>,
    string_tombstones: BTreeSet<String>,
}

#[derive(Default)]
struct BufferedKeys {
    int_keys: BTreeSet<i64>,
    string_keys: BTreeSet<String>,
}

struct RewrittenGroups {
    ids: Vec<u64>,
    compacted_rows: Option<Vec<Vec<Value>>>,
}

impl<'a> From<NodeDmlEffects<'a>> for MaterializationEffects<'a> {
    fn from(effects: NodeDmlEffects<'a>) -> Self {
        let mut materialized = Self {
            int_updates: BTreeMap::new(),
            string_updates: BTreeMap::new(),
            int_tombstones: BTreeSet::new(),
            string_tombstones: BTreeSet::new(),
        };
        for (key, row) in effects.updates {
            match key {
                PkKey::Int64(key) => {
                    materialized.int_updates.insert(key, row);
                }
                PkKey::String(key) => {
                    materialized.string_updates.insert(key.to_owned(), row);
                }
            }
        }
        for key in effects.tombstones {
            match key {
                PkKey::Int64(key) => {
                    materialized.int_tombstones.insert(key);
                }
                PkKey::String(key) => {
                    materialized.string_tombstones.insert(key.to_owned());
                }
            }
        }
        materialized
    }
}

impl<'a> MaterializationEffects<'a> {
    fn take_tombstone(&mut self, value: &Value) -> DevonResult<bool> {
        match value {
            Value::Int64(key) => Ok(self.int_tombstones.remove(key)),
            Value::String(key) => Ok(self.string_tombstones.remove(key.as_str())),
            other => Err(corrupt(format!(
                "node row primary key must be Int64 or String, found {other}"
            ))),
        }
    }

    fn take_update(&mut self, value: &Value) -> DevonResult<Option<&'a [Value]>> {
        match value {
            Value::Int64(key) => Ok(self.int_updates.remove(key)),
            Value::String(key) => Ok(self.string_updates.remove(key.as_str())),
            other => Err(corrupt(format!(
                "node row primary key must be Int64 or String, found {other}"
            ))),
        }
    }
}

impl BufferedKeys {
    fn insert(&mut self, value: &Value) -> DevonResult<bool> {
        match value {
            Value::Int64(key) => Ok(self.int_keys.insert(*key)),
            Value::String(key) => Ok(self.string_keys.insert(key.clone())),
            other => Err(corrupt(format!(
                "node row primary key must be Int64 or String, found {other}"
            ))),
        }
    }

    fn contains(&self, value: &Value) -> bool {
        match value {
            Value::Int64(key) => self.int_keys.contains(key),
            Value::String(key) => self.string_keys.contains(key.as_str()),
            _ => false,
        }
    }
}

impl NodeTable {
    /// Creates an empty in-memory buffer for a validated catalog node table.
    #[must_use]
    pub const fn new(schema: NodeTableSchema) -> Self {
        Self {
            schema,
            buffered_rows: Vec::new(),
        }
    }

    /// Returns this table's validated schema.
    #[must_use]
    pub fn schema(&self) -> &NodeTableSchema {
        &self.schema
    }

    /// Validates and durably appends one row to this table.
    pub fn insert(&mut self, wal: &mut WalWriter, row: Vec<Value>) -> DevonResult<()> {
        validate_row(&self.schema, &row)?;
        let payload = encode_wal_record(self.schema.name(), &row)?;
        wal.append(&payload)?;
        wal.sync()?;
        self.buffered_rows.push(row);
        Ok(())
    }

    /// Validates and buffers one row recovered from the database WAL.
    pub fn recover_row(&mut self, row: Vec<Value>) -> DevonResult<()> {
        validate_row(&self.schema, &row)?;
        self.buffered_rows.push(row);
        Ok(())
    }

    /// Returns whether this table has rows awaiting a checkpoint.
    #[must_use]
    pub fn has_buffered_rows(&self) -> bool {
        !self.buffered_rows.is_empty()
    }

    /// Materializes every buffered row and publishes the resulting groups.
    pub fn checkpoint(&mut self, pager: &Pager, catalog: &mut Catalog) -> DevonResult<()> {
        self.checkpoint_with_dml(
            pager,
            catalog,
            NodeDmlEffects::default(),
            DeleteCompaction::Forbidden,
        )
    }

    /// Materializes buffered rows after applying committed update/delete effects.
    ///
    /// Updates replace their matching row in place. Deletes are accepted only
    /// when `delete_compaction` explicitly permits the resulting offset shifts.
    pub fn checkpoint_with_dml(
        &mut self,
        pager: &Pager,
        catalog: &mut Catalog,
        effects: NodeDmlEffects<'_>,
        delete_compaction: DeleteCompaction,
    ) -> DevonResult<()> {
        if self.buffered_rows.is_empty()
            && effects.updates.is_empty()
            && effects.tombstones.is_empty()
        {
            return Ok(());
        }
        ensure_catalog_schema(&self.schema, catalog)?;
        validate_dml_effects(&self.schema, &effects, delete_compaction)?;
        let mut effects = MaterializationEffects::from(effects);
        let storage = materialize_rows_with_dml(
            pager,
            &self.schema,
            catalog.table_storage(self.schema.name()),
            &self.buffered_rows,
            &mut effects,
            delete_compaction,
        )?;
        ensure_effects_consumed(self.schema.name(), &effects)?;
        catalog.set_table_storage(self.schema.name(), storage)?;
        self.buffered_rows.clear();
        Ok(())
    }

    /// Reads checkpointed groups followed by the uncheckpointed row tail.
    pub fn scan(&self, pager: &Pager, catalog: &Catalog) -> DevonResult<Vec<Vec<Value>>> {
        let mut rows = Vec::new();
        let group_ids = catalog
            .table_storage(self.schema.name())
            .map_or(&[][..], |storage| storage.groups.as_slice());
        let types = schema_types(&self.schema);
        for (index, group_id) in group_ids.iter().enumerate() {
            let group = NodeGroup::read(pager, *group_id, &types)?;
            validate_group_position(&group, index, group_ids.len())?;
            rows.extend(group_rows(&group)?);
        }
        rows.extend(self.buffered_rows.iter().cloned());
        Ok(rows)
    }
}

fn encode_wal_record(table: &str, row: &[Value]) -> DevonResult<Vec<u8>> {
    serde_json::to_vec(&WalRecord {
        table: table.to_owned(),
        row: row.to_vec(),
    })
    .map_err(|error| invalid_argument(format!("node insert cannot be encoded: {error}")))
}

/// Validates one row against a node-table schema without producing side effects.
pub(crate) fn validate_row(schema: &NodeTableSchema, row: &[Value]) -> DevonResult<()> {
    if row.len() != schema.columns().len() {
        return Err(invalid_argument(format!(
            "node table `{}` expects {} columns but row has {} values",
            schema.name(),
            schema.columns().len(),
            row.len()
        )));
    }
    for (column, value) in schema.columns().iter().zip(row) {
        if !value.matches_type(&column.ty) {
            return Err(invalid_argument(format!(
                "column `{}` in node table `{}` expects {} but received {value}",
                column.name,
                schema.name(),
                column.ty
            )));
        }
    }
    Ok(())
}

fn ensure_catalog_schema(schema: &NodeTableSchema, catalog: &Catalog) -> DevonResult<()> {
    let catalog_schema = catalog
        .node_table(schema.name())
        .ok_or_else(|| DevonError::NotFound {
            what: format!("node table `{}`", schema.name()),
        })?;
    if catalog_schema != schema {
        return Err(invalid_argument(format!(
            "node table `{}` schema differs from the catalog",
            schema.name()
        )));
    }
    Ok(())
}

fn materialize_rows(
    pager: &Pager,
    schema: &NodeTableSchema,
    persisted: Option<&TableStorage>,
    buffered_rows: &[Vec<Value>],
) -> DevonResult<TableStorage> {
    let types = schema_types(schema);
    let mut groups = persisted.cloned().unwrap_or_default().groups;
    let mut next_row = 0;
    rewrite_partial_tail(pager, &types, &mut groups, buffered_rows, &mut next_row)?;
    while next_row < buffered_rows.len() {
        let mut group = NodeGroup::new(types.clone())?;
        fill_group(&mut group, buffered_rows, &mut next_row)?;
        groups.push(group.write(pager)?);
    }
    Ok(TableStorage { groups })
}

fn materialize_rows_with_dml(
    pager: &Pager,
    schema: &NodeTableSchema,
    persisted: Option<&TableStorage>,
    buffered_rows: &[Vec<Value>],
    effects: &mut MaterializationEffects<'_>,
    delete_compaction: DeleteCompaction,
) -> DevonResult<TableStorage> {
    let types = schema_types(schema);
    let primary_key_column = primary_key_column(schema)?;
    let (buffered_rows, buffered_keys) =
        normalize_buffered_rows(buffered_rows, primary_key_column)?;
    let persisted_groups = persisted.map_or(&[][..], |storage| storage.groups.as_slice());
    let RewrittenGroups {
        ids: mut groups,
        compacted_rows: mut compacted,
    } = rewrite_persisted_groups(
        pager,
        &types,
        persisted_groups,
        primary_key_column,
        effects,
        &buffered_keys,
        delete_compaction,
    )?;
    let (buffered, _, _) = apply_dml_effects(
        &buffered_rows,
        primary_key_column,
        effects,
        None,
        delete_compaction,
    )?;
    if let Some(rows) = compacted.as_mut() {
        rows.extend(buffered);
        write_full_groups(pager, &types, &mut groups, rows)?;
        write_partial_group(pager, &types, &mut groups, rows)?;
        return Ok(TableStorage { groups });
    }
    if buffered.is_empty() {
        return Ok(TableStorage { groups });
    }
    materialize_rows(pager, schema, Some(&TableStorage { groups }), &buffered)
}

fn rewrite_persisted_groups(
    pager: &Pager,
    types: &[LogicalType],
    persisted_groups: &[u64],
    primary_key_column: usize,
    effects: &mut MaterializationEffects<'_>,
    buffered_keys: &BufferedKeys,
    delete_compaction: DeleteCompaction,
) -> DevonResult<RewrittenGroups> {
    let mut groups = Vec::with_capacity(persisted_groups.len());
    let mut compacted: Option<Vec<Vec<Value>>> = None;
    for (index, group_id) in persisted_groups.iter().copied().enumerate() {
        let group = NodeGroup::read(pager, group_id, types)?;
        validate_group_position(&group, index, persisted_groups.len())?;
        let (rows, changed, deleted) = apply_dml_effects(
            &group_rows(&group)?,
            primary_key_column,
            effects,
            Some(buffered_keys),
            delete_compaction,
        )?;
        if let Some(pending) = compacted.as_mut() {
            pending.extend(rows);
            write_full_groups(pager, types, &mut groups, pending)?;
        } else if deleted {
            compacted = Some(rows);
        } else if changed {
            groups.push(write_group(pager, types, rows)?);
        } else {
            groups.push(group_id);
        }
    }
    Ok(RewrittenGroups {
        ids: groups,
        compacted_rows: compacted,
    })
}

fn apply_dml_effects(
    rows: &[Vec<Value>],
    primary_key_column: usize,
    effects: &mut MaterializationEffects<'_>,
    shadowed_keys: Option<&BufferedKeys>,
    delete_compaction: DeleteCompaction,
) -> DevonResult<(Vec<Vec<Value>>, bool, bool)> {
    let mut output = Vec::with_capacity(rows.len());
    let mut changed = false;
    let mut deleted = false;
    for row in rows {
        let key = row.get(primary_key_column).ok_or_else(|| {
            corrupt(format!(
                "node row has no primary-key column at index {primary_key_column}"
            ))
        })?;
        if shadowed_keys.is_some_and(|keys| keys.contains(key)) {
            require_delete_compaction(delete_compaction)?;
            changed = true;
            deleted = true;
        } else if effects.take_tombstone(key)? {
            changed = true;
            deleted = true;
        } else if let Some(replacement) = effects.take_update(key)? {
            output.push(replacement.to_vec());
            changed = true;
        } else {
            output.push(row.clone());
        }
    }
    Ok((output, changed, deleted))
}

fn normalize_buffered_rows(
    rows: &[Vec<Value>],
    primary_key_column: usize,
) -> DevonResult<(Vec<Vec<Value>>, BufferedKeys)> {
    let mut keys = BufferedKeys::default();
    let mut normalized = Vec::with_capacity(rows.len());
    for row in rows.iter().rev() {
        let key = row.get(primary_key_column).ok_or_else(|| {
            corrupt(format!(
                "node row has no primary-key column at index {primary_key_column}"
            ))
        })?;
        if keys.insert(key)? {
            normalized.push(row.clone());
        }
    }
    normalized.reverse();
    Ok((normalized, keys))
}

fn require_delete_compaction(delete_compaction: DeleteCompaction) -> DevonResult<()> {
    if delete_compaction == DeleteCompaction::Forbidden {
        return Err(invalid_argument(
            "delete compaction was not affirmed; reviving an inserted primary key shifts load-bearing node offsets",
        ));
    }
    Ok(())
}

fn write_full_groups(
    pager: &Pager,
    types: &[LogicalType],
    groups: &mut Vec<u64>,
    rows: &mut Vec<Vec<Value>>,
) -> DevonResult<()> {
    while rows.len() >= NODE_GROUP_CAPACITY {
        let remaining = rows.split_off(NODE_GROUP_CAPACITY);
        let full = std::mem::replace(rows, remaining);
        groups.push(write_group(pager, types, full)?);
    }
    Ok(())
}

fn write_partial_group(
    pager: &Pager,
    types: &[LogicalType],
    groups: &mut Vec<u64>,
    rows: &mut Vec<Vec<Value>>,
) -> DevonResult<()> {
    if !rows.is_empty() {
        groups.push(write_group(pager, types, std::mem::take(rows))?);
    }
    Ok(())
}

fn write_group(pager: &Pager, types: &[LogicalType], rows: Vec<Vec<Value>>) -> DevonResult<u64> {
    let mut group = NodeGroup::new(types.to_vec())?;
    for row in rows {
        group.push_row(row)?;
    }
    group.write(pager)
}

fn validate_dml_effects(
    schema: &NodeTableSchema,
    effects: &NodeDmlEffects<'_>,
    delete_compaction: DeleteCompaction,
) -> DevonResult<()> {
    if !effects.tombstones.is_empty() && delete_compaction == DeleteCompaction::Forbidden {
        return Err(invalid_argument(format!(
            "delete compaction for node table `{}` was not affirmed; removing a row may shift load-bearing node offsets",
            schema.name()
        )));
    }
    if effects
        .updates
        .keys()
        .any(|key| effects.tombstones.contains(key))
    {
        return Err(corrupt(format!(
            "node table `{}` has a primary key with both update and delete effects",
            schema.name()
        )));
    }
    let primary_key_column = primary_key_column(schema)?;
    for (key, row) in &effects.updates {
        validate_row(schema, row).map_err(|error| {
            corrupt(format!(
                "checkpoint update for node table `{}` carries an invalid replacement row: {error}",
                schema.name()
            ))
        })?;
        if !key_matches_value(*key, &row[primary_key_column]) {
            return Err(corrupt(format!(
                "checkpoint update key {} disagrees with its replacement row in node table `{}`",
                display_key(*key),
                schema.name()
            )));
        }
    }
    Ok(())
}

fn ensure_effects_consumed(table: &str, effects: &MaterializationEffects<'_>) -> DevonResult<()> {
    if let Some(key) = effects.int_updates.keys().next() {
        return Err(corrupt(format!(
            "checkpoint update for node table `{table}` targets primary key {key} which exists nowhere in persisted or overlay rows"
        )));
    }
    if let Some(key) = effects.string_updates.keys().next() {
        return Err(corrupt(format!(
            "checkpoint update for node table `{table}` targets primary key {key:?} which exists nowhere in persisted or overlay rows"
        )));
    }
    if let Some(key) = effects.int_tombstones.iter().next() {
        return Err(corrupt(format!(
            "checkpoint delete for node table `{table}` targets primary key {key} which exists nowhere in persisted or overlay rows"
        )));
    }
    if let Some(key) = effects.string_tombstones.iter().next() {
        return Err(corrupt(format!(
            "checkpoint delete for node table `{table}` targets primary key {key:?} which exists nowhere in persisted or overlay rows"
        )));
    }
    Ok(())
}

fn primary_key_column(schema: &NodeTableSchema) -> DevonResult<usize> {
    schema
        .columns()
        .iter()
        .position(|column| column.primary_key)
        .ok_or_else(|| {
            corrupt(format!(
                "node table `{}` has no primary-key column",
                schema.name()
            ))
        })
}

fn key_matches_value(key: PkKey<'_>, value: &Value) -> bool {
    match (key, value) {
        (PkKey::Int64(left), Value::Int64(right)) => left == *right,
        (PkKey::String(left), Value::String(right)) => left == right,
        _ => false,
    }
}

fn display_key(key: PkKey<'_>) -> String {
    match key {
        PkKey::Int64(value) => value.to_string(),
        PkKey::String(value) => format!("{value:?}"),
    }
}

fn rewrite_partial_tail(
    pager: &Pager,
    types: &[LogicalType],
    groups: &mut Vec<u64>,
    buffered_rows: &[Vec<Value>],
    next_row: &mut usize,
) -> DevonResult<()> {
    let Some(tail_id) = groups.last().copied() else {
        return Ok(());
    };
    let tail = NodeGroup::read(pager, tail_id, types)?;
    if tail.row_count() >= NODE_GROUP_CAPACITY {
        return Ok(());
    }

    let mut replacement = NodeGroup::new(types.to_vec())?;
    for row in group_rows(&tail)? {
        replacement.push_row(row)?;
    }
    fill_group(&mut replacement, buffered_rows, next_row)?;
    groups.pop();
    groups.push(replacement.write(pager)?);
    Ok(())
}

fn fill_group(group: &mut NodeGroup, rows: &[Vec<Value>], next_row: &mut usize) -> DevonResult<()> {
    while group.row_count() < NODE_GROUP_CAPACITY && *next_row < rows.len() {
        group.push_row(rows[*next_row].clone())?;
        *next_row += 1;
    }
    Ok(())
}

fn group_rows(group: &NodeGroup) -> DevonResult<Vec<Vec<Value>>> {
    let mut rows = Vec::with_capacity(group.row_count());
    for row_index in 0..group.row_count() {
        let mut row = Vec::with_capacity(group.column_count());
        for column_index in 0..group.column_count() {
            let value = group.value(row_index, column_index).ok_or_else(|| {
                corrupt(format!(
                    "node group is missing row {row_index}, column {column_index}"
                ))
            })?;
            row.push(value.clone());
        }
        rows.push(row);
    }
    Ok(rows)
}

fn validate_group_position(group: &NodeGroup, index: usize, group_count: usize) -> DevonResult<()> {
    if index + 1 < group_count && group.row_count() < NODE_GROUP_CAPACITY {
        return Err(corrupt(format!(
            "node group {index} is partial but is not the table's last group"
        )));
    }
    Ok(())
}

fn schema_types(schema: &NodeTableSchema) -> Vec<LogicalType> {
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
        schema::{Column, NodeTableSchema},
        value::Value,
    };
    use tempfile::tempdir;

    use super::{
        DeleteCompaction, NODE_GROUP_CAPACITY, NodeGroup, NodeTable, Pager, encode_wal_record,
    };
    use crate::catalog::Catalog;
    use crate::node_group::ZoneMapValue;
    use crate::overlay::{NodeDmlEffects, PkKey};
    use crate::txn_log::{WalPayload, decode_payload};
    use crate::wal::{WalWriter, replay};

    const PAGE_SIZE: u32 = 4096;
    const DB_ID: [u8; 16] = *b"node-table-db-id";

    fn schema(name: &str) -> NodeTableSchema {
        NodeTableSchema::new(
            name.to_owned(),
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
        .unwrap()
    }

    fn row(id: usize) -> Vec<Value> {
        vec![
            Value::Int64(id as i64),
            Value::String(format!("person-{id}")),
        ]
    }

    fn schema_with_age(name: &str) -> NodeTableSchema {
        NodeTableSchema::new(
            name.to_owned(),
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
                Column {
                    name: "age".to_owned(),
                    ty: LogicalType::Int64,
                    primary_key: false,
                },
            ],
        )
        .unwrap()
    }

    fn row_with_age(id: usize) -> Vec<Value> {
        vec![
            Value::Int64(id as i64),
            Value::String(format!("person-{id}")),
            Value::Int64(id as i64),
        ]
    }

    fn create_catalog(pager: &Pager, table_schema: &NodeTableSchema) -> Catalog {
        let mut catalog = Catalog::default();
        catalog.add_node_table(table_schema.clone()).unwrap();
        catalog.save(pager, 1).unwrap();
        catalog
    }

    fn open_wal(pager: &Pager, path: &Path) -> WalWriter {
        WalWriter::open(path, pager.superblock().checkpoint_lsn + 1).unwrap()
    }

    fn recover_table(table: &mut NodeTable, wal_path: &Path, checkpoint_lsn: u64) {
        let table_name = table.schema().name().to_owned();
        for (lsn, payload) in replay(wal_path).unwrap() {
            if lsn <= checkpoint_lsn {
                continue;
            }
            let WalPayload::NodeInsert {
                table: record_table,
                row,
            } = decode_payload(&payload).unwrap()
            else {
                panic!("legacy node-table WAL writer emitted a non-node-insert payload");
            };
            if record_table == table_name {
                table.recover_row(row).unwrap();
            }
        }
    }

    #[test]
    fn pre_checkpoint_scan_returns_buffered_rows() {
        let directory = tempdir().unwrap();
        let db_path = directory.path().join("buffered.devondb");
        let wal_path = directory.path().join("buffered.devondb-wal");
        let pager = Pager::create(db_path, PAGE_SIZE, DB_ID).unwrap();
        let table_schema = schema("Person");
        let catalog = create_catalog(&pager, &table_schema);
        let mut wal = open_wal(&pager, &wal_path);
        let mut table = NodeTable::new(table_schema);
        let expected = vec![row(1), row(2), row(3)];

        for value in &expected {
            table.insert(&mut wal, value.clone()).unwrap();
        }

        assert_eq!(table.scan(&pager, &catalog).unwrap(), expected);
    }

    /// Commits the superblock feature bits a governed read of freshly
    /// checkpointed groups requires (`ZONE_MAPS` + `COLUMN_ENCODINGS`).
    /// These tests read groups without the catalog save that derives the
    /// bits in production. The default writer selects encodings, so
    /// directories may carry flags bit 1. Redundant where a save
    /// follows; required where one does not.
    fn publish_feature_bits(pager: &Pager) {
        let mut superblock = pager.superblock();
        superblock.feature_flags |=
            crate::superblock::ZONE_MAPS_FLAG | crate::superblock::COLUMN_ENCODINGS_FLAG;
        superblock.checkpoint_lsn += 1;
        pager.commit_superblock(superblock).unwrap();
    }

    #[test]
    fn checkpointed_rows_survive_pager_and_wal_reopen() {
        let directory = tempdir().unwrap();
        let db_path = directory.path().join("reopen.devondb");
        let wal_path = directory.path().join("reopen.devondb-wal");
        let pager = Pager::create(&db_path, PAGE_SIZE, DB_ID).unwrap();
        let table_schema = schema("Person");
        let mut catalog = create_catalog(&pager, &table_schema);
        let expected = vec![row(10), row(11), row(12)];
        let mut wal = open_wal(&pager, &wal_path);
        let mut table = NodeTable::new(table_schema.clone());
        for value in &expected {
            table.insert(&mut wal, value.clone()).unwrap();
        }
        table.checkpoint(&pager, &mut catalog).unwrap();
        publish_feature_bits(&pager);
        let publish_lsn = pager.superblock().checkpoint_lsn + 1;
        catalog.save(&pager, publish_lsn).unwrap();
        drop(table);
        drop(wal);
        drop(pager);

        let pager = Pager::open(db_path).unwrap();
        let catalog = Catalog::load(&pager).unwrap();
        let _wal = open_wal(&pager, &wal_path);
        let table = NodeTable::new(table_schema);
        assert_eq!(table.scan(&pager, &catalog).unwrap(), expected);
    }

    #[test]
    fn checkpoint_writes_full_groups_then_one_partial_group() {
        let directory = tempdir().unwrap();
        let db_path = directory.path().join("groups.devondb");
        let wal_path = directory.path().join("groups.devondb-wal");
        let pager = Pager::create(db_path, PAGE_SIZE, DB_ID).unwrap();
        let table_schema = schema("Person");
        let mut catalog = create_catalog(&pager, &table_schema);
        let mut wal = open_wal(&pager, &wal_path);
        let mut table = NodeTable::new(table_schema.clone());
        let row_count = NODE_GROUP_CAPACITY + 5;
        for id in 0..row_count {
            table.insert(&mut wal, row(id)).unwrap();
        }

        table.checkpoint(&pager, &mut catalog).unwrap();
        publish_feature_bits(&pager);

        let storage = catalog.table_storage("Person").unwrap();
        assert_eq!(storage.groups.len(), 2);
        let types = table_schema
            .columns()
            .iter()
            .map(|column| column.ty)
            .collect::<Vec<_>>();
        let counts = storage
            .groups
            .iter()
            .map(|group_id| {
                NodeGroup::read(&pager, *group_id, &types)
                    .unwrap()
                    .row_count()
            })
            .collect::<Vec<_>>();
        assert_eq!(counts, vec![NODE_GROUP_CAPACITY, 5]);
        assert_eq!(table.scan(&pager, &catalog).unwrap().len(), row_count);
    }

    #[test]
    fn second_checkpoint_rewrites_the_partial_tail_group() {
        let directory = tempdir().unwrap();
        let db_path = directory.path().join("tail.devondb");
        let wal_path = directory.path().join("tail.devondb-wal");
        let pager = Pager::create(db_path, PAGE_SIZE, DB_ID).unwrap();
        let table_schema = schema("Person");
        let mut catalog = create_catalog(&pager, &table_schema);
        let mut wal = open_wal(&pager, &wal_path);
        let mut table = NodeTable::new(table_schema);
        for id in 0..3 {
            table.insert(&mut wal, row(id)).unwrap();
        }
        table.checkpoint(&pager, &mut catalog).unwrap();
        publish_feature_bits(&pager);
        let first_tail = catalog.table_storage("Person").unwrap().groups[0];
        for id in 3..7 {
            table.insert(&mut wal, row(id)).unwrap();
        }

        table.checkpoint(&pager, &mut catalog).unwrap();
        publish_feature_bits(&pager);

        let storage = catalog.table_storage("Person").unwrap();
        assert_eq!(storage.groups.len(), 1);
        assert_ne!(storage.groups[0], first_tail);
        assert_eq!(
            table.scan(&pager, &catalog).unwrap(),
            (0..7).map(row).collect::<Vec<_>>()
        );
    }

    #[test]
    fn reopen_recovers_uncheckpointed_rows_from_wal() {
        let directory = tempdir().unwrap();
        let db_path = directory.path().join("recovery.devondb");
        let wal_path = directory.path().join("recovery.devondb-wal");
        let pager = Pager::create(&db_path, PAGE_SIZE, DB_ID).unwrap();
        let table_schema = schema("Person");
        let catalog = create_catalog(&pager, &table_schema);
        let expected = vec![row(20), row(21)];
        let mut wal = open_wal(&pager, &wal_path);
        let mut table = NodeTable::new(table_schema.clone());
        for value in &expected {
            table.insert(&mut wal, value.clone()).unwrap();
        }
        drop(table);
        drop(wal);
        drop(catalog);
        drop(pager);

        let pager = Pager::open(db_path).unwrap();
        let catalog = Catalog::load(&pager).unwrap();
        let mut table = NodeTable::new(table_schema);
        recover_table(&mut table, &wal_path, pager.superblock().checkpoint_lsn);
        assert_eq!(table.scan(&pager, &catalog).unwrap(), expected);
    }

    #[test]
    fn recovery_ignores_records_for_other_tables() {
        let directory = tempdir().unwrap();
        let db_path = directory.path().join("other-table.devondb");
        let wal_path = directory.path().join("other-table.devondb-wal");
        let pager = Pager::create(db_path, PAGE_SIZE, DB_ID).unwrap();
        let table_schema = schema("Person");
        let catalog = create_catalog(&pager, &table_schema);
        let payload = encode_wal_record("Company", &row(30)).unwrap();
        let mut wal = WalWriter::open(&wal_path, 2).unwrap();
        wal.append(&payload).unwrap();
        wal.sync().unwrap();
        drop(wal);

        let mut table = NodeTable::new(table_schema);
        recover_table(&mut table, &wal_path, pager.superblock().checkpoint_lsn);

        assert!(table.scan(&pager, &catalog).unwrap().is_empty());
    }

    #[test]
    fn insert_type_mismatch_names_the_column_and_writes_nothing() {
        let directory = tempdir().unwrap();
        let db_path = directory.path().join("mismatch.devondb");
        let wal_path = directory.path().join("mismatch.devondb-wal");
        let pager = Pager::create(db_path, PAGE_SIZE, DB_ID).unwrap();
        let table_schema = schema("Person");
        let catalog = create_catalog(&pager, &table_schema);
        let mut wal = open_wal(&pager, &wal_path);
        let mut table = NodeTable::new(table_schema);

        let error = table
            .insert(&mut wal, vec![Value::Int64(1), Value::Bool(true)])
            .unwrap_err();

        let DevonError::InvalidArgument { context } = error else {
            panic!("expected InvalidArgument, got {error}");
        };
        assert!(context.contains("name"));
        assert_eq!(fs::metadata(wal_path).unwrap().len(), 0);
        assert!(table.scan(&pager, &catalog).unwrap().is_empty());
    }

    #[test]
    fn checkpoint_without_buffered_rows_does_not_advance_lsn() {
        let directory = tempdir().unwrap();
        let db_path = directory.path().join("no-op.devondb");
        let wal_path = directory.path().join("no-op.devondb-wal");
        let pager = Pager::create(db_path, PAGE_SIZE, DB_ID).unwrap();
        let table_schema = schema("Person");
        let mut catalog = create_catalog(&pager, &table_schema);
        let _wal = open_wal(&pager, &wal_path);
        let mut table = NodeTable::new(table_schema);
        let lsn = pager.superblock().checkpoint_lsn;

        table.checkpoint(&pager, &mut catalog).unwrap();

        assert_eq!(pager.superblock().checkpoint_lsn, lsn);
        assert_eq!(catalog.table_storage("Person"), None);
    }

    #[test]
    fn dml_update_rewrites_only_its_full_interior_group_and_recomputes_stats() {
        let directory = tempdir().unwrap();
        let pager = Pager::create(
            directory.path().join("interior-update.devondb"),
            PAGE_SIZE,
            DB_ID,
        )
        .unwrap();
        let table_schema = schema_with_age("Person");
        let mut catalog = create_catalog(&pager, &table_schema);
        let mut initial = NodeTable::new(table_schema.clone());
        for id in 0..(2 * NODE_GROUP_CAPACITY + 3) {
            initial.recover_row(row_with_age(id)).unwrap();
        }
        initial.checkpoint(&pager, &mut catalog).unwrap();
        publish_feature_bits(&pager);
        let old_groups = catalog.table_storage("Person").unwrap().groups.clone();
        let target = NODE_GROUP_CAPACITY + 17;
        let replacement = vec![
            Value::Int64(target as i64),
            Value::String("updated".to_owned()),
            Value::Int64(50_000),
        ];
        let mut effects = NodeDmlEffects::default();
        effects
            .updates
            .insert(PkKey::Int64(target as i64), &replacement);

        NodeTable::new(table_schema.clone())
            .checkpoint_with_dml(&pager, &mut catalog, effects, DeleteCompaction::Forbidden)
            .unwrap();

        let new_groups = &catalog.table_storage("Person").unwrap().groups;
        assert_eq!(new_groups.len(), 3);
        assert_eq!(new_groups[0], old_groups[0]);
        assert_ne!(new_groups[1], old_groups[1]);
        assert_eq!(new_groups[2], old_groups[2]);
        let rows = NodeTable::new(table_schema.clone())
            .scan(&pager, &catalog)
            .unwrap();
        assert_eq!(rows[target], replacement);
        assert_eq!(rows[target - 1][0], Value::Int64(target as i64 - 1));
        assert_eq!(rows[target + 1][0], Value::Int64(target as i64 + 1));
        let types = super::schema_types(&table_schema);
        let directory = NodeGroup::read_directory(&pager, new_groups[1], &types).unwrap();
        let stats = directory.zone_maps().unwrap();
        assert_eq!(stats[2].min, Some(ZoneMapValue::Int64(2048)));
        assert_eq!(stats[2].max, Some(ZoneMapValue::Int64(50_000)));
    }

    #[test]
    fn dml_delete_compacts_only_when_the_caller_affirms_offset_shifts() {
        let directory = tempdir().unwrap();
        let pager = Pager::create(
            directory.path().join("delete-compaction.devondb"),
            PAGE_SIZE,
            DB_ID,
        )
        .unwrap();
        let table_schema = schema("Person");
        let mut catalog = create_catalog(&pager, &table_schema);
        let mut initial = NodeTable::new(table_schema.clone());
        for id in 0..(NODE_GROUP_CAPACITY + 3) {
            initial.recover_row(row(id)).unwrap();
        }
        initial.checkpoint(&pager, &mut catalog).unwrap();
        publish_feature_bits(&pager);
        let old_groups = catalog.table_storage("Person").unwrap().groups.clone();
        let mut effects = NodeDmlEffects::default();
        effects.tombstones.insert(PkKey::Int64(10));

        NodeTable::new(table_schema.clone())
            .checkpoint_with_dml(&pager, &mut catalog, effects, DeleteCompaction::Permitted)
            .unwrap();

        let new_groups = catalog.table_storage("Person").unwrap().groups.clone();
        assert_ne!(new_groups[0], old_groups[0]);
        assert_ne!(new_groups[1], old_groups[1]);
        let types = super::schema_types(&table_schema);
        let counts = new_groups
            .iter()
            .map(|id| NodeGroup::read_row_count(&pager, *id, types.len()).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(counts, [NODE_GROUP_CAPACITY, 2]);
        let rows = NodeTable::new(table_schema.clone())
            .scan(&pager, &catalog)
            .unwrap();
        assert_eq!(rows.len(), NODE_GROUP_CAPACITY + 2);
        assert_eq!(rows[10][0], Value::Int64(11));

        let mut refused = NodeDmlEffects::default();
        refused.tombstones.insert(PkKey::Int64(20));
        let error = NodeTable::new(table_schema)
            .checkpoint_with_dml(&pager, &mut catalog, refused, DeleteCompaction::Forbidden)
            .unwrap_err();
        assert!(matches!(error, DevonError::InvalidArgument { .. }));
        assert_eq!(catalog.table_storage("Person").unwrap().groups, new_groups);
    }

    #[test]
    fn dml_effects_apply_to_buffered_overlay_inserts() {
        let directory = tempdir().unwrap();
        let pager = Pager::create(
            directory.path().join("overlay-effects.devondb"),
            PAGE_SIZE,
            DB_ID,
        )
        .unwrap();
        let table_schema = schema("Person");
        let mut catalog = create_catalog(&pager, &table_schema);
        let mut table = NodeTable::new(table_schema);
        table.recover_row(row(7)).unwrap();
        table.recover_row(row(8)).unwrap();
        let replacement = vec![Value::Int64(7), Value::String("updated".to_owned())];
        let mut effects = NodeDmlEffects::default();
        effects.updates.insert(PkKey::Int64(7), &replacement);
        effects.tombstones.insert(PkKey::Int64(8));

        table
            .checkpoint_with_dml(&pager, &mut catalog, effects, DeleteCompaction::Permitted)
            .unwrap();
        publish_feature_bits(&pager);

        assert_eq!(table.scan(&pager, &catalog).unwrap(), [replacement]);
    }

    #[test]
    fn dangling_dml_effects_are_checkpoint_corruption() {
        let directory = tempdir().unwrap();
        let pager = Pager::create(
            directory.path().join("dangling-effects.devondb"),
            PAGE_SIZE,
            DB_ID,
        )
        .unwrap();
        let table_schema = schema("Person");
        let mut catalog = create_catalog(&pager, &table_schema);
        let mut initial = NodeTable::new(table_schema.clone());
        initial.recover_row(row(1)).unwrap();
        initial.checkpoint(&pager, &mut catalog).unwrap();
        publish_feature_bits(&pager);
        let replacement = vec![Value::Int64(99), Value::String("missing".to_owned())];
        let mut update = NodeDmlEffects::default();
        update.updates.insert(PkKey::Int64(99), &replacement);
        let update_error = NodeTable::new(table_schema.clone())
            .checkpoint_with_dml(&pager, &mut catalog, update, DeleteCompaction::Permitted)
            .unwrap_err();
        assert!(matches!(update_error, DevonError::Corrupt { .. }));

        let mut delete = NodeDmlEffects::default();
        delete.tombstones.insert(PkKey::Int64(99));
        let delete_error = NodeTable::new(table_schema)
            .checkpoint_with_dml(&pager, &mut catalog, delete, DeleteCompaction::Permitted)
            .unwrap_err();
        assert!(matches!(delete_error, DevonError::Corrupt { .. }));
    }
}
