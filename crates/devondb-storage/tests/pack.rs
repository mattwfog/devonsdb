//! DEVONPACK container tests (`docs/SCALE.md` §7): layout goldens,
//! the full corruption matrix with exact messages, truncation at every
//! region, a short last frame, the backend's read-only fence, and
//! byte-for-byte random-access equivalence with the source pages.
#![cfg(feature = "pack")]

use std::fs;
use std::path::PathBuf;

use devondb_storage::backend::MemoryBackend;
use devondb_storage::pack::{
    CODEC_ZSTD, CONTAINER_VERSION, DEFAULT_FRAME_PAGES, DIRECTORY_CRC_LEN, DIRECTORY_ENTRY_LEN,
    HEADER_LEN, PACK_MAGIC, PackFile, pack_pages,
};
use devondb_storage::pager::Pager;
use devondb_types::DevonError;

const PAGE_SIZE: u32 = 4096;
const DB_ID: [u8; 16] = *b"pack-container-t";

/// A memory-backed source pager whose data pages carry a distinct pattern.
fn source_pager(page_count: u64) -> Pager {
    let pager = Pager::create_memory_for_test(MemoryBackend::new(), PAGE_SIZE, DB_ID).unwrap();
    for page_id in 2..page_count {
        let mut page = vec![0_u8; PAGE_SIZE as usize];
        for (index, byte) in page.iter_mut().enumerate() {
            *byte = (page_id as u8) ^ (index % 251) as u8;
        }
        pager.write_page(page_id, &page).unwrap();
    }
    pager
}

/// Packs `source` into an in-memory container image.
fn pack_bytes(source: &Pager, frame_pages: u32) -> Vec<u8> {
    let mut bytes = Vec::new();
    pack_pages(source, &mut bytes, frame_pages).unwrap();
    bytes
}

/// Writes a container image to a temp file and opens it as a [`PackFile`].
fn open_pack(directory: &tempfile::TempDir, bytes: &[u8]) -> PackFile {
    let path = directory.path().join("test.devonpack");
    fs::write(&path, bytes).unwrap();
    PackFile::open(&path).unwrap()
}

/// Opens a container image through the pager, on the `Pack` backend.
fn open_pager(directory: &tempfile::TempDir, bytes: &[u8]) -> Pager {
    let path = directory.path().join("test.devonpack");
    fs::write(&path, bytes).unwrap();
    Pager::open_pack(PackFile::open(&path).unwrap()).unwrap()
}

/// The error a mutated container image produces at [`PackFile::open`].
fn open_error(directory: &tempfile::TempDir, bytes: &[u8]) -> DevonError {
    let path = directory.path().join("mutated.devonpack");
    fs::write(&path, bytes).unwrap();
    PackFile::open(&path).unwrap_err()
}

fn assert_corrupt(error: &DevonError, expected: &str) {
    match error {
        DevonError::Corrupt { context } => {
            assert_eq!(context, expected, "wrong corruption message")
        }
        other => panic!("expected Corrupt, got {other:?}"),
    }
}

/// Recomputes the header crc after mutating a header field, so the mutation
/// under test is reached instead of the header crc class.
fn fix_header_crc(bytes: &mut [u8]) {
    let crc = crc32c::crc32c(&bytes[..36]);
    bytes[36..40].copy_from_slice(&crc.to_le_bytes());
}

#[test]
fn header_layout_is_frozen_for_a_fixed_input() {
    let directory = tempfile::tempdir().unwrap();
    let source = source_pager(3);
    let bytes = pack_bytes(&source, 2);

    let mut expected = Vec::new();
    expected.extend_from_slice(&PACK_MAGIC);
    expected.push(CONTAINER_VERSION);
    expected.extend_from_slice(&CODEC_ZSTD.to_le_bytes());
    expected.extend_from_slice(&PAGE_SIZE.to_le_bytes());
    expected.extend_from_slice(&3_u64.to_le_bytes());
    expected.extend_from_slice(&2_u32.to_le_bytes());
    expected.extend_from_slice(&2_u64.to_le_bytes());
    assert_eq!(expected.len(), 36);
    assert_eq!(bytes[..36], expected[..], "header content bytes moved");
    assert_eq!(
        bytes[36..40],
        crc32c::crc32c(&bytes[..36]).to_le_bytes(),
        "header crc32c must cover the 36 content bytes"
    );

    // Directory: two 16-byte entries, then its crc32c, then the frames.
    let directory_end = HEADER_LEN + 2 * DIRECTORY_ENTRY_LEN;
    let first_offset = u64::from_le_bytes(bytes[HEADER_LEN..HEADER_LEN + 8].try_into().unwrap());
    assert_eq!(first_offset, (directory_end + DIRECTORY_CRC_LEN) as u64);
    let first_len = u32::from_le_bytes(bytes[HEADER_LEN + 8..HEADER_LEN + 12].try_into().unwrap());
    let second_offset =
        u64::from_le_bytes(bytes[HEADER_LEN + 16..HEADER_LEN + 24].try_into().unwrap());
    assert_eq!(second_offset, first_offset + u64::from(first_len));
    let directory_crc = u32::from_le_bytes(
        bytes[directory_end..directory_end + DIRECTORY_CRC_LEN]
            .try_into()
            .unwrap(),
    );
    assert_eq!(
        directory_crc,
        crc32c::crc32c(&bytes[HEADER_LEN..directory_end]),
        "directory crc32c must cover the raw entry bytes"
    );

    let pack = open_pack(&directory, &bytes);
    assert_eq!(pack.page_size(), PAGE_SIZE);
    assert_eq!(pack.page_count(), 3);
    assert_eq!(pack.frame_pages(), 2);
    assert_eq!(pack.byte_len(), 3 * u64::from(PAGE_SIZE));
    assert_eq!(pack.frame_bytes(), 2 * PAGE_SIZE as usize);
}

