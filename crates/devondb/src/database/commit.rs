use super::*;
use devondb_storage::overlay::shed_fulltext_caches;

use std::collections::BTreeSet;

use super::{
    checkpoint::{checkpoint_locked, checkpoint_shared, should_auto_checkpoint},
    typing::validate_plan,
    view::{ReadView, primary_key, resolve_node_key, resolve_node_row, run_view},
};
use devondb_storage::{
    catalog::{
        InterfaceColumn as CatalogInterfaceColumn, InterfaceEntry, NodeClassEntry, PinEntry,
        RelClassEntry,
    },
    superblock::{DML_WAL_FLAG, HNSW_MUTATION_WAL_FLAG, REL_TOMBSTONE_WAL_FLAG},
    txn_log::RelEndpoint,
    wal::WalWriter,
};
use devondb_types::schema::{fold, suggestion_suffix};

type RelDeleteClaim = (RelEndpoint, u64);
type RelDeleteRecords = BTreeMap<String, Vec<RelDeleteClaim>>;

pub(crate) fn execute_transaction(
    transaction: &mut Transaction,
    statement: &Statement,
) -> DevonResult<()> {
    if transaction.writes.hnsw_create.is_some() {
        return Err(invalid_argument(
            "CREATE HNSW INDEX must be the only statement in its transaction",
        ));
    }
    if ontology_pending(transaction)
        && !matches!(
            statement,
            Statement::CreateInterface { .. } | Statement::CreateClass { .. }
        )
    {
        return Err(invalid_argument(
            "ontology DDL may be combined only with other ontology DDL in a transaction",
        ));
    }
    if pins_pending(transaction)
        && !matches!(
            statement,
            Statement::PinPlan { .. } | Statement::UnpinPlan { .. }
        )
    {
        return Err(invalid_argument(
            "pin DDL may be combined only with other pin DDL in a transaction",
        ));
    }
    match statement {
        Statement::CreateNodeTable { name, columns } => {
            let schema = NodeTableSchema::new(name.clone(), columns.clone())?;
            let mut catalog = (*transaction.catalog).clone();
            reject_interface_collision_for_node(&catalog, schema.name())?;
            catalog.add_node_table(schema.clone())?;
            let ddl = DdlOp::CreateNodeTable(schema);
            charge_write_set(transaction, WriteSet::ddl_bytes(&ddl)?)?;
            transaction.catalog = Arc::new(catalog);
            transaction.writes.ddl.push(ddl);
            Ok(())
        }
        Statement::CreateRelTable {
            name,
            from,
            to,
            columns,
        } => {
            let schema =
                RelTableSchema::new(name.clone(), from.clone(), to.clone(), columns.clone())?;
            let mut catalog = (*transaction.catalog).clone();
            catalog.add_rel_table(schema.clone())?;
            let ddl = DdlOp::CreateRelTable(schema);
            charge_write_set(transaction, WriteSet::ddl_bytes(&ddl)?)?;
            transaction.catalog = Arc::new(catalog);
            transaction.writes.ddl.push(ddl);
            Ok(())
        }
        Statement::InsertNode { table, rows } => execute_node_insert(transaction, table, rows),
        Statement::InsertRel { table, rows } => execute_rel_insert(transaction, table, rows),
        Statement::UpsertNode { table, rows } => execute_node_upsert(transaction, table, rows),
        // COPY bypasses the WAL under the bulk fence (docs/SCALE.md §5.3) and
        // therefore never executes inside a write transaction; Database
        // intercepts it before transactional dispatch.
        Statement::CopyNode { .. } => Err(invalid_argument(
            "copy does not run inside a transaction; execute it directly on the database",
        )),
        Statement::UpdateNode {
            table,
            set,
            key_column,
            key,
        } => execute_node_update(transaction, table, set, key_column, key),
        Statement::DeleteNode {
            table,
            key_column,
            key,
        } => execute_node_delete(transaction, table, key_column, key),
        Statement::DetachDeleteNode {
            table,
            key_column,
            key,
        } => execute_detach_delete(transaction, table, key_column, key),
        Statement::CreateInterface { name, columns } => {
            ensure_ontology_only(transaction)?;
            let mut catalog = (*transaction.catalog).clone();
            reject_node_collision_for_interface(&catalog, name)?;
            catalog.declare_interface(InterfaceEntry {
                name: name.clone(),
                columns: columns
                    .iter()
                    .map(|column| CatalogInterfaceColumn {
                        name: column.name.clone(),
                        ty: column.ty,
                    })
                    .collect(),
            })?;
            transaction.catalog = Arc::new(catalog);
            Ok(())
        }
        statement @ Statement::CreateClass { .. } => {
            ensure_ontology_only(transaction)?;
            execute_create_class(transaction, statement)
        }
        Statement::CreateHnswIndex {
            name,
            table,
            column,
            metric,
        } => {
            let pending =
                super::hnsw::prepare_index_build(transaction, name, table, column, *metric)?;
            transaction.writes.hnsw_create = Some(pending);
            Ok(())
        }
        Statement::PinPlan { name, text, plan } => {
            ensure_pins_only(transaction)?;
            let canonical = plan.to_json()?;
            let decoded = Plan::from_json(&canonical)?;
            validate_plan(&decoded, &transaction.catalog)?;
            let mut catalog = (*transaction.catalog).clone();
            catalog.pin(PinEntry::from_plan_json(
                name.clone(),
                text.clone(),
                &canonical,
                // Replaced by the metadata publication LSN at commit.
                0,
            )?)?;
            transaction.catalog = Arc::new(catalog);
            Ok(())
        }
        Statement::UnpinPlan { name } => {
            ensure_pins_only(transaction)?;
            let mut catalog = (*transaction.catalog).clone();
            catalog.unpin(name)?;
            transaction.catalog = Arc::new(catalog);
            Ok(())
        }
    }
}

fn ensure_ontology_only(transaction: &Transaction) -> DevonResult<()> {
    if transaction.writes.is_empty() && !pins_pending(transaction) {
        Ok(())
    } else {
        Err(invalid_argument(
            "ontology DDL may be combined only with other ontology DDL in a transaction",
        ))
    }
}

fn reject_interface_collision_for_node(catalog: &Catalog, name: &str) -> DevonResult<()> {
    if let Some(interface) = catalog.interface(name) {
        return Err(invalid_argument(format!(
            "node table `{name}` conflicts with interface `{}` under folded name resolution",
            interface.name
        )));
    }
    Ok(())
}

fn reject_node_collision_for_interface(catalog: &Catalog, name: &str) -> DevonResult<()> {
    if let Some(table) = catalog.node_table(name) {
        return Err(invalid_argument(format!(
            "interface `{name}` conflicts with node table `{}` under folded name resolution",
            table.name()
        )));
    }
    Ok(())
}

fn ensure_pins_only(transaction: &Transaction) -> DevonResult<()> {
    if transaction.writes.is_empty() && !ontology_pending(transaction) {
        Ok(())
    } else {
        Err(invalid_argument(
            "pin DDL may be combined only with other pin DDL in a transaction",
        ))
    }
}

fn execute_create_class(transaction: &mut Transaction, statement: &Statement) -> DevonResult<()> {
    let Statement::CreateClass {
        table,
        display,
        plural,
        label,
        summary,
        color,
        description,
        verb,
        inverse,
        implements,
    } = statement
    else {
        return Err(corrupt("class executor received a non-class statement"));
    };
    let mut catalog = (*transaction.catalog).clone();
    if catalog.node_table(table).is_some() {
        reject_relationship_clauses(table, verb, inverse)?;
        catalog.declare_node_class(NodeClassEntry {
            table: table.clone(),
            display: display.clone(),
            plural: plural.clone(),
            label: label.clone(),
            summary: summary.clone(),
            color: color.clone(),
            description: description.clone(),
            implements: implements.clone(),
        })?;
    } else if catalog.rel_table(table).is_some() {
        reject_node_clauses(statement)?;
        catalog.declare_rel_class(RelClassEntry {
            table: table.clone(),
            verb: verb.clone(),
            inverse: inverse.clone(),
        })?;
    } else {
        return Err(unknown_class_table(&catalog, table));
    }
    transaction.catalog = Arc::new(catalog);
    Ok(())
}

