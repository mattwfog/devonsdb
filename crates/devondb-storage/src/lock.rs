//! Cross-process coordination locks: the writer lease and publication gate.
//!
//! Semantics are specified in `docs/MULTIPROCESS.md` (BINDING) — option A
//! with its open-question resolutions: whole-file std file locks (flock
//! semantics) on sidecars derived from the canonicalized main-file path
//! (`<canonical-main>.lock-writer`, `<canonical-main>.lock-publish`),
//! every acquisition nonblocking (invariant 2), and lock files never
//! unlinked during normal operation (invariant 8).

use std::fs::{File, OpenOptions, TryLockError};
use std::path::{Path, PathBuf};

use devondb_types::{DevonError, DevonResult};

const WRITER_SUFFIX: &str = ".lock-writer";
const PUBLISH_SUFFIX: &str = ".lock-publish";

/// Canonical sidecar paths used to coordinate access to one database file.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LockPaths {
    /// Sidecar on which a writer holds its lifetime exclusive lease.
    pub writer: PathBuf,
    /// Sidecar used for short-lived shared and exclusive publication locks.
    pub publish: PathBuf,
}

impl LockPaths {
    /// Derives both sidecars from the canonical identity of an existing main file.
    pub fn for_main(main: &Path) -> DevonResult<LockPaths> {
        let canonical = std::fs::canonicalize(main)?;
        Ok(Self {
            writer: append_suffix(&canonical, WRITER_SUFFIX),
            publish: append_suffix(&canonical, PUBLISH_SUFFIX),
        })
    }
}

/// A nonblocking, exclusive writer lease held for this value's lifetime.
///
/// The writer sidecar is persistent coordination infrastructure. Dropping a
/// lease releases its kernel lock but never unlinks the sidecar.
#[derive(Debug)]
pub struct WriterLease {
    file: File,
}

impl WriterLease {
    /// Tries to acquire the database's exclusive writer lease without waiting.
    pub fn try_acquire(paths: &LockPaths) -> DevonResult<WriterLease> {
        let file = open_sidecar(&paths.writer)?;
        map_try_lock(file.try_lock(), "writer lease", &paths.writer)?;
        Ok(Self { file })
    }
}

impl Drop for WriterLease {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

/// An open handle for short-lived publication coordination locks.
///
/// The publication sidecar is reused permanently. Neither this handle nor its
/// guards unlink it when they are dropped.
#[derive(Debug)]
pub struct PublicationGate {
    file: File,
    path: PathBuf,
}

impl PublicationGate {
    /// Opens or creates the publication sidecar without taking a lock.
    pub fn open(paths: &LockPaths) -> DevonResult<PublicationGate> {
        Ok(Self {
            file: open_sidecar(&paths.publish)?,
            path: paths.publish.clone(),
        })
    }

    /// Tries to take the publication gate exclusively without waiting.
    pub fn try_exclusive(&self) -> DevonResult<PublicationGuard<'_>> {
        map_try_lock(self.file.try_lock(), "publication gate", &self.path)?;
        Ok(PublicationGuard { file: &self.file })
    }

    /// Tries to take the publication gate in shared mode without waiting.
    pub fn try_shared(&self) -> DevonResult<PublicationGuard<'_>> {
        map_try_lock(self.file.try_lock_shared(), "publication gate", &self.path)?;
        Ok(PublicationGuard { file: &self.file })
    }
}

/// A shared or exclusive publication lock released when the guard is dropped.
#[derive(Debug)]
pub struct PublicationGuard<'a> {
    file: &'a File,
}

impl Drop for PublicationGuard<'_> {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

fn append_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut suffixed = path.as_os_str().to_os_string();
    suffixed.push(suffix);
    suffixed.into()
}

fn open_sidecar(path: &Path) -> DevonResult<File> {
    Ok(OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?)
}

fn map_try_lock(result: Result<(), TryLockError>, role: &str, path: &Path) -> DevonResult<()> {
    match result {
        Ok(()) => Ok(()),
        Err(TryLockError::WouldBlock) => Err(DevonError::Busy {
            context: format!("{role} is held for {}", path.display()),
        }),
        Err(TryLockError::Error(error)) => Err(DevonError::Io(error)),
    }
}
