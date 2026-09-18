use super::*;

use std::collections::BTreeSet;

use super::{
    commit::{apply_ddl_to_catalog, summary_from_delta},
    options::WAL_HEADER_LEN,
    view::primary_key,
};
use devondb_storage::{txn_log::RelEndpoint, wal::replay_from};
use devondb_types::schema::fold;

pub(super) struct ReplayedWal {
    pub(super) groups: Vec<CommittedGroup>,
    pub(super) first_lsn: Option<u64>,
    pub(super) committed_offset: u64,
    pub(super) intact_offset: u64,
}

pub(super) fn replay_wal_from(path: &Path, offset: u64) -> DevonResult<ReplayedWal> {
    let (records, intact_offset) = match replay_from(path, offset) {
        Ok(replay) => replay,
        Err(DevonError::Io(error))
            if offset == 0 && error.kind() == std::io::ErrorKind::NotFound =>
        {
            (Vec::new(), 0)
        }
        Err(error) => return Err(error),
    };
    let groups = group_transactions(records.iter().map(|(lsn, bytes)| (*lsn, bytes)), 0)?;
    let committed_offset = match groups.last() {
        Some(group) => offset_after_lsn(offset, &records, group.commit_lsn)?,
        None => offset,
    };
    Ok(ReplayedWal {
        groups,
        first_lsn: records.first().map(|(lsn, _)| *lsn),
        committed_offset,
        intact_offset,
    })
}

fn offset_after_lsn(start: u64, records: &[(u64, Vec<u8>)], target_lsn: u64) -> DevonResult<u64> {
    let mut offset = start;
    for (lsn, payload) in records {
        let payload_len = u64::try_from(payload.len())
            .map_err(|_| invalid_argument("WAL payload length cannot be represented as u64"))?;
        offset = offset
            .checked_add(WAL_HEADER_LEN)
            .and_then(|value| value.checked_add(payload_len))
            .ok_or_else(|| invalid_argument("WAL cursor exceeds u64::MAX"))?;
        if *lsn == target_lsn {
            return Ok(offset);
        }
    }
    Err(corrupt(format!(
        "WAL commit LSN {target_lsn} is missing from its replay batch"
    )))
}

pub(super) fn recover_published_state(
    pager: &Pager,
    budget: &Arc<MemoryBudget>,
    catalog: Catalog,
    groups: Vec<CommittedGroup>,
    checkpoint_lsn: u64,
) -> DevonResult<Arc<PublishedState>> {
    RecoveryState::from_checkpoint(pager, catalog, checkpoint_lsn)?.apply_groups(budget, groups)
}

pub(super) fn extend_published_state(
    pager: &Pager,
    budget: &Arc<MemoryBudget>,
    current: &PublishedState,
    groups: Vec<CommittedGroup>,
) -> DevonResult<Arc<PublishedState>> {
    RecoveryState::from_published(pager, current)?.apply_groups(budget, groups)
}

struct RecoveryState {
    catalog: Catalog,
    checkpointed_node_totals: BTreeMap<String, u64>,
    overlay_node_totals: BTreeMap<String, u64>,
    chain: Option<Arc<CommitLink>>,
    recent_summaries: Vec<(u64, Arc<CommitSummary>)>,
    last_commit_lsn: u64,
    /// The durable superblock's checkpoint LSN: WAL replay extends the
    /// overlay, never the on-disk catalog, so the recovered state pins
    /// the generation recovery started from.
    catalog_generation: u64,
    feature_flags: u64,
}

impl RecoveryState {
    fn from_checkpoint(pager: &Pager, catalog: Catalog, checkpoint_lsn: u64) -> DevonResult<Self> {
        let checkpointed_node_totals = checkpointed_node_totals(pager, &catalog)?;
        Ok(Self {
            catalog,
            checkpointed_node_totals,
            overlay_node_totals: BTreeMap::new(),
            chain: None,
            recent_summaries: Vec::new(),
            last_commit_lsn: checkpoint_lsn,
            catalog_generation: checkpoint_lsn,
            feature_flags: pager.superblock().feature_flags,
        })
    }

