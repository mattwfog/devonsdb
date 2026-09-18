//! DEVONPACK container support for the facade (`docs/SCALE.md` §7, BINDING),
//! behind the `pack` cargo feature.
//!
//! Open by magic: every `Database::open*` sniffs the first 9 bytes; a
//! `DEVONPACK` file opens as read-only shared state (no WAL writer, no
//! writer lease, no publication gate — the shape `open_read_only_with`
//! builds, minus the multiprocess flag requirement) over a [`Pager`] on the
//! `Pack` backend. The backend's ONE decoded-frame buffer
//! (`frame_pages × page_size` bytes) is charged to the memory budget once,
//! here at open; a `memory_limit` below one frame is refused honestly with
//! both numbers named.

use std::fs::File;
use std::path::Path;

use devondb_storage::pack::{PackFile, pack_pages};

use super::*;

/// Opens a DEVONPACK container as read-only shared state.
///
/// The container is a checkpoint image: no WAL is replayed, trimmed, or
/// created for it, and no stale-spill sweep runs (a read-only open's only
/// filesystem footprint is reads plus its own locked spill directory).
pub(super) fn open(path: &Path, options: Options) -> DevonResult<Database> {
    let (pack, budget, frame_bytes) = prepare_pack(path, options)?;
    let pager = pager_with_budget_and_reclaimer(Pager::open_pack(pack)?, &budget);
    let spill_dir = Arc::new(acquire_spill_handle_dir(&spill_tmp_path(path))?);
    let checkpoint_lsn = pager.superblock().checkpoint_lsn;
    let catalog = Catalog::load(&pager)?;
    let published = recover_published_state(&pager, &budget, catalog, Vec::new(), checkpoint_lsn)?;
    Ok(Database::from_parts(
        pager,
        None,
        wal_path(path),
        published,
        budget,
        spill_dir,
        true,
        None,
        None,
        false,
        frame_bytes,
    ))
}

/// Opens a DEVONPACK inspection handle without creating `<pack>.tmp`.
pub(super) fn open_inspect(path: &Path, options: Options) -> DevonResult<Database> {
    let (pack, budget, frame_bytes) = prepare_pack(path, options)?;
    let pager = pager_with_budget_and_reclaimer(Pager::open_pack(pack)?, &budget);
    let checkpoint_lsn = pager.superblock().checkpoint_lsn;
    let catalog = Catalog::load(&pager)?;
    let published = recover_published_state(&pager, &budget, catalog, Vec::new(), checkpoint_lsn)?;
    Ok(Database::from_inspect_parts(
        pager,
        wal_path(path),
        published,
        budget,
        spill_tmp_path(path),
        None,
        frame_bytes,
    ))
}

fn prepare_pack(
    path: &Path,
    options: Options,
) -> DevonResult<(PackFile, Arc<MemoryBudget>, usize)> {
    let pack = PackFile::open(path)?;
    let frame_bytes = pack.frame_bytes();
    let budget = Arc::new(MemoryBudget::new(options.memory_limit));
    if !budget.try_charge(frame_bytes) {
        return Err(DevonError::BudgetExceeded {
            context: format!(
                "DEVONPACK open requires {frame_bytes} bytes for the decoded-frame buffer \
                 (frame_pages {} × page_size {}) but memory_limit is {} bytes",
                pack.frame_pages(),
                pack.page_size(),
                options.memory_limit
            ),
        });
    }
    Ok((pack, budget, frame_bytes))
}

impl Database {
    /// Writes the whole database as one DEVONPACK container at `out`
    /// (`docs/SCALE.md` §7.1): checkpoints first, so the WAL is empty and
    /// the main file is the whole database, then streams every main-file
    /// page — superblock slots included — into the container.
    ///
    /// `frame_pages` is writer policy (pages per zstd frame;
    /// `devondb_storage::pack::DEFAULT_FRAME_PAGES` when the caller has no
    /// opinion). Packing a read-only container is refused like any other
    /// mutator.
    pub fn pack(&mut self, out: impl AsRef<Path>, frame_pages: u32) -> DevonResult<()> {
        self.shared.require_writable("pack")?;
        self.checkpoint()?;
        self.shared.pager.sync()?;
        let out = out.as_ref();
        let mut file = File::create(out)?;
        pack_pages(&self.shared.pager, &mut file, frame_pages)?;
        file.sync_all()?;
        sync_parent_directory(out)?;
        Ok(())
    }
}

/// Makes the new container's directory entry durable, mirroring the pager's
/// create-time parent sync (`File::open` on a directory is a platform
/// capability; only a real sync failure is an error).
fn sync_parent_directory(path: &Path) -> DevonResult<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let Ok(directory) = File::open(parent) else {
        return Ok(());
    };
    directory.sync_all()?;
    Ok(())
}