fn reject_relationship_clauses(
    table: &str,
    verb: &Option<String>,
    inverse: &Option<String>,
) -> DevonResult<()> {
    let mut clauses = Vec::new();
    if verb.is_some() {
        clauses.push("verb");
    }
    if inverse.is_some() {
        clauses.push("inverse");
    }
    if clauses.is_empty() {
        return Ok(());
    }
    Err(invalid_argument(format!(
        "node class for `{table}` does not admit relationship clause(s): {}",
        clauses.join(", ")
    )))
}

fn reject_node_clauses(statement: &Statement) -> DevonResult<()> {
    let Statement::CreateClass {
        table,
        display,
        plural,
        label,
        summary,
        color,
        description,
        implements,
        ..
    } = statement
    else {
        return Err(corrupt(
            "node-clause validator received a non-class statement",
        ));
    };
    let clauses = [
        (display.is_some(), "display"),
        (plural.is_some(), "plural"),
        (label.is_some(), "label"),
        (!summary.is_empty(), "summary"),
        (color.is_some(), "color"),
        (description.is_some(), "description"),
        (!implements.is_empty(), "implements"),
    ]
    .into_iter()
    .filter_map(|(present, name)| present.then_some(name))
    .collect::<Vec<_>>();
    if clauses.is_empty() {
        return Ok(());
    }
    Err(invalid_argument(format!(
        "relationship class for `{table}` does not admit node clause(s): {}",
        clauses.join(", ")
    )))
}

fn unknown_class_table(catalog: &Catalog, table: &str) -> DevonError {
    invalid_argument(format!(
        "class target table `{table}` was not found{}",
        suggestion_suffix(
            table,
            catalog
                .node_tables()
                .iter()
                .map(|schema| schema.name())
                .chain(catalog.rel_tables().iter().map(|schema| schema.name()))
        )
    ))
}

fn ontology_pending(transaction: &Transaction) -> bool {
    transaction.catalog.ontology() != transaction.state.catalog.ontology()
}

fn pins_pending(transaction: &Transaction) -> bool {
    transaction.catalog.pins() != transaction.state.catalog.pins()
}

fn metadata_pending(transaction: &Transaction) -> bool {
    ontology_pending(transaction) || pins_pending(transaction)
}

fn execute_node_insert(
    transaction: &mut Transaction,
    table: &str,
    rows: &[Vec<Value>],
) -> DevonResult<()> {
    let schema = transaction
        .catalog
        .node_table(table)
        .cloned()
        .ok_or_else(|| DevonError::NotFound {
            what: format!(
                "node table `{table}`{}",
                suggestion_suffix(
                    table,
                    transaction
                        .catalog
                        .node_tables()
                        .iter()
                        .map(|schema| schema.name())
                )
            ),
        })?;
    let canonical_table = schema.name().to_owned();
    let (key_index, key_column) = primary_key(&schema)?;
    let mut staged_keys = Vec::with_capacity(rows.len());
    for row in rows {
        let key = validate_node_insert_row(&schema, table, row, key_index, key_column)?;
        if staged_keys.contains(&key)
            || transaction_key_exists(transaction, &canonical_table, &key)?
        {
            return Err(invalid_argument(format!(
                "duplicate primary key `{key}` in node table `{table}`"
            )));
        }
        staged_keys.push(key);
    }
    stage_node_inserts(transaction, &canonical_table, rows, &staged_keys)
}

fn stage_node_inserts(
    transaction: &mut Transaction,
    canonical_table: &str,
    rows: &[Vec<Value>],
    staged_keys: &[Value],
) -> DevonResult<()> {
    let requested = transaction
        .writes
        .node_insert_bytes(canonical_table, rows, staged_keys)?;
    charge_write_set(transaction, requested)?;
    transaction
        .writes
        .nodes
        .entry(canonical_table.to_owned())
        .or_default()
        .extend(rows.iter().cloned());
    transaction
        .writes
        .inserted_pks
        .entry(canonical_table.to_owned())
        .or_default()
        .extend(staged_keys.iter().cloned());
    Ok(())
}

fn execute_node_upsert(
    transaction: &mut Transaction,
    table: &str,
    rows: &[Vec<Value>],
) -> DevonResult<()> {
    let schema = resolve_dml_table(transaction, table)?;
    let canonical_table = schema.name().to_owned();
    let (key_index, key_column) = primary_key(&schema)?;
    let keys = rows
        .iter()
        .map(|row| validate_node_insert_row(&schema, table, row, key_index, key_column))
        .collect::<DevonResult<Vec<_>>>()?;
    let actions = resolve_upsert_actions(transaction, &canonical_table, &keys)?;

    for ((row, key), action) in rows.iter().zip(keys).zip(actions) {
        match action {
            UpsertAction::Insert => stage_node_inserts(
                transaction,
                &canonical_table,
                std::slice::from_ref(row),
                std::slice::from_ref(&key),
            )?,
            UpsertAction::Update => {
                let set = full_row_update_set(&schema, row, key_index)?;
                execute_node_update(transaction, &canonical_table, &set, key_column, &key)?;
            }
        }
    }
    Ok(())
}

#[derive(Eq, Ord, PartialEq, PartialOrd)]
enum UpsertKey {
    Int64(i64),
    String(String),
}

enum UpsertAction {
    Insert,
    Update,
}

fn resolve_upsert_actions(
    transaction: &Transaction,
    table: &str,
    keys: &[Value],
) -> DevonResult<Vec<UpsertAction>> {
    let view = transaction_read_view(transaction, false)?;
    let mut present = transaction
        .writes
        .inserted_pks
        .get(table)
        .into_iter()
        .flatten()
        .map(upsert_key)
        .collect::<DevonResult<BTreeSet<_>>>()?;
    keys.iter()
        .map(|key| resolve_upsert_action(&view, table, key, &mut present))
        .collect()
}

fn resolve_upsert_action(
    view: &ReadView,
    table: &str,
    key: &Value,
    present: &mut BTreeSet<UpsertKey>,
) -> DevonResult<UpsertAction> {
    let normalized_key = upsert_key(key)?;
    if present.contains(&normalized_key) {
        return Ok(UpsertAction::Update);
    }
    let action = match resolve_node_key(view, table, key) {
        Ok(_) => UpsertAction::Update,
        Err(DevonError::NotFound { .. }) => UpsertAction::Insert,
        Err(error) => return Err(error),
    };
    present.insert(normalized_key);
    Ok(action)
}

fn upsert_key(value: &Value) -> DevonResult<UpsertKey> {
    match value {
        Value::Int64(value) => Ok(UpsertKey::Int64(*value)),
        Value::String(value) => Ok(UpsertKey::String(value.clone())),
        other => Err(corrupt(format!(
            "validated upsert primary key has unsupported value {other}"
        ))),
    }
}

fn validate_node_insert_row(
    schema: &NodeTableSchema,
    table: &str,
    row: &[Value],
    key_index: usize,
    key_column: &str,
) -> DevonResult<Value> {
    let mut validator = NodeTable::new(schema.clone());
    validator.recover_row(row.to_vec())?;
    let key = row
        .get(key_index)
        .cloned()
        .ok_or_else(|| corrupt("validated node row lost its primary-key value"))?;
    if key == Value::Null {
        return Err(invalid_argument(format!(
            "primary key column `{key_column}` in node table `{table}` cannot be null"
        )));
    }
    Ok(key)
}

fn full_row_update_set(
    schema: &NodeTableSchema,
    row: &[Value],
    key_index: usize,
) -> DevonResult<Vec<devondb_plan::statement::SetItem>> {
    schema
        .columns()
        .iter()
        .enumerate()
        .filter(|(index, _)| *index != key_index)
        .map(|(index, column)| {
            let value = row
                .get(index)
                .cloned()
                .ok_or_else(|| corrupt("validated upsert row lost a non-key value"))?;
            Ok(devondb_plan::statement::SetItem {
                column: column.name.clone(),
                value,
            })
        })
        .collect()
}