    fn from_published(pager: &Pager, current: &PublishedState) -> DevonResult<Self> {
        let catalog = (*current.catalog).clone();
        let checkpointed_node_totals = checkpointed_node_totals(pager, &catalog)?;
        let mut overlay_node_totals = BTreeMap::new();
        for link in current.commit_links_oldest_first() {
            increment_overlay_totals(&mut overlay_node_totals, &link.delta, link.commit_lsn)?;
        }
        Ok(Self {
            catalog,
            checkpointed_node_totals,
            overlay_node_totals,
            chain: current.chain.clone(),
            recent_summaries: current.recent_summaries.clone(),
            last_commit_lsn: current.last_commit_lsn,
            catalog_generation: current.catalog_generation,
            feature_flags: pager.superblock().feature_flags,
        })
    }

    fn apply_groups(
        mut self,
        budget: &Arc<MemoryBudget>,
        groups: Vec<CommittedGroup>,
    ) -> DevonResult<Arc<PublishedState>> {
        for group in groups {
            self.apply_group(budget, group)?;
        }
        Ok(Arc::new(PublishedState {
            catalog: Arc::new(self.catalog),
            chain: self.chain,
            last_commit_lsn: self.last_commit_lsn,
            catalog_generation: self.catalog_generation,
            recent_summaries: self.recent_summaries,
        }))
    }

    fn apply_group(
        &mut self,
        budget: &Arc<MemoryBudget>,
        group: CommittedGroup,
    ) -> DevonResult<()> {
        let commit_lsn = group.commit_lsn;
        if commit_lsn <= self.last_commit_lsn {
            return Err(corrupt(format!(
                "WAL commit LSN {commit_lsn} does not strictly follow previous commit LSN {}",
                self.last_commit_lsn
            )));
        }
        let delta = CommitDelta::from_committed_group(group)
            .map_err(|error| recovery_error(commit_lsn, "invalid transaction payload", error))?;
        self.catalog = apply_ddl_to_catalog(&self.catalog, &delta.ddl).map_err(|error| {
            corrupt(format!(
                "WAL transaction committed at LSN {commit_lsn} has invalid DDL: {error}"
            ))
        })?;
        if super::hnsw::delta_mutates_indexed_table(&self.catalog, &delta)
            && self.feature_flags & devondb_storage::superblock::HNSW_MUTATION_WAL_FLAG == 0
        {
            return Err(corrupt(format!(
                "WAL transaction at LSN {commit_lsn} mutates an HNSW table without HNSW_MUTATION_WAL"
            )));
        }
        register_recovered_tables(
            &self.catalog,
            &mut self.checkpointed_node_totals,
            &mut self.overlay_node_totals,
        );
        validate_recovered_delta(
            &self.catalog,
            &self.checkpointed_node_totals,
            &self.overlay_node_totals,
            &delta,
            commit_lsn,
        )?;
        let summary = summary_from_delta(&self.catalog, &delta)
            .map_err(|error| recovery_error(commit_lsn, "invalid conflict summary", error))?;
        let summary = CommitSummary::new_arc(summary, Arc::clone(budget)).map_err(|error| {
            recovery_error(commit_lsn, "could not retain conflict summary", error)
        })?;
        increment_overlay_totals(&mut self.overlay_node_totals, &delta, commit_lsn)?;
        self.chain = Some(
            CommitLink::new_arc(self.chain.clone(), commit_lsn, delta, Arc::clone(budget))
                .map_err(|error| {
                    recovery_error(commit_lsn, "could not retain committed overlay", error)
                })?,
        );
        self.recent_summaries.push((commit_lsn, summary));
        self.last_commit_lsn = commit_lsn;
        Ok(())
    }
}

