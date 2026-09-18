#[cfg(feature = "pack")]
#[path = "pack.rs"]
mod pack;

use super::*;
use devondb_storage::overlay::{FullTextResult, shed_fulltext_caches};

use super::{
    checkpoint::{checkpoint_shared, should_pressure_fold},
    recovery::{
        ReplayedWal, extend_published_state, recover_published_state, replay_wal_from,
        trim_unterminated_tail,
    },
};
use std::sync::{TryLockError, Weak, atomic::AtomicUsize};

use devondb_storage::lock::{LockPaths, PublicationGate, PublicationGuard, WriterLease};
use devondb_storage::superblock::{
    DML_WAL_FLAG, MULTIPROCESS_COORDINATION_FLAG, REL_TOMBSTONE_WAL_FLAG,
};
use devondb_types::schema::{fold, suggestion_suffix};

/// The rows and column names produced by a DevonPlan query.
#[derive(Debug, Clone, PartialEq)]
pub struct QueryResult {
    /// Output column names in row-value order.
    pub columns: Vec<String>,
    /// Materialized result rows.
    pub rows: Vec<Vec<Value>>,
}

/// Default `memory_limit`: 64 MiB, the edge envelope.
const DEFAULT_MEMORY_LIMIT: usize = 64 * 1024 * 1024;
/// Minimum accepted `memory_limit`.
const MIN_MEMORY_LIMIT: usize = 1024 * 1024;
pub(super) const WAL_HEADER_LEN: u64 = 16;

/// Configuration for opening or creating a database.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Options {
    /// Page size in bytes; used on create only, ignored on open.
    pub page_size: u32,
    /// Memory budget in bytes; default 64 MiB, minimum 1 MiB.
    pub memory_limit: usize,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            page_size: 4096,
            memory_limit: DEFAULT_MEMORY_LIMIT,
        }
    }
}

impl Options {
    fn validate(&self) -> DevonResult<()> {
        if self.memory_limit < MIN_MEMORY_LIMIT {
            return Err(invalid_argument(format!(
                "memory_limit is {} bytes but the minimum is {MIN_MEMORY_LIMIT} bytes (1 MiB)",
                self.memory_limit
            )));
        }
        Ok(())
    }
}

fn heal_empty_wal_flags(
    pager: &Pager,
    wal_path: &Path,
    live_chain: bool,
    publication_gate: Option<&PublicationGate>,
) -> DevonResult<()> {
    let scoped =
        DML_WAL_FLAG | REL_TOMBSTONE_WAL_FLAG | devondb_storage::superblock::HNSW_MUTATION_WAL_FLAG;
    let flags = pager.superblock().feature_flags;
    // Live insert groups can coexist with obsolete governed DML. Retain the
    // fence until a checkpoint can drain the entire WAL without losing inserts.
    if flags & scoped == 0 || live_chain {
        return Ok(());
    }
    let gate = publication_gate
        .ok_or_else(|| corrupt("writable WAL-bit healing has no publication gate"))?;
    let _publication_guard = gate.try_exclusive()?;
    super::truncate_wal(wal_path)?;
    pager.commit_feature_flags(flags & !scoped)
}

/// The WAL writer and path protected by the one commit/checkpoint lock.
///
/// `wal` is `None` exactly when the database is open read-only: a read-only
/// open never creates, truncates, or appends to the sidecar, and the
/// `require_writable` gates keep commit/checkpoint from reaching it.
pub(crate) struct CommitPipe {
    pub(super) wal: Option<WalWriter>,
    pub(super) wal_path: PathBuf,
    /// Main-file path used only for the runtime auto-checkpoint size policy.
    /// Pack, inspection, and follower handles leave it absent because they
    /// cannot publish checkpoints.
    pub(super) main_path: Option<PathBuf>,
}

impl CommitPipe {
    pub(super) fn writer(&mut self) -> DevonResult<&mut WalWriter> {
        self.wal.as_mut().ok_or_else(|| DevonError::ReadOnly {
            context: "no usable WAL writer: the database is read-only, or a \
                      checkpoint failed while rotating the WAL — reopen the \
                      database to recover"
                .to_owned(),
        })
    }
}

/// State shared by every database handle, snapshot, and transaction.
pub(crate) struct Shared {
    pub(crate) pager: Arc<Pager>,
    pub(crate) budget: Arc<MemoryBudget>,
    pub(super) spill_config: SpillConfig,
    /// Owns this handle's locked spill subdirectory: the lock
    /// lives as long as any snapshot retains this state, so a writer
    /// reopen's sweep never removes runs a live handle can still read.
    _spill_dir: Option<Arc<SpillDirGuard>>,
    /// Shared lock on an already-existing writer-lease sidecar. Inspect
    /// handles never create coordination files; when the sidecar exists,
    /// retaining this file lock excludes writers for the handle lifetime.
    _inspect_lease: Option<File>,
    published: Mutex<Arc<PublishedState>>,
    pub(super) commit: Mutex<CommitPipe>,
    // Interior mutability lets the final writer-capable facade release the
    // lease while pinned read snapshots continue retaining `Shared` state.
    writer_lease: Mutex<Option<WriterLease>>,
    publication_gate: Option<PublicationGate>,
    database_handles: AtomicUsize,
    active_writer_operations: AtomicUsize,
    follower: bool,
    pub(super) write_txns: Mutex<BTreeMap<u64, u64>>,
    /// Weak handles to every state this handle has published or opened
    /// with. The minimum CATALOG GENERATION over live upgrades is the
    /// in-process component of the free-page pin horizon
    /// (`docs/FREE_PAGES.md` § The pin horizon): read snapshots, write
    /// transactions, and the currently published state all hold their
    /// `Arc<PublishedState>` alive, so one registry covers them all.
    pinned_states: Mutex<Vec<Weak<PublishedState>>>,
    next_txn_id: AtomicU64,
    /// True for a side-effect-free inspection handle.
    inspect: bool,
    /// True when the file enables a read-safe feature flag this build does
    /// not fully support: reads stay available, but writes and checkpoints
    /// could drop that feature's state (`docs/FORMAT.md` § Feature flag
    /// registry), so they are refused.
    read_only: bool,
    /// Bytes charged to `budget` for the DEVONPACK decoded-frame buffer
    /// (`docs/SCALE.md` §7.2): nonzero exactly when this state serves a
    /// pack container, which is read-only by construction. Released when
    /// the last handle sharing this state drops.
    pack_frame_bytes: usize,
}

