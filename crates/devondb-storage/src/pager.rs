//! Pager: database file create/open, dual-superblock protocol, page I/O.
//!
//! File layout per `docs/FORMAT.md` § Main file layout (binding).
//! Interior mutability allows one `Pager` to be shared by concurrent readers
//! behind `&self`, with a budgeted page cache behind the same API
//! (`docs/MVCC.md` §10).
//!
//! # Concurrency model
//!
//! - `read_page_ref` takes the frame mutex for one lookup. On a miss it may
//!   keep that mutex across exactly one positional page read before inserting
//!   the frame, preventing duplicate concurrent fills. The uncharged fallback
//!   reads without the frame mutex. No read takes the writer or state mutex.
//! - The `state` mutex guards the authoritative superblock copy and slot.
//!   It is held only for O(1) clone/flip — never across any file I/O or
//!   `sync_all` (`docs/MVCC.md` §4.3 rule 2, §7.3 rule 5).
//! - The `writer` mutex serializes the write side end-to-end: page
//!   allocation (the allocation cursor is the file length, so the length
//!   read and the zeroed-page append must be atomic w.r.t. other
//!   writers), page writes (a page write past EOF extends the file, which
//!   would silently move the allocation cursor under a concurrent
//!   `allocate_page`), and the dual-slot superblock commit. Readers never
//!   take it, so holding it across `commit_superblock`'s fsync never
//!   blocks a reader.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, Mutex, MutexGuard, PoisonError,
    atomic::{AtomicBool, AtomicU64, Ordering},
};

use devondb_types::{DevonError, DevonResult};

use crate::backend::{Backend, LocalFileBackend, MemoryBackend, PagerBackend};
use crate::budget::MemoryBudget;
use crate::free_pages::{EXTENSION_END, LedgerEntry, LedgerPage, SuperblockExtension};
use crate::superblock::{
    FREE_PAGES_FLAG, SUPPORTED_FORMAT_VERSION, Superblock, choose_authoritative,
};

const SUPERBLOCK_HEADER_LEN: usize = 64;
const SUPERBLOCK_SLOTS: u64 = 2;
const VALID_PAGE_SIZES: [u32; 5] = [4096, 8192, 16_384, 32_768, 65_536];
const FRAME_OVERHEAD: usize = 64;
const MIN_CACHE_FRAMES: usize = 8;
// Incremental checkpoint compaction is deliberately small: at most sixteen
// ledger pages are inspected and at most sixty-four database pages are
// returned to the filesystem per publication. The 25% trigger avoids extra
// metadata writes for ordinary one-generation CoW slack.
const COMPACTION_FREE_PERCENT: usize = 25;
const COMPACTION_LEDGER_PAGE_LIMIT: usize = 16;
const COMPACTION_ENTRY_LIMIT: usize = 4_096;
const COMPACTION_TRUNCATE_PAGE_LIMIT: usize = 64;
/// Eligible ledger entries a run allocation may examine beyond the run
/// itself before falling back to append. Sixteen 4 KiB ledger
/// pages — the compaction walk's own bound — so one allocation never reads
/// more ledger than a checkpoint's compaction scan does.
const RUN_SCAN_SKIP_LIMIT: usize = 4_096;

/// An immutable page buffer. Holding a `PageRef` pins its cache frame.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PageRef(Arc<[u8]>);

impl PageRef {
    fn from_vec(page: Vec<u8>) -> Self {
        Self(Arc::from(page))
    }
}

impl Deref for PageRef {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

struct PageCache {
    frames: Mutex<FrameTable>,
    budget: Arc<MemoryBudget>,
    frame_charge: usize,
}

impl PageCache {
    fn new(page_size: u32, budget: Arc<MemoryBudget>) -> Self {
        Self {
            frames: Mutex::new(FrameTable::default()),
            budget,
            frame_charge: page_size as usize + FRAME_OVERHEAD,
        }
    }

    fn lock_frames(&self) -> MutexGuard<'_, FrameTable> {
        self.frames.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn charge_after_reclaim(&self, frames: &mut FrameTable) -> bool {
        if self.budget.try_charge(self.frame_charge) {
            return true;
        }
        while frames.evict_one() {
            self.budget.release(self.frame_charge);
            if self.budget.try_charge(self.frame_charge) {
                return true;
            }
        }
        false
    }

    fn invalidate(&self, page_id: u64) {
        if self.lock_frames().remove(page_id) {
            self.budget.release(self.frame_charge);
        }
    }

    /// Ladder step 1 for non-cache callers (`docs/MVCC.md` §7.2): evicts
    /// clean unpinned frames until `bytes` have been released back to the
    /// budget or nothing evictable remains. Returns whether any frame was
    /// evicted.
    fn shed(&self, bytes: usize) -> bool {
        let mut frames = self.lock_frames();
        let mut released = 0_usize;
        let mut any = false;
        while released < bytes && frames.evict_one() {
            self.budget.release(self.frame_charge);
            released = released.saturating_add(self.frame_charge);
            any = true;
        }
        any
    }
}

impl Drop for PageCache {
    fn drop(&mut self) {
        let frame_count = self
            .frames
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner)
            .frames
            .len();
        self.budget
            .release(frame_count.saturating_mul(self.frame_charge));
    }
}

#[derive(Default)]
struct FrameTable {
    frames: HashMap<u64, Frame>,
    clock: Vec<u64>,
    hand: usize,
}

struct Frame {
    data: PageRef,
    referenced: bool,
}

impl FrameTable {
    fn get(&mut self, page_id: u64) -> Option<PageRef> {
        self.frames.get_mut(&page_id).map(|frame| {
            frame.referenced = true;
            frame.data.clone()
        })
    }

    fn insert(&mut self, page_id: u64, data: PageRef) {
        self.frames.insert(
            page_id,
            Frame {
                data,
                referenced: true,
            },
        );
        self.clock.push(page_id);
    }

    fn remove(&mut self, page_id: u64) -> bool {
        if self.frames.remove(&page_id).is_none() {
            return false;
        }
        if let Some(position) = self
            .clock
            .iter()
            .position(|candidate| *candidate == page_id)
        {
            self.clock.remove(position);
            self.adjust_hand_after_remove(position);
        }
        true
    }

    fn evict_one(&mut self) -> bool {
        if self.frames.len() < MIN_CACHE_FRAMES {
            return false;
        }
        let visits = self.clock.len().saturating_mul(2);
        for _ in 0..visits {
            let page_id = self.clock[self.hand];
            let Some(frame) = self.frames.get_mut(&page_id) else {
                let position = self.hand;
                self.clock.remove(position);
                self.adjust_hand_after_remove(position);
                continue;
            };
            if frame.referenced {
                frame.referenced = false;
                self.advance_hand();
            } else if Arc::strong_count(&frame.data.0) == 1 {
                return self.remove(page_id);
            } else {
                self.advance_hand();
            }
        }
        false
    }

    fn advance_hand(&mut self) {
        self.hand = (self.hand + 1) % self.clock.len();
    }

    fn adjust_hand_after_remove(&mut self, position: usize) {
        if self.clock.is_empty() {
            self.hand = 0;
        } else if position < self.hand {
            self.hand -= 1;
        } else if self.hand == self.clock.len() {
            self.hand = 0;
        }
    }
}

/// Owns an open database backend and its currently authoritative superblock.
///
/// All methods take `&self`; see the module docs for the locking rules.
/// Every length, positional read, positional write, and sync goes through
/// the closed `Backend` enum (`docs/OBJECT_STORAGE.md` § The pager-backend
/// seam); creation and directory sync remain factory code outside the trait.
pub struct Pager {
    backend: Backend,
    /// Canonical byte backend operations intentionally have no truncate
    /// method. Local pagers retain their path solely for recoverable
    /// `set_len`; memory and pack backends leave it absent.
    path: Option<PathBuf>,
    /// Immutable for the life of the file: `ensure_commit_is_valid`
    /// rejects any committed superblock that changes the page size, so
    /// reads and writes use it without touching the `state` mutex.
    page_size: u32,
    /// Write-side serialization; see the module docs. Never taken by readers.
    writer: Mutex<()>,
    /// Authoritative superblock copy and slot; O(1) holds only.
    state: Mutex<PagerState>,
    /// Present only when the facade attaches the shared memory budget.
    cache: Option<PageCache>,
    /// Diagnostic count of data-page read requests, including cache hits.
    /// Atomic bookkeeping keeps concurrent snapshot reads lock-free.
    page_reads: AtomicU64,
    /// Set after this pager writes a stats-bearing directory. Catalog save
    /// consumes the fact by publishing the sticky feature bit; an atomic keeps
    /// concurrent checkpoint bookkeeping independent of the read locks.
    wrote_zone_maps: AtomicBool,
    /// Set after the adaptive writer persists a directory carrying the
    /// `COLUMN_ENCODINGS` section. Validated publication-time reads admit
    /// that in-flight directory until catalog save publishes feature bit 13.
    wrote_column_encodings: AtomicBool,
    /// Free-page retirement bookkeeping (`docs/FREE_PAGES.md`).
    /// Lock ordering: writer → free → frames; every `free` acquisition
    /// happens while the writer mutex is held, and no I/O runs under it.
    free: Mutex<FreeState>,
    /// The pin horizon: the oldest catalog generation any live snapshot,
    /// recovery-reachable superblock, or follower may still read. Ledger
    /// entries are reclaimable only when `retired_lsn < min_pin`. Raised
    /// at publication boundaries by the facade, monotone by `fetch_max` —
    /// a stale-low value is conservative (leaks, never corrupts).
    min_pin: AtomicU64,
}

struct PagerState {
    superblock: Superblock,
    authoritative_slot: u8,
}

