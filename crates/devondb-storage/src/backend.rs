//! Byte backends under the pager (`docs/OBJECT_STORAGE.md` § The
//! pager-backend seam, binding).
//!
//! The pager performs exactly four kinds of byte operation on an already
//! constructed backend — length, positional exact read, positional exact
//! write, durable sync — captured by [`PagerBackend`]. Backend
//! construction and local directory durability stay in the pager's
//! factory code: HTTP and OPFS targets must not pretend they have a
//! parent directory or a Rust `Path`.
//!
//! Dispatch is a closed enum with one exhaustive match per method —
//! static dispatch, never a boxed trait object (`docs/OBJECT_STORAGE.md`
//! § Dispatch shape). With only the local variant compiled in, the
//! compiler can erase the match.

use std::fs::File;
use std::io;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use devondb_types::{DevonError, DevonResult};

/// The four byte operations the pager performs on an already-constructed
/// backend (`docs/OBJECT_STORAGE.md` § The pager-backend seam, verbatim).
///
/// Deliberately no `open`, `create`, `path`, `metadata`, `seek`, HTTP
/// range, ETag, cache, prefetch, or async method: factories construct a
/// backend. `read_exact_at` accepts arbitrary lengths, so the pager may
/// turn one multi-page miss window into one backend request. The
/// exact-read contract preserves the pager's short-read law: the backend
/// returns an I/O/protocol failure and never exposes partially filled
/// buffers.
///
/// `Send + Sync` bounds live on the native containing types, not on this
/// portable trait: a single-worker wasm32 OPFS handle has a different
/// threading model.
pub(crate) trait PagerBackend {
    /// The total byte length of the backend.
    fn len(&self) -> DevonResult<u64>;
    /// Reads exactly `dst.len()` bytes at `offset`; a short read is an
    /// I/O error and never leaves partially filled bytes behind.
    fn read_exact_at(&self, offset: u64, dst: &mut [u8]) -> DevonResult<()>;
    /// Writes all of `src` at `offset`, extending the backend when the
    /// write runs past the current end.
    fn write_all_at(&self, offset: u64, src: &[u8]) -> DevonResult<()>;
    /// Flushes all bytes and metadata to durable storage.
    fn sync_all(&self) -> DevonResult<()>;
}

#[cfg(feature = "pack")]
mod pack;

#[cfg(feature = "pack")]
use pack::PackBackend;

/// The closed set of byte backends a native `Pager` can hold — static
/// enum dispatch, never a boxed trait object (`docs/OBJECT_STORAGE.md`
/// § Dispatch shape).
pub(crate) enum Backend {
    /// A local filesystem file — the only shipping variant today.
    Local(LocalFileBackend),
    /// The read-only DEVONPACK distribution container (`docs/SCALE.md` §7).
    #[cfg(feature = "pack")]
    Pack(PackBackend),
    /// In-memory bytes for deterministic pager tests.
    #[doc(hidden)]
    Memory(MemoryBackend),
}

impl PagerBackend for Backend {
    fn len(&self) -> DevonResult<u64> {
        match self {
            Self::Local(backend) => backend.len(),
            #[cfg(feature = "pack")]
            Self::Pack(backend) => backend.len(),
            Self::Memory(backend) => backend.len(),
        }
    }

    fn read_exact_at(&self, offset: u64, dst: &mut [u8]) -> DevonResult<()> {
        match self {
            Self::Local(backend) => backend.read_exact_at(offset, dst),
            #[cfg(feature = "pack")]
            Self::Pack(backend) => backend.read_exact_at(offset, dst),
            Self::Memory(backend) => backend.read_exact_at(offset, dst),
        }
    }

    fn write_all_at(&self, offset: u64, src: &[u8]) -> DevonResult<()> {
        match self {
            Self::Local(backend) => backend.write_all_at(offset, src),
            #[cfg(feature = "pack")]
            Self::Pack(backend) => backend.write_all_at(offset, src),
            Self::Memory(backend) => backend.write_all_at(offset, src),
        }
    }