impl Drop for Shared {
    fn drop(&mut self) {
        if self.pack_frame_bytes > 0 {
            self.budget.release(self.pack_frame_bytes);
        }
    }
}

impl Shared {
    pub(super) fn require_writable(&self, operation: &str) -> DevonResult<()> {
        if self.pack_frame_bytes > 0 {
            return Err(DevonError::ReadOnly {
                context: format!(
                    "{operation} refused: a DEVONPACK container is read-only (docs/SCALE.md §7.2)"
                ),
            });
        }
        if self.follower {
            return Err(DevonError::ReadOnly {
                context: format!(
                    "{operation} refused: this database handle has the read-only multiprocess follower role"
                ),
            });
        }
        if self.inspect {
            return Err(DevonError::ReadOnly {
                context: format!(
                    "{operation} refused: this database was opened for side-effect-free inspection"
                ),
            });
        }
        if self.read_only {
            return Err(DevonError::ReadOnly {
                context: format!(
                    "{operation} refused: the database file enables feature flags \
                     {:#x} that this build supports read-only",
                    self.pager.superblock().feature_flags
                ),
            });
        }
        Ok(())
    }

    pub(super) fn current_state(&self) -> Arc<PublishedState> {
        Arc::clone(&lock(&self.published))
    }

    pub(super) fn publish(&self, state: Arc<PublishedState>) {
        lock(&self.pinned_states).push(Arc::downgrade(&state));
        *lock(&self.published) = state;
        self.raise_min_pin_from_pins();
    }

    /// Raises the pager's free-page pin horizon to the minimum pinned
    /// catalog generation over live states — computed at publication
    /// boundaries, monotone on the pager side, stale-low-safe
    /// (`docs/FREE_PAGES.md` § The pin horizon).
    ///
    /// Held back entirely on MULTIPROCESS-activated files: a cross-process
    /// follower's pins are invisible to this registry, so reclamation stays
    /// off there — retirement still
    /// records entries; they simply never become eligible.
    fn raise_min_pin_from_pins(&self) {
        if self.pager.superblock().feature_flags
            & devondb_storage::superblock::MULTIPROCESS_COORDINATION_FLAG
            != 0
        {
            return;
        }
        let minimum = {
            let mut pins = lock(&self.pinned_states);
            pins.retain(|weak| weak.strong_count() > 0);
            pins.iter()
                .filter_map(Weak::upgrade)
                .map(|state| state.catalog_generation)
                .min()
        };
        if let Some(minimum) = minimum {
            self.pager.raise_min_pin(minimum);
        }
    }

    pub(super) fn lock_commit_and_gate(
        &self,
    ) -> DevonResult<(MutexGuard<'_, CommitPipe>, PublicationGuard<'_>)> {
        let pipe = lock(&self.commit);
        let gate = self
            .publication_gate
            .as_ref()
            .ok_or_else(|| DevonError::ReadOnly {
                context: "publication gate is unavailable for a read-only database".to_owned(),
            })?;
        // Commit/checkpoint serialization is established before the writer
        // attempts the nonblocking cross-process publication gate.
        let publication = gate.try_exclusive()?;
        Ok((pipe, publication))
    }

    pub(super) fn begin_writer_operation(&self) -> WriterOperation<'_> {
        self.active_writer_operations
            .fetch_add(1, Ordering::Relaxed);
        WriterOperation { shared: self }
    }

    pub(crate) fn deregister_write_txn(&self, txn_id: u64) {
        lock(&self.write_txns).remove(&txn_id);
        self.release_writer_if_unused();
    }

    fn release_writer_if_unused(&self) {
        if self.database_handles.load(Ordering::Acquire) == 0
            && self.active_writer_operations.load(Ordering::Acquire) == 0
            && lock(&self.write_txns).is_empty()
        {
            lock(&self.writer_lease).take();
        }
    }

    /// Returns whether every public database facade sharing this state closed.
    pub(super) fn final_database_handle_closed(&self) -> bool {
        self.database_handles.load(Ordering::Acquire) == 0
    }

    /// Best-effort publication when the final writer facade closes cleanly.
    ///
    /// Drop cannot report an error, and an acknowledged commit is already
    /// durable in the WAL, so a failed close checkpoint deliberately leaves
    /// recovery to the next open. A still-live write transaction may keep its
    /// old catalog pinned across this publication; if it later commits after
    /// every facade closed, the commit path performs the corresponding final
    /// drain before releasing the writer lease.
    fn checkpoint_on_clean_close(self: &Arc<Self>) {
        if self.require_writable("clean-close checkpoint").is_err()
            || self.active_writer_operations.load(Ordering::Acquire) != 0
            || self.current_state().chain.is_none()
        {
            return;
        }
        let _writer_operation = self.begin_writer_operation();
        let _ = checkpoint_shared(self);
    }
}

