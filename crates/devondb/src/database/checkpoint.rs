use super::*;

use std::{
    collections::{BTreeMap, HashMap},
    fs,
    sync::{Mutex, OnceLock, PoisonError, Weak},
};

use super::options::lock;
use devondb_storage::node_table::DeleteCompaction;
use devondb_storage::rel_table::{DetachCheckpointBlocked, OffsetRemap};

struct BlockedSession {
    shared: Weak<Shared>,
    failure: DetachCheckpointBlocked,
}

static DETACH_CHECKPOINT_BLOCKED: OnceLock<Mutex<HashMap<usize, BlockedSession>>> = OnceLock::new();

const AUTO_CHECKPOINT_ENV: &str = "DEVONDB_AUTOCHECKPOINT";

pub(super) fn checkpoint_shared(shared: &Arc<Shared>) -> DevonResult<()> {
    // Database::drop decrements the facade count before requesting its
    // best-effort clean-close drain. Keep explicit checkpoint calls active:
    // they necessarily retain the Database handle used to make the call.
    if auto_checkpoint_disabled() && shared.final_database_handle_closed() {
        return Ok(());
    }
    let (mut pipe, _publication_guard) = shared.lock_commit_and_gate()?;
    checkpoint_locked(shared, &mut pipe)
}

pub(super) fn checkpoint_locked(shared: &Arc<Shared>, pipe: &mut CommitPipe) -> DevonResult<()> {
    enforce_detach_backoff(shared)?;
    let current = shared.current_state();
    if current.chain.is_none() {
        // The WAL holds nothing pending, so all checkpoint-scoped bits may
        // be cleared here too. This heals a crash between a prior
        // checkpoint's WAL truncation and its bits 6|10|14 clear.
        pipe.wal = None;
        truncate_wal(&pipe.wal_path)?;
        pipe.wal = Some(WalWriter::open(&pipe.wal_path, next_lsn(&shared.pager)?)?);
        clear_checkpoint_wal_flags(shared)?;
        clear_detach_backoff(shared);
        let recent_summaries = retained_summaries(shared, &current);
        if recent_summaries.len() != current.recent_summaries.len() {
            shared.publish(Arc::new(PublishedState {
                catalog: Arc::clone(&current.catalog),
                chain: None,
                last_commit_lsn: current.last_commit_lsn,
                catalog_generation: current.catalog_generation,
                recent_summaries,
            }));
        }
        return Ok(());
    }
    let mut catalog = (*current.catalog).clone();
    let remaps = epoch_remaps(shared, &current)?;
    materialize_nodes(shared, &current, &mut catalog, &remaps)?;
    if let Err(error) = materialize_relationships(shared, &current, &mut catalog, &remaps) {
        if let Some(failure) = DetachCheckpointBlocked::from_error(&error) {
            let pages = catalog.prospective_pages(&shared.pager, &current.catalog);
            devondb_storage::free_pages::queue_prospective_pages(
                shared.pager.superblock().db_id,
                pages,
            );
            install_detach_backoff(shared, failure);
        }
        return Err(error);
    }
    super::hnsw::checkpoint_indexes(shared, &current, &mut catalog)?;
    catalog.save(&shared.pager, current.last_commit_lsn)?;
    clear_detach_backoff(shared);
    // The old writer's append offset is meaningless the moment the file is
    // truncated: appending through it would leave a zero gap that recovery
    // silently discards as a torn tail. Take it down first so a failed
    // truncate/reopen leaves no usable writer — later commits then fail
    // loudly through CommitPipe::writer instead of corrupting the WAL.
    pipe.wal = None;
    truncate_wal(&pipe.wal_path)?;
    pipe.wal = Some(WalWriter::open(&pipe.wal_path, next_lsn(&shared.pager)?)?);
    // DML_WAL, REL_TOMBSTONE_WAL and HNSW_MUTATION_WAL have checkpoint-scoped lifetimes
    // (FORMAT.md § Feature flag registry): clear them only AFTER truncation.
    // A crash before this line leaves bits-set-with-empty-WAL, which is a
    // safe refusal for old binaries and is healed by writable reopen.
    clear_checkpoint_wal_flags(shared)?;

    let recent_summaries = retained_summaries(shared, &current);
    shared.publish(Arc::new(PublishedState {
        catalog: Arc::new(catalog),
        chain: None,
        last_commit_lsn: current.last_commit_lsn,
        // The save above published the superblock at this LSN, so it is
        // the new state's catalog generation (the free-page pin key).
        catalog_generation: current.last_commit_lsn,
        recent_summaries,
    }));
    Ok(())
}