fn execute_node_update(
    transaction: &mut Transaction,
    table: &str,
    set: &[devondb_plan::statement::SetItem],
    key_column: &str,
    key: &Value,
) -> DevonResult<()> {
    let schema = resolve_dml_table(transaction, table)?;
    let (key_index, _) = validate_dml_key_column(&schema, key_column)?;
    let view = transaction_read_view(transaction, false)?;
    let mut replacement = resolve_node_row(&view, schema.name(), key)?;
    for item in set {
        let column_index = resolve_dml_column(&schema, &item.column)?;
        if column_index == key_index {
            return Err(invalid_argument(format!(
                "update of primary key column `{}` in node table `{}` is not supported; use delete plus insert",
                schema.columns()[column_index].name,
                schema.name()
            )));
        }
        replacement[column_index] = item.value.clone();
    }
    let mut validator = NodeTable::new(schema.clone());
    validator.recover_row(replacement.clone())?;
    let requested = WriteSet::node_dml_bytes(
        &transaction.writes.node_updates,
        schema.name(),
        &replacement,
    )?;
    charge_write_set(transaction, requested)?;
    if replace_own_insert(
        transaction,
        schema.name(),
        key_index,
        key,
        replacement.clone(),
    ) {
        return Ok(());
    }
    upsert_node_update(
        &mut transaction.writes,
        schema.name(),
        key_index,
        key,
        replacement,
    );
    Ok(())
}

fn execute_node_delete(
    transaction: &mut Transaction,
    table: &str,
    key_column: &str,
    key: &Value,
) -> DevonResult<()> {
    let schema = resolve_dml_table(transaction, table)?;
    reject_referenced_delete(&transaction.catalog, &schema)?;
    let (key_index, _) = validate_dml_key_column(&schema, key_column)?;
    let view = transaction_read_view(transaction, false)?;
    let row = resolve_node_row(&view, schema.name(), key)?;
    let canonical_key = row
        .get(key_index)
        .cloned()
        .ok_or_else(|| corrupt("resolved DML row lost its primary-key value"))?;
    if remove_own_insert(transaction, schema.name(), key_index, &canonical_key)? {
        return Ok(());
    }
    let empty_updates = BTreeMap::<String, Vec<Vec<Value>>>::new();
    let requested = WriteSet::node_dml_bytes(
        &empty_updates,
        schema.name(),
        std::slice::from_ref(&canonical_key),
    )?;
    charge_write_set(transaction, requested)?;
    remove_node_update(
        &mut transaction.writes,
        schema.name(),
        key_index,
        &canonical_key,
    );
    let deletes = transaction
        .writes
        .node_deletes
        .entry(schema.name().to_owned())
        .or_default();
    if !deletes.contains(&canonical_key) {
        deletes.push(canonical_key);
    }
    Ok(())
}

fn execute_detach_delete(
    transaction: &mut Transaction,
    table: &str,
    key_column: &str,
    key: &Value,
) -> DevonResult<()> {
    let schema = resolve_dml_table(transaction, table)?;
    let (key_index, _) = validate_dml_key_column(&schema, key_column)?;
    let view = transaction_read_view(transaction, false)?;
    let row = resolve_node_row(&view, schema.name(), key)?;
    let canonical_key = row
        .get(key_index)
        .cloned()
        .ok_or_else(|| corrupt("resolved detach row lost its primary-key value"))?;
    let offset = resolve_node_key(&view, schema.name(), &canonical_key)?;
    let tombstones = detach_tombstone_claims(&transaction.catalog, schema.name(), offset);

    if own_insert_position(transaction, schema.name(), key_index, &canonical_key).is_some() {
        remove_own_insert(transaction, schema.name(), key_index, &canonical_key)?;
        fold_pending_incident_edges(transaction, schema.name(), &canonical_key);
        return Ok(());
    }

    let empty_updates = BTreeMap::<String, Vec<Vec<Value>>>::new();
    let delete_bytes = WriteSet::node_dml_bytes(
        &empty_updates,
        schema.name(),
        std::slice::from_ref(&canonical_key),
    )?;
    let intent = crate::txn::PendingDetach {
        table: schema.name().to_owned(),
        key: canonical_key.clone(),
    };
    let requested = delete_bytes
        .checked_add(transaction.writes.detach_bytes(&intent, &tombstones)?)
        .ok_or_else(|| DevonError::BudgetExceeded {
            context: "detach write-set byte estimate exceeds usize::MAX".to_owned(),
        })?;
    charge_write_set(transaction, requested)?;

    fold_pending_incident_edges(transaction, schema.name(), &canonical_key);
    remove_node_update(
        &mut transaction.writes,
        schema.name(),
        key_index,
        &canonical_key,
    );
    let deletes = transaction
        .writes
        .node_deletes
        .entry(schema.name().to_owned())
        .or_default();
    if !deletes.contains(&canonical_key) {
        deletes.push(canonical_key);
    }
    transaction.writes.detaches.push(intent);
    for (rel, from, offset) in tombstones {
        let roles = transaction.writes.rel_tombstones.entry(rel).or_default();
        if from {
            roles.from_offsets.insert(offset);
        } else {
            roles.to_offsets.insert(offset);
        }
    }
    Ok(())
}

fn own_insert_position(
    transaction: &Transaction,
    table: &str,
    key_index: usize,
    key: &Value,
) -> Option<usize> {
    transaction
        .writes
        .nodes
        .get(table)
        .and_then(|rows| rows.iter().position(|row| row.get(key_index) == Some(key)))
}

fn detach_tombstone_claims(
    catalog: &Catalog,
    table: &str,
    offset: u64,
) -> Vec<(String, bool, u64)> {
    let mut claims = Vec::new();
    for rel in catalog.rel_tables() {
        if fold(rel.from()) == fold(table) {
            claims.push((rel.name().to_owned(), true, offset));
        }
        if fold(rel.to()) == fold(table) {
            claims.push((rel.name().to_owned(), false, offset));
        }
    }
    claims
}

fn fold_pending_incident_edges(transaction: &mut Transaction, table: &str, key: &Value) {
    transaction.writes.edges.retain(|rel, edges| {
        let Some(schema) = transaction.catalog.rel_table(rel) else {
            return true;
        };
        let from_matches = fold(schema.from()) == fold(table);
        let to_matches = fold(schema.to()) == fold(table);
        edges.retain(|edge| {
            !(from_matches && edge.from_key == *key || to_matches && edge.to_key == *key)
        });
        !edges.is_empty()
    });
}

fn resolve_dml_table(transaction: &Transaction, table: &str) -> DevonResult<NodeTableSchema> {
    transaction
        .catalog
        .node_table(table)
        .cloned()
        .ok_or_else(|| DevonError::NotFound {
            what: format!(
                "node table `{table}`{}",
                suggestion_suffix(
                    table,
                    transaction
                        .catalog
                        .node_tables()
                        .iter()
                        .map(|schema| schema.name())
                )
            ),
        })
}

fn validate_dml_key_column<'a>(
    schema: &'a NodeTableSchema,
    key_column: &str,
) -> DevonResult<(usize, &'a str)> {
    let supplied_index = resolve_dml_column(schema, key_column)?;
    let (key_index, canonical_key) = primary_key(schema)?;
    if supplied_index != key_index {
        return Err(invalid_argument(format!(
            "update/delete where column `{key_column}` is not primary key `{canonical_key}` in node table `{}`; predicate-driven bulk DML is not supported",
            schema.name()
        )));
    }
    Ok((key_index, canonical_key))
}

fn resolve_dml_column(schema: &NodeTableSchema, column: &str) -> DevonResult<usize> {
    schema
        .column_index(column)
        .ok_or_else(|| DevonError::NotFound {
            what: format!(
                "column `{column}` in node table `{}`{}",
                schema.name(),
                suggestion_suffix(
                    column,
                    schema.columns().iter().map(|column| column.name.as_str())
                )
            ),
        })
}

fn reject_referenced_delete(catalog: &Catalog, schema: &NodeTableSchema) -> DevonResult<()> {
    let referenced = catalog.rel_tables().iter().any(|rel| {
        fold(rel.from()) == fold(schema.name()) || fold(rel.to()) == fold(schema.name())
    });
    if referenced {
        return Err(invalid_argument(format!(
            "delete refused for node table `{}` because a relationship table names it as an endpoint; detach-delete is not supported",
            schema.name()
        )));
    }
    Ok(())
}

fn replace_own_insert(
    transaction: &mut Transaction,
    table: &str,
    key_index: usize,
    key: &Value,
    replacement: Vec<Value>,
) -> bool {
    let Some(rows) = transaction.writes.nodes.get_mut(table) else {
        return false;
    };
    let Some(position) = rows.iter().position(|row| row.get(key_index) == Some(key)) else {
        return false;
    };
    rows[position] = replacement;
    true
}