#[test]
fn default_frame_policy_is_256_pages() {
    assert_eq!(DEFAULT_FRAME_PAGES, 256, "docs/SCALE.md §7.1 default");
}

#[test]
fn three_frame_file_with_short_last_frame_round_trips() {
    let directory = tempfile::tempdir().unwrap();
    let source = source_pager(5);
    let bytes = pack_bytes(&source, 2);
    let pack = open_pack(&directory, &bytes);
    assert_eq!(pack.page_count(), 5);
    assert_eq!(pack.frame_pages(), 2);

    let packed = open_pager(&directory, &bytes);
    for page_id in 2..5 {
        assert_eq!(
            packed.read_page(page_id).unwrap(),
            source.read_page(page_id).unwrap(),
            "page {page_id} must survive the short-last-frame container"
        );
    }
}

#[test]
fn random_access_reads_across_frames_equal_source_pages() {
    let directory = tempfile::tempdir().unwrap();
    let source = source_pager(9);
    let bytes = pack_bytes(&source, 2);
    let packed = open_pager(&directory, &bytes);

    // Shuffled order crosses frame boundaries repeatedly, exercising the
    // one-frame buffer's eviction on every other read.
    for page_id in [8, 2, 7, 3, 6, 4, 5, 2, 8, 4] {
        assert_eq!(
            packed.read_page(page_id).unwrap(),
            source.read_page(page_id).unwrap(),
            "page {page_id} differs from the source"
        );
    }
    assert_eq!(packed.superblock().db_id, source.superblock().db_id);
}

#[test]
fn writes_and_syncs_hit_the_backend_read_only_fence() {
    let directory = tempfile::tempdir().unwrap();
    let bytes = pack_bytes(&source_pager(4), DEFAULT_FRAME_PAGES);
    let packed = open_pager(&directory, &bytes);

    let page = vec![0_u8; PAGE_SIZE as usize];
    let write = packed.write_page(2, &page).unwrap_err();
    assert!(
        matches!(write, DevonError::ReadOnly { ref context } if context.contains("DEVONPACK")),
        "write_page must hit the backend fence: {write:?}"
    );
    let sync = packed.sync().unwrap_err();
    assert!(
        matches!(sync, DevonError::ReadOnly { ref context } if context.contains("DEVONPACK")),
        "sync must hit the backend fence: {sync:?}"
    );
    let allocate = packed.allocate_page().unwrap_err();
    assert!(
        matches!(allocate, DevonError::ReadOnly { .. }),
        "allocate_page must fail before returning a page id: {allocate:?}"
    );
}

#[test]
fn corrupt_magic_version_and_codec_are_named() {
    let directory = tempfile::tempdir().unwrap();
    let bytes = pack_bytes(&source_pager(3), 2);

    let mut magic = bytes.clone();
    magic[0] = b'X';
    assert_corrupt(
        &open_error(&directory, &magic),
        "bad pack magic: the header does not start with DEVONPACK",
    );

    let mut version = bytes.clone();
    version[9] = 2;
    fix_header_crc(&mut version);
    assert_corrupt(
        &open_error(&directory, &version),
        "unsupported pack container version 2: this build reads version 1",
    );

    let mut codec = bytes.clone();
    codec[10..12].copy_from_slice(&7_u16.to_le_bytes());
    fix_header_crc(&mut codec);
    assert_corrupt(
        &open_error(&directory, &codec),
        "unsupported pack codec 7: this build reads codec 1 (zstd)",
    );
}

#[test]
fn corrupt_header_crc_and_page_size_are_named() {
    let directory = tempfile::tempdir().unwrap();
    let bytes = pack_bytes(&source_pager(3), 2);

    let mut crc = bytes.clone();
    crc[36] ^= 0xff;
    let stored = u32::from_le_bytes(crc[36..40].try_into().unwrap());
    let computed = crc32c::crc32c(&crc[..36]);
    assert_corrupt(
        &open_error(&directory, &crc),
        &format!("pack header crc32c mismatch: stored {stored:#010x}, computed {computed:#010x}"),
    );

    let mut page_size = bytes.clone();
    page_size[12..16].copy_from_slice(&8191_u32.to_le_bytes());
    fix_header_crc(&mut page_size);
    assert_corrupt(
        &open_error(&directory, &page_size),
        "pack header page_size 8191 is not a power of two between 4096 and 65536",
    );
}