/// Clears all checkpoint-scoped WAL feature bits once the WAL holds no
/// governed records. A no-op when none of the bits are set.
fn clear_checkpoint_wal_flags(shared: &Shared) -> DevonResult<()> {
    let checkpoint_flags = devondb_storage::superblock::DML_WAL_FLAG
        | devondb_storage::superblock::REL_TOMBSTONE_WAL_FLAG
        | devondb_storage::superblock::HNSW_MUTATION_WAL_FLAG;
    let superblock = shared.pager.superblock();
    if superblock.feature_flags & checkpoint_flags == 0 {
        return Ok(());
    }
    shared
        .pager
        .commit_feature_flags(superblock.feature_flags & !checkpoint_flags)
}

fn materialize_nodes(
    shared: &Shared,
    state: &PublishedState,
    catalog: &mut Catalog,
    remaps: &BTreeMap<String, OffsetRemap>,
) -> DevonResult<()> {
    let schemas = catalog.node_tables().to_vec();
    for schema in schemas {
        let table_name = schema.name().to_owned();
        let mut table = NodeTable::new(schema);
        recover_overlay_inserts(state, &table_name, &mut table)?;
        let effects = state.node_dml_effects(&table_name)?;
        let remap = remap_for(remaps, &table_name)?;
        let delete_compaction = delete_compaction_for(catalog, &table_name, remap);
        table.checkpoint_with_dml(&shared.pager, catalog, effects, delete_compaction)?;
    }
    Ok(())
}

fn recover_overlay_inserts(
    state: &PublishedState,
    table_name: &str,
    table: &mut NodeTable,
) -> DevonResult<()> {
    for link in state.commit_links_oldest_first() {
        if let Some(rows) = link.delta.nodes.get(table_name) {
            for row in rows {
                table.recover_row(row.clone())?;
            }
        }
    }
    Ok(())
}

fn delete_compaction_for(
    catalog: &Catalog,
    table_name: &str,
    remap: &OffsetRemap,
) -> DeleteCompaction {
    let referenced = catalog.rel_tables().iter().any(|schema| {
        schema.from().eq_ignore_ascii_case(table_name)
            || schema.to().eq_ignore_ascii_case(table_name)
    });
    if referenced && remap.is_identity() {
        DeleteCompaction::Forbidden
    } else {
        DeleteCompaction::Permitted
    }
}

fn materialize_relationships(
    shared: &Shared,
    state: &PublishedState,
    catalog: &mut Catalog,
    remaps: &BTreeMap<String, OffsetRemap>,
) -> DevonResult<()> {
    let schemas = catalog.rel_tables().to_vec();
    for schema in schemas {
        let table_name = schema.name().to_owned();
        let mut table = RelTable::new(schema);
        for edge in state.rel_edges(&table_name) {
            table.recover_edge(edge.from, edge.to, edge.values.clone())?;
        }
        table.set_endpoint_tombstones(state.rel_tombstones(&table_name));
        let from_remap = remap_for(remaps, table.schema().from())?;
        let to_remap = remap_for(remaps, table.schema().to())?;
        table.checkpoint_with_remap(
            &shared.pager,
            &state.catalog,
            catalog,
            from_remap,
            to_remap,
            &shared.budget,
        )?;
    }
    Ok(())
}

fn epoch_remaps(
    shared: &Shared,
    state: &PublishedState,
) -> DevonResult<BTreeMap<String, OffsetRemap>> {
    let mut deleted: BTreeMap<String, Vec<u64>> = BTreeMap::new();
    for schema in state.catalog.rel_tables() {
        let tombstones = state.rel_tombstones(schema.name());
        deleted
            .entry(folded(schema.from()))
            .or_default()
            .extend(tombstones.from_offsets);
        deleted
            .entry(folded(schema.to()))
            .or_default()
            .extend(tombstones.to_offsets);
    }

    let mut remaps = BTreeMap::new();
    for schema in state.catalog.node_tables() {
        let checkpointed = checkpointed_row_count(shared, &state.catalog, schema)?;
        let overlay_slots =
            u64::try_from(state.node_slots(schema.name()).count()).map_err(|_| {
                DevonError::Corrupt {
                    context: format!(
                        "node table `{}` overlay slot count exceeds u64",
                        schema.name()
                    ),
                }
            })?;
        let old_domain =
            checkpointed
                .checked_add(overlay_slots)
                .ok_or_else(|| DevonError::Corrupt {
                    context: format!(
                        "node table `{}` physical domain overflows u64",
                        schema.name()
                    ),
                })?;
        let key = folded(schema.name());
        let table_deleted = deleted.remove(&key).unwrap_or_default();
        remaps.insert(key, OffsetRemap::new(old_domain, table_deleted)?);
    }
    Ok(remaps)
}