    fn sync_all(&self) -> DevonResult<()> {
        match self {
            Self::Local(backend) => backend.sync_all(),
            #[cfg(feature = "pack")]
            Self::Pack(backend) => backend.sync_all(),
            Self::Memory(backend) => backend.sync_all(),
        }
    }
}

/// Positional file I/O seam.
///
/// devondb targets unix today; a windows port implements these two
/// functions over `std::os::windows::fs::FileExt` (`seek_read` /
/// `seek_write` loops) without touching any caller.
mod positional_io {
    use std::fs::File;
    use std::io;
    use std::os::unix::fs::FileExt;

    /// Reads exactly `buf.len()` bytes at `offset` without moving any cursor.
    pub fn read_exact_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<()> {
        FileExt::read_exact_at(file, buf, offset)
    }

    /// Writes all of `buf` at `offset` without moving any cursor.
    pub fn write_all_at(file: &File, buf: &[u8], offset: u64) -> io::Result<()> {
        FileExt::write_all_at(file, buf, offset)
    }
}

/// The local filesystem backend: positional I/O over the `File` the pager
/// holds today, byte-identical to the pre-seam behavior (`FileExt` on
/// unix; a short read is an I/O error, never partial bytes).
pub(crate) struct LocalFileBackend {
    file: File,
}

impl LocalFileBackend {
    /// Wraps an already-open database file.
    pub(crate) fn new(file: File) -> Self {
        Self { file }
    }
}

impl PagerBackend for LocalFileBackend {
    fn len(&self) -> DevonResult<u64> {
        Ok(self.file.metadata()?.len())
    }

    fn read_exact_at(&self, offset: u64, dst: &mut [u8]) -> DevonResult<()> {
        positional_io::read_exact_at(&self.file, dst, offset)?;
        Ok(())
    }

    fn write_all_at(&self, offset: u64, src: &[u8]) -> DevonResult<()> {
        positional_io::write_all_at(&self.file, src, offset)?;
        Ok(())
    }

    fn sync_all(&self) -> DevonResult<()> {
        self.file.sync_all()?;
        Ok(())
    }
}

/// In-memory [`PagerBackend`] for deterministic pager tests: a growable
/// byte vector behind a mutex with injectable I/O failure. Clones share
/// the same bytes, so a test can "reopen" the same logical image. Local
/// semantics are mirrored exactly: a write past the end extends with
/// zeroes, and a read past the end is an `UnexpectedEof` I/O error that
/// never partially fills the destination.
#[derive(Clone, Debug, Default)]
#[doc(hidden)]
pub struct MemoryBackend {
    inner: Arc<MemoryInner>,
}

#[derive(Debug, Default)]
struct MemoryInner {
    bytes: Mutex<Vec<u8>>,
    fail_with: Mutex<Option<io::ErrorKind>>,
}

impl MemoryBackend {
    /// An empty in-memory image.
    #[doc(hidden)]
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Makes every subsequent backend operation fail with
    /// `Some(kind)` until cleared with `None`.
    #[doc(hidden)]
    pub fn fail_with(&self, kind: Option<io::ErrorKind>) {
        *self.lock_fail_with() = kind;
    }

    /// Truncates the image to `len` bytes (short-read injection).
    #[doc(hidden)]
    pub fn truncate(&self, len: u64) {
        self.lock_bytes()
            .truncate(usize::try_from(len).unwrap_or(usize::MAX));
    }

    fn injected_fault(&self) -> DevonResult<()> {
        match *self.lock_fail_with() {
            Some(kind) => Err(DevonError::Io(io::Error::new(
                kind,
                "injected memory-backend fault",
            ))),
            None => Ok(()),
        }
    }

    fn lock_bytes(&self) -> MutexGuard<'_, Vec<u8>> {
        self.inner
            .bytes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn lock_fail_with(&self) -> MutexGuard<'_, Option<io::ErrorKind>> {
        self.inner
            .fail_with
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

impl PagerBackend for MemoryBackend {
    fn len(&self) -> DevonResult<u64> {
        self.injected_fault()?;
        Ok(self.lock_bytes().len() as u64)
    }

