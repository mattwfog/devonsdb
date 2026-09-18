//! Pager-backend boundary tests (`docs/OBJECT_STORAGE.md` § The
//! pager-backend seam).
//!
//! The SAME open/read/write corpus runs through the `Local` backend (a
//! real file) and the `Memory` backend, byte for byte. Short-read and
//! I/O-error injection on the memory backend assert the pager's error
//! classes are unchanged. A source check pins the dispatch shape: the
//! pager must never hold a trait object.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use devondb_storage::backend::MemoryBackend;
use devondb_storage::budget::MemoryBudget;
use devondb_storage::pager::Pager;
use devondb_storage::superblock::DML_WAL_FLAG;
use devondb_types::DevonError;

const PAGE_SIZE: u32 = 4096;
const DB_ID: [u8; 16] = *b"backend-seam-tst";
const FRAME_OVERHEAD: usize = 64;
const CORPUS_PAGES: u64 = 8;

/// One logical database handle: local file or shared in-memory image.
enum Handle {
    Local { path: PathBuf },
    Memory { backend: MemoryBackend },
}

impl Handle {
    fn create(&self) -> Pager {
        match self {
            Self::Local { path } => Pager::create(path, PAGE_SIZE, DB_ID).unwrap(),
            Self::Memory { backend } => {
                Pager::create_memory_for_test(backend.clone(), PAGE_SIZE, DB_ID).unwrap()
            }
        }
    }

    fn open(&self) -> Pager {
        match self {
            Self::Local { path } => Pager::open(path).unwrap(),
            Self::Memory { backend } => Pager::open_memory_for_test(backend.clone()).unwrap(),
        }
    }
}

fn handles(test_name: &str) -> (tempfile::TempDir, Vec<Handle>) {
    let directory = tempfile::tempdir().unwrap();
    let local = Handle::Local {
        path: directory.path().join(format!("{test_name}.devondb")),
    };
    let memory = Handle::Memory {
        backend: MemoryBackend::new(),
    };
    (directory, vec![local, memory])
}

/// The shared corpus: create, allocate, write, read (plain and cached),
/// both publication paths, sync, drop, reopen, and byte comparison.
fn run_corpus(handle: &Handle) -> (devondb_storage::superblock::Superblock, Vec<Vec<u8>>) {
    let pager = handle.create();
    let page_ids: Vec<u64> = (0..CORPUS_PAGES)
        .map(|_| pager.allocate_page().unwrap())
        .collect();
    for (index, page_id) in page_ids.iter().enumerate() {
        let fill = u8::try_from(index + 1).unwrap();
        pager
            .write_page(*page_id, &vec![fill; PAGE_SIZE as usize])
            .unwrap();
    }
    for (index, page_id) in page_ids.iter().enumerate() {
        let page = pager.read_page(*page_id).unwrap();
        assert!(page.iter().all(|byte| *byte == index as u8 + 1));
    }

    // Exercise the cached read path (frame fill + hit) on the same bytes.
    let budget = Arc::new(MemoryBudget::new(
        (PAGE_SIZE as usize + FRAME_OVERHEAD) * 16,
    ));
    let cached = pager.with_budget(budget);
    for (index, page_id) in page_ids.iter().enumerate() {
        let page = cached.read_page_ref(*page_id).unwrap();
        assert!(page.iter().all(|byte| *byte == index as u8 + 1));
        let hit = cached.read_page_ref(*page_id).unwrap();
        assert_eq!(page, hit);
    }

    // Dual-slot publication, then a flag-only publication over both slots.
    let mut superblock = cached.superblock();
    superblock.checkpoint_lsn = 42;
    superblock.catalog_root = page_ids[0];
    cached.commit_superblock(superblock).unwrap();
    cached.commit_feature_flags(DML_WAL_FLAG).unwrap();
    cached.sync().unwrap();
    drop(cached);

    // Reopen and compare authoritative state plus every data page's bytes.
    let reopened = handle.open();
    let superblock = reopened.superblock();
    assert_eq!(superblock.checkpoint_lsn, 42);
    assert_eq!(superblock.catalog_root, page_ids[0]);
    assert_eq!(superblock.feature_flags, DML_WAL_FLAG);
    let pages = page_ids
        .iter()
        .map(|page_id| reopened.read_page(*page_id).unwrap())
        .collect();
    (superblock, pages)
}