fn remove_own_insert(
    transaction: &mut Transaction,
    table: &str,
    key_index: usize,
    key: &Value,
) -> DevonResult<bool> {
    let Some(rows) = transaction.writes.nodes.get_mut(table) else {
        return Ok(false);
    };
    let Some(position) = rows.iter().position(|row| row.get(key_index) == Some(key)) else {
        return Ok(false);
    };
    rows.remove(position);
    let remove_rows_entry = rows.is_empty();
    if remove_rows_entry {
        transaction.writes.nodes.remove(table);
    }
    let keys = transaction
        .writes
        .inserted_pks
        .get_mut(table)
        .ok_or_else(|| corrupt("own inserted row has no primary-key write-set entry"))?;
    let key_position = keys
        .iter()
        .position(|candidate| candidate == key)
        .ok_or_else(|| corrupt("own inserted row primary key disappeared from write set"))?;
    keys.remove(key_position);
    if keys.is_empty() {
        transaction.writes.inserted_pks.remove(table);
    }
    Ok(true)
}

fn upsert_node_update(
    writes: &mut WriteSet,
    table: &str,
    key_index: usize,
    key: &Value,
    replacement: Vec<Value>,
) {
    let rows = writes.node_updates.entry(table.to_owned()).or_default();
    if let Some(position) = rows.iter().position(|row| row.get(key_index) == Some(key)) {
        rows[position] = replacement;
    } else {
        rows.push(replacement);
    }
}

fn remove_node_update(writes: &mut WriteSet, table: &str, key_index: usize, key: &Value) {
    let Some(rows) = writes.node_updates.get_mut(table) else {
        return;
    };
    rows.retain(|row| row.get(key_index) != Some(key));
    if rows.is_empty() {
        writes.node_updates.remove(table);
    }
}

fn execute_rel_insert(
    transaction: &mut Transaction,
    table: &str,
    rows: &[devondb_plan::statement::RelRow],
) -> DevonResult<()> {
    let schema = transaction
        .catalog
        .rel_table(table)
        .cloned()
        .ok_or_else(|| DevonError::NotFound {
            what: format!(
                "relationship table `{table}`{}",
                suggestion_suffix(
                    table,
                    transaction
                        .catalog
                        .rel_tables()
                        .iter()
                        .map(|schema| schema.name())
                )
            ),
        })?;
    let canonical_table = schema.name().to_owned();
    let view = transaction_read_view(transaction, false)?;
    let mut staged = Vec::with_capacity(rows.len());
    for row in rows {
        let mut validator = RelTable::new(schema.clone());
        validator.recover_edge(0, 0, row.values.clone())?;
        reject_own_deleted_endpoint(transaction, schema.from(), &row.from_key)?;
        reject_own_deleted_endpoint(transaction, schema.to(), &row.to_key)?;
        resolve_node_key(&view, schema.from(), &row.from_key)?;
        resolve_node_key(&view, schema.to(), &row.to_key)?;
        staged.push(PendingEdge {
            from_key: row.from_key.clone(),
            to_key: row.to_key.clone(),
            values: row.values.clone(),
        });
    }
    let from_claims_staged =
        edge_endpoint_table_staged(&transaction.catalog, &transaction.writes, schema.from())?;
    let to_claims_staged =
        edge_endpoint_table_staged(&transaction.catalog, &transaction.writes, schema.to())?;
    let requested = transaction.writes.rel_insert_bytes(
        &canonical_table,
        &staged,
        schema.from(),
        schema.to(),
        from_claims_staged,
        to_claims_staged,
    )?;
    charge_write_set(transaction, requested)?;
    transaction
        .writes
        .edges
        .entry(canonical_table)
        .or_default()
        .extend(staged);
    Ok(())
}

fn reject_own_deleted_endpoint(
    transaction: &Transaction,
    table: &str,
    key: &Value,
) -> DevonResult<()> {
    let canonical = transaction
        .catalog
        .node_table(table)
        .map_or(table, NodeTableSchema::name);
    // A surviving reinsert has a fresh endpoint identity. Keep the old
    // delete/tombstone record, but permit edges to the new own insert.
    if transaction
        .writes
        .inserted_pks
        .get(canonical)
        .is_some_and(|keys| keys.contains(key))
    {
        return Ok(());
    }
    if transaction
        .writes
        .node_deletes
        .get(canonical)
        .is_some_and(|deleted| deleted.contains(key))
    {
        return Err(DevonError::NotFound {
            what: format!("node table `{canonical}` primary key {key}"),
        });
    }
    Ok(())
}

fn charge_write_set(transaction: &mut Transaction, requested: usize) -> DevonResult<()> {
    if try_charge_write_set(transaction, requested)? {
        return Ok(());
    }
    // Ladder step 1 (docs/MVCC.md §7.2): evict clean cache frames — the
    // cache admits pages up to the whole budget and yields only to
    // eviction, never to a bare try_charge. Otherwise primary-key existence
    // reads can fill the budget and cause every write-set charge to fail.
    transaction.shared.pager.shed_cache(requested);
    if try_charge_write_set(transaction, requested)? {
        return Ok(());
    }
    // Step 1.5: shed the PK resolution and decoded-group caches. They are the
    // one charge neither eviction nor step 2's
    // checkpoint can free for THIS transaction — its own pinned snapshot
    // keeps their owning state's registry entry alive. Without shedding,
    // table-sized caches can fill the budget and block even small writes.
    shed_pk_caches();
    // Full-text indexes are larger and rebuilt less often, so shed them after PK caches.
    shed_fulltext_caches();
    if try_charge_write_set(transaction, requested)? {
        return Ok(());
    }
    // Step 2: drain the committed overlay. Its own page traffic re-warms
    // the cache, so evict again before the final retry.
    checkpoint_shared(&Arc::clone(&transaction.shared))?;
    transaction.shared.pager.shed_cache(requested);
    if try_charge_write_set(transaction, requested)? {
        return Ok(());
    }
    Err(DevonError::BudgetExceeded {
        context: format!(
            "write set requested {requested} bytes with {} bytes charged and limit {}",
            transaction.shared.budget.charged(),
            transaction.shared.budget.limit()
        ),
    })
}

fn try_charge_write_set(transaction: &mut Transaction, requested: usize) -> DevonResult<bool> {
    if !transaction.shared.budget.try_charge(requested) {
        return Ok(false);
    }
    let Some(charged) = transaction.writes.charged.checked_add(requested) else {
        transaction.shared.budget.release(requested);
        return Err(DevonError::BudgetExceeded {
            context: "write set charge exceeds usize::MAX".to_owned(),
        });
    };
    transaction.writes.charged = charged;
    Ok(true)
}

fn transaction_key_exists(
    transaction: &Transaction,
    table: &str,
    key: &Value,
) -> DevonResult<bool> {
    if transaction
        .writes
        .inserted_pks
        .get(table)
        .is_some_and(|keys| keys.contains(key))
    {
        return Ok(true);
    }
    let view = transaction_read_view(transaction, false)?;
    match resolve_node_key(&view, table, key) {
        Ok(_) => Ok(true),
        Err(DevonError::NotFound { .. }) => Ok(false),
        Err(error) => Err(error),
    }
}

pub(crate) fn run_transaction(transaction: &Transaction, plan: &Plan) -> DevonResult<QueryResult> {
    let view = transaction_read_view(transaction, true)?;
    run_view(&view, plan)
}

fn transaction_read_view(transaction: &Transaction, resolve_edges: bool) -> DevonResult<ReadView> {
    let mut delta = CommitDelta {
        nodes: transaction.writes.nodes.clone(),
        node_updates: transaction.writes.node_updates.clone(),
        node_deletes: transaction.writes.node_deletes.clone(),
        rel_tombstones: transaction.writes.rel_tombstones.clone(),
        ddl: transaction.writes.ddl.clone(),
        ..CommitDelta::default()
    };
    if resolve_edges {
        delta.edges = resolve_edges_for_view(
            &transaction.shared,
            &transaction.state,
            &transaction.catalog,
            &delta,
            &transaction.writes.edges,
        )?;
    }
    Ok(ReadView::new(
        Arc::clone(&transaction.shared),
        Arc::clone(&transaction.state),
        Arc::clone(&transaction.catalog),
        Some(Arc::new(delta)),
    ))
}