/// Session-side free-page state. The durable source of truth is the
/// superblock extension plus the ledger pages; this mirror exists so slot
/// writes can compose the extension without re-reading it.
#[derive(Default)]
struct FreeState {
    /// The extension as of the last successful superblock write. `None`
    /// when the file carries no ledger — bit clear, or degraded below.
    extension: Option<SuperblockExtension>,
    /// The extension CRC failed at open: the old chain is abandoned
    /// (leaked, recoverable by the offline sweep) and the next
    /// publication starts a fresh one. Never `Corrupt`, never a slot
    /// flip (`docs/FREE_PAGES.md` § On-disk layout).
    degraded: bool,
    /// Staged by [`Pager::retire_pages`], written into the next
    /// `commit_superblock` slot page, and promoted to `extension` when
    /// that commit succeeds. Never exposed by `commit_feature_flags`:
    /// a flags-only publication must not make an unflipped publication's
    /// retirement entries reachable.
    pending_extension: Option<SuperblockExtension>,
    /// Ledger pages whose consumed prefix advanced since the last durable
    /// flush: ledger page id → new absolute `consumed_count`. Flushed and
    /// synced by `retire_pages` BEFORE the superblock flip that publishes
    /// the catalog referencing the reused pages, following the consumption
    /// durability law.
    consumed_in_session: BTreeMap<u64, u32>,
    /// Pages handed out by ledger reuse during this pager session.
    reused_in_session: u64,
    /// Reclaimable tail pages physically removed this session.
    truncated_in_session: u64,
    /// Eligible prefix entries skipped while finding a later contiguous run.
    /// They remain free and serve later allocations in this publication; any
    /// leftovers are republished by `retire_pages` before the catalog flip.
    deferred_reuse: BTreeSet<u64>,
    /// Deferred ids included in the staged extension. Removed from the
    /// session pool only after the publishing superblock flip succeeds.
    pending_deferred_reuse: Vec<u64>,
    /// `retired_total` of the last abandoned chain: keeps the cumulative
    /// counter strictly monotone across a degrade, where the extension
    /// mirror is `None` but history already happened.
    retired_total_floor: u64,
}

struct ScannedReuseEntry {
    ledger_page: u64,
    consumed_after: u32,
    entry: LedgerEntry,
}

struct ReuseScan {
    entries: Vec<ScannedReuseEntry>,
    /// Ledger-order indices of the selected run; `entries` is truncated to
    /// the consumed prefix (through the run's last ledger position).
    selected: Option<BTreeSet<usize>>,
    ledger_pages: HashSet<u64>,
}

/// Test-only view of the free-page mirror for state-transition pins that
/// need a fault the storage harness cannot inject (a failed flip sync).
#[derive(Default)]
pub struct FreeStateForTest {
    /// See [`FreeState::retired_total_floor`].
    pub retired_total_floor: u64,
    /// See [`FreeState::pending_extension`].
    pub pending_extension: Option<SuperblockExtension>,
}

/// Test-only failed-flip floor transition: the slot MAY be durable, so the
/// session floor rises to the staged total before the error is returned.
pub fn apply_failed_flip_floor(state: &mut FreeStateForTest) {
    if let Some(pending) = state.pending_extension.take() {
        state.retired_total_floor = state.retired_total_floor.max(pending.retired_total);
    }
}

/// Generates a random 128-bit database identifier.
pub fn generate_db_id() -> DevonResult<[u8; 16]> {
    let mut db_id = [0_u8; 16];
    getrandom::fill(&mut db_id).map_err(|error| {
        std::io::Error::other(format!("failed to generate database identifier: {error}"))
    })?;
    Ok(db_id)
}

impl Pager {
    /// Creates a new database at `path` and durably initializes both slots.
    pub fn create(path: impl AsRef<Path>, page_size: u32, db_id: [u8; 16]) -> DevonResult<Self> {
        let path = path.as_ref();
        let superblock = initial_superblock(page_size, db_id);
        let page = encode_page(&superblock)?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(path)?;
        let backend = Backend::Local(LocalFileBackend::new(file));
        backend.write_all_at(0, &page)?;
        backend.write_all_at(u64::from(page_size), &page)?;
        backend.sync_all()?;
        sync_parent_directory(path)?;

        Ok(Self::new(
            backend,
            Some(path.to_path_buf()),
            superblock,
            0,
            FreeState::default(),
        ))
    }

    /// Test-only counterpart of [`Pager::create`] over a [`MemoryBackend`]:
    /// identical byte operations, no path and no parent-directory sync.
    #[doc(hidden)]
    pub fn create_memory_for_test(
        backend: MemoryBackend,
        page_size: u32,
        db_id: [u8; 16],
    ) -> DevonResult<Self> {
        let superblock = initial_superblock(page_size, db_id);
        let page = encode_page(&superblock)?;
        let backend = Backend::Memory(backend);
        backend.write_all_at(0, &page)?;
        backend.write_all_at(u64::from(page_size), &page)?;
        backend.sync_all()?;

        Ok(Self::new(
            backend,
            None,
            superblock,
            0,
            FreeState::default(),
        ))
    }

    /// Opens an existing database using the valid slot with the highest LSN.
    pub fn open(path: impl AsRef<Path>) -> DevonResult<Self> {
        let path = path.as_ref();
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        Self::open_backend_with_path(
            Backend::Local(LocalFileBackend::new(file)),
            Some(path.to_path_buf()),
        )
    }

    /// Test-only counterpart of [`Pager::open`] over a [`MemoryBackend`].
    #[doc(hidden)]
    pub fn open_memory_for_test(backend: MemoryBackend) -> DevonResult<Self> {
        Self::open_backend(Backend::Memory(backend))
    }

    /// The backend-agnostic open sequence: read and arbitrate both
    /// superblock slots, validate the length, load the free-page state.
    ///
    /// `pub(crate)` for the `pack` feature's container open
    /// (`docs/SCALE.md` §7.2): the sequence is identical for every backend.
    pub(crate) fn open_backend(backend: Backend) -> DevonResult<Self> {
        Self::open_backend_with_path(backend, None)
    }

    fn open_backend_with_path(backend: Backend, path: Option<PathBuf>) -> DevonResult<Self> {
        let slot_zero = read_slot(&backend, 0, None);
        let page_size = slot_zero.as_ref().ok().map(|slot| slot.page_size);
        let slot_one = read_second_slot(&backend, page_size);
        let authoritative_slot = selected_slot(&slot_zero, &slot_one);
        let superblock = choose_authoritative(slot_zero, slot_one)?;
        validate_file_length(backend.len()?, superblock.page_size)?;
        let free = read_free_state(&backend, &superblock, authoritative_slot)?;

        Ok(Self::new(
            backend,
            path,
            superblock,
            authoritative_slot,
            free,
        ))
    }

    fn new(
        backend: Backend,
        path: Option<PathBuf>,
        superblock: Superblock,
        authoritative_slot: u8,
        free: FreeState,
    ) -> Self {
        Self {
            page_size: superblock.page_size,
            backend,
            path,
            writer: Mutex::new(()),
            state: Mutex::new(PagerState {
                superblock,
                authoritative_slot,
            }),
            cache: None,
            page_reads: AtomicU64::new(0),
            wrote_zone_maps: AtomicBool::new(false),
            wrote_column_encodings: AtomicBool::new(false),
            free: Mutex::new(free),
            min_pin: AtomicU64::new(0),
        }
    }

    /// Attaches the shared memory budget and activates the page cache.
    #[must_use]
    pub fn with_budget(mut self, budget: Arc<MemoryBudget>) -> Self {
        self.cache = Some(PageCache::new(self.page_size, budget));
        self
    }

    /// Reads one allocated data page, copying bytes out of a pinned frame.
    pub fn read_page(&self, page_id: u64) -> DevonResult<Vec<u8>> {
        Ok(self.read_page_ref(page_id)?.to_vec())
    }

    /// Reads one allocated data page and pins its cache frame for the result's lifetime.
    ///
    /// When the shared budget cannot accept a frame after clean unpinned
    /// eviction, this returns an uncharged, uncached one-shot page instead.
    pub fn read_page_ref(&self, page_id: u64) -> DevonResult<PageRef> {
        let offset = self.readable_page_offset(page_id)?;
        self.page_reads.fetch_add(1, Ordering::Relaxed);
        let Some(cache) = &self.cache else {
            return self.read_page_at(offset).map(PageRef::from_vec);
        };
        let mut frames = cache.lock_frames();
        if let Some(page) = frames.get(page_id) {
            return Ok(page);
        }
        if !cache.charge_after_reclaim(&mut frames) {
            drop(frames);
            return self.read_page_at(offset).map(PageRef::from_vec);
        }
        let page = match self.read_page_at(offset) {
            Ok(page) => PageRef::from_vec(page),
            Err(error) => {
                cache.budget.release(cache.frame_charge);
                return Err(error);
            }
        };
        frames.insert(page_id, page.clone());
        Ok(page)
    }

    fn readable_page_offset(&self, page_id: u64) -> DevonResult<u64> {
        ensure_data_page(page_id)?;
        let offset = page_offset(page_id, self.page_size)?;
        let end = offset
            .checked_add(u64::from(self.page_size))
            .ok_or_else(|| invalid_argument("page end offset overflows u64"))?;
        if end > self.backend.len()? {
            return Err(invalid_argument(
                "page is beyond the end of the database file",
            ));
        }
        Ok(offset)
    }

    fn read_page_at(&self, offset: u64) -> DevonResult<Vec<u8>> {
        let mut page = vec![0_u8; self.page_size as usize];
        self.backend.read_exact_at(offset, &mut page)?;
        Ok(page)
    }

    /// Writes exactly one data page, extending the file when necessary.
    pub fn write_page(&self, page_id: u64, data: &[u8]) -> DevonResult<()> {
        ensure_data_page(page_id)?;
        if data.len() != self.page_size as usize {
            return Err(invalid_argument(
                "page data length must equal the database page size",
            ));
        }

        // One page I/O under the writer lock (permitted by docs/MVCC.md
        // §7.3 rule 5): a write past EOF extends the file, and the file
        // length is the allocation cursor.
        let _writer = self.lock_writer();
        self.write_page_locked(page_id, data)
    }