struct FollowerProgress {
    main_path: PathBuf,
    wal_path: PathBuf,
    last_seen_checkpoint_lsn: u64,
    last_seen_commit_lsn: u64,
    wal_cursor: u64,
}

struct FollowerHandle {
    publication_gate: PublicationGate,
    progress: Mutex<FollowerProgress>,
    current: Mutex<Arc<Shared>>,
    budget: Arc<MemoryBudget>,
    /// The follower's own locked spill directory, carried across context
    /// refreshes so every refreshed `Shared` spills into the same place.
    spill_dir: Arc<SpillDirGuard>,
}

/// Keeps the writer lease alive while a consuming transaction finishes its
/// publication after the final `Database` handle may have been dropped.
pub(super) struct WriterOperation<'a> {
    shared: &'a Shared,
}

impl Drop for WriterOperation<'_> {
    fn drop(&mut self) {
        if self
            .shared
            .active_writer_operations
            .fetch_sub(1, Ordering::AcqRel)
            == 1
        {
            self.shared.release_writer_if_unused();
        }
    }
}

pub(super) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// An open embedded devondb database.
pub struct Database {
    pub(super) shared: Arc<Shared>,
    follower: Option<Arc<FollowerHandle>>,
}

impl Clone for Database {
    fn clone(&self) -> Self {
        self.shared.database_handles.fetch_add(1, Ordering::Relaxed);
        Self {
            shared: Arc::clone(&self.shared),
            follower: self.follower.clone(),
        }
    }
}

impl Drop for Database {
    fn drop(&mut self) {
        if self.shared.database_handles.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.shared.checkpoint_on_clean_close();
            self.shared.release_writer_if_unused();
        }
    }
}

impl Database {
    /// Creates a new database file and its empty WAL sidecar.
    pub fn create(path: impl AsRef<Path>, page_size: u32) -> DevonResult<Self> {
        Self::create_with(
            path,
            Options {
                page_size,
                ..Options::default()
            },
        )
    }

    /// Creates a new database with explicit options.
    pub fn create_with(path: impl AsRef<Path>, options: Options) -> DevonResult<Self> {
        options.validate()?;
        let path = path.as_ref();
        let wal_path = wal_path(path);
        let spill_base = spill_tmp_path(path);
        sweep_stale_spill_dirs(&spill_base)?;
        let spill_dir = Arc::new(acquire_spill_handle_dir(&spill_base)?);
        let budget = Arc::new(MemoryBudget::new(options.memory_limit));
        let pager = pager_with_budget_and_reclaimer(
            Pager::create(path, options.page_size, generate_db_id()?)?,
            &budget,
        );
        let lock_paths = LockPaths::for_main(path)?;
        let writer_lease = Some(WriterLease::try_acquire(&lock_paths)?);
        let publication_gate = Some(PublicationGate::open(&lock_paths)?);
        truncate_wal(&wal_path)?;
        let wal = Some(WalWriter::open(&wal_path, next_lsn(&pager)?)?);
        let catalog = Arc::new(Catalog::default());
        let published = Arc::new(PublishedState {
            catalog,
            chain: None,
            last_commit_lsn: pager.superblock().checkpoint_lsn,
            catalog_generation: pager.superblock().checkpoint_lsn,
            recent_summaries: Vec::new(),
        });
        Ok(Self::from_local_parts(
            pager,
            wal,
            wal_path,
            fs::canonicalize(path)?,
            published,
            budget,
            spill_dir,
            false,
            writer_lease,
            publication_gate,
            false,
            0,
        ))
    }

    /// Opens an existing database and replays complete committed WAL groups.
    pub fn open(path: impl AsRef<Path>) -> DevonResult<Self> {
        Self::open_with(path, Options::default())
    }

    /// TEST-ONLY negative control for the pin-horizon invariant: recomputes
    /// the free-page pin horizon keyed on the SNAPSHOT LSN instead of the
    /// pinned catalog generation. A
    /// snapshot's LSN is always ≥ its base generation, so this un-pins
    /// pages a pinned catalog still names; the test asserts that this breaks
    /// the pin-horizon invariant.
    #[doc(hidden)]
    pub fn debug_raise_min_pin_with_snapshot_lsn_key(&self) {
        let minimum = {
            let mut pins = lock(&self.shared.pinned_states);
            pins.retain(|weak| weak.strong_count() > 0);
            pins.iter()
                .filter_map(Weak::upgrade)
                .map(|state| state.last_commit_lsn)
                .min()
        };
        if let Some(minimum) = minimum {
            self.shared.pager.raise_min_pin(minimum);
        }
    }