pub(crate) fn commit_transaction(transaction: &mut Transaction) -> DevonResult<()> {
    if metadata_pending(transaction) {
        let shared = Arc::clone(&transaction.shared);
        let _writer_operation = shared.begin_writer_operation();
        let (mut pipe, _publication_guard) = shared.lock_commit_and_gate()?;
        let result = publish_metadata(&shared, &mut pipe, transaction);
        transaction.finish();
        return result;
    }
    if transaction.writes.is_empty() {
        transaction.finish();
        return Ok(());
    }

    if transaction.writes.hnsw_create.is_some() {
        let shared = Arc::clone(&transaction.shared);
        let _writer_operation = shared.begin_writer_operation();
        let (mut pipe, _publication_guard) = shared.lock_commit_and_gate()?;
        let result = transaction
            .writes
            .hnsw_create
            .as_ref()
            .ok_or_else(|| corrupt("pending HNSW index disappeared before publication"))
            .and_then(|pending| super::hnsw::publish_index_build(&shared, &mut pipe, pending));
        transaction.finish();
        return result;
    }

    let shared = Arc::clone(&transaction.shared);
    let _writer_operation = shared.begin_writer_operation();
    let (mut pipe, _publication_guard) = shared.lock_commit_and_gate()?;
    let result = commit_locked(&shared, &mut pipe, transaction);
    transaction.finish();
    result?;
    if should_auto_checkpoint(&shared, &pipe) || shared.final_database_handle_closed() {
        // The commit is durable and published; auto-checkpoint is an
        // opportunistic drain (docs/MVCC.md §5.5 step 8) whose failure
        // must not redefine the commit's outcome — a caller retrying a
        // "failed" commit would duplicate durable writes. The WAL stays
        // intact, so the next commit or an explicit checkpoint() retries
        // the drain and reports its own errors.
        let _ = checkpoint_locked(&shared, &mut pipe);
    }
    Ok(())
}

fn publish_metadata(
    shared: &Arc<Shared>,
    pipe: &mut CommitPipe,
    transaction: &Transaction,
) -> DevonResult<()> {
    if !transaction.writes.is_empty() {
        return Err(corrupt(
            "metadata transaction unexpectedly contains non-metadata writes",
        ));
    }
    checkpoint_locked(shared, pipe)?;
    let current = shared.current_state();
    let mut catalog = (*current.catalog).clone();
    let publish_lsn = next_lsn(&shared.pager)?;
    if ontology_pending(transaction) {
        replay_ontology_additions(&mut catalog, transaction)?;
    }
    if pins_pending(transaction) {
        replay_pin_changes(&mut catalog, transaction, publish_lsn)?;
    }
    catalog.save(&shared.pager, publish_lsn)?;
    rotate_empty_wal(shared, pipe)?;
    // Deliberately NO adopt_pk_caches or adopt_fulltext_caches here: checkpoint rewrites storage
    // maps, so checkpointed offsets may move — the PK resolution caches
    // MUST rebuild from the new state; the post-checkpoint rebuild test
    // enforces this. Do not "fix" this asymmetry.
    shared.publish(Arc::new(PublishedState {
        catalog: Arc::new(catalog),
        chain: None,
        last_commit_lsn: publish_lsn,
        catalog_generation: publish_lsn,
        recent_summaries: current.recent_summaries.clone(),
    }));
    Ok(())
}

fn replay_pin_changes(
    catalog: &mut Catalog,
    transaction: &Transaction,
    publish_lsn: u64,
) -> DevonResult<()> {
    let base = transaction.state.catalog.pins();
    let pending = transaction.catalog.pins();
    for entry in base {
        match pin_by_name(pending, &entry.name) {
            Some(staged) if staged == entry => {}
            Some(_) | None => catalog.unpin(&entry.name)?,
        }
    }
    for entry in pending {
        let unchanged = pin_by_name(base, &entry.name).is_some_and(|base| base == entry);
        if !unchanged {
            let mut published = entry.clone();
            published.created_lsn = publish_lsn;
            catalog.pin(published)?;
        }
    }
    Ok(())
}

fn pin_by_name<'a>(pins: &'a [PinEntry], name: &str) -> Option<&'a PinEntry> {
    let folded = fold(name);
    pins.iter().find(|pin| fold(&pin.name) == folded)
}

fn replay_ontology_additions(catalog: &mut Catalog, transaction: &Transaction) -> DevonResult<()> {
    let base = transaction.state.catalog.ontology();
    let pending = transaction
        .catalog
        .ontology()
        .ok_or_else(|| corrupt("pending ontology disappeared before commit"))?;
    let base_interfaces = base.map_or(&[][..], |ontology| ontology.interfaces.as_slice());
    let base_nodes = base.map_or(&[][..], |ontology| ontology.node_classes.as_slice());
    let base_rels = base.map_or(&[][..], |ontology| ontology.rel_classes.as_slice());
    for entry in appended(base_interfaces, &pending.interfaces, "interfaces")? {
        reject_node_collision_for_interface(catalog, &entry.name)?;
        catalog.declare_interface(entry.clone())?;
    }
    for entry in appended(base_nodes, &pending.node_classes, "node classes")? {
        catalog.declare_node_class(entry.clone())?;
    }
    for entry in appended(base_rels, &pending.rel_classes, "relationship classes")? {
        catalog.declare_rel_class(entry.clone())?;
    }
    Ok(())
}

fn appended<'a, T: PartialEq>(base: &[T], pending: &'a [T], kind: &str) -> DevonResult<&'a [T]> {
    pending.strip_prefix(base).ok_or_else(|| {
        corrupt(format!(
            "transaction-local ontology {kind} do not extend the snapshot catalog"
        ))
    })
}

fn rotate_empty_wal(shared: &Shared, pipe: &mut CommitPipe) -> DevonResult<()> {
    pipe.wal = None;
    truncate_wal(&pipe.wal_path)?;
    pipe.wal = Some(WalWriter::open(&pipe.wal_path, next_lsn(&shared.pager)?)?);
    Ok(())
}

fn commit_locked(
    shared: &Arc<Shared>,
    pipe: &mut CommitPipe,
    transaction: &mut Transaction,
) -> DevonResult<()> {
    let mut prepared = prepare_commit(shared, transaction)?;
    let reservation = match transfer_commit_charge(
        &shared.budget,
        &mut transaction.writes,
        &prepared.delta,
        &prepared.summary,
    ) {
        Ok(reservation) => reservation,
        Err(DevonError::BudgetExceeded { .. }) => {
            drop(prepared);
            // Ladder step 1 (docs/MVCC.md §7.2): evict clean cache frames
            // before draining the overlay; the checkpoint's own page
            // traffic re-warms the cache, so evict once more after it.
            shared.pager.shed_cache(shared.budget.limit());
            checkpoint_locked(shared, pipe)?;
            shared.pager.shed_cache(shared.budget.limit());
            prepared = prepare_commit(shared, transaction)?;
            transfer_commit_charge(
                &shared.budget,
                &mut transaction.writes,
                &prepared.delta,
                &prepared.summary,
            )?
        }
        Err(error) => return Err(error),
    };
    publish_prepared_commit(shared, pipe, prepared, reservation)
}

struct PreparedCommit {
    current: Arc<PublishedState>,
    catalog: Catalog,
    delta: CommitDelta,
    rel_delete_records: RelDeleteRecords,
    summary: CommitSummary,
}

fn prepare_commit(shared: &Arc<Shared>, transaction: &Transaction) -> DevonResult<PreparedCommit> {
    let current = shared.current_state();
    detect_conflict(&current, transaction)?;
    validate_ddl_interface_collisions(&current.catalog, &transaction.writes.ddl)?;
    let catalog = apply_ddl_to_catalog(&current.catalog, &transaction.writes.ddl)
        .map_err(|error| corrupt(format!("validated transaction DDL became invalid: {error}")))?;
    validate_commit_dml_refusals(&catalog, &transaction.writes)?;
    let (delta, rel_delete_records) =
        build_commit_delta(shared, &current, &catalog, &transaction.writes)?;
    let summary = summary_from_writes(&catalog, &transaction.writes)?;
    Ok(PreparedCommit {
        current,
        catalog,
        delta,
        rel_delete_records,
        summary,
    })
}

fn validate_ddl_interface_collisions(catalog: &Catalog, ddl: &[DdlOp]) -> DevonResult<()> {
    for operation in ddl {
        if let DdlOp::CreateNodeTable(schema) = operation {
            reject_interface_collision_for_node(catalog, schema.name())?;
        }
    }
    Ok(())
}