    /// The write-page body for callers already holding the writer lock.
    /// Write-through: the cached frame is removed, which is the invariant
    /// that keeps in-place ledger rewrites coherent (`docs/MVCC.md` §7.3
    /// rule 3); callers must not bypass this path.
    fn write_page_locked(&self, page_id: u64, data: &[u8]) -> DevonResult<()> {
        let offset = page_offset(page_id, self.page_size)?;
        self.backend.write_all_at(offset, data)?;
        if let Some(cache) = &self.cache {
            cache.invalidate(page_id);
        }
        Ok(())
    }

    /// Flushes all database pages and file metadata to durable storage.
    ///
    /// Takes no lock: every write it must make durable completed before
    /// the caller invoked it, and holding a lock across `sync_all` is
    /// forbidden (`docs/MVCC.md` §4.3 rule 2).
    pub fn sync(&self) -> DevonResult<()> {
        self.backend.sync_all()?;
        Ok(())
    }

    /// Evicts clean unpinned cache frames until `bytes` have been released
    /// back to the shared budget or nothing evictable remains — ladder
    /// step 1 of `docs/MVCC.md` §7.2 for callers whose charges compete
    /// with the page cache (write sets, commit links, summaries). Returns
    /// whether any frame was evicted. A no-op when no cache is attached.
    pub fn shed_cache(&self, bytes: usize) -> bool {
        self.cache.as_ref().is_some_and(|cache| cache.shed(bytes))
    }

    /// Allocates one zeroed page and returns its page identifier —
    /// ledger reuse first, file append as the degraded mode
    /// (`docs/FREE_PAGES.md` § Recommendation).
    pub fn allocate_page(&self) -> DevonResult<u64> {
        self.allocate_run(1)
    }

    /// Allocates `count` zeroed pages with CONTIGUOUS ascending ids and
    /// returns the first id. Multi-page column payloads are addressed as
    /// `first_page + byte_len` runs on disk, so contiguity is a format
    /// obligation, not an optimization.
    ///
    /// Reuse coalesces the reclaimable ledger prefix (`retired_lsn <
    /// min_pin`, at most `count + RUN_SCAN_SKIP_LIMIT` entries) by page
    /// address and serves the physically contiguous run of `count` pages
    /// that ends earliest in ledger order — a retired generation is one
    /// address run whose ledger neighbours are single pages, so ledger-order
    /// run detection would otherwise miss it and append. Eligible entries
    /// skipped ahead of the selection remain available to later allocations
    /// and are republished if still unused at the checkpoint flip. No fit
    /// falls through to one atomic append of the whole run.
    pub fn allocate_run(&self, count: usize) -> DevonResult<u64> {
        if count == 0 {
            return Err(invalid_argument("cannot allocate a run of zero pages"));
        }
        // The file length is the allocation cursor: the length read and
        // the zeroed append must be atomic w.r.t. other writers, and the
        // ledger pop must be atomic w.r.t. other allocators. Page I/O
        // under the writer lock, no fsync.
        let _writer = self.lock_writer();
        if let Some(first) = self.try_reuse_run_locked(count)? {
            return Ok(first);
        }
        self.append_run_locked(count)
    }

    /// Serves a contiguous run from the reclaimable ledger prefix, or
    /// `None` when the prefix cannot serve it. Caller holds the writer lock.
    fn try_reuse_run_locked(&self, count: usize) -> DevonResult<Option<u64>> {
        if let Some(first) = self.try_deferred_run_locked(count)? {
            return Ok(Some(first));
        }
        let min_pin = self.min_pin.load(Ordering::Acquire);
        if min_pin == 0 {
            return Ok(None);
        }
        let (extension, skip_capacity) = {
            let free = self.lock_free();
            (
                free.extension,
                LedgerPage::capacity(self.page_size as usize)
                    .saturating_sub(free.deferred_reuse.len()),
            )
        };
        let Some(extension) = extension else {
            return Ok(None);
        };
        if !extension.has_ledger() {
            return Ok(None);
        }

        let Some(scan) =
            self.scan_reclaimable_run_locked(extension, min_pin, count, skip_capacity)?
        else {
            return Ok(None);
        };
        self.stage_reuse_scan_locked(scan)
    }

    /// Serves a run from eligible entries skipped by an earlier allocation.
    /// Caller holds the writer lock; no I/O runs while the free lock is held.
    fn try_deferred_run_locked(&self, count: usize) -> DevonResult<Option<u64>> {
        let candidates = {
            let free = self.lock_free();
            find_contiguous_run(&free.deferred_reuse, count)
        };
        let Some(candidates) = candidates else {
            return Ok(None);
        };
        if candidates
            .iter()
            .any(|page_id| !self.is_reusable_page_id(*page_id))
        {
            self.degrade_free_pages_locked();
            return Ok(None);
        }
        self.zero_reused_pages_locked(&candidates)?;
        let mut free = self.lock_free();
        for page_id in &candidates {
            free.deferred_reuse.remove(page_id);
        }
        free.reused_in_session += candidates.len() as u64;
        Ok(candidates.first().copied())
    }

    /// Collects the eligible ledger prefix and selects the physically
    /// contiguous run of `count` pages that ends earliest in ledger order
    /// (see [`Pager::allocate_run`]). Caller holds the writer lock.
    fn scan_reclaimable_run_locked(
        &self,
        extension: SuperblockExtension,
        min_pin: u64,
        count: usize,
        skip_capacity: usize,
    ) -> DevonResult<Option<ReuseScan>> {
        let mut scan = ReuseScan {
            entries: Vec::new(),
            selected: None,
            ledger_pages: HashSet::new(),
        };
        let entry_limit = count.saturating_add(RUN_SCAN_SKIP_LIMIT);
        let mut cursor = extension.retire_ledger_head;
        loop {
            let Some(decoded) = self.scan_ledger_page_locked(cursor, &mut scan)? else {
                return Ok(None);
            };
            let consumed = self.effective_consumed(cursor, &decoded) as usize;
            let mut exhausted = false;
            for (index, entry) in decoded.entries.iter().copied().enumerate().skip(consumed) {
                if entry.retired_lsn >= min_pin || scan.entries.len() >= entry_limit {
                    exhausted = true;
                    break;
                }
                scan.entries.push(ScannedReuseEntry {
                    ledger_page: cursor,
                    consumed_after: u32::try_from(index + 1).unwrap_or(u32::MAX),
                    entry,
                });
            }
            if scan.entries.len() >= count {
                select_earliest_run(&mut scan, count);
            }
            if scan.selected.is_some() {
                return Ok(Some(scan));
            }
            if exhausted || cursor == extension.retire_ledger_tail || decoded.next_page == 0 {
                trim_unselected_scan(&mut scan, skip_capacity);
                return Ok(Some(scan));
            }
            cursor = decoded.next_page;
        }
    }

    /// Reads one ledger page while enforcing the bounded, acyclic walk law.
    fn scan_ledger_page_locked(
        &self,
        cursor: u64,
        scan: &mut ReuseScan,
    ) -> DevonResult<Option<LedgerPage>> {
        if !scan.ledger_pages.insert(cursor) || scan.ledger_pages.len() > self.page_count() {
            self.degrade_free_pages_locked();
            return Ok(None);
        }
        match self.read_ledger_page_locked(cursor) {
            Ok(decoded) => Ok(Some(decoded)),
            Err(_) => {
                self.degrade_free_pages_locked();
                Ok(None)
            }
        }
    }

    /// Consumes the scanned prefix through the selected run's last ledger
    /// position, handing out the run and retaining every other consumed
    /// entry for later allocations in this publication.
    fn stage_reuse_scan_locked(&self, scan: ReuseScan) -> DevonResult<Option<u64>> {
        if scan.entries.iter().any(|candidate| {
            scan.ledger_pages.contains(&candidate.entry.page_id)
                || !self.is_reusable_page_id(candidate.entry.page_id)
        }) {
            self.degrade_free_pages_locked();
            return Ok(None);
        }
        let selected = scan.selected.clone().unwrap_or_default();
        let selected_ids: BTreeSet<u64> = selected
            .iter()
            .filter_map(|index| scan.entries.get(*index))
            .map(|candidate| candidate.entry.page_id)
            .collect();
        let run: Vec<u64> = selected_ids.iter().copied().collect();
        self.zero_reused_pages_locked(&run)?;

        let mut free = self.lock_free();
        for candidate in &scan.entries {
            free.consumed_in_session
                .entry(candidate.ledger_page)
                .and_modify(|consumed| *consumed = (*consumed).max(candidate.consumed_after))
                .or_insert(candidate.consumed_after);
            if !selected_ids.contains(&candidate.entry.page_id) {
                free.deferred_reuse.insert(candidate.entry.page_id);
            }
        }
        free.reused_in_session += run.len() as u64;
        Ok(run.first().copied())
    }

    /// Zeroes validated reused pages before handing them to the caller.
    fn zero_reused_pages_locked(&self, page_ids: &[u64]) -> DevonResult<()> {
        let zeroes = vec![0_u8; self.page_size as usize];
        for page_id in page_ids {
            self.write_page_locked(*page_id, &zeroes)?;
        }
        Ok(())
    }

    /// Appends `count` zeroed pages at the file tail. Caller holds the
    /// writer lock, which is what makes the run contiguous.
    fn append_run_locked(&self, count: usize) -> DevonResult<u64> {
        let page_size = u64::from(self.page_size);
        let file_len = self.backend.len()?;
        if file_len % page_size != 0 {
            return Err(corrupt("database file length is not page-aligned"));
        }

        let page_id = file_len / page_size;
        if page_id < SUPERBLOCK_SLOTS {
            return Err(corrupt("database file is missing a superblock page"));
        }

        let page = vec![0_u8; self.page_size as usize];
        for index in 0..count {
            let offset = file_len
                .checked_add(
                    page_size
                        .checked_mul(index as u64)
                        .ok_or_else(|| invalid_argument("page run length overflows u64"))?,
                )
                .ok_or_else(|| invalid_argument("page run end overflows u64"))?;
            self.backend.write_all_at(offset, &page)?;
        }
        Ok(page_id)
    }