    /// Opens an existing database with explicit options.
    pub fn open_with(path: impl AsRef<Path>, options: Options) -> DevonResult<Self> {
        options.validate()?;
        let path = path.as_ref();
        if sniffs_as_pack(path) {
            return open_pack(path, options);
        }
        let wal_path = wal_path(path);
        let spill_base = spill_tmp_path(path);
        sweep_stale_spill_dirs(&spill_base)?;
        let spill_dir = Arc::new(acquire_spill_handle_dir(&spill_base)?);
        let budget = Arc::new(MemoryBudget::new(options.memory_limit));
        let pager = pager_with_budget_and_reclaimer(Pager::open(path)?, &budget);
        let read_only = pager.superblock().requires_read_only();
        let (writer_lease, publication_gate) = if read_only {
            (None, None)
        } else {
            let lock_paths = LockPaths::for_main(path)?;
            (
                Some(WriterLease::try_acquire(&lock_paths)?),
                Some(PublicationGate::open(&lock_paths)?),
            )
        };
        let checkpoint_lsn = pager.superblock().checkpoint_lsn;
        let catalog = Catalog::load(&pager)?;

        // A read-only open takes no writer lease and opens no publication
        // gate. It must not create, truncate, or trim any sidecar: this build
        // does not fully understand the file's feature flags, so its only
        // filesystem footprint is reads (docs/FORMAT.md § Feature flag
        // registry). The writable path may create and trim as before.
        let records = if read_only {
            match replay(&wal_path) {
                Ok(records) => records,
                Err(DevonError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                    Vec::new()
                }
                Err(error) => return Err(error),
            }
        } else {
            drop(WalWriter::open(&wal_path, next_lsn(&pager)?)?);
            replay(&wal_path)?
        };
        let groups = group_transactions(
            records.iter().map(|(lsn, bytes)| (*lsn, bytes)),
            checkpoint_lsn,
        )?;
        let published = recover_published_state(&pager, &budget, catalog, groups, checkpoint_lsn)?;
        let wal = if read_only {
            None
        } else {
            trim_unterminated_tail(&wal_path, &records)?;
            heal_empty_wal_flags(
                &pager,
                &wal_path,
                published.chain.is_some(),
                publication_gate.as_ref(),
            )?;
            Some(WalWriter::open(&wal_path, next_lsn(&pager)?)?)
        };

        Ok(Self::from_local_parts(
            pager,
            wal,
            wal_path,
            fs::canonicalize(path)?,
            published,
            budget,
            spill_dir,
            read_only,
            writer_lease,
            publication_gate,
            false,
            0,
        ))
    }

    /// Opens a stable, side-effect-free inspection snapshot.
    ///
    /// The open creates no WAL, spill, or coordination artifacts and never
    /// trims or heals existing bytes. Complete committed WAL groups newer
    /// than the checkpoint are replayed into memory; an incomplete tail is
    /// ignored according to the WAL recovery law. If an existing writer
    /// lease sidecar is present, inspection takes and retains a shared lock,
    /// so an active writer is refused with [`DevonError::Busy`] and no writer
    /// can start until the inspection handle closes.
    pub fn open_inspect(path: impl AsRef<Path>) -> DevonResult<Self> {
        Self::open_inspect_with(path, Options::default())
    }

    /// Opens a side-effect-free inspection snapshot with explicit options.
    ///
    /// See [`Database::open_inspect`] for WAL and concurrency semantics.
    pub fn open_inspect_with(path: impl AsRef<Path>, options: Options) -> DevonResult<Self> {
        options.validate()?;
        let path = path.as_ref();
        if sniffs_as_pack(path) {
            return open_pack_inspect(path, options);
        }
        let main_path = fs::canonicalize(path)?;
        let inspect_lease = acquire_inspect_lease(&main_path)?;
        let budget = Arc::new(MemoryBudget::new(options.memory_limit));
        let pager = pager_with_budget_and_reclaimer(Pager::open(&main_path)?, &budget);
        let checkpoint_lsn = pager.superblock().checkpoint_lsn;
        let catalog = Catalog::load(&pager)?;
        let wal_path = wal_path(&main_path);
        let replayed = replay_wal_from(&wal_path, 0)?;
        let groups = groups_after(replayed.groups, checkpoint_lsn);
        let published = recover_published_state(&pager, &budget, catalog, groups, checkpoint_lsn)?;
        Ok(Self::from_inspect_parts(
            pager,
            wal_path,
            published,
            budget,
            spill_tmp_path(&main_path),
            inspect_lease,
            0,
        ))
    }

    /// Opens an existing coordinated database as a read-only follower.
    ///
    /// The file must first be activated with [`Database::activate_multiprocess`].
    /// This open takes no writer lease and never waits for the publication gate.
    pub fn open_read_only(path: impl AsRef<Path>) -> DevonResult<Self> {
        Self::open_read_only_with(path, Options::default())
    }