fn publish_prepared_commit(
    shared: &Arc<Shared>,
    pipe: &mut CommitPipe,
    prepared: PreparedCommit,
    mut reservation: CommitReservation,
) -> DevonResult<()> {
    let PreparedCommit {
        current,
        catalog,
        delta,
        rel_delete_records,
        summary,
    } = prepared;
    let summary = CommitSummary::new_arc_reserved(
        summary,
        Arc::clone(&shared.budget),
        reservation.summary_bytes,
    )?;
    reservation.relinquish_summary();
    // No fallible step may separate the WAL fsync from publication: an
    // error there would report failure for a durable commit, which recovery
    // would then replay (docs/MVCC.md §5.5's atomicity argument). Validate
    // the link estimate against the reservation BEFORE any WAL write so the
    // post-sync construction cannot mismatch.
    let link_bytes = COMMIT_LINK_OVERHEAD_BYTES
        .checked_add(delta.estimated_bytes()?)
        .ok_or_else(|| corrupt("commit link estimate overflows usize"))?;
    if link_bytes != reservation.link_bytes {
        return Err(DevonError::InvalidArgument {
            context: format!(
                "commit reservation of {} bytes does not match the link estimate of {link_bytes} bytes",
                reservation.link_bytes
            ),
        });
    }
    let records = wal_records(&delta, &rel_delete_records);
    let has_dml = !delta.node_updates.is_empty() || !delta.node_deletes.is_empty();
    let has_rel_tombstones = !rel_delete_records.is_empty();
    let has_hnsw_mutations = super::hnsw::delta_mutates_indexed_table(&catalog, &delta);
    let durable = (|| {
        let encoded = encode_transaction(&records)?;
        let wal = pipe.writer()?;
        set_wal_feature_flags(shared, has_dml, has_rel_tombstones, has_hnsw_mutations)?;
        let mut commit_lsn = None;
        for payload in encoded {
            commit_lsn = Some(wal.append(&payload)?);
        }
        wal.sync()?;
        commit_lsn.ok_or_else(|| corrupt("non-empty transaction encoded no commit record"))
    })();
    let commit_lsn = durable?;

    let link = CommitLink::new_arc_reserved(
        current.chain.clone(),
        commit_lsn,
        delta,
        Arc::clone(&shared.budget),
        reservation.link_bytes,
    )
    .map_err(|error| {
        corrupt(format!(
            "commit at LSN {commit_lsn} is durable but publication failed: {error}; \
             reopen the database to recover the committed transaction"
        ))
    })?;
    reservation.relinquish_link();
    let mut recent_summaries = current.recent_summaries.clone();
    recent_summaries.push((commit_lsn, summary));
    let next = Arc::new(PublishedState {
        catalog: Arc::new(catalog),
        chain: Some(link),
        last_commit_lsn: commit_lsn,
        // A commit extends the overlay; the on-disk catalog generation is
        // unchanged, and pinning it (never the commit LSN) is what keeps
        // this snapshot's pages unreclaimable.
        catalog_generation: current.catalog_generation,
        recent_summaries,
    });
    // A commit never moves checkpointed offsets, so the PK resolution
    // caches remain exactly valid for the successor state.
    PublishedState::adopt_pk_caches(&current, &next);
    PublishedState::adopt_fulltext_caches(&current, &next);
    shared.publish(next);
    Ok(())
}

fn set_wal_feature_flags(
    shared: &Shared,
    has_dml: bool,
    has_rel_tombstones: bool,
    has_hnsw_mutations: bool,
) -> DevonResult<()> {
    let flags = shared.pager.superblock().feature_flags;
    let mut required = 0;
    if has_dml {
        required |= DML_WAL_FLAG;
    }
    if has_rel_tombstones {
        required |= REL_TOMBSTONE_WAL_FLAG;
    }
    if has_hnsw_mutations {
        required |= HNSW_MUTATION_WAL_FLAG;
    }
    if flags & required != required {
        shared.pager.commit_feature_flags(flags | required)?;
    }
    Ok(())
}

struct CommitReservation {
    budget: Arc<MemoryBudget>,
    link_bytes: usize,
    summary_bytes: usize,
}

impl CommitReservation {
    fn relinquish_link(&mut self) {
        self.link_bytes = 0;
    }

    fn relinquish_summary(&mut self) {
        self.summary_bytes = 0;
    }
}

impl Drop for CommitReservation {
    fn drop(&mut self) {
        self.budget
            .release(self.link_bytes.saturating_add(self.summary_bytes));
    }
}

fn detect_conflict(current: &PublishedState, transaction: &Transaction) -> DevonResult<()> {
    let dml_pks = dml_pks_from_writes(&transaction.catalog, &transaction.writes)?;
    let detach_pks = detach_pks_from_writes(&transaction.writes);
    let edge_endpoint_pks =
        edge_endpoint_pks_from_writes(&transaction.catalog, &transaction.writes)?;
    // Cross-claims deliberately use retained summaries, not the reachable
    // tombstone chain: checkpoint may clear that chain while this transaction
    // still owns an older snapshot. The existing minimum-write-snapshot
    // pruning rule retains every winning summary in that conflict window.
    for (commit_lsn, summary) in current.recent_summaries.iter().rev() {
        if *commit_lsn <= transaction.state.last_commit_lsn {
            break;
        }
        for (table, keys) in transaction.writes.inserted_pks.iter().chain(&dml_pks) {
            if let Some(key) = conflicting_key(summary, table, keys) {
                let inserted = transaction
                    .writes
                    .inserted_pks
                    .get(table)
                    .is_some_and(|inserted| inserted.contains(key))
                    && summary
                        .inserted_pks
                        .get(table)
                        .is_some_and(|inserted| inserted.contains(key));
                return Err(write_conflict(table, key, *commit_lsn, inserted));
            }
        }
        if let Some((table, key)) = asymmetric_conflict(&detach_pks, &summary.edge_endpoint_pks)
            .or_else(|| asymmetric_conflict(&edge_endpoint_pks, &summary.detach_pks))
        {
            return Err(DevonError::TransactionConflict {
                context: format!(
                    "node table `{table}` primary key {key} was touched by a concurrent detach/incident-edge insert (committed at LSN {commit_lsn})"
                ),
            });
        }
        for name in transaction.writes.ddl.iter().map(ddl_name) {
            if summary
                .created_tables
                .iter()
                .any(|created| fold(created) == fold(name))
            {
                return Err(DevonError::TransactionConflict {
                    context: format!(
                        "table `{name}` was created by a concurrent transaction (committed at LSN {commit_lsn})"
                    ),
                });
            }
        }
    }
    Ok(())
}

fn asymmetric_conflict<'a>(
    ours: &'a BTreeMap<String, Vec<Value>>,
    committed: &BTreeMap<String, Vec<Value>>,
) -> Option<(&'a str, &'a Value)> {
    for (table, keys) in ours {
        if let Some(key) = keys.iter().find(|key| {
            committed
                .get(table)
                .is_some_and(|winning| winning.contains(key))
        }) {
            return Some((table, key));
        }
    }
    None
}

fn conflicting_key<'a>(
    summary: &CommitSummary,
    table: &str,
    keys: &'a [Value],
) -> Option<&'a Value> {
    keys.iter().find(|key| {
        summary
            .inserted_pks
            .get(table)
            .is_some_and(|committed| committed.contains(key))
            || summary
                .dml_pks
                .get(table)
                .is_some_and(|committed| committed.contains(key))
    })
}

fn write_conflict(table: &str, key: &Value, commit_lsn: u64, inserted: bool) -> DevonError {
    let action = if inserted { "inserted" } else { "written" };
    DevonError::TransactionConflict {
        context: format!(
            "node table `{table}` primary key {key} was {action} by a concurrent transaction (committed at LSN {commit_lsn})"
        ),
    }
}

fn build_commit_delta(
    shared: &Arc<Shared>,
    current: &Arc<PublishedState>,
    catalog: &Catalog,
    writes: &WriteSet,
) -> DevonResult<(CommitDelta, RelDeleteRecords)> {
    let mut delta = CommitDelta {
        nodes: writes.nodes.clone(),
        node_updates: writes.node_updates.clone(),
        node_deletes: writes.node_deletes.clone(),
        ddl: writes.ddl.clone(),
        ..CommitDelta::default()
    };
    delta.edges = resolve_edges_for_view(
        shared,
        current,
        &Arc::new(catalog.clone()),
        &delta,
        &writes.edges,
    )
    .map_err(|error| match error {
        // A key that resolved at execute must resolve at commit; only that
        // invariant breach is corruption. Resource errors (BudgetExceeded,
        // I/O) pass through with their real class.
        DevonError::NotFound { .. } => corrupt(format!(
            "relationship endpoint stopped resolving at commit: {error}"
        )),
        other => other,
    })?;
    let rel_delete_records = resolve_detaches_for_commit(
        shared,
        current,
        &Arc::new(catalog.clone()),
        &delta,
        &writes.detaches,
    )?;
    delta.rel_tombstones = rel_tombstones_from_records(&rel_delete_records);
    super::hnsw::propose_commit_deltas(shared, current, catalog, &mut delta)?;
    Ok((delta, rel_delete_records))
}