    /// The number of pages the current file can hold. Caller holds the
    /// writer lock when used as a ledger-walk bound.
    fn page_count(&self) -> usize {
        let length = self.backend.len().unwrap_or(0);
        usize::try_from(length / u64::from(self.page_size)).unwrap_or(usize::MAX)
    }

    /// Whether a ledger entry may be handed out or rewritten: never a
    /// superblock page and never past EOF (a bogus id must not extend the
    /// allocation cursor). Caller holds the writer lock.
    fn is_reusable_page_id(&self, page_id: u64) -> bool {
        ensure_data_page(page_id).is_ok()
            && page_id < u64::try_from(self.page_count()).unwrap_or(u64::MAX)
    }

    /// Reads and decodes one ledger page. Caller holds the writer lock;
    /// the read itself goes through the standard cached path.
    fn read_ledger_page_locked(&self, page_id: u64) -> DevonResult<LedgerPage> {
        LedgerPage::decode(&self.read_page_ref(page_id)?)
    }

    /// A ledger page's consumed count as this session sees it: the durable
    /// count, overridden by any unflushed in-session advance.
    fn effective_consumed(&self, page_id: u64, decoded: &LedgerPage) -> u32 {
        self.lock_free()
            .consumed_in_session
            .get(&page_id)
            .copied()
            .unwrap_or(decoded.consumed_count)
            .max(decoded.consumed_count)
    }

    /// Returns reclaimable prefix entries within the incremental scan bound.
    /// Caller holds the writer lock.
    fn compaction_scan_locked(
        &self,
        extension: SuperblockExtension,
        min_pin: u64,
    ) -> DevonResult<Option<ReuseScan>> {
        let mut scan = ReuseScan {
            entries: Vec::new(),
            selected: None,
            ledger_pages: HashSet::new(),
        };
        let mut cursor = extension.retire_ledger_head;
        loop {
            let Some(decoded) = self.scan_ledger_page_locked(cursor, &mut scan)? else {
                return Ok(None);
            };
            let consumed = self.effective_consumed(cursor, &decoded) as usize;
            for (index, entry) in decoded.entries.iter().copied().enumerate().skip(consumed) {
                if entry.retired_lsn >= min_pin || scan.entries.len() == COMPACTION_ENTRY_LIMIT {
                    return Ok(Some(scan));
                }
                scan.entries.push(ScannedReuseEntry {
                    ledger_page: cursor,
                    consumed_after: u32::try_from(index + 1).unwrap_or(u32::MAX),
                    entry,
                });
            }
            if cursor == extension.retire_ledger_tail
                || decoded.next_page == 0
                || scan.ledger_pages.len() == COMPACTION_LEDGER_PAGE_LIMIT
            {
                return Ok(Some(scan));
            }
            cursor = decoded.next_page;
        }
    }

    /// Truncates a reclaimable physical suffix before this checkpoint adds
    /// its new retirements. No live page moves: any live tail page, including
    /// a ledger page, stops the suffix. Caller holds the writer lock.
    fn maybe_compact_free_tail_locked(&self) -> DevonResult<()> {
        if self.path.is_none() {
            return Ok(());
        }
        let min_pin = self.min_pin.load(Ordering::Acquire);
        let extension = self.lock_free().extension;
        let Some(extension) = extension.filter(|value| min_pin != 0 && value.has_ledger()) else {
            return Ok(());
        };
        let Some(scan) = self.compaction_scan_locked(extension, min_pin)? else {
            return Ok(());
        };
        let candidates = self.compaction_candidates_locked(&scan)?;
        if !compaction_threshold_reached(candidates.len(), self.page_count()) {
            return Ok(());
        }
        let truncated = reclaimable_tail(&candidates, self.page_count());
        if truncated.is_empty() {
            return Ok(());
        }
        self.consume_compaction_prefix_locked(&scan, &truncated);
        if self.flush_consumed_locked()? {
            self.backend.sync_all()?;
        }
        self.truncate_local_tail_locked(&truncated)
    }

    fn compaction_candidates_locked(&self, scan: &ReuseScan) -> DevonResult<BTreeSet<u64>> {
        let mut candidates = self.lock_free().deferred_reuse.clone();
        candidates.extend(scan.entries.iter().map(|candidate| candidate.entry.page_id));
        if candidates.iter().any(|page_id| {
            scan.ledger_pages.contains(page_id) || !self.is_reusable_page_id(*page_id)
        }) {
            self.degrade_free_pages_locked();
            return Ok(BTreeSet::new());
        }
        Ok(candidates)
    }

    /// Advances ledger consumption only through the last truncated entry;
    /// skipped lower free pages stay available through the deferred pool.
    fn consume_compaction_prefix_locked(&self, scan: &ReuseScan, truncated: &BTreeSet<u64>) {
        let last = scan
            .entries
            .iter()
            .rposition(|candidate| truncated.contains(&candidate.entry.page_id));
        let mut free = self.lock_free();
        if let Some(last) = last {
            for candidate in &scan.entries[..=last] {
                free.consumed_in_session
                    .entry(candidate.ledger_page)
                    .and_modify(|consumed| *consumed = (*consumed).max(candidate.consumed_after))
                    .or_insert(candidate.consumed_after);
                if !truncated.contains(&candidate.entry.page_id) {
                    free.deferred_reuse.insert(candidate.entry.page_id);
                }
            }
        }
        for page_id in truncated {
            free.deferred_reuse.remove(page_id);
        }
    }