    /// Opens an existing coordinated database as a read-only follower with options.
    ///
    /// Initial recovery runs under one nonblocking shared publication-gate hold.
    /// Gate contention returns [`DevonError::Busy`] so the caller can retry.
    pub fn open_read_only_with(path: impl AsRef<Path>, options: Options) -> DevonResult<Self> {
        options.validate()?;
        // A pack container is read-only by construction: no lease, no gate,
        // and crucially no MULTIPROCESS_COORDINATION requirement.
        if sniffs_as_pack(path.as_ref()) {
            return open_pack(path.as_ref(), options);
        }
        let main_path = fs::canonicalize(path.as_ref())?;
        let lock_paths = LockPaths::for_main(&main_path)?;
        let publication_gate = PublicationGate::open(&lock_paths)?;
        let publication_guard = publication_gate.try_shared()?;
        let budget = Arc::new(MemoryBudget::new(options.memory_limit));
        let pager = pager_with_budget_and_reclaimer(Pager::open(&main_path)?, &budget);
        require_multiprocess_flag(&pager)?;
        let wal_path = wal_path(&main_path);
        let checkpoint_lsn = pager.superblock().checkpoint_lsn;
        let catalog = Catalog::load(&pager)?;
        let replayed = replay_wal_from(&wal_path, 0)?;
        let groups = groups_after(replayed.groups, checkpoint_lsn);
        let published = recover_published_state(&pager, &budget, catalog, groups, checkpoint_lsn)?;
        let follower = FollowerProgress {
            main_path: main_path.clone(),
            wal_path: wal_path.clone(),
            last_seen_checkpoint_lsn: checkpoint_lsn,
            last_seen_commit_lsn: published.last_commit_lsn,
            wal_cursor: replayed.committed_offset,
        };
        // The follower's own locked spill directory: no sweep on this path
        // (read-only opens never wiped shared state), and the held lock is
        // what a writer reopen's sweep respects.
        let spill_dir = Arc::new(acquire_spill_handle_dir(&spill_tmp_path(&main_path))?);
        drop(publication_guard);
        let shared = Self::shared_from_parts(
            pager,
            None,
            wal_path,
            None,
            published,
            Arc::clone(&budget),
            Arc::clone(&spill_dir),
            false,
            None,
            None,
            true,
            0,
        );
        Ok(Self {
            shared: Arc::clone(&shared),
            follower: Some(Arc::new(FollowerHandle {
                publication_gate,
                progress: Mutex::new(follower),
                current: Mutex::new(shared),
                budget,
                spill_dir,
            })),
        })
    }