fn validate_commit_dml_refusals(catalog: &Catalog, writes: &WriteSet) -> DevonResult<()> {
    for table in writes.node_updates.keys().chain(writes.node_deletes.keys()) {
        catalog
            .node_table(table)
            .ok_or_else(|| corrupt(format!("DML write set names unknown node table `{table}`")))?;
    }
    for table in writes.node_deletes.keys() {
        let schema = catalog.node_table(table).ok_or_else(|| {
            corrupt(format!(
                "delete write set names unknown node table `{table}`"
            ))
        })?;
        if !writes
            .detaches
            .iter()
            .any(|intent| fold(&intent.table) == fold(table))
        {
            reject_referenced_delete(catalog, schema)?;
        }
    }
    Ok(())
}

fn resolve_detaches_for_commit(
    shared: &Arc<Shared>,
    state: &Arc<PublishedState>,
    catalog: &Arc<Catalog>,
    own: &CommitDelta,
    detaches: &[crate::txn::PendingDetach],
) -> DevonResult<RelDeleteRecords> {
    if detaches.is_empty() {
        return Ok(BTreeMap::new());
    }
    let mut resolution_delta = own.clone();
    // PendingDetach always names a preexisting row: detaching an own
    // insert folds locally. A same-key reinsert must not retarget the old
    // endpoint tombstone onto the newly appended physical slot.
    resolution_delta.nodes.clear();
    resolution_delta.node_deletes.clear();
    resolution_delta.rel_tombstones.clear();
    let view = ReadView::new(
        Arc::clone(shared),
        Arc::clone(state),
        Arc::clone(catalog),
        Some(Arc::new(resolution_delta)),
    );
    let mut resolved = Vec::with_capacity(detaches.len());
    for intent in detaches {
        let offset =
            resolve_node_key(&view, &intent.table, &intent.key).map_err(|error| match error {
                DevonError::NotFound { .. } => corrupt(format!(
                    "detach target `{}` primary key {} stopped resolving at commit",
                    intent.table, intent.key
                )),
                other => other,
            })?;
        resolved.push((intent, offset));
    }

    let mut records = BTreeMap::new();
    for rel in catalog.rel_tables() {
        let mut claims = Vec::new();
        let mut seen = BTreeSet::new();
        for (intent, offset) in &resolved {
            append_rel_delete_claim(
                &mut claims,
                &mut seen,
                rel.from(),
                &intent.table,
                RelEndpoint::From,
                *offset,
            );
            append_rel_delete_claim(
                &mut claims,
                &mut seen,
                rel.to(),
                &intent.table,
                RelEndpoint::To,
                *offset,
            );
        }
        if !claims.is_empty() {
            records.insert(rel.name().to_owned(), claims);
        }
    }
    Ok(records)
}

fn append_rel_delete_claim(
    claims: &mut Vec<RelDeleteClaim>,
    seen: &mut BTreeSet<RelDeleteClaim>,
    endpoint_table: &str,
    detached_table: &str,
    endpoint: RelEndpoint,
    offset: u64,
) {
    if fold(endpoint_table) == fold(detached_table) && seen.insert((endpoint, offset)) {
        claims.push((endpoint, offset));
    }
}

fn rel_tombstones_from_records(
    records: &RelDeleteRecords,
) -> BTreeMap<String, devondb_storage::overlay::RelEndpointTombstones> {
    let mut tombstones = BTreeMap::new();
    for (rel, claims) in records {
        let roles = tombstones
            .entry(rel.clone())
            .or_insert_with(devondb_storage::overlay::RelEndpointTombstones::default);
        for (endpoint, offset) in claims {
            match endpoint {
                RelEndpoint::From => {
                    roles.from_offsets.insert(*offset);
                }
                RelEndpoint::To => {
                    roles.to_offsets.insert(*offset);
                }
            }
        }
    }
    tombstones
}

fn resolve_edges_for_view(
    shared: &Arc<Shared>,
    state: &Arc<PublishedState>,
    catalog: &Arc<Catalog>,
    own: &CommitDelta,
    pending: &BTreeMap<String, Vec<PendingEdge>>,
) -> DevonResult<BTreeMap<String, Vec<OverlayEdge>>> {
    let view = ReadView::new(
        Arc::clone(shared),
        Arc::clone(state),
        Arc::clone(catalog),
        Some(Arc::new(own.clone())),
    );
    let mut resolved = BTreeMap::new();
    for (table, edges) in pending {
        let schema = catalog.rel_table(table).ok_or_else(|| {
            corrupt(format!(
                "write set names unknown relationship table `{table}`"
            ))
        })?;
        let mut table_edges = Vec::with_capacity(edges.len());
        for edge in edges {
            table_edges.push(OverlayEdge {
                from: resolve_node_key(&view, schema.from(), &edge.from_key)?,
                to: resolve_node_key(&view, schema.to(), &edge.to_key)?,
                values: edge.values.clone(),
            });
        }
        resolved.insert(table.clone(), table_edges);
    }
    Ok(resolved)
}

/// Transfers a write-set charge into separately owned link and summary reservations.
fn transfer_commit_charge(
    budget: &Arc<MemoryBudget>,
    writes: &mut WriteSet,
    delta: &CommitDelta,
    summary: &CommitSummary,
) -> DevonResult<CommitReservation> {
    let link_bytes = COMMIT_LINK_OVERHEAD_BYTES
        .checked_add(delta.estimated_bytes()?)
        .ok_or_else(|| DevonError::BudgetExceeded {
            context: "committed overlay byte estimate exceeds usize::MAX".to_owned(),
        })?;
    let summary_bytes = summary.estimated_bytes()?;
    let requested =
        link_bytes
            .checked_add(summary_bytes)
            .ok_or_else(|| DevonError::BudgetExceeded {
                context: "commit publication byte estimate exceeds usize::MAX".to_owned(),
            })?;
    if requested > writes.charged && !budget.try_charge(requested - writes.charged) {
        return Err(DevonError::BudgetExceeded {
            context: format!(
                "commit publication requested {requested} bytes with {} write-set bytes reserved, {} bytes charged, and limit {}",
                writes.charged,
                budget.charged(),
                budget.limit()
            ),
        });
    }
    if writes.charged > requested {
        budget.release(writes.charged - requested);
    }
    writes.charged = 0;
    Ok(CommitReservation {
        budget: Arc::clone(budget),
        link_bytes,
        summary_bytes,
    })
}

fn wal_records(delta: &CommitDelta, rel_delete_records: &RelDeleteRecords) -> Vec<WalPayload> {
    let mut records = Vec::new();
    records.extend(delta.ddl.iter().cloned().map(|ddl| {
        WalPayload::Ddl(match ddl {
            DdlOp::CreateNodeTable(schema) => DdlPayload::CreateNodeTable(schema),
            DdlOp::CreateRelTable(schema) => DdlPayload::CreateRelTable(schema),
        })
    }));
    for (table, rows) in &delta.nodes {
        records.extend(rows.iter().cloned().map(|row| WalPayload::NodeInsert {
            table: table.clone(),
            row,
        }));
    }
    for (rel, edges) in &delta.edges {
        records.extend(edges.iter().map(|edge| WalPayload::RelInsert {
            rel: rel.clone(),
            from: edge.from,
            to: edge.to,
            values: edge.values.clone(),
        }));
    }
    for (table, rows) in &delta.node_updates {
        records.extend(rows.iter().cloned().map(|row| WalPayload::NodeUpdate {
            table: table.clone(),
            row,
        }));
    }
    for (rel, claims) in rel_delete_records {
        records.extend(
            claims
                .iter()
                .map(|(endpoint, offset)| WalPayload::RelDelete {
                    rel: rel.clone(),
                    endpoint: *endpoint,
                    offset: *offset,
                }),
        );
    }
    for (table, keys) in &delta.node_deletes {
        records.extend(keys.iter().cloned().map(|key| WalPayload::NodeDelete {
            table: table.clone(),
            key,
        }));
    }
    records
}