#[test]
fn corrupt_directory_crc_is_named() {
    let directory = tempfile::tempdir().unwrap();
    let bytes = pack_bytes(&source_pager(3), 2);
    let directory_end = HEADER_LEN + 2 * DIRECTORY_ENTRY_LEN;

    let mut mutated = bytes.clone();
    mutated[directory_end] ^= 0xff;
    let stored = u32::from_le_bytes(
        mutated[directory_end..directory_end + DIRECTORY_CRC_LEN]
            .try_into()
            .unwrap(),
    );
    let computed = crc32c::crc32c(&mutated[HEADER_LEN..directory_end]);
    assert_corrupt(
        &open_error(&directory, &mutated),
        &format!(
            "pack directory crc32c mismatch: stored {stored:#010x}, computed {computed:#010x}"
        ),
    );
}

#[test]
fn disagreeing_page_count_is_named() {
    let directory = tempfile::tempdir().unwrap();
    let bytes = pack_bytes(&source_pager(3), 2);

    // Claim 5 pages with the same 2-frame directory: 5 pages at 2 pages
    // per frame require 3 frames.
    let mut mutated = bytes.clone();
    mutated[16..24].copy_from_slice(&5_u64.to_le_bytes());
    fix_header_crc(&mut mutated);
    assert_corrupt(
        &open_error(&directory, &mutated),
        "pack page_count mismatch: 5 pages at 2 pages per frame require 3 frames, \
         the header declares 2",
    );
}

#[test]
fn truncation_at_header_directory_and_frame_is_named() {
    let directory = tempfile::tempdir().unwrap();
    let bytes = pack_bytes(&source_pager(3), 2);

    assert_corrupt(
        &open_error(&directory, &bytes[..39]),
        "pack header truncated: file has 39 bytes, the header requires 40",
    );
    assert_corrupt(
        &open_error(&directory, &bytes[..5]),
        "pack header truncated: file has 5 bytes, the header requires 40",
    );

    let mid_directory = HEADER_LEN + 20;
    assert_corrupt(
        &open_error(&directory, &bytes[..mid_directory]),
        &format!(
            "pack directory truncated: {} bytes plus crc at offset {HEADER_LEN}, file has {mid_directory}",
            2 * DIRECTORY_ENTRY_LEN
        ),
    );
    let without_directory_crc = HEADER_LEN + 2 * DIRECTORY_ENTRY_LEN;
    assert_corrupt(
        &open_error(&directory, &bytes[..without_directory_crc]),
        &format!(
            "pack directory truncated: {} bytes plus crc at offset {HEADER_LEN}, file has {without_directory_crc}",
            2 * DIRECTORY_ENTRY_LEN
        ),
    );

    let mid_frame = bytes.len() - 3;
    let last_offset = u64::from_le_bytes(
        bytes[HEADER_LEN + DIRECTORY_ENTRY_LEN..HEADER_LEN + DIRECTORY_ENTRY_LEN + 8]
            .try_into()
            .unwrap(),
    );
    let last_len = u32::from_le_bytes(
        bytes[HEADER_LEN + DIRECTORY_ENTRY_LEN + 8..HEADER_LEN + DIRECTORY_ENTRY_LEN + 12]
            .try_into()
            .unwrap(),
    );
    assert_corrupt(
        &open_error(&directory, &bytes[..mid_frame]),
        &format!(
            "pack frame 1 truncated: directory names {last_len} bytes at offset {last_offset}, file has {mid_frame}"
        ),
    );
}

#[test]
fn corrupt_frame_crc_is_named_on_read() {
    let directory = tempfile::tempdir().unwrap();
    let source = source_pager(6);
    let mut bytes = pack_bytes(&source, 2);

    // Flip one byte inside frame 2 (pages 4 and 5); the superblock frames
    // stay intact so the pager open sequence succeeds.
    let entry2 = HEADER_LEN + 2 * DIRECTORY_ENTRY_LEN;
    let offset = u64::from_le_bytes(bytes[entry2..entry2 + 8].try_into().unwrap()) as usize;
    let stored = u32::from_le_bytes(bytes[entry2 + 12..entry2 + 16].try_into().unwrap());
    bytes[offset] ^= 0xff;
    let path: PathBuf = directory.path().join("frame-crc.devonpack");
    fs::write(&path, &bytes).unwrap();
    let packed = Pager::open_pack(PackFile::open(&path).unwrap()).unwrap();

    let error = packed.read_page(4).unwrap_err();
    let computed = crc32c::crc32c(&bytes[offset..offset + frame_len(&bytes, entry2)]);
    assert_corrupt(
        &error,
        &format!("pack frame 2 crc32c mismatch: stored {stored:#010x}, computed {computed:#010x}"),
    );
    // Pages in the intact frames still read.
    assert_eq!(packed.read_page(2).unwrap(), source.read_page(2).unwrap());
}

fn frame_len(bytes: &[u8], entry: usize) -> usize {
    u32::from_le_bytes(bytes[entry + 8..entry + 12].try_into().unwrap()) as usize
}