    fn read_exact_at(&self, offset: u64, dst: &mut [u8]) -> DevonResult<()> {
        self.injected_fault()?;
        let bytes = self.lock_bytes();
        let range = checked_range(offset, dst.len())?;
        let Some(src) = bytes.get(range.clone()) else {
            return Err(short_read());
        };
        dst.copy_from_slice(src);
        Ok(())
    }

    fn write_all_at(&self, offset: u64, src: &[u8]) -> DevonResult<()> {
        self.injected_fault()?;
        let mut bytes = self.lock_bytes();
        let range = checked_range(offset, src.len())?;
        if range.end > bytes.len() {
            bytes.resize(range.end, 0);
        }
        bytes[range].copy_from_slice(src);
        Ok(())
    }

    fn sync_all(&self) -> DevonResult<()> {
        self.injected_fault()
    }
}

fn checked_range(offset: u64, len: usize) -> DevonResult<std::ops::Range<usize>> {
    let start = usize::try_from(offset)
        .map_err(|_| DevonError::Io(io::Error::other("memory-backend offset overflows usize")))?;
    let end = start
        .checked_add(len)
        .ok_or_else(|| DevonError::Io(io::Error::other("memory-backend range overflows usize")))?;
    Ok(start..end)
}

fn short_read() -> DevonError {
    DevonError::Io(io::Error::new(
        io::ErrorKind::UnexpectedEof,
        "memory-backend short read",
    ))
}

// The native containing types are safe for the pager's concurrent `&self`
// use; the portable trait itself carries no such bound
// (`docs/OBJECT_STORAGE.md` § The pager-backend seam).
const _: () = {
    const fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Backend>();
    assert_send_sync::<LocalFileBackend>();
    #[cfg(feature = "pack")]
    assert_send_sync::<PackBackend>();
    assert_send_sync::<MemoryBackend>();
};

#[cfg(test)]
mod tests {
    use super::{MemoryBackend, PagerBackend};

    #[test]
    fn memory_backend_extends_with_zeroes_on_sparse_write() {
        let backend = MemoryBackend::new();
        backend.write_all_at(4, &[1, 2, 3]).unwrap();
        assert_eq!(backend.len().unwrap(), 7);
        let mut dst = vec![0xff; 7];
        backend.read_exact_at(0, &mut dst).unwrap();
        assert_eq!(dst, vec![0, 0, 0, 0, 1, 2, 3]);
    }

    #[test]
    fn memory_backend_short_read_errors_without_partial_bytes() {
        let backend = MemoryBackend::new();
        backend.write_all_at(0, &[1, 2, 3]).unwrap();
        let mut dst = vec![0xff; 8];
        let error = backend.read_exact_at(0, &mut dst).err().unwrap();
        assert!(matches!(error, devondb_types::DevonError::Io(_)));
        assert_eq!(dst, vec![0xff; 8], "no partially initialized bytes");
    }

    #[test]
    fn memory_backend_injected_fault_reaches_every_method() {
        let backend = MemoryBackend::new();
        backend.fail_with(Some(std::io::ErrorKind::PermissionDenied));
        assert!(matches!(
            backend.len(),
            Err(devondb_types::DevonError::Io(_))
        ));
        assert!(matches!(
            backend.read_exact_at(0, &mut [0; 1]),
            Err(devondb_types::DevonError::Io(_))
        ));
        assert!(matches!(
            backend.write_all_at(0, &[0]),
            Err(devondb_types::DevonError::Io(_))
        ));
        assert!(matches!(
            backend.sync_all(),
            Err(devondb_types::DevonError::Io(_))
        ));
        backend.fail_with(None);
        assert_eq!(backend.len().unwrap(), 0);
    }

    #[test]
    fn memory_backend_clones_share_bytes() {
        let backend = MemoryBackend::new();
        backend.write_all_at(0, &[9; 4]).unwrap();
        let clone = backend.clone();
        let mut dst = [0_u8; 4];
        clone.read_exact_at(0, &mut dst).unwrap();
        assert_eq!(dst, [9; 4]);
        clone.truncate(2);
        assert_eq!(backend.len().unwrap(), 2);
    }
}
