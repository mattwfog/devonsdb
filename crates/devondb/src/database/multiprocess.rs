use super::*;

use super::checkpoint::checkpoint_locked;
use super::options::lock;
use devondb_storage::{
    lock::{LockPaths, PublicationGate},
    superblock::MULTIPROCESS_COORDINATION_FLAG,
};

/// Activates cross-process coordination for an existing database file.
///
/// Activation is an offline, idempotent operation. It refuses a live
/// cooperating writer or reader and a file that this build may only open
/// read-only.
pub fn activate_multiprocess(path: impl AsRef<Path>) -> DevonResult<()> {
    let path = path.as_ref();
    let lock_paths = LockPaths::for_main(path)?;
    let publication_gate = PublicationGate::open(&lock_paths)?;
    let _publication_guard = publication_gate.try_exclusive()?;
    let database = Database::open(path)?;
    database
        .shared
        .require_writable("multiprocess activation")?;
    let flags = database.shared.pager.superblock().feature_flags;
    if flags & MULTIPROCESS_COORDINATION_FLAG != 0 {
        return Ok(());
    }

    let mut pipe = lock(&database.shared.commit);
    database
        .shared
        .pager
        .commit_feature_flags(flags | MULTIPROCESS_COORDINATION_FLAG)?;
    checkpoint_locked(&database.shared, &mut pipe)
}

impl Database {
    /// Activates cross-process coordination for an existing database file.
    ///
    /// The caller must ensure no legacy process has the file open: legacy
    /// binaries do not participate in the lock protocol and cannot be evicted.
    pub fn activate_multiprocess(path: impl AsRef<Path>) -> DevonResult<()> {
        activate_multiprocess(path)
    }
}