    #[allow(clippy::too_many_arguments)]
    #[cfg(feature = "pack")]
    fn from_parts(
        pager: Arc<Pager>,
        wal: Option<WalWriter>,
        wal_path: PathBuf,
        published: Arc<PublishedState>,
        budget: Arc<MemoryBudget>,
        spill_dir: Arc<SpillDirGuard>,
        read_only: bool,
        writer_lease: Option<WriterLease>,
        publication_gate: Option<PublicationGate>,
        follower: bool,
        pack_frame_bytes: usize,
    ) -> Self {
        Self {
            shared: Self::shared_from_parts(
                pager,
                wal,
                wal_path,
                None,
                published,
                budget,
                spill_dir,
                read_only,
                writer_lease,
                publication_gate,
                follower,
                pack_frame_bytes,
            ),
            follower: None,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn from_local_parts(
        pager: Arc<Pager>,
        wal: Option<WalWriter>,
        wal_path: PathBuf,
        main_path: PathBuf,
        published: Arc<PublishedState>,
        budget: Arc<MemoryBudget>,
        spill_dir: Arc<SpillDirGuard>,
        read_only: bool,
        writer_lease: Option<WriterLease>,
        publication_gate: Option<PublicationGate>,
        follower: bool,
        pack_frame_bytes: usize,
    ) -> Self {
        Self {
            shared: Self::shared_from_parts(
                pager,
                wal,
                wal_path,
                Some(main_path),
                published,
                budget,
                spill_dir,
                read_only,
                writer_lease,
                publication_gate,
                follower,
                pack_frame_bytes,
            ),
            follower: None,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn shared_from_parts(
        pager: Arc<Pager>,
        wal: Option<WalWriter>,
        wal_path: PathBuf,
        main_path: Option<PathBuf>,
        published: Arc<PublishedState>,
        budget: Arc<MemoryBudget>,
        spill_dir: Arc<SpillDirGuard>,
        read_only: bool,
        writer_lease: Option<WriterLease>,
        publication_gate: Option<PublicationGate>,
        follower: bool,
        pack_frame_bytes: usize,
    ) -> Arc<Shared> {
        let spill_config = SpillConfig {
            budget: Arc::clone(&budget),
            tmp_dir: spill_dir.dir().to_path_buf(),
        };
        let pinned_states = Mutex::new(vec![Arc::downgrade(&published)]);
        let shared = Arc::new(Shared {
            pager,
            budget,
            spill_config,
            _spill_dir: Some(spill_dir),
            _inspect_lease: None,
            published: Mutex::new(published),
            commit: Mutex::new(CommitPipe {
                wal,
                wal_path,
                main_path,
            }),
            writer_lease: Mutex::new(writer_lease),
            publication_gate,
            database_handles: AtomicUsize::new(1),
            active_writer_operations: AtomicUsize::new(0),
            follower,
            write_txns: Mutex::new(BTreeMap::new()),
            pinned_states,
            next_txn_id: AtomicU64::new(1),
            inspect: false,
            read_only,
            pack_frame_bytes,
        });
        // The opened state is itself a pin: the horizon starts at its
        // generation, and the strict `<` eligibility comparison supplies
        // the one-generation superblock-arbitration delay from there.
        shared.raise_min_pin_from_pins();
        shared
    }

    fn from_inspect_parts(
        pager: Arc<Pager>,
        wal_path: PathBuf,
        published: Arc<PublishedState>,
        budget: Arc<MemoryBudget>,
        spill_path: PathBuf,
        inspect_lease: Option<File>,
        pack_frame_bytes: usize,
    ) -> Self {
        let spill_config = SpillConfig {
            budget: Arc::clone(&budget),
            tmp_dir: spill_path,
        };
        let pinned_states = Mutex::new(vec![Arc::downgrade(&published)]);
        let shared = Arc::new(Shared {
            pager,
            budget,
            spill_config,
            _spill_dir: None,
            _inspect_lease: inspect_lease,
            published: Mutex::new(published),
            commit: Mutex::new(CommitPipe {
                wal: None,
                wal_path,
                main_path: None,
            }),
            writer_lease: Mutex::new(None),
            publication_gate: None,
            database_handles: AtomicUsize::new(1),
            active_writer_operations: AtomicUsize::new(0),
            follower: false,
            write_txns: Mutex::new(BTreeMap::new()),
            pinned_states,
            next_txn_id: AtomicU64::new(1),
            inspect: true,
            read_only: true,
            pack_frame_bytes,
        });
        shared.raise_min_pin_from_pins();
        Self {
            shared,
            follower: None,
        }
    }

    /// Returns a pinned derived index over checkpointed rows only.
    /// Overlay visibility must be merged by a query operator.
    #[doc(hidden)]
    pub fn fulltext_index(&self, table: &str, column: &str) -> DevonResult<FullTextResult> {
        let shared = self.current_shared();
        PublishedState::fulltext_index(
            &shared.current_state(),
            &shared.pager,
            &shared.budget,
            table,
            column,
        )
    }

    /// Reports whether the current publication has built this full-text index.
    #[doc(hidden)]
    #[must_use]
    pub fn fulltext_index_present(&self, table: &str, column: &str) -> bool {
        PublishedState::fulltext_index_present(
            &self.current_shared().current_state(),
            table,
            column,
        )
    }

    /// Pins the current committed state with one bounded Arc clone.
    ///
    /// A follower first attempts a nonblocking refresh. A contended gate or
    /// refresh error leaves the previously published immutable state available;
    /// callers can use [`Database::refresh`] when they need the error detail.
    pub fn snapshot(&self) -> Snapshot {
        if self.follower.is_some() {
            let _ = self.refresh();
        }
        let shared = self.current_shared();
        Snapshot::new(Arc::clone(&shared), shared.current_state())
    }

    /// Attempts to advance a follower to the newest published database state.
    ///
    /// Returns `Ok(false)` immediately when the publication gate or another
    /// refresh on this handle is active, or when no newer state exists. Writer
    /// handles also return `Ok(false)` because their published state is current.
    pub fn refresh(&self) -> DevonResult<bool> {
        let Some(follower) = &self.follower else {
            return Ok(false);
        };
        let mut progress = match follower.progress.try_lock() {
            Ok(progress) => progress,
            Err(TryLockError::WouldBlock) => return Ok(false),
            Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
        };
        let _publication_guard = match follower.publication_gate.try_shared() {
            Ok(guard) => guard,
            Err(DevonError::Busy { .. }) => return Ok(false),
            Err(error) => return Err(error),
        };
        self.refresh_follower(follower, &mut progress)
    }

    /// Returns the commit LSN visible to newly pinned snapshots on this handle.
    #[must_use]
    pub fn observed_commit_lsn(&self) -> u64 {
        self.current_shared().current_state().last_commit_lsn
    }

    /// Returns the shared memory accountant for usage diagnostics.
    #[doc(hidden)]
    #[must_use]
    pub fn memory_budget(&self) -> Arc<MemoryBudget> {
        Arc::clone(&self.shared.budget)
    }

    fn current_shared(&self) -> Arc<Shared> {
        self.follower.as_ref().map_or_else(
            || Arc::clone(&self.shared),
            |follower| Arc::clone(&lock(&follower.current)),
        )
    }

    fn refresh_follower(
        &self,
        follower: &FollowerHandle,
        progress: &mut FollowerProgress,
    ) -> DevonResult<bool> {
        let pager = pager_with_budget(Pager::open(&progress.main_path)?, &follower.budget);
        require_multiprocess_flag(&pager)?;
        let checkpoint_lsn = pager.superblock().checkpoint_lsn;
        if checkpoint_lsn < progress.last_seen_checkpoint_lsn {
            return Err(corrupt(format!(
                "follower checkpoint LSN decreased from {} to {checkpoint_lsn}",
                progress.last_seen_checkpoint_lsn
            )));
        }
        let wal_path = progress.wal_path.clone();
        let wal_len = wal_file_len(&wal_path)?;
        if checkpoint_lsn != progress.last_seen_checkpoint_lsn || wal_len < progress.wal_cursor {
            return self.rebase_follower(follower, progress, pager, &wal_path);
        }
        let replayed = replay_wal_from(&wal_path, progress.wal_cursor)?;
        if !cursor_names_expected_lsn(progress, &replayed, wal_len) {
            return self.rebase_follower(follower, progress, pager, &wal_path);
        }
        self.extend_follower(follower, progress, pager, replayed)
    }

    fn extend_follower(
        &self,
        follower: &FollowerHandle,
        progress: &mut FollowerProgress,
        pager: Arc<Pager>,
        replayed: ReplayedWal,
    ) -> DevonResult<bool> {
        if replayed.groups.is_empty() {
            return Ok(false);
        }
        let current_shared = Arc::clone(&lock(&follower.current));
        let current = current_shared.current_state();
        let published =
            extend_published_state(&pager, &follower.budget, &current, replayed.groups)?;
        if published.last_commit_lsn < progress.last_seen_commit_lsn {
            return Err(corrupt("follower commit LSN would decrease during refresh"));
        }
        let last_commit_lsn = published.last_commit_lsn;
        self.publish_follower_context(follower, progress, pager, published);
        progress.wal_cursor = replayed.committed_offset;
        progress.last_seen_commit_lsn = last_commit_lsn;
        Ok(true)
    }

    fn rebase_follower(
        &self,
        follower: &FollowerHandle,
        progress: &mut FollowerProgress,
        pager: Arc<Pager>,
        wal_path: &Path,
    ) -> DevonResult<bool> {
        let checkpoint_lsn = pager.superblock().checkpoint_lsn;
        let catalog = Catalog::load(&pager)?;
        let replayed = replay_wal_from(wal_path, 0)?;
        let groups = groups_after(replayed.groups, checkpoint_lsn);
        let published =
            recover_published_state(&pager, &follower.budget, catalog, groups, checkpoint_lsn)?;
        if published.last_commit_lsn < progress.last_seen_commit_lsn {
            return Err(corrupt(format!(
                "follower commit LSN would decrease from {} to {} during rebase",
                progress.last_seen_commit_lsn, published.last_commit_lsn
            )));
        }
        let advanced = checkpoint_lsn != progress.last_seen_checkpoint_lsn
            || published.last_commit_lsn > progress.last_seen_commit_lsn;
        let last_commit_lsn = published.last_commit_lsn;
        self.publish_follower_context(follower, progress, pager, published);
        progress.last_seen_checkpoint_lsn = checkpoint_lsn;
        progress.last_seen_commit_lsn = last_commit_lsn;
        progress.wal_cursor = replayed.committed_offset;
        Ok(advanced)
    }

    fn publish_follower_context(
        &self,
        follower: &FollowerHandle,
        progress: &FollowerProgress,
        pager: Arc<Pager>,
        published: Arc<PublishedState>,
    ) {
        install_pager_reclaimer(&follower.budget, &pager);
        let shared = Self::shared_from_parts(
            pager,
            None,
            progress.wal_path.clone(),
            None,
            published,
            Arc::clone(&follower.budget),
            Arc::clone(&follower.spill_dir),
            false,
            None,
            None,
            true,
            0,
        );
        *lock(&follower.current) = shared;
    }

    /// Begins an optimistic write transaction and registers its snapshot LSN.
    pub fn begin(&self) -> DevonResult<Transaction> {
        self.shared.require_writable("write transaction")?;
        let txn_id = self
            .shared
            .next_txn_id
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
            .map_err(|_| invalid_argument("transaction id space is exhausted"))?;
        let mut write_txns = lock(&self.shared.write_txns);
        let state = self.shared.current_state();
        write_txns.insert(txn_id, state.last_commit_lsn);
        drop(write_txns);
        Ok(Transaction::new(Arc::clone(&self.shared), txn_id, state))
    }

    /// Executes one statement as its own durable transaction.
    pub fn execute(&mut self, statement: &Statement) -> DevonResult<()> {
        if let Statement::CopyNode {
            table,
            path,
            sort_by,
        } = statement
        {
            // COPY bypasses the WAL under the bulk fence (docs/SCALE.md
            // §5.3); it never runs inside a write transaction.
            return super::copy::execute_copy(self, table, path, sort_by.as_deref());
        }
        // Fold BEFORE beginning, where nothing of ours pins anything, once
        // the OVERLAY holds half the budget — the backstop above §6's
        // end-of-commit trigger. This prevents sequential large statements
        // from accumulating overlay charges until the next write is refused.
        if should_pressure_fold(&self.shared) {
            checkpoint_shared(&self.shared)?;
        }
        let mut transaction = self.begin()?;
        transaction.execute(statement)?;
        transaction.commit()
    }

    /// Runs a plan against a newly pinned committed snapshot.
    pub fn run(&mut self, plan: &Plan) -> DevonResult<QueryResult> {
        self.snapshot().run(plan)
    }

    /// Persists a named canonical plan with its original source text.
    pub fn pin(&mut self, name: &str, text: &str, plan: &Plan) -> DevonResult<()> {
        self.execute(&Statement::PinPlan {
            name: name.to_owned(),
            text: text.to_owned(),
            plan: plan.clone(),
        })
    }

    /// Removes a pinned plan resolved under ASCII folding.
    pub fn unpin(&mut self, name: &str) -> DevonResult<()> {
        self.execute(&Statement::UnpinPlan {
            name: name.to_owned(),
        })
    }

    /// Decodes and runs the exact canonical JSON stored for a named pin.
    pub fn run_pin(&mut self, name: &str) -> DevonResult<QueryResult> {
        if self.follower.is_some() {
            let _ = self.refresh();
        }
        let state = self.current_shared().current_state();
        let folded = fold(name);
        let entry = state
            .catalog
            .pins()
            .iter()
            .find(|pin| fold(&pin.name) == folded)
            .cloned()
            .ok_or_else(|| DevonError::NotFound {
                what: format!(
                    "pin `{name}`{}",
                    suggestion_suffix(
                        name,
                        state.catalog.pins().iter().map(|pin| pin.name.as_str())
                    )
                ),
            })?;
        let stored = entry.plan_json()?;
        let plan = Plan::from_json(&stored)?;
        self.run(&plan)
    }

    /// Drains the committed overlay into node groups and then relationship CSR.
    pub fn checkpoint(&mut self) -> DevonResult<()> {
        self.shared.require_writable("checkpoint")?;
        checkpoint_shared(&self.shared)
    }
}

fn pager_with_budget(pager: Pager, budget: &Arc<MemoryBudget>) -> Arc<Pager> {
    Arc::new(pager.with_budget(Arc::clone(budget)))
}

fn pager_with_budget_and_reclaimer(pager: Pager, budget: &Arc<MemoryBudget>) -> Arc<Pager> {
    let pager = pager_with_budget(pager, budget);
    install_pager_reclaimer(budget, &pager);
    pager
}

fn install_pager_reclaimer(budget: &MemoryBudget, pager: &Arc<Pager>) {
    let weak_pager = Arc::downgrade(pager);
    budget.set_reclaimer(Arc::new(move |bytes| {
        let from_cache = weak_pager
            .upgrade()
            .is_some_and(|pager| pager.shed_cache(bytes));
        // Second rung: also shed the PK resolution and
        // decoded-group caches — the derived-data charge class eviction
        // cannot reach. Both rungs always run: `shed_cache` reports "any
        // frame evicted", not "freed enough", and the caller retries
        // exactly once, so stopping at a partial eviction turns a
        // satisfiable request into a refusal.
        let from_pk = shed_pk_caches();
        let from_fulltext = shed_fulltext_caches();
        from_cache || from_pk || from_fulltext
    }));
}

fn require_multiprocess_flag(pager: &Pager) -> DevonResult<()> {
    if pager.superblock().feature_flags & MULTIPROCESS_COORDINATION_FLAG != 0 {
        return Ok(());
    }
    Err(invalid_argument(
        "open_read_only requires MULTIPROCESS_COORDINATION; call \
         Database::activate_multiprocess(path) before opening a follower",
    ))
}

const WRITER_LOCK_SUFFIX: &str = ".lock-writer";

fn acquire_inspect_lease(main_path: &Path) -> DevonResult<Option<File>> {
    let mut lock_path = OsString::from(main_path.as_os_str());
    lock_path.push(WRITER_LOCK_SUFFIX);
    let lock_path = PathBuf::from(lock_path);
    let file = match File::open(&lock_path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    match file.try_lock_shared() {
        Ok(()) => Ok(Some(file)),
        Err(std::fs::TryLockError::WouldBlock) => Err(DevonError::Busy {
            context: format!(
                "inspection refused because the writer lease is held for {}",
                lock_path.display()
            ),
        }),
        Err(std::fs::TryLockError::Error(error)) => Err(error.into()),
    }
}

fn groups_after(groups: Vec<CommittedGroup>, checkpoint_lsn: u64) -> Vec<CommittedGroup> {
    groups
        .into_iter()
        .filter(|group| group.commit_lsn > checkpoint_lsn)
        .collect()
}

/// The 9-byte DEVONPACK container magic (`docs/SCALE.md` §7.1), duplicated
/// from `devondb_storage::pack::PACK_MAGIC` so a no-`pack`-feature build can
/// still recognize — and honestly refuse — a container instead of reporting
/// superblock corruption.
const PACK_MAGIC: [u8; 9] = *b"DEVONPACK";

/// True when the file's first 9 bytes are the DEVONPACK magic. A shorter or
/// unreadable file is not a pack; the normal open reports its own error.
fn sniffs_as_pack(path: &Path) -> bool {
    let mut magic = [0_u8; 9];
    File::open(path)
        .and_then(|mut file| std::io::Read::read_exact(&mut file, &mut magic))
        .is_ok_and(|()| magic == PACK_MAGIC)
}

/// Routes a DEVONPACK container to the pack open, or refuses honestly when
/// this build lacks the `pack` feature (`docs/SCALE.md` §7.2).
#[cfg(feature = "pack")]
fn open_pack(path: &Path, options: Options) -> DevonResult<Database> {
    pack::open(path, options)
}

#[cfg(feature = "pack")]
fn open_pack_inspect(path: &Path, options: Options) -> DevonResult<Database> {
    pack::open_inspect(path, options)
}

/// The no-feature refusal: a pack is never a superblock-corruption error.
#[cfg(not(feature = "pack"))]
fn open_pack(_path: &Path, _options: Options) -> DevonResult<Database> {
    Err(invalid_argument("pack files require the `pack` feature"))
}

#[cfg(not(feature = "pack"))]
fn open_pack_inspect(_path: &Path, _options: Options) -> DevonResult<Database> {
    Err(invalid_argument("pack files require the `pack` feature"))
}

fn wal_file_len(path: &Path) -> DevonResult<u64> {
    match fs::metadata(path) {
        Ok(metadata) => Ok(metadata.len()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(0),
        Err(error) => Err(error.into()),
    }
}

fn cursor_names_expected_lsn(
    progress: &FollowerProgress,
    replayed: &ReplayedWal,
    wal_len: u64,
) -> bool {
    match replayed.first_lsn {
        Some(first_lsn) => progress.last_seen_commit_lsn.checked_add(1) == Some(first_lsn),
        None => replayed.intact_offset == wal_len,
    }
}