fn checkpointed_row_count(
    shared: &Shared,
    catalog: &Catalog,
    schema: &NodeTableSchema,
) -> DevonResult<u64> {
    let groups = catalog
        .table_storage(schema.name())
        .map_or(&[][..], |storage| storage.groups.as_slice());
    groups.iter().try_fold(0_u64, |total, page_id| {
        let rows = NodeGroup::read_row_count(&shared.pager, *page_id, schema.columns().len())?;
        total
            .checked_add(rows as u64)
            .ok_or_else(|| DevonError::Corrupt {
                context: format!("node table `{}` row count overflows u64", schema.name()),
            })
    })
}

fn remap_for<'a>(
    remaps: &'a BTreeMap<String, OffsetRemap>,
    table: &str,
) -> DevonResult<&'a OffsetRemap> {
    remaps
        .get(&folded(table))
        .ok_or_else(|| DevonError::Corrupt {
            context: format!("checkpoint has no epoch remap for node table `{table}`"),
        })
}

fn folded(name: &str) -> String {
    devondb_types::schema::fold(name).into_owned()
}

fn detach_backoffs() -> &'static Mutex<HashMap<usize, BlockedSession>> {
    DETACH_CHECKPOINT_BLOCKED.get_or_init(|| Mutex::new(HashMap::new()))
}

fn shared_key(shared: &Arc<Shared>) -> usize {
    Arc::as_ptr(shared) as usize
}

fn enforce_detach_backoff(shared: &Arc<Shared>) -> DevonResult<()> {
    let key = shared_key(shared);
    let failure = {
        let mut blocked = detach_backoffs()
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        blocked.retain(|_, session| session.shared.strong_count() > 0);
        blocked.get(&key).and_then(|session| {
            session
                .shared
                .upgrade()
                .filter(|owner| Arc::ptr_eq(owner, shared))
                .map(|_| session.failure.clone())
        })
    };
    let Some(mut failure) = failure else {
        return Ok(());
    };
    shared.pager.shed_cache(failure.requested);
    let charged = shared.budget.charged();
    if failure.requested <= shared.budget.limit().saturating_sub(charged) {
        clear_detach_backoff(shared);
        return Ok(());
    }
    failure.charged = charged;
    failure.limit = shared.budget.limit();
    Err(DevonError::BudgetExceeded {
        context: failure.context(),
    })
}

fn install_detach_backoff(shared: &Arc<Shared>, failure: DetachCheckpointBlocked) {
    let session = BlockedSession {
        shared: Arc::downgrade(shared),
        failure,
    };
    detach_backoffs()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(shared_key(shared), session);
}

fn clear_detach_backoff(shared: &Arc<Shared>) {
    detach_backoffs()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .remove(&shared_key(shared));
}

fn retained_summaries(shared: &Shared, state: &PublishedState) -> Vec<(u64, Arc<CommitSummary>)> {
    let write_txns = lock(&shared.write_txns);
    write_txns.values().min().copied().map_or_else(
        || {
            clear_recent_summaries(&state.recent_summaries);
            Vec::new()
        },
        |minimum| prune_recent_summaries(&state.recent_summaries, minimum),
    )
}

const MIN_AUTO_CHECKPOINT_WAL_BYTES: u64 = 4 * 1024 * 1024;
const AUTO_CHECKPOINT_DB_FRACTION: u64 = 8;

pub(super) fn should_auto_checkpoint(shared: &Shared, pipe: &CommitPipe) -> bool {
    if auto_checkpoint_disabled() {
        return false;
    }
    // §6's end-of-commit trigger: the overlay's own charge (links AND
    // retained summaries, docs/MVCC.md §7.2) against the high-water mark.
    shared.current_state().overlay_charged_bytes() > shared.budget.limit() / 4
        || wal_crossed_size_threshold(pipe)
}

fn auto_checkpoint_disabled() -> bool {
    std::env::var_os(AUTO_CHECKPOINT_ENV).is_some_and(|value| value == "off")
}