fn validate_recovered_delta(
    catalog: &Catalog,
    checkpointed_node_totals: &BTreeMap<String, u64>,
    overlay_node_totals: &BTreeMap<String, u64>,
    delta: &CommitDelta,
    commit_lsn: u64,
) -> DevonResult<()> {
    for (table, rows) in &delta.nodes {
        let schema = catalog.node_table(table).cloned().ok_or_else(|| {
            corrupt(format!(
                "WAL transaction committed at LSN {commit_lsn} names unknown node table `{table}`"
            ))
        })?;
        let mut validator = NodeTable::new(schema);
        for row in rows {
            validator.recover_row(row.clone()).map_err(|error| {
                corrupt(format!(
                    "WAL transaction committed at LSN {commit_lsn} has invalid row for node table `{table}`: {error}"
                ))
            })?;
        }
    }
    validate_recovered_dml(catalog, delta, commit_lsn)?;
    validate_recovered_rel_tombstones(
        catalog,
        checkpointed_node_totals,
        overlay_node_totals,
        delta,
        commit_lsn,
    )?;
    for (table, edges) in &delta.edges {
        validate_recovered_edges(
            catalog,
            checkpointed_node_totals,
            overlay_node_totals,
            delta,
            table,
            edges,
            commit_lsn,
        )?;
    }
    Ok(())
}

fn validate_recovered_dml(
    catalog: &Catalog,
    delta: &CommitDelta,
    commit_lsn: u64,
) -> DevonResult<()> {
    for (table, rows) in &delta.node_updates {
        let schema = recovered_dml_schema(catalog, table, commit_lsn)?;
        let mut validator = NodeTable::new(schema.clone());
        for row in rows {
            validator.recover_row(row.clone()).map_err(|error| {
                corrupt(format!(
                    "WAL transaction committed at LSN {commit_lsn} has invalid update for node table `{table}`: {error}"
                ))
            })?;
        }
    }
    for (table, keys) in &delta.node_deletes {
        let schema = recovered_dml_schema(catalog, table, commit_lsn)?;
        reject_recovered_referenced_delete(catalog, schema, table, keys, delta, commit_lsn)?;
        let (key_index, key_column) = primary_key(schema)?;
        for key in keys {
            if *key == Value::Null || !key.matches_type(&schema.columns()[key_index].ty) {
                return Err(corrupt(format!(
                    "WAL transaction committed at LSN {commit_lsn} has invalid delete key for `{table}.{key_column}`: {key}"
                )));
            }
        }
    }
    Ok(())
}

fn recovered_dml_schema<'a>(
    catalog: &'a Catalog,
    table: &str,
    commit_lsn: u64,
) -> DevonResult<&'a NodeTableSchema> {
    catalog.node_table(table).ok_or_else(|| {
        corrupt(format!(
            "WAL transaction committed at LSN {commit_lsn} names unknown DML node table `{table}`"
        ))
    })
}

fn reject_recovered_referenced_delete(
    catalog: &Catalog,
    schema: &NodeTableSchema,
    table: &str,
    keys: &[Value],
    delta: &CommitDelta,
    commit_lsn: u64,
) -> DevonResult<()> {
    for rel in catalog.rel_tables() {
        let claims = delta.rel_tombstones.get(rel.name());
        if fold(rel.from()) == fold(schema.name())
            && claims.map_or(0, |claims| claims.from_offsets.len()) != keys.len()
        {
            return Err(incomplete_detach_error(
                commit_lsn,
                table,
                rel.name(),
                "from",
            ));
        }
        if fold(rel.to()) == fold(schema.name())
            && claims.map_or(0, |claims| claims.to_offsets.len()) != keys.len()
        {
            return Err(incomplete_detach_error(commit_lsn, table, rel.name(), "to"));
        }
    }
    Ok(())
}

fn incomplete_detach_error(commit_lsn: u64, table: &str, rel: &str, endpoint: &str) -> DevonError {
    corrupt(format!(
        "WAL transaction committed at LSN {commit_lsn} deletes from relationship endpoint node table `{table}` without complete `{rel}` {endpoint} tombstones"
    ))
}