    fn truncate_local_tail_locked(&self, truncated: &BTreeSet<u64>) -> DevonResult<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        if let Some(cache) = &self.cache {
            for page_id in truncated {
                cache.invalidate(*page_id);
            }
        }
        let page_count = self.page_count().saturating_sub(truncated.len());
        let new_len = u64::try_from(page_count)
            .unwrap_or(u64::MAX)
            .checked_mul(u64::from(self.page_size))
            .ok_or_else(|| invalid_argument("compacted file length overflows u64"))?;
        let file = OpenOptions::new().write(true).open(path)?;
        file.set_len(new_len)?;
        file.sync_all()?;
        self.lock_free().truncated_in_session += truncated.len() as u64;
        Ok(())
    }

    /// Durably retires pages made unreachable by the publication at
    /// `publish_lsn`, flushing consumption durability rewrites in the same
    /// pass (`docs/FREE_PAGES.md` § Retirement and reclamation sequence).
    ///
    /// Every write this makes lands and syncs BEFORE the caller's
    /// `commit_superblock` — and none of it is reachable until that flip:
    /// this publication's entries go into FRESH ledger pages that only the
    /// staged extension names (chain walks are bounded by the extension's
    /// tail), so a crash or a failed commit leaks the fresh pages instead
    /// of exposing retirement entries whose publication never happened.
    /// The staged extension rides in the next slot write and becomes the
    /// session's truth only when that write succeeds.
    pub fn retire_pages(&self, superseded: Vec<u64>, publish_lsn: u64) -> DevonResult<()> {
        let _writer = self.lock_writer();
        self.maybe_compact_free_tail_locked()?;
        let page_size = self.page_size as usize;
        let capacity = LedgerPage::capacity(page_size);
        let extension = {
            let free = self.lock_free();
            free.extension
        };

        // Unlink every fully consumed page from the head of the chain.
        // Their retirement entries land in this publication's fresh pages
        // — never in themselves (the tail-recycle ordering law).
        let mut unlinked: Vec<u64> = Vec::new();
        let mut surviving_head = 0_u64;
        let mut surviving_tail = 0_u64;
        let mut visited: HashSet<u64> = HashSet::new();
        let page_count = self.page_count();
        if let Some(extension) = extension.filter(SuperblockExtension::has_ledger) {
            surviving_tail = extension.retire_ledger_tail;
            let mut cursor = extension.retire_ledger_head;
            loop {
                if !visited.insert(cursor) || visited.len() > page_count {
                    self.degrade_free_pages_locked();
                    unlinked.clear();
                    surviving_head = 0;
                    surviving_tail = 0;
                    break;
                }
                let decoded = match self.read_ledger_page_locked(cursor) {
                    Ok(decoded) => decoded,
                    Err(_) => {
                        // Degrade, never fail the publication: abandon the
                        // chain (leaked, sweepable) and start fresh below.
                        self.degrade_free_pages_locked();
                        unlinked.clear();
                        surviving_head = 0;
                        surviving_tail = 0;
                        break;
                    }
                };
                let consumed = self.effective_consumed(cursor, &decoded);
                if (consumed as usize) < decoded.entries.len() {
                    surviving_head = cursor;
                    break;
                }
                unlinked.push(cursor);
                if decoded.next_page != 0 && visited.contains(&decoded.next_page) {
                    self.degrade_free_pages_locked();
                    unlinked.clear();
                    surviving_head = 0;
                    surviving_tail = 0;
                    break;
                }
                if cursor == extension.retire_ledger_tail || decoded.next_page == 0 {
                    surviving_tail = 0;
                    break;
                }
                cursor = decoded.next_page;
            }
        }

        let mut deferred: Vec<u64> = self.lock_free().deferred_reuse.iter().copied().collect();
        let mut to_append: Vec<u64> = superseded;
        to_append.extend(&unlinked);
        to_append.extend(&deferred);
        to_append.sort_unstable();
        to_append.dedup();
        if to_append.iter().any(|page| *page < SUPERBLOCK_SLOTS) {
            return Err(invalid_argument(
                "cannot retire a superblock page into the free-page ledger",
            ));
        }

        if to_append.is_empty() {
            // Nothing retired: flush any consumption rewrites and restage
            // the current extension unchanged so the coming flip carries it.
            let flushed_anything = self.flush_consumed_locked()?;
            if flushed_anything {
                self.backend.sync_all()?;
            }
            let mut free = self.lock_free();
            free.pending_extension = free.extension;
            free.pending_deferred_reuse.clear();
            return Ok(());
        }

        // Fresh ledger pages for this publication's entries: reclaimable
        // entries from the surviving chain first, file append otherwise.
        // Ledger pages come from the surviving head's next eligible entry,
        // then from the deferred pool. After a run selection
        // consumes past the eligible prefix, the head's next entry is
        // pinned while the pool still holds eligible pages — appending here
        // leaked two pages per checkpoint), then from the file tail. A
        // deferred page taken for the ledger leaves the entries being
        // republished, so the page count is re-derived after each pop.
        let mut fresh_ids: Vec<u64> = Vec::new();
        while fresh_ids.len() < to_append.len().div_ceil(capacity) {
            let reused =
                self.pop_one_for_ledger_locked(surviving_head, surviving_tail, &to_append)?;
            let page_id = match reused {
                Some(page_id) => page_id,
                // A deferred page becomes a ledger page only while another
                // entry remains to publish: an extension must never name a
                // ledger page that no entry chunk was written into.
                None => match (to_append.len() >= 2)
                    .then(|| self.pop_deferred_for_ledger_locked(&mut deferred, &mut to_append))
                    .flatten()
                {
                    Some(page_id) => page_id,
                    None => self.append_run_locked(1)?,
                },
            };
            fresh_ids.push(page_id);
        }
        let fresh_count = fresh_ids.len();

        // Consumption durability law: every consumed_count advanced since
        // the last flush — including pops made just above — is rewritten
        // and synced before the flip that publishes reused pages.
        let flushed_anything = self.flush_consumed_locked()?;
        if flushed_anything {
            self.backend.sync_all()?;
        }

        for (index, chunk) in to_append.chunks(capacity).enumerate() {
            let ledger = LedgerPage {
                next_page: fresh_ids.get(index + 1).copied().unwrap_or(0),
                consumed_count: 0,
                entries: chunk
                    .iter()
                    .map(|page_id| LedgerEntry {
                        page_id: *page_id,
                        retired_lsn: publish_lsn,
                    })
                    .collect(),
            };
            self.write_page_locked(fresh_ids[index], &ledger.encode(page_size)?)?;
        }

        // Link the surviving tail to the fresh chain. Walks stop at the
        // extension's tail, so this in-place pointer is inert until the flip.
        if surviving_tail != 0 {
            let mut tail_page = self.read_ledger_page_locked(surviving_tail)?;
            tail_page.consumed_count = self.effective_consumed(surviving_tail, &tail_page);
            tail_page.next_page = fresh_ids[0];
            self.write_page_locked(surviving_tail, &tail_page.encode(page_size)?)?;
        }
        self.backend.sync_all()?;

        let appended = to_append.len() as u64;
        let mut free = self.lock_free();
        let previous_total = free
            .extension
            .map_or(free.retired_total_floor, |extension| {
                extension.retired_total
            });
        free.pending_extension = Some(SuperblockExtension {
            retire_ledger_head: if surviving_head != 0 {
                surviving_head
            } else {
                fresh_ids[0]
            },
            retire_ledger_tail: fresh_ids[fresh_count - 1],
            retired_total: previous_total + appended,
        });
        free.pending_deferred_reuse = deferred;
        Ok(())
    }

    /// Pops one reclaimable entry from the surviving chain for use as a
    /// fresh ledger page, or `None` to append. Never pops an entry naming
    /// a page this publication is retiring (the never-own-entry law).
    fn pop_one_for_ledger_locked(
        &self,
        surviving_head: u64,
        surviving_tail: u64,
        to_append: &[u64],
    ) -> DevonResult<Option<u64>> {
        if surviving_head == 0 {
            return Ok(None);
        }
        let min_pin = self.min_pin.load(Ordering::Acquire);
        if min_pin == 0 {
            return Ok(None);
        }
        let decoded = self.read_ledger_page_locked(surviving_head)?;
        let consumed = self.effective_consumed(surviving_head, &decoded) as usize;
        let Some(entry) = decoded.entries.get(consumed) else {
            return Ok(None);
        };
        if entry.retired_lsn >= min_pin
            || entry.page_id < SUPERBLOCK_SLOTS
            || to_append.binary_search(&entry.page_id).is_ok()
        {
            return Ok(None);
        }
        // Guard the walk-boundary invariant: the head must still be within
        // the tail-bounded chain for its entries to be trustworthy.
        if surviving_tail == 0 || !self.is_reusable_page_id(entry.page_id) {
            return Ok(None);
        }
        let mut free = self.lock_free();
        free.consumed_in_session.insert(
            surviving_head,
            u32::try_from(consumed + 1).unwrap_or(u32::MAX),
        );
        free.reused_in_session += 1;
        Ok(Some(entry.page_id))
    }

    /// Takes the lowest deferred (eligible, skipped-ahead) page for use as a
    /// fresh ledger page, withdrawing it from the pool and from the entries
    /// being republished so the ledger never lists its own page as free.
    fn pop_deferred_for_ledger_locked(
        &self,
        deferred: &mut Vec<u64>,
        to_append: &mut Vec<u64>,
    ) -> Option<u64> {
        let page_id = deferred
            .iter()
            .copied()
            .find(|page_id| *page_id >= SUPERBLOCK_SLOTS && self.is_reusable_page_id(*page_id))?;
        deferred.retain(|candidate| *candidate != page_id);
        if let Ok(index) = to_append.binary_search(&page_id) {
            to_append.remove(index);
        }
        let mut free = self.lock_free();
        free.deferred_reuse.remove(&page_id);
        free.reused_in_session += 1;
        Some(page_id)
    }

    /// Rewrites the consumed_count (+ CRC) of every ledger page advanced
    /// since the last flush. Caller holds the writer lock and syncs after.
    fn flush_consumed_locked(&self) -> DevonResult<bool> {
        let dirty: Vec<(u64, u32)> = {
            let free = self.lock_free();
            free.consumed_in_session
                .iter()
                .map(|(page, consumed)| (*page, *consumed))
                .collect()
        };
        if dirty.is_empty() {
            return Ok(false);
        }
        let page_size = self.page_size as usize;
        for (page_id, consumed) in dirty {
            // A page that no longer decodes cannot re-offer its entries on
            // reopen either, so skipping its flush stays double-offer-safe.
            let Ok(mut decoded) = self.read_ledger_page_locked(page_id) else {
                continue;
            };
            if consumed > decoded.consumed_count {
                decoded.consumed_count = consumed;
                self.write_page_locked(page_id, &decoded.encode(page_size)?)?;
            }
        }
        self.lock_free().consumed_in_session.clear();
        Ok(true)
    }

    /// Abandons the current ledger chain after a validation failure —
    /// degraded mode: reads untouched, allocation appends, the next
    /// publication starts a fresh chain. The cumulative `retired_total`
    /// survives as a floor so the counter stays strictly monotone.
    fn degrade_free_pages_locked(&self) {
        let mut free = self.lock_free();
        if let Some(extension) = free.extension.take() {
            free.retired_total_floor = free.retired_total_floor.max(extension.retired_total);
        }
        free.degraded = true;
        free.consumed_in_session.clear();
    }

    /// Raises the pin horizon. Monotone: a lower candidate is ignored, so
    /// a stale caller can only be conservative, never unsafe.
    pub fn raise_min_pin(&self, candidate: u64) {
        self.min_pin.fetch_max(candidate, Ordering::AcqRel);
    }

    /// The current pin horizon for diagnostics and invariant tests.
    #[must_use]
    pub fn min_pin(&self) -> u64 {
        self.min_pin.load(Ordering::Acquire)
    }

    /// Whether this file will carry a retirement ledger after the next
    /// slot write — the catalog-save input for the sticky `FREE_PAGES` bit.
    #[must_use]
    pub fn has_free_pages_ledger(&self) -> bool {
        let free = self.lock_free();
        free.pending_extension
            .or(free.extension)
            .is_some_and(|extension| extension.has_ledger())
    }

    /// The durable extension mirror, `None` when the file has no ledger or
    /// this session is degraded.
    #[must_use]
    pub fn free_pages_extension(&self) -> Option<SuperblockExtension> {
        self.lock_free().extension
    }

    /// Pages handed out by ledger reuse during this pager session.
    #[must_use]
    pub fn free_pages_session_reused(&self) -> u64 {
        self.lock_free().reused_in_session
    }

    /// Reclaimable tail pages physically truncated during this session.
    #[must_use]
    pub fn free_pages_session_truncated(&self) -> u64 {
        self.lock_free().truncated_in_session
    }

    /// Whether this session abandoned the ledger after an extension or
    /// ledger-page validation failure — degraded mode: reads untouched,
    /// allocation appends, the next publication starts a fresh chain.
    #[must_use]
    pub fn free_pages_degraded(&self) -> bool {
        self.lock_free().degraded
    }

    /// Walks the durable ledger chain head → tail, returning each page id
    /// with its decoded contents — the doctor/verification surface.
    pub fn free_pages_ledger_pages(&self) -> DevonResult<Vec<(u64, LedgerPage)>> {
        let Some(extension) = self
            .free_pages_extension()
            .filter(SuperblockExtension::has_ledger)
        else {
            return Ok(Vec::new());
        };
        let mut pages = Vec::new();
        let mut cursor = extension.retire_ledger_head;
        let mut visited = HashSet::new();
        loop {
            if !visited.insert(cursor) || visited.len() > self.page_count() {
                return Err(corrupt(
                    "free-page ledger chain revisits a page (cycle detected)",
                ));
            }
            let decoded = LedgerPage::decode(&self.read_page_ref(cursor)?)?;
            let next = decoded.next_page;
            pages.push((cursor, decoded));
            if cursor == extension.retire_ledger_tail {
                // FREE_PAGES.md § Ledger pages: `next_page` 0 = tail. A
                // tail naming a successor (itself included) is damage.
                if next != 0 {
                    return Err(corrupt(
                        "free-page ledger tail page names a successor (cycle detected)",
                    ));
                }
                return Ok(pages);
            }
            if next == 0 {
                return Ok(pages);
            }
            cursor = next;
        }
    }

    /// Durably commits `superblock` to the non-authoritative slot.
    pub fn commit_superblock(&self, superblock: Superblock) -> DevonResult<()> {
        // Dual-slot arbitration ordering requires this whole sequence —
        // validate against the current superblock, write the
        // non-authoritative slot, fsync it, flip authority — to be atomic
        // w.r.t. other commits: a second commit interleaving before the
        // flip would validate against a stale LSN and target (and
        // overwrite) the same slot. The fsync therefore stays inside the
        // WRITER lock, which readers never take, and outside the STATE
        // mutex, which they do — so the docs/MVCC.md §4.3 rule 2 promise
        // (readers never wait on an fsync) holds.
        let _writer = self.lock_writer();
        let (current, current_slot) = {
            let state = self.lock_state();
            (state.superblock.clone(), state.authoritative_slot)
        };
        ensure_commit_is_valid(&current, &superblock)?;
        let page = self.encode_slot_page(&superblock, true)?;
        let next_slot = 1_u8 - current_slot;
        let offset = u64::from(next_slot) * u64::from(self.page_size);

        self.backend.write_all_at(offset, &page)?;
        if let Err(error) = self.backend.sync_all() {
            let mut free = self.lock_free();
            if let Some(pending) = free.pending_extension {
                free.retired_total_floor = free.retired_total_floor.max(pending.retired_total);
            }
            return Err(error);
        }
        {
            let mut state = self.lock_state();
            state.superblock = superblock;
            state.authoritative_slot = next_slot;
        }
        let mut free = self.lock_free();
        if let Some(pending) = free.pending_extension.take() {
            free.extension = Some(pending);
            free.degraded = false;
            let republished = std::mem::take(&mut free.pending_deferred_reuse);
            for page_id in republished {
                free.deferred_reuse.remove(&page_id);
            }
        }
        Ok(())
    }

    /// Composes one slot page: the 64-byte header plus, when the
    /// superblock carries the `FREE_PAGES` bit, the extension region.
    /// Without the bit the region stays zero — the zero-fence law.
    fn encode_slot_page(
        &self,
        superblock: &Superblock,
        include_pending: bool,
    ) -> DevonResult<Vec<u8>> {
        let mut page = encode_page(superblock)?;
        if superblock.feature_flags & FREE_PAGES_FLAG != 0 {
            let free = self.lock_free();
            let extension = if include_pending {
                free.pending_extension.or(free.extension)
            } else {
                free.extension
            };
            if let Some(extension) = extension {
                extension.encode_into(&mut page)?;
            }
        }
        Ok(page)
    }

    /// Durably publishes a feature-flags-only superblock change without
    /// advancing the checkpoint LSN.
    ///
    /// `commit_superblock` requires LSN advance because it arbitrates
    /// BETWEEN slots; a flag lifecycle change (the checkpoint-scoped
    /// `DML_WAL` bit, `docs/FORMAT.md` § Feature flag registry) has no new
    /// materialized state to arbitrate. This writes the updated superblock
    /// to BOTH slots (non-authoritative first), each with its own fsync, so
    /// reopen's equal-LSN tie-break (slot 0) yields the new flags whichever
    /// slot it picks. A crash between the two writes leaves one slot on the
    /// old flags — safe for both callers by their ordering contracts: a
    /// flag SET happens before the WAL records it governs are fsynced, and
    /// a flag CLEAR happens after the WAL is truncated.
    pub fn commit_feature_flags(&self, feature_flags: u64) -> DevonResult<()> {
        let _writer = self.lock_writer();
        let (mut superblock, current_slot) = {
            let state = self.lock_state();
            (state.superblock.clone(), state.authoritative_slot)
        };
        if superblock.feature_flags == feature_flags {
            return Ok(());
        }
        superblock.feature_flags = feature_flags;
        // Current extension only, never pending: a flags-only publication
        // must not make an unflipped publication's retirement entries
        // reachable ahead of the catalog that justifies them.
        let page = self.encode_slot_page(&superblock, false)?;
        let other_slot = 1_u8 - current_slot;
        for slot in [other_slot, current_slot] {
            let offset = u64::from(slot) * u64::from(self.page_size);
            self.backend.write_all_at(offset, &page)?;
            self.backend.sync_all()?;
        }
        let mut state = self.lock_state();
        state.superblock = superblock;
        Ok(())
    }

    /// Returns a copy of the currently authoritative superblock.
    #[must_use]
    pub fn superblock(&self) -> Superblock {
        self.lock_state().superblock.clone()
    }

    /// The byte backend under this pager. `pub(crate)` for the `pack`
    /// container writer, which must read raw bytes — superblock pages
    /// included — below the data-page gate (`docs/SCALE.md` §7.1).
    #[cfg(feature = "pack")]
    pub(crate) fn backend(&self) -> &Backend {
        &self.backend
    }

    /// Returns the number of data-page read requests since open or the last
    /// reset. Cache hits count because callers use this to prove work avoided.
    #[must_use]
    pub fn page_read_count(&self) -> u64 {
        self.page_reads.load(Ordering::Relaxed)
    }

    /// The shared-budget bytes currently charged to cached page frames —
    /// lets budget accounting separate cache occupancy from other charges.
    #[must_use]
    pub fn cache_charged_bytes(&self) -> usize {
        self.cache.as_ref().map_or(0, |cache| {
            cache
                .lock_frames()
                .frames
                .len()
                .saturating_mul(cache.frame_charge)
        })
    }

    /// Resets the diagnostic data-page read counter to zero.
    pub fn reset_page_read_count(&self) {
        self.page_reads.store(0, Ordering::Relaxed);
    }

    /// Records that a node-group writer produced a zone-map directory that a
    /// following catalog publication must govern with the sticky feature bit.
    pub(crate) fn note_zone_maps_written(&self) {
        self.wrote_zone_maps.store(true, Ordering::Release);
    }

    /// Returns whether this pager has written a zone-map directory since open.
    #[must_use]
    pub(crate) fn has_written_zone_maps(&self) -> bool {
        self.wrote_zone_maps.load(Ordering::Acquire)
    }

    /// Records that the adaptive node-group writer produced a directory whose
    /// `COLUMN_ENCODINGS` section awaits its governing catalog publication.
    pub(crate) fn note_column_encodings_written(&self) {
        self.wrote_column_encodings.store(true, Ordering::Release);
    }

    /// Returns whether this pager has written an adaptive encodings-bearing
    /// directory since open.
    #[must_use]
    pub(crate) fn has_written_column_encodings(&self) -> bool {
        self.wrote_column_encodings.load(Ordering::Acquire)
    }

    /// Locks the write-serialization mutex, recovering from poisoning:
    /// the guarded data is `()`, so a panic while holding it leaves
    /// nothing inconsistent.
    fn lock_writer(&self) -> MutexGuard<'_, ()> {
        self.writer.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Locks the superblock state, recovering from poisoning: the state
    /// is only ever replaced by whole-field assignments that cannot
    /// panic midway, so a poisoned guard still holds consistent data.
    fn lock_state(&self) -> MutexGuard<'_, PagerState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Locks the free-page state, recovering from poisoning: every
    /// mutation is a whole-field assignment or map insert that cannot
    /// leave the mirror torn.
    fn lock_free(&self) -> MutexGuard<'_, FreeState> {
        self.free.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Reads the `FREE_PAGES` extension of the chosen slot at open. Gated on