/// Returns whether the WAL crossed the engine-owned publication threshold.
///
/// Four MiB is about 1,000 default-size pages: large enough to amortize a
/// publication on small databases without letting their WAL grow unchecked.
/// The one-eighth term scales the interval with a large database so a small
/// delta does not repeatedly force whole-catalog publication work. Metadata
/// failures merely defer this opportunistic checkpoint; the fsynced WAL is
/// still the durability boundary and close/manual checkpoint can retry.
fn wal_crossed_size_threshold(pipe: &CommitPipe) -> bool {
    let Some(main_path) = &pipe.main_path else {
        return false;
    };
    let Ok(wal_bytes) = fs::metadata(&pipe.wal_path).map(|metadata| metadata.len()) else {
        return false;
    };
    let Ok(main_bytes) = fs::metadata(main_path).map(|metadata| metadata.len()) else {
        return false;
    };
    let threshold = MIN_AUTO_CHECKPOINT_WAL_BYTES.max(main_bytes / AUTO_CHECKPOINT_DB_FRACTION);
    wal_bytes >= threshold
}

/// The pre-begin pressure fold keys on the overlay metric: committed deltas
/// hold budget as published chain links,
/// and the emergency checkpoint inside a write-set charge can never free
/// the charging transaction's own chain — its snapshot pins it — so the
/// facade folds BEFORE beginning a statement's transaction. The metric is
/// the OVERLAY's charged bytes, never the budget total: a warm page cache
/// legitimately holds around half the budget, and a total-charge key would
/// make every autocommit under a warm cache serialize behind a
/// checkpoint. At `limit / 2` this is the documented backstop above §6's
/// end-of-commit `limit / 4` trigger — reachable because an end-of-commit
/// drain failure is deliberately swallowed (commit.rs), letting the
/// overlay keep growing until the next statement lands here.
pub(super) fn should_pressure_fold(shared: &Shared) -> bool {
    let state = shared.current_state();
    state.chain.is_some() && state.overlay_charged_bytes() > shared.budget.limit() / 2
}

#[cfg(test)]
mod tests {
    use super::*;

    use devondb_types::schema::Column;
    use tempfile::tempdir;

    const PAGE_SIZE: u32 = 4096;