#[test]
fn local_and_memory_backends_run_the_identical_corpus() {
    let (_directory, handles) = handles("corpus");
    let results: Vec<_> = handles.iter().map(run_corpus).collect();
    let (local_superblock, local_pages) = &results[0];
    let (memory_superblock, memory_pages) = &results[1];
    assert_eq!(local_superblock, memory_superblock);
    assert_eq!(local_pages, memory_pages);
}

#[test]
fn injected_io_error_preserves_the_pagers_error_class() {
    let backend = MemoryBackend::new();
    let pager = Pager::create_memory_for_test(backend.clone(), PAGE_SIZE, DB_ID).unwrap();
    let page_id = pager.allocate_page().unwrap();
    pager
        .write_page(page_id, &vec![7; PAGE_SIZE as usize])
        .unwrap();

    backend.fail_with(Some(io::ErrorKind::PermissionDenied));
    assert!(matches!(
        pager.write_page(page_id, &vec![8; PAGE_SIZE as usize]),
        Err(DevonError::Io(_))
    ));
    assert!(matches!(pager.read_page(page_id), Err(DevonError::Io(_))));
    assert!(matches!(pager.sync(), Err(DevonError::Io(_))));

    // Clearing the fault restores full function on the same image.
    backend.fail_with(None);
    pager
        .write_page(page_id, &vec![9; PAGE_SIZE as usize])
        .unwrap();
    assert!(
        pager
            .read_page(page_id)
            .unwrap()
            .iter()
            .all(|byte| *byte == 9)
    );
    pager.sync().unwrap();
}

#[test]
fn short_read_keeps_the_io_error_class_on_both_backends() {
    let (_directory, handles) = handles("short-read");
    for handle in &handles {
        let pager = handle.create();
        drop(pager);
        // Truncate the image mid-header: the superblock read at open hits
        // the backend's exact-read contract, never partially filled bytes.
        match handle {
            Handle::Local { path } => {
                fs::OpenOptions::new()
                    .write(true)
                    .open(path)
                    .unwrap()
                    .set_len(32)
                    .unwrap();
            }
            Handle::Memory { backend } => backend.truncate(32),
        }
        let error = match handle {
            Handle::Local { path } => Pager::open(path).err().unwrap(),
            Handle::Memory { backend } => {
                Pager::open_memory_for_test(backend.clone()).err().unwrap()
            }
        };
        assert!(
            matches!(error, DevonError::Io(_)),
            "short read must stay an I/O error, got {error}"
        );
    }
}

#[test]
fn reads_beyond_the_end_stay_invalid_argument_on_both_backends() {
    let (_directory, handles) = handles("beyond-end");
    for handle in &handles {
        let pager = handle.create();
        assert!(matches!(
            pager.read_page(2),
            Err(DevonError::InvalidArgument { .. })
        ));
    }
}

/// Dispatch-shape pin: the pager must route bytes through the closed
/// `Backend` enum, never a trait object (`docs/OBJECT_STORAGE.md`
/// § Dispatch shape: static enum dispatch, no boxed dynamic dispatch).
#[test]
fn pager_source_holds_no_backend_trait_object() {
    let source_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    for file in ["pager.rs", "backend.rs"] {
        let source = fs::read_to_string(source_root.join(file)).unwrap();
        assert!(
            !source.contains("dyn PagerBackend"),
            "{file} must use closed-enum static dispatch, not a trait object"
        );
        assert!(
            !source.contains("Box<dyn"),
            "{file} must not box any trait object for backend dispatch"
        );
    }
}