/// the feature bit; a CRC mismatch is degraded mode, never an error.
fn read_free_state(
    backend: &Backend,
    superblock: &Superblock,
    authoritative_slot: u8,
) -> DevonResult<FreeState> {
    if superblock.feature_flags & FREE_PAGES_FLAG == 0 {
        return Ok(FreeState::default());
    }
    let mut state = FreeState::default();
    for slot in 0..SUPERBLOCK_SLOTS {
        let offset = slot * u64::from(superblock.page_size);
        let mut prefix = vec![0_u8; EXTENSION_END];
        if backend.read_exact_at(offset, &mut prefix).is_err() {
            continue;
        }
        if let Some(extension) = SuperblockExtension::decode_from(&prefix) {
            state.retired_total_floor = state.retired_total_floor.max(extension.retired_total);
            if slot == u64::from(authoritative_slot) {
                state.extension = Some(extension);
            }
        } else if slot == u64::from(authoritative_slot) {
            state.degraded = true;
        }
    }
    Ok(state)
}

fn compaction_threshold_reached(free_pages: usize, total_pages: usize) -> bool {
    total_pages > SUPERBLOCK_SLOTS as usize
        && free_pages.saturating_mul(100) >= total_pages.saturating_mul(COMPACTION_FREE_PERCENT)
}

fn reclaimable_tail(candidates: &BTreeSet<u64>, page_count: usize) -> BTreeSet<u64> {
    let mut tail = BTreeSet::new();
    let Ok(mut cursor) = u64::try_from(page_count) else {
        return tail;
    };
    for _ in 0..COMPACTION_TRUNCATE_PAGE_LIMIT {
        let Some(page_id) = cursor.checked_sub(1) else {
            break;
        };
        if page_id < SUPERBLOCK_SLOTS || !candidates.contains(&page_id) {
            break;
        }
        tail.insert(page_id);
        cursor = page_id;
    }
    tail
}

fn find_contiguous_run(entries: &BTreeSet<u64>, count: usize) -> Option<Vec<u64>> {
    let mut run = Vec::with_capacity(count);
    for page_id in entries {
        if run
            .last()
            .is_some_and(|previous: &u64| previous.checked_add(1) != Some(*page_id))
        {
            run.clear();
        }
        run.push(*page_id);
        if run.len() == count {
            return Some(run);
        }
    }
    None
}

fn trim_unselected_scan(scan: &mut ReuseScan, skip_capacity: usize) {
    if scan.selected.is_none() {
        scan.entries.truncate(skip_capacity);
    }
}