    fn person_columns() -> Vec<Column> {
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
        ]
    }

    fn person(id: i64, name: &str) -> Vec<Value> {
        vec![Value::Int64(id), Value::String(name.to_owned())]
    }

    fn create_people(database: &mut Database, rows: Vec<Vec<Value>>) {
        database
            .execute(&Statement::CreateNodeTable {
                name: "Person".to_owned(),
                columns: person_columns(),
            })
            .unwrap();
        database
            .execute(&Statement::InsertNode {
                table: "Person".to_owned(),
                rows,
            })
            .unwrap();
        database.checkpoint().unwrap();
    }

    fn publish_deltas(database: &Database, deltas: Vec<CommitDelta>) {
        let current = database.shared.current_state();
        let mut chain = current.chain.clone();
        let mut commit_lsn = current.last_commit_lsn;
        for delta in deltas {
            commit_lsn += 1;
            chain = Some(
                CommitLink::new_arc(
                    chain,
                    commit_lsn,
                    delta,
                    Arc::clone(&database.shared.budget),
                )
                .unwrap(),
            );
        }
        database.shared.publish(Arc::new(PublishedState {
            catalog: Arc::clone(&current.catalog),
            chain,
            last_commit_lsn: commit_lsn,
            catalog_generation: current.catalog_generation,
            recent_summaries: Vec::new(),
        }));
    }

    fn scan_people(database: &Database) -> Vec<Vec<Value>> {
        let state = database.shared.current_state();
        let schema = state.catalog.node_table("Person").unwrap().clone();
        NodeTable::new(schema)
            .scan(&database.shared.pager, &state.catalog)
            .unwrap()
    }

    #[test]
    fn checkpoint_materializes_persisted_and_overlay_dml_then_reopens_as_a_no_op() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("checkpoint-dml.devondb");
        let mut database = Database::create(&path, PAGE_SIZE).unwrap();
        create_people(
            &mut database,
            vec![
                person(1, "one"),
                person(2, "two"),
                person(3, "three"),
                person(6, "six-old"),
            ],
        );
        let mut inserts = CommitDelta::default();
        inserts.nodes.insert(
            "Person".to_owned(),
            vec![person(4, "four-old"), person(5, "five-doomed")],
        );
        let mut dml = CommitDelta::default();
        dml.node_updates.insert(
            "Person".to_owned(),
            vec![person(2, "two-new"), person(4, "four-new")],
        );
        dml.node_deletes.insert(
            "Person".to_owned(),
            vec![Value::Int64(1), Value::Int64(5), Value::Int64(6)],
        );
        let mut revival = CommitDelta::default();
        revival
            .nodes
            .insert("Person".to_owned(), vec![person(6, "six-revived")]);
        publish_deltas(&database, vec![inserts, dml, revival]);

        database.checkpoint().unwrap();

        assert!(database.shared.current_state().chain.is_none());
        drop(database);
        let mut reopened = Database::open(&path).unwrap();
        assert_eq!(
            scan_people(&reopened),
            [
                person(2, "two-new"),
                person(3, "three"),
                person(4, "four-new"),
                person(6, "six-revived"),
            ]
        );
        let before = reopened.shared.current_state();
        let group_ids = before
            .catalog
            .table_storage("Person")
            .unwrap()
            .groups
            .clone();
        let checkpoint_lsn = reopened.shared.pager.superblock().checkpoint_lsn;
        drop(before);

        reopened.checkpoint().unwrap();

        let after = reopened.shared.current_state();
        assert_eq!(
            after.catalog.table_storage("Person").unwrap().groups,
            group_ids
        );
        assert_eq!(
            reopened.shared.pager.superblock().checkpoint_lsn,
            checkpoint_lsn
        );
    }

    #[test]
    fn checkpoint_refuses_delete_compaction_for_an_edge_referenced_table() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("referenced-delete.devondb");
        let mut database = Database::create(path, PAGE_SIZE).unwrap();
        create_people(&mut database, vec![person(1, "one")]);
        database
            .execute(&Statement::CreateRelTable {
                name: "Knows".to_owned(),
                from: "Person".to_owned(),
                to: "Person".to_owned(),
                columns: Vec::new(),
            })
            .unwrap();
        database.checkpoint().unwrap();
        let mut dml = CommitDelta::default();
        dml.node_deletes
            .insert("Person".to_owned(), vec![Value::Int64(1)]);
        publish_deltas(&database, vec![dml]);

        let error = database.checkpoint().unwrap_err();

        let DevonError::InvalidArgument { context } = error else {
            panic!("expected InvalidArgument, got {error}");
        };
        assert!(context.contains("delete compaction"));
        assert_eq!(scan_people(&database), [person(1, "one")]);
        assert!(database.shared.current_state().chain.is_some());
    }

    #[test]
    fn pressure_fold_keys_on_the_overlay_charge_not_the_budget_total() {
        // A warm page cache legitimately holds around half the budget, so a
        // total-charge key would checkpoint
        // before EVERY autocommit statement under a warm cache. With a
        // pending chain but a tiny overlay, neither trigger may fire no
        // matter how charged the rest of the budget is.
        let directory = tempdir().unwrap();
        let path = directory.path().join("fold-metric.devondb");
        let mut database = Database::create_with(
            &path,
            Options {
                page_size: PAGE_SIZE,
                memory_limit: 1024 * 1024,
            },
        )
        .unwrap();
        create_people(&mut database, vec![person(1, "one")]);
        let mut delta = CommitDelta::default();
        delta
            .nodes
            .insert("Person".to_owned(), vec![person(2, "two")]);
        publish_deltas(&database, vec![delta]);
        assert!(database.shared.current_state().chain.is_some());

        // Simulate the warm cache: charge well past limit/2 outside the
        // overlay.
        assert!(database.shared.budget.try_charge(600 * 1024));
        assert!(!should_pressure_fold(&database.shared));
        let pipe = lock(&database.shared.commit);
        assert!(!should_auto_checkpoint(&database.shared, &pipe));
        drop(pipe);
        database.shared.budget.release(600 * 1024);
    }

    #[test]
    fn pressure_fold_fires_when_the_overlay_itself_holds_half_the_limit() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("fold-fires.devondb");
        let mut database = Database::create_with(
            &path,
            Options {
                page_size: PAGE_SIZE,
                memory_limit: 1024 * 1024,
            },
        )
        .unwrap();
        create_people(&mut database, vec![person(1, "one")]);
        let mut delta = CommitDelta::default();
        delta.nodes.insert(
            "Person".to_owned(),
            vec![person(2, &"x".repeat(600 * 1024))],
        );
        publish_deltas(&database, vec![delta]);

        assert!(
            database.shared.current_state().overlay_charged_bytes() > 512 * 1024,
            "fixture must place over limit/2 in the overlay itself"
        );
        assert!(should_pressure_fold(&database.shared));
        let pipe = lock(&database.shared.commit);
        assert!(should_auto_checkpoint(&database.shared, &pipe));
    }
}
