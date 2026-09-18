//! The `Pack` byte backend: the pager's read side over a DEVONPACK
//! container (`docs/SCALE.md` §7.2, BINDING), behind the `pack` feature.
//!
//! One decoded-frame buffer (`frame_pages × page_size` bytes — the
//! documented, fixed working set, charged to the memory budget once at open
//! by the facade) holds the last decoded frame; touching a second frame
//! evicts the first. The frame crc32c is verified BEFORE decode, and
//! `write_all_at`/`sync_all` return [`DevonError::ReadOnly`] — the second
//! fence of `docs/OBJECT_STORAGE.md` § Read-only enforcement.

use std::sync::{Mutex, MutexGuard, PoisonError};

use devondb_types::{DevonError, DevonResult};

use super::{Backend, PagerBackend};
use crate::pack::PackFile;
use crate::pager::Pager;

/// The read-only pager backend over an open [`PackFile`].
pub(crate) struct PackBackend {
    pack: PackFile,
    /// The ONE decoded-frame buffer; `None` until the first read. Interior
    /// mutability is required because `PagerBackend` reads take `&self`.
    decoded: Mutex<Option<DecodedFrame>>,
}

struct DecodedFrame {
    index: u64,
    bytes: Vec<u8>,
}

impl PackBackend {
    pub(crate) fn new(pack: PackFile) -> Self {
        Self {
            pack,
            decoded: Mutex::new(None),
        }
    }

    /// Returns the decoded bytes of `frame_index`, decoding (after crc
    /// verification) on a buffer miss and evicting the previous frame.
    fn decoded_frame(&self, frame_index: u64) -> DevonResult<MutexGuard<'_, Option<DecodedFrame>>> {
        let mut decoded = self.lock_decoded();
        let cached = matches!(decoded.as_ref(), Some(frame) if frame.index == frame_index);
        if !cached {
            let bytes = self.pack.read_frame(frame_index)?;
            *decoded = Some(DecodedFrame {
                index: frame_index,
                bytes,
            });
        }
        Ok(decoded)
    }

    fn lock_decoded(&self) -> MutexGuard<'_, Option<DecodedFrame>> {
        self.decoded.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl PagerBackend for PackBackend {
    fn len(&self) -> DevonResult<u64> {
        Ok(self.pack.byte_len())
    }

    fn read_exact_at(&self, offset: u64, dst: &mut [u8]) -> DevonResult<()> {
        if offset
            .checked_add(dst.len() as u64)
            .is_none_or(|end| end > self.pack.byte_len())
        {
            return Err(DevonError::Io(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "pack read beyond the container's logical length",
            )));
        }
        let frame_span = self.pack.frame_bytes() as u64;
        let mut written = 0_usize;
        while written < dst.len() {
            let position = offset + written as u64;
            let frame_index = position / frame_span;
            let in_frame = (position % frame_span) as usize;
            let decoded = self.decoded_frame(frame_index)?;
            let frame = decoded.as_ref().ok_or_else(|| DevonError::Corrupt {
                context: format!("pack frame {frame_index} missing after decode"),
            })?;
            let take = (dst.len() - written).min(frame.bytes.len() - in_frame);
            dst[written..written + take].copy_from_slice(&frame.bytes[in_frame..in_frame + take]);
            written += take;
            drop(decoded);
        }
        Ok(())
    }

    fn write_all_at(&self, _offset: u64, _src: &[u8]) -> DevonResult<()> {
        Err(read_only())
    }

    fn sync_all(&self) -> DevonResult<()> {
        Err(read_only())
    }
}

impl Pager {
    /// Opens a DEVONPACK container through the normal pager open sequence
    /// (superblock arbitration, length validation, free-page state) on the
    /// read-only `Pack` backend (`docs/SCALE.md` §7.2).
    pub fn open_pack(pack: PackFile) -> DevonResult<Self> {
        Self::open_backend(Backend::Pack(PackBackend::new(pack)))
    }
}

fn read_only() -> DevonError {
    DevonError::ReadOnly {
        context: "writes and syncs are refused: a DEVONPACK container is read-only \
                  (docs/SCALE.md §7.2)"
            .to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::PackBackend;
    use crate::backend::{MemoryBackend, PagerBackend};
    use crate::pack::{PackFile, pack_pages};
    use crate::pager::Pager;

    const PAGE_SIZE: u32 = 4096;
    const DB_ID: [u8; 16] = *b"pack-backend-ut0";

    /// A pack with 5 pages of distinct patterns in 2-page frames; the
    /// source image stays readable for byte-for-byte comparison.
    fn pack_backend(directory: &tempfile::TempDir) -> (PackBackend, MemoryBackend) {
        let image = MemoryBackend::new();
        let source = Pager::create_memory_for_test(image.clone(), PAGE_SIZE, DB_ID).unwrap();
        for page_id in 2..5 {
            let mut page = vec![0_u8; PAGE_SIZE as usize];
            page.fill(page_id as u8);
            source.write_page(page_id, &page).unwrap();
        }
        let mut bytes = Vec::new();
        pack_pages(&source, &mut bytes, 2).unwrap();
        let path = directory.path().join("unit.devonpack");
        std::fs::write(&path, &bytes).unwrap();
        let pack = PackFile::open(&path).unwrap();
        let byte_len = pack.byte_len();
        let backend = PackBackend::new(pack);
        assert_eq!(backend.len().unwrap(), byte_len);
        (backend, image)
    }

    #[test]
    fn read_span_crossing_frame_boundaries_matches_source_bytes() {
        let directory = tempfile::tempdir().unwrap();
        let (backend, image) = pack_backend(&directory);

        // From mid-frame 0 (page 1) through mid-frame 2 (page 4): three
        // frame decodes behind one continuous span.
        let page = PAGE_SIZE as u64;
        let offset = page + page / 2;
        let mut span = vec![0_u8; (3 * page) as usize];
        backend.read_exact_at(offset, &mut span).unwrap();
        let mut expected = vec![0_u8; span.len()];
        image.read_exact_at(offset, &mut expected).unwrap();
        assert_eq!(span, expected, "cross-frame span differs from source");

        // Page-aligned reads out of order: every read evicts the buffer.
        for page_id in [4, 2, 3, 4, 2] {
            let mut page_bytes = vec![0_u8; PAGE_SIZE as usize];
            backend
                .read_exact_at(page_id * page, &mut page_bytes)
                .unwrap();
            let mut expected = vec![0_u8; PAGE_SIZE as usize];
            image.read_exact_at(page_id * page, &mut expected).unwrap();
            assert_eq!(page_bytes, expected, "page {page_id} differs");
        }
    }

    #[test]
    fn read_past_logical_end_is_a_short_read_error() {
        let directory = tempfile::tempdir().unwrap();
        let (backend, _) = pack_backend(&directory);
        let mut byte = [0_u8; 1];
        let error = backend
            .read_exact_at(backend.len().unwrap(), &mut byte)
            .unwrap_err();
        assert!(matches!(
            error,
            devondb_types::DevonError::Io(ref io) if io.kind() == std::io::ErrorKind::UnexpectedEof
        ));
    }
}