/// Selects, among the physically contiguous runs of `count` scanned pages,
/// the run whose last ledger position is earliest — the choice that consumes
/// the oldest retirements and defers the fewest skipped entries. Truncates
/// the scan to the consumed prefix. A page id listed twice is treated as one
/// entry: only its first ledger position is ever selected, and the duplicate
/// is deferred, never handed out.
fn select_earliest_run(scan: &mut ReuseScan, count: usize) {
    if count == 0 {
        return;
    }
    let mut position: HashMap<u64, usize> = HashMap::with_capacity(scan.entries.len());
    for (index, candidate) in scan.entries.iter().enumerate() {
        position.entry(candidate.entry.page_id).or_insert(index);
    }
    let mut ids: Vec<u64> = position.keys().copied().collect();
    ids.sort_unstable();

    let mut best: Option<(usize, usize)> = None;
    let mut run_start = 0;
    let mut window: std::collections::VecDeque<usize> = std::collections::VecDeque::new();
    for (i, id) in ids.iter().enumerate() {
        if i > 0 && ids[i - 1].checked_add(1) != Some(*id) {
            run_start = i;
            window.clear();
        }
        let at = position[id];
        while window
            .back()
            .is_some_and(|back| position[&ids[*back]] <= at)
        {
            window.pop_back();
        }
        window.push_back(i);
        while window
            .front()
            .is_some_and(|front| *front + count <= i + 1 && *front < i + 1 - count)
        {
            window.pop_front();
        }
        if i + 1 - run_start >= count {
            let through = position[&ids[*window.front().unwrap_or(&i)]];
            if best.is_none_or(|(best_through, _)| through < best_through) {
                best = Some((through, i + 1 - count));
            }
        }
    }
    let Some((through, start)) = best else {
        return;
    };
    let selected: BTreeSet<usize> = ids[start..start + count]
        .iter()
        .map(|id| position[id])
        .collect();
    scan.entries.truncate(through + 1);
    scan.selected = Some(selected);
}

fn sync_parent_directory(path: &Path) -> DevonResult<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    // Some platforms cannot open directories as files. Ignore only that
    // platform limitation; once opened, a failed directory sync is real I/O.
    let Ok(directory) = File::open(parent) else {
        return Ok(());
    };
    directory.sync_all()?;
    Ok(())
}

fn validate_file_length(file_len: u64, page_size: u32) -> DevonResult<()> {
    let page_size = u64::from(page_size);
    let minimum_len = page_size * SUPERBLOCK_SLOTS;
    if file_len < minimum_len {
        return Err(corrupt(&format!(
            "database file length {file_len} is shorter than two pages"
        )));
    }
    if !file_len.is_multiple_of(page_size) {
        return Err(corrupt(&format!(
            "database file length {file_len} is not page-aligned"
        )));
    }
    Ok(())
}

fn encode_page(superblock: &Superblock) -> DevonResult<Vec<u8>> {
    let mut header = [0_u8; SUPERBLOCK_HEADER_LEN];
    superblock.encode(&mut header)?;
    let mut page = vec![0_u8; superblock.page_size as usize];
    page[..SUPERBLOCK_HEADER_LEN].copy_from_slice(&header);
    Ok(page)
}

fn initial_superblock(page_size: u32, db_id: [u8; 16]) -> Superblock {
    Superblock {
        format_version: SUPPORTED_FORMAT_VERSION,
        min_reader_version: SUPPORTED_FORMAT_VERSION,
        feature_flags: 0,
        page_size,
        db_id,
        checkpoint_lsn: 0,
        catalog_root: 0,
    }
}

fn read_slot(
    backend: &Backend,
    offset: u64,
    expected_page_size: Option<u32>,
) -> DevonResult<Superblock> {
    let mut header = [0_u8; SUPERBLOCK_HEADER_LEN];
    backend.read_exact_at(offset, &mut header)?;
    let superblock = Superblock::decode(&header)?;
    if expected_page_size.is_some_and(|expected| expected != superblock.page_size) {
        return Err(corrupt("superblock slots disagree on page size"));
    }
    Ok(superblock)
}

fn read_second_slot(backend: &Backend, page_size: Option<u32>) -> DevonResult<Superblock> {
    if let Some(page_size) = page_size {
        return read_slot(backend, u64::from(page_size), Some(page_size));
    }

    for candidate in VALID_PAGE_SIZES {
        if let Ok(superblock) = read_slot(backend, u64::from(candidate), Some(candidate)) {
            return Ok(superblock);
        }
    }
    Err(corrupt("could not locate a valid second superblock slot"))
}

fn selected_slot(a: &DevonResult<Superblock>, b: &DevonResult<Superblock>) -> u8 {
    match (a, b) {
        (Ok(a), Ok(b)) if b.checkpoint_lsn > a.checkpoint_lsn => 1,
        (Ok(_), _) => 0,
        (Err(_), Ok(_)) => 1,
        (Err(_), Err(_)) => 0,
    }
}

fn ensure_data_page(page_id: u64) -> DevonResult<()> {
    if page_id < SUPERBLOCK_SLOTS {
        return Err(invalid_argument(
            "pages 0 and 1 are reserved for superblocks",
        ));
    }
    Ok(())
}

fn page_offset(page_id: u64, page_size: u32) -> DevonResult<u64> {
    page_id
        .checked_mul(u64::from(page_size))
        .ok_or_else(|| invalid_argument("page offset overflows u64"))
}

fn ensure_commit_is_valid(current: &Superblock, next: &Superblock) -> DevonResult<()> {
    if next.page_size != current.page_size {
        return Err(invalid_argument(
            "a commit cannot change the database page size",
        ));
    }
    if next.db_id != current.db_id {
        return Err(invalid_argument(
            "a commit cannot change the database identifier",
        ));
    }
    if next.checkpoint_lsn <= current.checkpoint_lsn {
        return Err(invalid_argument(
            "a committed superblock must advance the checkpoint LSN",
        ));
    }
    Ok(())
}

fn invalid_argument(context: &str) -> DevonError {
    DevonError::InvalidArgument {
        context: context.to_owned(),
    }
}