fn summary_from_writes(catalog: &Catalog, writes: &WriteSet) -> DevonResult<CommitSummary> {
    Ok(CommitSummary::new_with_detach_claims(
        writes.inserted_pks.clone(),
        dml_pks_from_writes(catalog, writes)?,
        detach_pks_from_writes(writes),
        edge_endpoint_pks_from_writes(catalog, writes)?,
        writes.ddl.iter().map(ddl_name).map(str::to_owned).collect(),
    ))
}

fn detach_pks_from_writes(writes: &WriteSet) -> BTreeMap<String, Vec<Value>> {
    let mut detach_pks = BTreeMap::<String, Vec<Value>>::new();
    for intent in &writes.detaches {
        let keys = detach_pks.entry(intent.table.clone()).or_default();
        if !keys.contains(&intent.key) {
            keys.push(intent.key.clone());
        }
    }
    detach_pks
}

fn edge_endpoint_pks_from_writes<'a>(
    catalog: &'a Catalog,
    writes: &'a WriteSet,
) -> DevonResult<BTreeMap<String, Vec<Value>>> {
    let mut endpoint_pks = BTreeMap::<String, Vec<Value>>::new();
    let mut seen = BTreeSet::<(&str, EndpointPk<'_>)>::new();
    for (rel, edges) in &writes.edges {
        let schema = catalog.rel_table(rel).ok_or_else(|| {
            corrupt(format!(
                "relationship write set names unknown table `{rel}` while deriving conflict claims"
            ))
        })?;
        for edge in edges {
            push_unique_claim(&mut endpoint_pks, &mut seen, schema.from(), &edge.from_key)?;
            push_unique_claim(&mut endpoint_pks, &mut seen, schema.to(), &edge.to_key)?;
        }
    }
    Ok(endpoint_pks)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum EndpointPk<'a> {
    Int64(i64),
    String(&'a str),
}

fn push_unique_claim<'a>(
    claims: &mut BTreeMap<String, Vec<Value>>,
    seen: &mut BTreeSet<(&'a str, EndpointPk<'a>)>,
    table: &'a str,
    key: &'a Value,
) -> DevonResult<()> {
    let claim = match key {
        Value::Int64(value) => EndpointPk::Int64(*value),
        Value::String(value) => EndpointPk::String(value),
        other => {
            return Err(corrupt(format!(
                "relationship endpoint claim for node table `{table}` has invalid primary key {other}"
            )));
        }
    };
    if seen.insert((table, claim)) {
        claims
            .entry(table.to_owned())
            .or_default()
            .push(key.clone());
    }
    Ok(())
}

fn edge_endpoint_table_staged(
    catalog: &Catalog,
    writes: &WriteSet,
    table: &str,
) -> DevonResult<bool> {
    for rel in writes.edges.keys() {
        let schema = catalog.rel_table(rel).ok_or_else(|| {
            corrupt(format!(
                "relationship write set names unknown table `{rel}` while estimating conflict claims"
            ))
        })?;
        if schema.from() == table || schema.to() == table {
            return Ok(true);
        }
    }
    Ok(false)
}

fn dml_pks_from_writes(
    catalog: &Catalog,
    writes: &WriteSet,
) -> DevonResult<BTreeMap<String, Vec<Value>>> {
    let mut dml_pks = writes.node_deletes.clone();
    for (table, rows) in &writes.node_updates {
        let schema = catalog
            .node_table(table)
            .ok_or_else(|| corrupt(format!("DML write set names unknown node table `{table}`")))?;
        let (key_index, _) = primary_key(schema)?;
        let keys = dml_pks.entry(table.clone()).or_default();
        for row in rows {
            let key = row
                .get(key_index)
                .cloned()
                .ok_or_else(|| corrupt("staged update lost its primary-key value"))?;
            if !keys.contains(&key) {
                keys.push(key);
            }
        }
    }
    Ok(dml_pks)
}

pub(super) fn summary_from_delta(
    catalog: &Catalog,
    delta: &CommitDelta,
) -> DevonResult<CommitSummary> {
    let mut inserted_pks = BTreeMap::new();
    for (table, rows) in &delta.nodes {
        let schema = catalog.node_table(table).ok_or_else(|| {
            corrupt(format!(
                "committed delta names unknown node table `{table}`"
            ))
        })?;
        let (key_index, _) = primary_key(schema)?;
        let keys = rows
            .iter()
            .map(|row| {
                row.get(key_index)
                    .cloned()
                    .ok_or_else(|| corrupt("committed row lost its primary-key value"))
            })
            .collect::<DevonResult<Vec<_>>>()?;
        inserted_pks.insert(table.clone(), keys);
    }
    let mut dml_pks = delta.node_deletes.clone();
    for (table, rows) in &delta.node_updates {
        let schema = catalog.node_table(table).ok_or_else(|| {
            corrupt(format!(
                "committed delta names unknown node table `{table}`"
            ))
        })?;
        let (key_index, _) = primary_key(schema)?;
        let keys = dml_pks.entry(table.clone()).or_default();
        for row in rows {
            let key = row
                .get(key_index)
                .cloned()
                .ok_or_else(|| corrupt("committed update lost its primary-key value"))?;
            if !keys.contains(&key) {
                keys.push(key);
            }
        }
    }
    // A restart has no surviving pre-restart writer, so conflict-only detach
    // and endpoint-key claims need not be reconstructed from offset WAL.
    Ok(CommitSummary::new_with_detach_claims(
        inserted_pks,
        dml_pks,
        BTreeMap::new(),
        BTreeMap::new(),
        delta.ddl.iter().map(ddl_name).map(str::to_owned).collect(),
    ))
}

fn ddl_name(ddl: &DdlOp) -> &str {
    match ddl {
        DdlOp::CreateNodeTable(schema) => schema.name(),
        DdlOp::CreateRelTable(schema) => schema.name(),
    }
}

pub(super) fn apply_ddl_to_catalog(catalog: &Catalog, ddl: &[DdlOp]) -> DevonResult<Catalog> {
    let mut updated = catalog.clone();
    for operation in ddl {
        match operation {
            DdlOp::CreateNodeTable(schema) => {
                reject_interface_collision_for_node(&updated, schema.name())?;
                updated.add_node_table(schema.clone())?;
            }
            DdlOp::CreateRelTable(schema) => updated.add_rel_table(schema.clone())?,
        }
    }
    Ok(updated)
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
        time::{SystemTime, UNIX_EPOCH},
    };

    use devondb_plan::statement::{RelRow, Statement};
    use devondb_types::{logical_type::LogicalType, schema::Column, value::Value};

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
                "devondb-commit-suggestion-test-{}-{timestamp}-{sequence}",
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

    fn column(name: &str, ty: LogicalType, primary_key: bool) -> Column {
        Column {
            name: name.to_owned(),
            ty,
            primary_key,
        }
    }

    fn create_person(database: &mut Database) {
        database
            .execute(&Statement::CreateNodeTable {
                name: "Person".to_owned(),
                columns: vec![column("id", LogicalType::Int64, true)],
            })
            .unwrap();
    }

    fn open_database(directory: &TestDirectory, name: &str) -> Database {
        Database::create(directory.database(name), PAGE_SIZE).unwrap()
    }

    #[test]
    fn insert_node_table_name_did_you_mean() {
        let directory = TestDirectory::new();
        let mut database = open_database(&directory, "insert.devondb");
        create_person(&mut database);

        let error = database
            .execute(&Statement::InsertNode {
                table: "Persn".to_owned(),
                rows: vec![vec![Value::Int64(1)]],
            })
            .unwrap_err();

        assert!(error.to_string().ends_with(" (did you mean `Person`?)"));
    }

    #[test]
    fn primary_key_miss_has_no_did_you_mean() {
        let directory = TestDirectory::new();
        let mut database = open_database(&directory, "key-miss.devondb");
        create_person(&mut database);
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
                rows: vec![vec![Value::Int64(1)]],
            })
            .unwrap();

        let error = database
            .execute(&Statement::InsertRel {
                table: "Knows".to_owned(),
                rows: vec![RelRow {
                    from_key: Value::Int64(999),
                    to_key: Value::Int64(1),
                    values: Vec::new(),
                }],
            })
            .unwrap_err();

        assert!(!error.to_string().contains("did you mean"));
    }
}