fn validate_recovered_rel_tombstones(
    catalog: &Catalog,
    checkpointed_node_totals: &BTreeMap<String, u64>,
    overlay_node_totals: &BTreeMap<String, u64>,
    delta: &CommitDelta,
    commit_lsn: u64,
) -> DevonResult<()> {
    for (rel, tombstones) in &delta.rel_tombstones {
        let schema = catalog.rel_table(rel).ok_or_else(|| {
            corrupt(format!(
                "WAL transaction committed at LSN {commit_lsn} names unknown relationship tombstone table `{rel}`"
            ))
        })?;
        validate_recovered_role_offsets(
            checkpointed_node_totals,
            overlay_node_totals,
            delta,
            rel,
            schema.from(),
            RelEndpoint::From,
            &tombstones.from_offsets,
            commit_lsn,
        )?;
        validate_recovered_role_offsets(
            checkpointed_node_totals,
            overlay_node_totals,
            delta,
            rel,
            schema.to(),
            RelEndpoint::To,
            &tombstones.to_offsets,
            commit_lsn,
        )?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn validate_recovered_role_offsets(
    checkpointed_node_totals: &BTreeMap<String, u64>,
    overlay_node_totals: &BTreeMap<String, u64>,
    delta: &CommitDelta,
    rel: &str,
    endpoint_table: &str,
    endpoint: RelEndpoint,
    offsets: &BTreeSet<u64>,
    commit_lsn: u64,
) -> DevonResult<()> {
    if offsets.is_empty() {
        return Ok(());
    }
    let detached = delta
        .node_deletes
        .keys()
        .any(|table| fold(table) == fold(endpoint_table));
    if !detached {
        return Err(corrupt(format!(
            "WAL transaction committed at LSN {commit_lsn} has `{rel}` {endpoint:?} tombstones but does not detach node table `{endpoint_table}`"
        )));
    }
    let total = recovered_node_total(
        checkpointed_node_totals,
        overlay_node_totals,
        delta,
        endpoint_table,
        commit_lsn,
    )?;
    if let Some(offset) = offsets.iter().find(|offset| **offset >= total) {
        return Err(corrupt(format!(
            "WAL transaction committed at LSN {commit_lsn} has out-of-range `{rel}` {endpoint:?} tombstone offset {offset} for domain {total}"
        )));
    }
    Ok(())
}

fn validate_recovered_edges(
    catalog: &Catalog,
    checkpointed_node_totals: &BTreeMap<String, u64>,
    overlay_node_totals: &BTreeMap<String, u64>,
    delta: &CommitDelta,
    table: &str,
    edges: &[OverlayEdge],
    commit_lsn: u64,
) -> DevonResult<()> {
    let schema = catalog.rel_table(table).cloned().ok_or_else(|| {
        corrupt(format!(
            "WAL transaction committed at LSN {commit_lsn} names unknown relationship table `{table}`"
        ))
    })?;
    let from_total = recovered_node_total(
        checkpointed_node_totals,
        overlay_node_totals,
        delta,
        schema.from(),
        commit_lsn,
    )?;
    let to_total = recovered_node_total(
        checkpointed_node_totals,
        overlay_node_totals,
        delta,
        schema.to(),
        commit_lsn,
    )?;
    let mut validator = RelTable::new(schema);
    for edge in edges {
        validator
            .recover_edge(edge.from, edge.to, edge.values.clone())
            .map_err(|error| {
                corrupt(format!(
                    "WAL transaction committed at LSN {commit_lsn} has invalid edge for relationship table `{table}`: {error}"
                ))
            })?;
        if edge.from >= from_total || edge.to >= to_total {
            return Err(corrupt(format!(
                "WAL transaction committed at LSN {commit_lsn} has out-of-range offsets ({}, {}) for relationship table `{table}`",
                edge.from, edge.to
            )));
        }
    }
    Ok(())
}

fn recovered_node_total(
    checkpointed_node_totals: &BTreeMap<String, u64>,
    overlay_node_totals: &BTreeMap<String, u64>,
    delta: &CommitDelta,
    table: &str,
    commit_lsn: u64,
) -> DevonResult<u64> {
    let base = checkpointed_node_totals.get(table).copied().ok_or_else(|| {
        corrupt(format!(
            "WAL transaction committed at LSN {commit_lsn} references missing endpoint node table `{table}`"
        ))
    })?;
    let overlay = overlay_node_totals.get(table).copied().unwrap_or(0);
    let own = u64::try_from(delta.nodes.get(table).map_or(0, Vec::len)).map_err(|_| {
        corrupt(format!(
            "WAL transaction committed at LSN {commit_lsn} has too many rows for node table `{table}`"
        ))
    })?;
    base.checked_add(overlay)
        .and_then(|total| total.checked_add(own))
        .ok_or_else(|| {
            corrupt(format!(
                "WAL transaction committed at LSN {commit_lsn} overflows the row count for node table `{table}`"
            ))
        })
}

fn checkpointed_node_totals(
    pager: &Pager,
    catalog: &Catalog,
) -> DevonResult<BTreeMap<String, u64>> {
    let mut totals = BTreeMap::new();
    for schema in catalog.node_tables() {
        let group_ids = catalog
            .table_storage(schema.name())
            .map_or(&[][..], |storage| storage.groups.as_slice());
        let mut total = 0_u64;
        for (group_index, group_id) in group_ids.iter().copied().enumerate() {
            let rows = NodeGroup::read_row_count(pager, group_id, schema.columns().len()).map_err(
                |error| {
                    corrupt(format!(
                        "node table `{}` group {group_index} has invalid metadata during recovery: {error}",
                        schema.name()
                    ))
                },
            )?;
            total = total
                .checked_add(u64::try_from(rows).map_err(|_| {
                    corrupt(format!(
                        "node table `{}` group row count exceeds u64::MAX during recovery",
                        schema.name()
                    ))
                })?)
                .ok_or_else(|| {
                    corrupt(format!(
                        "checkpointed row count for node table `{}` exceeds u64::MAX during recovery",
                        schema.name()
                    ))
                })?;
        }
        totals.insert(schema.name().to_owned(), total);
    }
    Ok(totals)
}

fn register_recovered_tables(
    catalog: &Catalog,
    checkpointed: &mut BTreeMap<String, u64>,
    overlay: &mut BTreeMap<String, u64>,
) {
    for schema in catalog.node_tables() {
        checkpointed.entry(schema.name().to_owned()).or_insert(0);
        overlay.entry(schema.name().to_owned()).or_insert(0);
    }
}

fn increment_overlay_totals(
    totals: &mut BTreeMap<String, u64>,
    delta: &CommitDelta,
    commit_lsn: u64,
) -> DevonResult<()> {
    for (table, rows) in &delta.nodes {
        let added = u64::try_from(rows.len()).map_err(|_| {
            corrupt(format!(
                "WAL transaction committed at LSN {commit_lsn} has too many rows for node table `{table}`"
            ))
        })?;
        let total = totals.entry(table.clone()).or_insert(0);
        *total = total.checked_add(added).ok_or_else(|| {
            corrupt(format!(
                "WAL transaction committed at LSN {commit_lsn} overflows the overlay row count for node table `{table}`"
            ))
        })?;
    }
    Ok(())
}

fn recovery_error(commit_lsn: u64, operation: &str, error: DevonError) -> DevonError {
    corrupt(format!(
        "WAL transaction committed at LSN {commit_lsn} has {operation}: {error}"
    ))
}

pub(super) fn trim_unterminated_tail(path: &Path, records: &[(u64, Vec<u8>)]) -> DevonResult<()> {
    let Some(last_commit_index) = records
        .iter()
        .rposition(|(_, payload)| matches!(decode_payload(payload), Ok(WalPayload::Commit { .. })))
    else {
        if records.is_empty() {
            return Ok(());
        }
        return truncate_wal(path);
    };
    if last_commit_index + 1 == records.len() {
        return Ok(());
    }
    let prefix_len =
        records[..=last_commit_index]
            .iter()
            .try_fold(0_u64, |total, (_, payload)| {
                let payload_len = u64::try_from(payload.len()).map_err(|_| {
                    invalid_argument("WAL payload length cannot be represented as u64")
                })?;
                total
                    .checked_add(WAL_HEADER_LEN)
                    .and_then(|value| value.checked_add(payload_len))
                    .ok_or_else(|| invalid_argument("WAL prefix length exceeds u64::MAX"))
            })?;
    let file = OpenOptions::new().write(true).open(path)?;
    file.set_len(prefix_len)?;
    file.sync_all()?;
    Ok(())
}