fn corrupt(context: &str) -> DevonError {
    DevonError::Corrupt {
        context: context.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use std::fs::{self, OpenOptions};
    use std::io::{Read, Seek, SeekFrom, Write};
    use std::sync::Arc;
    use std::thread;

    use devondb_types::DevonError;
    use tempfile::tempdir;

    use crate::budget::MemoryBudget;

    use super::{FRAME_OVERHEAD, Pager, Superblock, generate_db_id};

    const PAGE_SIZE: u32 = 4096;
    const DB_ID: [u8; 16] = *b"pager-test-db-id";

    fn authoritative_slot(pager: &Pager) -> u8 {
        pager.lock_state().authoritative_slot
    }

    fn frame_charge() -> usize {
        PAGE_SIZE as usize + FRAME_OVERHEAD
    }

    #[test]
    fn flag_only_publication_survives_reopen_without_lsn_advance() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("flags.devondb");
        let pager = Pager::create(&path, PAGE_SIZE, DB_ID).unwrap();
        let before = pager.superblock();

        let flags = before.feature_flags | crate::superblock::DML_WAL_FLAG;
        pager.commit_feature_flags(flags).unwrap();
        let after = pager.superblock();
        assert_eq!(after.feature_flags, flags);
        assert_eq!(after.checkpoint_lsn, before.checkpoint_lsn);
        // Both slots carry the new flags, so the equal-LSN slot-0 tie-break
        // cannot resurrect the old value on reopen.
        for slot in 0_u64..2 {
            let mut file = OpenOptions::new().read(true).open(&path).unwrap();
            file.seek(SeekFrom::Start(slot * u64::from(PAGE_SIZE)))
                .unwrap();
            let mut page = vec![0_u8; PAGE_SIZE as usize];
            file.read_exact(&mut page).unwrap();
            let decoded = Superblock::decode(&page).unwrap();
            assert_eq!(decoded.feature_flags, flags, "slot {slot}");
        }
        drop(pager);

        let reopened = Pager::open(&path).unwrap();
        assert_eq!(reopened.superblock().feature_flags, flags);

        reopened.commit_feature_flags(before.feature_flags).unwrap();
        assert_eq!(reopened.superblock().feature_flags, before.feature_flags);
    }

    fn populate_pages(pager: &Pager, count: usize) -> Vec<u64> {
        (0..count)
            .map(|index| {
                let page_id = pager.allocate_page().unwrap();
                let byte = u8::try_from(index + 1).unwrap();
                pager
                    .write_page(page_id, &vec![byte; PAGE_SIZE as usize])
                    .unwrap();
                page_id
            })
            .collect()
    }

    fn cache_contains(pager: &Pager, page_id: u64) -> bool {
        pager
            .cache
            .as_ref()
            .unwrap()
            .lock_frames()
            .frames
            .contains_key(&page_id)
    }

    fn cache_len(pager: &Pager) -> usize {
        pager.cache.as_ref().unwrap().lock_frames().frames.len()
    }

    #[test]
    fn cache_pins_block_eviction() {
        let directory = tempdir().unwrap();
        let budget = Arc::new(MemoryBudget::new(frame_charge() * 9));
        let pager = Pager::create(directory.path().join("pins.devondb"), PAGE_SIZE, DB_ID)
            .unwrap()
            .with_budget(Arc::clone(&budget));
        let page_ids = populate_pages(&pager, 10);
        let pinned = pager.read_page_ref(page_ids[0]).unwrap();
        for page_id in &page_ids[1..9] {
            drop(pager.read_page_ref(*page_id).unwrap());
        }

        drop(pager.read_page_ref(page_ids[9]).unwrap());

        assert!(cache_contains(&pager, page_ids[0]));
        assert!(!cache_contains(&pager, page_ids[1]));
        assert!(Arc::ptr_eq(
            &pinned.0,
            &pager.read_page_ref(page_ids[0]).unwrap().0
        ));
        assert_eq!(cache_len(&pager), 9);
        assert_eq!(budget.charged(), frame_charge() * 9);
    }

    #[test]
    fn cache_eviction_at_budget_uses_clock_second_chance() {
        let directory = tempdir().unwrap();
        let budget = Arc::new(MemoryBudget::new(frame_charge() * 9));
        let pager = Pager::create(directory.path().join("eviction.devondb"), PAGE_SIZE, DB_ID)
            .unwrap()
            .with_budget(Arc::clone(&budget));
        let page_ids = populate_pages(&pager, 10);
        for page_id in &page_ids[..9] {
            drop(pager.read_page_ref(*page_id).unwrap());
        }

        drop(pager.read_page_ref(page_ids[9]).unwrap());

        assert!(!cache_contains(&pager, page_ids[0]));
        assert!(cache_contains(&pager, page_ids[9]));
        assert_eq!(cache_len(&pager), 9);
        assert_eq!(budget.charged(), frame_charge() * 9);
    }

    #[test]
    fn cache_uncharged_fallback_works_at_the_frame_floor() {
        let directory = tempdir().unwrap();
        let budget = Arc::new(MemoryBudget::new(frame_charge() * 8));
        let pager = Pager::create(directory.path().join("fallback.devondb"), PAGE_SIZE, DB_ID)
            .unwrap()
            .with_budget(Arc::clone(&budget));
        let page_ids = populate_pages(&pager, 9);
        let pinned = page_ids[..8]
            .iter()
            .map(|page_id| pager.read_page_ref(*page_id).unwrap())
            .collect::<Vec<_>>();

        let first = pager.read_page_ref(page_ids[8]).unwrap();
        let second = pager.read_page_ref(page_ids[8]).unwrap();

        assert!(first.iter().all(|byte| *byte == 9));
        assert!(!Arc::ptr_eq(&first.0, &second.0));
        assert!(!cache_contains(&pager, page_ids[8]));
        assert_eq!(cache_len(&pager), 8);
        assert_eq!(budget.charged(), frame_charge() * 8);
        assert_eq!(pinned.len(), 8);
    }

    #[test]
    fn cache_write_invalidates_the_cached_frame() {
        let directory = tempdir().unwrap();
        let budget = Arc::new(MemoryBudget::new(frame_charge() * 9));
        let pager = Pager::create(
            directory.path().join("invalidation.devondb"),
            PAGE_SIZE,
            DB_ID,
        )
        .unwrap()
        .with_budget(Arc::clone(&budget));
        let page_id = populate_pages(&pager, 1)[0];
        let stale_pin = pager.read_page_ref(page_id).unwrap();
        assert_eq!(budget.charged(), frame_charge());

        pager
            .write_page(page_id, &vec![0xa5; PAGE_SIZE as usize])
            .unwrap();

        assert!(!cache_contains(&pager, page_id));
        assert_eq!(budget.charged(), 0);
        assert!(stale_pin.iter().all(|byte| *byte == 1));
        assert!(
            pager
                .read_page_ref(page_id)
                .unwrap()
                .iter()
                .all(|byte| *byte == 0xa5)
        );
    }

    #[test]
    fn cache_concurrent_readers_return_exact_page_bytes() {
        let directory = tempdir().unwrap();
        let budget = Arc::new(MemoryBudget::new(frame_charge() * 9));
        let pager = Pager::create(
            directory.path().join("cache-concurrent.devondb"),
            PAGE_SIZE,
            DB_ID,
        )
        .unwrap()
        .with_budget(Arc::clone(&budget));
        let page_ids = populate_pages(&pager, 12);

        thread::scope(|scope| {
            for _ in 0..8 {
                scope.spawn(|| {
                    for _ in 0..25 {
                        for (index, page_id) in page_ids.iter().enumerate() {
                            let page = pager.read_page_ref(*page_id).unwrap();
                            let expected = u8::try_from(index + 1).unwrap();
                            assert!(page.iter().all(|byte| *byte == expected));
                        }
                    }
                });
            }
        });

        assert!(budget.charged() <= budget.limit());
        assert!(cache_len(&pager) >= 8);
    }

    #[test]
    fn create_then_open_round_trips_superblock() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("round-trip.devondb");
        let pager = Pager::create(&path, PAGE_SIZE, DB_ID).unwrap();
        let expected = pager.superblock();
        drop(pager);

        let reopened = Pager::open(path).unwrap();
        assert_eq!(reopened.superblock(), expected);
    }

    #[test]
    fn generated_database_ids_differ() {
        assert_ne!(generate_db_id().unwrap(), generate_db_id().unwrap());
    }

    #[test]
    fn open_uses_slot_one_when_slot_zero_is_zeroed() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("damaged-slot.devondb");
        let pager = Pager::create(&path, PAGE_SIZE, DB_ID).unwrap();
        let expected = pager.superblock();
        drop(pager);

        let mut file = OpenOptions::new().write(true).open(&path).unwrap();
        file.write_all(&vec![0_u8; PAGE_SIZE as usize]).unwrap();
        file.sync_all().unwrap();
        drop(file);

        let reopened = Pager::open(path).unwrap();
        assert_eq!(reopened.superblock(), expected);
        assert_eq!(authoritative_slot(&reopened), 1);
    }

    #[test]
    fn page_write_then_read_round_trips_and_checks_size() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("page-io.devondb");
        let pager = Pager::create(path, PAGE_SIZE, DB_ID).unwrap();
        let page = vec![0x5a; PAGE_SIZE as usize];

        pager.write_page(2, &page).unwrap();
        assert_eq!(pager.read_page(2).unwrap(), page);
        assert!(matches!(
            pager.write_page(3, &[0_u8; 12]),
            Err(DevonError::InvalidArgument { .. })
        ));
        assert!(matches!(
            pager.read_page(3),
            Err(DevonError::InvalidArgument { .. })
        ));
    }

    #[test]
    fn concurrent_reads_return_each_pages_exact_bytes() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("concurrent-reads.devondb");
        let pager = Pager::create(path, PAGE_SIZE, DB_ID).unwrap();
        let page_ids: Vec<u64> = (0..8).map(|_| pager.allocate_page().unwrap()).collect();
        for page_id in &page_ids {
            let fill = u8::try_from(*page_id).unwrap();
            pager
                .write_page(*page_id, &vec![fill; PAGE_SIZE as usize])
                .unwrap();
        }

        thread::scope(|scope| {
            for _ in 0..8 {
                scope.spawn(|| {
                    for _ in 0..50 {
                        for page_id in &page_ids {
                            let page = pager.read_page(*page_id).unwrap();
                            let expected = u8::try_from(*page_id).unwrap();
                            assert_eq!(page.len(), PAGE_SIZE as usize);
                            assert!(page.iter().all(|byte| *byte == expected));
                        }
                    }
                });
            }
        });
    }

    #[test]
    fn sync_after_page_write_succeeds() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("sync.devondb");
        let pager = Pager::create(path, PAGE_SIZE, DB_ID).unwrap();

        pager
            .write_page(2, &vec![0x5a; PAGE_SIZE as usize])
            .unwrap();
        pager.sync().unwrap();
    }

    #[test]
    fn open_rejects_file_truncated_to_one_and_a_half_pages() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("unaligned.devondb");
        let pager = Pager::create(&path, PAGE_SIZE, DB_ID).unwrap();
        drop(pager);
        let truncated_len = u64::from(PAGE_SIZE + PAGE_SIZE / 2);
        OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(truncated_len)
            .unwrap();

        let error = Pager::open(path).err().unwrap();
        let DevonError::Corrupt { context } = error else {
            panic!("expected corruption error, got {error}");
        };
        assert!(context.contains(&truncated_len.to_string()));
    }

    #[test]
    fn open_rejects_one_page_file() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("one-page.devondb");
        let pager = Pager::create(&path, PAGE_SIZE, DB_ID).unwrap();
        drop(pager);
        let truncated_len = u64::from(PAGE_SIZE);
        OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(truncated_len)
            .unwrap();

        let error = Pager::open(path).err().unwrap();
        let DevonError::Corrupt { context } = error else {
            panic!("expected corruption error, got {error}");
        };
        assert!(context.contains(&truncated_len.to_string()));
    }

    #[test]
    fn reserved_pages_are_rejected() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("reserved.devondb");
        let pager = Pager::create(path, PAGE_SIZE, DB_ID).unwrap();
        let page = vec![0_u8; PAGE_SIZE as usize];

        for page_id in [0, 1] {
            assert!(matches!(
                pager.read_page(page_id),
                Err(DevonError::InvalidArgument { .. })
            ));
            assert!(matches!(
                pager.write_page(page_id, &page),
                Err(DevonError::InvalidArgument { .. })
            ));
        }
    }

    #[test]
    fn allocate_page_returns_increasing_ids_and_grows_file() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("allocate.devondb");
        let pager = Pager::create(&path, PAGE_SIZE, DB_ID).unwrap();

        assert_eq!(pager.allocate_page().unwrap(), 2);
        assert_eq!(pager.allocate_page().unwrap(), 3);
        assert_eq!(fs::metadata(path).unwrap().len(), u64::from(PAGE_SIZE) * 4);
        assert_eq!(pager.read_page(2).unwrap(), vec![0_u8; PAGE_SIZE as usize]);
    }

    #[test]
    fn superblock_commits_alternate_slots_and_survive_reopen() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("commits.devondb");
        let pager = Pager::create(&path, PAGE_SIZE, DB_ID).unwrap();
        let mut first = pager.superblock();
        first.checkpoint_lsn = 10;
        first.catalog_root = 2;
        pager.commit_superblock(first.clone()).unwrap();
        assert_eq!(authoritative_slot(&pager), 1);

        let mut second = first.clone();
        second.checkpoint_lsn = 11;
        second.catalog_root = 3;
        pager.commit_superblock(second.clone()).unwrap();
        assert_eq!(authoritative_slot(&pager), 0);
        drop(pager);

        let mut file = OpenOptions::new().read(true).open(&path).unwrap();
        let mut slot_zero = vec![0_u8; PAGE_SIZE as usize];
        let mut slot_one = vec![0_u8; PAGE_SIZE as usize];
        file.read_exact(&mut slot_zero).unwrap();
        file.seek(SeekFrom::Start(u64::from(PAGE_SIZE))).unwrap();
        file.read_exact(&mut slot_one).unwrap();
        assert_eq!(Superblock::decode(&slot_zero).unwrap(), second);
        assert_eq!(Superblock::decode(&slot_one).unwrap(), first);
        drop(file);

        let reopened = Pager::open(path).unwrap();
        assert_eq!(reopened.superblock(), second);
        assert_eq!(authoritative_slot(&reopened), 0);
    }
}
