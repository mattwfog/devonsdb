//! The format freeze fence.
//!
//! `docs/FORMAT.md` § Compatibility rules are PERMANENT from `format_version`
//! 1 forward. These tests are the mechanical enforcement of that promise —
//! not documentation of it. Every assertion here is a literal, deliberately:
//! a fence that tracks a constant cannot detect a change to that constant,
//! and the whole point of a freeze is that the constant stops moving.
//!
//! If one of these fails, the question is never "update the test." It is
//! "does this change break the format promise, and does it therefore require
//! a major release with an in-place upgrade path?"

use std::{
    ffi::OsStr,
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use devondb::{Database, DevonError};
use devondb_storage::{
    pager::Pager,
    superblock::{
        MULTIPROCESS_COORDINATION_FLAG, READ_SAFE_FLAG_MASK, REL_TOMBSTONE_WAL_FLAG,
        SUPPORTED_FLAG_MASK, SUPPORTED_FORMAT_VERSION,
    },
};

/// The frozen on-disk format version.
const FROZEN_FORMAT_VERSION: u32 = 1;

/// Superblock field offsets (`docs/FORMAT.md` § Superblock).
const FORMAT_VERSION_OFFSET: usize = 8;
const MIN_READER_VERSION_OFFSET: usize = 12;
const FEATURE_FLAGS_OFFSET: usize = 16;
const CHECKSUM_OFFSET: usize = 60;
const SUPERBLOCK_HEADER_LEN: usize = 64;

/// The pre-freeze files minted during development. A frozen reader MUST keep
/// opening every one of them, forever. This list only grows.
const PRE_FREEZE_V0_ANCHORS: [&str; 6] = [
    "m1-person.devondb",
    "m2-social.devondb",
    "m5-pins.devondb",
    "ont-classes.devondb",
    "geo-places.devondb",
    "s1-stats.devondb",
];

/// Post-freeze release and feature-bit anchors. Each was minted at the frozen
/// version, and a frozen reader MUST keep opening every one of them, forever.
/// This list only grows.
const POST_FREEZE_V1_ANCHORS: [&str; 10] = [
    "release-0.1.0.devondb",
    "scalar-v2.devondb",
    "multiprocess.devondb",
    "free-pages.devondb",
    "encoding-constant.devondb",
    "encoding-rle.devondb",
    "encoding-bitpack_for.devondb",
    "encoding-dictionary.devondb",
    "encoding-fsst.devondb",
    "encoding-alp.devondb",
];

#[test]
fn format_version_is_frozen_at_one() {
    assert_eq!(
        SUPPORTED_FORMAT_VERSION, FROZEN_FORMAT_VERSION,
        "the on-disk format version is FROZEN (docs/FORMAT.md § Compatibility \
         rules). Raising it is a breaking change: it requires a major devondb \
         release and an in-place upgrade path from the previous format \
         (rule 2). Additive change rides a feature bit instead (rule 3)."
    );
}

#[test]
fn multiprocess_feature_bit_registry_is_frozen() {
    const BIT_8: u64 = 1 << 8;

    assert_eq!(MULTIPROCESS_COORDINATION_FLAG, BIT_8);
    assert_eq!(SUPPORTED_FLAG_MASK & BIT_8, BIT_8);
    assert_eq!(READ_SAFE_FLAG_MASK & BIT_8, 0);
}

#[test]
fn rel_tombstone_wal_bit_is_registered_and_supported() {
    const BIT_10: u64 = 1 << 10;
    assert_eq!(REL_TOMBSTONE_WAL_FLAG, BIT_10);
    assert_eq!(SUPPORTED_FLAG_MASK & BIT_10, BIT_10);
    assert_eq!(READ_SAFE_FLAG_MASK & BIT_10, 0);

    let directory = TestDirectory::new();
    let path = directory.path.join("pending-rel-tombstone.devondb");
    let pager = Pager::create(&path, 4096, *b"rel-delete-bit10").unwrap();
    drop(pager);
    doctor_feature_flags(&path, BIT_10);

    let database = Database::open(&path).unwrap();
    drop(database);
}

#[test]
fn unknown_bit_11_still_refuses_before_payload_decode() {
    const BIT_11: u64 = 1 << 11;
    assert_eq!(SUPPORTED_FLAG_MASK & BIT_11, 0);
    assert_eq!(READ_SAFE_FLAG_MASK & BIT_11, 0);

    let directory = TestDirectory::new();
    let path = directory.path.join("unknown-bit11.devondb");
    let pager = Pager::create(&path, 4096, *b"unknown-flag-011").unwrap();
    drop(pager);
    doctor_feature_flags(&path, BIT_11);

    let error = Database::open(&path).err().unwrap();
    assert!(matches!(
        error,
        DevonError::VersionMismatch {
            file_version: FROZEN_FORMAT_VERSION,
            min_reader_version: FROZEN_FORMAT_VERSION,
            supported: SUPPORTED_FORMAT_VERSION,
        }
    ));
}

#[test]
fn a_frozen_reader_opens_every_pre_freeze_file_unchanged() {
    // Rule 1 — "reading old files always works" — is devondb's #1
    // differentiator, and this is where it is enforced rather than promised.
    // The corpus files were written by a v0 writer that no longer exists;
    // nothing but backward compatibility keeps them readable.
    let corpus = corpus_directory();
    let scratch = TestDirectory::new();
    for file in PRE_FREEZE_V0_ANCHORS {
        let committed = corpus.join(file);
        assert!(committed.exists(), "pre-freeze anchor {file} is missing");

        let stored = fs::read(&committed).unwrap();
        let format_version = read_u32_at(&stored, FORMAT_VERSION_OFFSET);
        let min_reader_version = read_u32_at(&stored, MIN_READER_VERSION_OFFSET);
        assert_eq!(
            format_version, 0,
            "{file} is meant to be a PRE-freeze v0 file; re-minting it at the \
             current version destroys the only evidence that old files still \
             read"
        );
        assert!(min_reader_version <= FROZEN_FORMAT_VERSION);

        // Open a COPY, never the committed file. `Database::open` creates a
        // `-wal` sidecar next to whatever it opens, and writing that into the
        // corpus directory races the corpus test's own "no WAL here" check —
        // a flaky-CI generator, and exactly the kind of side effect a fence
        // over committed bytes must not have.
        let working = scratch.path.join(file);
        fs::copy(&committed, &working).unwrap();

        // The real proof: a frozen-version reader opens a v0 file.
        let database = Database::open(&working).unwrap();
        drop(database);

        assert_eq!(
            fs::read(&working).unwrap(),
            stored,
            "opening pre-freeze anchor {file} altered its bytes"
        );
        assert_eq!(
            fs::read(&committed).unwrap(),
            stored,
            "the freeze fence must never write to the committed corpus"
        );
    }
}

#[test]
fn a_frozen_reader_opens_every_post_freeze_release_anchor_unchanged() {
    // The release-anchor half of rule 1: every released writer leaves behind
    // a file minted at the frozen version, and this fence proves each one
    // still opens byte-for-byte unchanged under the current reader.
    let corpus = corpus_directory();
    let scratch = TestDirectory::new();
    for file in POST_FREEZE_V1_ANCHORS {
        let committed = corpus.join(file);
        assert!(committed.exists(), "post-freeze anchor {file} is missing");

        let stored = fs::read(&committed).unwrap();
        let format_version = read_u32_at(&stored, FORMAT_VERSION_OFFSET);
        let min_reader_version = read_u32_at(&stored, MIN_READER_VERSION_OFFSET);
        assert_eq!(
            format_version, FROZEN_FORMAT_VERSION,
            "{file} must be a post-freeze anchor written at the frozen version"
        );
        assert!(min_reader_version <= FROZEN_FORMAT_VERSION);

        // Open a COPY, never the committed file (see the pre-freeze fence).
        let working = scratch.path.join(file);
        fs::copy(&committed, &working).unwrap();

        let database = Database::open(&working).unwrap();
        drop(database);

        assert_eq!(
            fs::read(&working).unwrap(),
            stored,
            "opening post-freeze anchor {file} altered its bytes"
        );
        assert_eq!(
            fs::read(&committed).unwrap(),
            stored,
            "the freeze fence must never write to the committed corpus"
        );
    }
}

#[test]
fn a_newly_created_file_is_written_at_the_frozen_version() {
    let directory = TestDirectory::new();
    let path = directory.path.join("frozen.devondb");
    let pager = Pager::create(&path, 4096, *b"format-freeze-01").unwrap();
    drop(pager);

    let bytes = fs::read(&path).unwrap();
    for slot in 0..2 {
        let base = slot * 4096;
        assert_eq!(
            read_u32_at(&bytes, base + FORMAT_VERSION_OFFSET),
            FROZEN_FORMAT_VERSION,
            "slot {slot} format_version"
        );
        assert_eq!(
            read_u32_at(&bytes, base + MIN_READER_VERSION_OFFSET),
            FROZEN_FORMAT_VERSION,
            "slot {slot} min_reader_version"
        );
    }
}

#[test]
fn the_writer_leaves_the_superblock_extension_region_zero() {
    // The bytes after the 64-byte header are the superblock's extension
    // region. Readers never load them — `read_slot` reads exactly 64 bytes —
    // and the slot CRC covers only bytes 0..60, so the region is neither
    // checksummed nor validated on read. That is a deliberate freeze-time
    // ruling (`docs/FORMAT.md` § Superblock): a future extension there is
    // fenced by its feature bit under the acceptance law, so a read-side
    // rejection would buy no unambiguity while adding a way for a file to
    // become unreadable — against rule 1.
    //
    // What DOES matter is the writer-side guarantee, because it is what makes
    // the region safe to claim later. Fence it here.
    let directory = TestDirectory::new();
    let path = directory.path.join("tail.devondb");
    let pager = Pager::create(&path, 4096, *b"format-freeze-02").unwrap();
    drop(pager);

    let bytes = fs::read(&path).unwrap();
    for slot in 0..2 {
        let base = slot * 4096;
        assert!(
            bytes[base + SUPERBLOCK_HEADER_LEN..base + 4096]
                .iter()
                .all(|byte| *byte == 0),
            "slot {slot}: the writer must leave the superblock extension region zero"
        );
    }
}

#[test]
fn every_corpus_anchor_has_a_bit_governed_superblock_extension_region() {
    // The committed corpus is the evidence that the writer-side guarantee has
    // held for every file devondb has ever produced — which is what let
    // FREE_PAGES can claim the region behind bit 5. The fence is now
    // bit-conditional, exactly as the freeze ruling planned: without the bit
    // every extension byte is zero; with it, bytes 64..92 carry a CRC-valid
    // extension and every unclaimed byte from 92 up stays under the
    // zero-fence law (`docs/FREE_PAGES.md` § On-disk layout).
    const FREE_PAGES_BIT: u64 = 1 << 5;
    const FEATURE_FLAGS_OFFSET: usize = 16;
    const EXTENSION_END: usize = 92;
    for file in PRE_FREEZE_V0_ANCHORS.iter().chain(&POST_FREEZE_V1_ANCHORS) {
        let bytes = fs::read(corpus_directory().join(file)).unwrap();
        for slot in 0..2 {
            let base = slot * 4096;
            let mut flags = [0_u8; 8];
            flags.copy_from_slice(
                &bytes[base + FEATURE_FLAGS_OFFSET..base + FEATURE_FLAGS_OFFSET + 8],
            );
            let free_pages = u64::from_le_bytes(flags) & FREE_PAGES_BIT != 0;
            if free_pages {
                assert!(
                    devondb_storage::free_pages::SuperblockExtension::decode_from(
                        &bytes[base..base + 4096]
                    )
                    .is_some(),
                    "{file} slot {slot}: FREE_PAGES bit set but the extension CRC is invalid"
                );
                assert!(
                    bytes[base + EXTENSION_END..base + 4096]
                        .iter()
                        .all(|byte| *byte == 0),
                    "{file} slot {slot}: unclaimed extension bytes past 92 must stay zero"
                );
            } else {
                assert!(
                    bytes[base + SUPERBLOCK_HEADER_LEN..base + 4096]
                        .iter()
                        .all(|byte| *byte == 0),
                    "{file} slot {slot}: nonzero superblock extension region without the bit"
                );
            }
        }
    }
}

#[test]
fn every_corpus_file_is_covered_by_the_freeze_fence() {
    // A new anchor that nobody lists here would silently escape the freeze
    // proof. The corpus only grows, so this comparison must be exhaustive.
    let mut found = fs::read_dir(corpus_directory())
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(OsStr::to_str) == Some("devondb"))
        .map(|path| path.file_name().and_then(OsStr::to_str).unwrap().to_owned())
        .collect::<Vec<_>>();
    found.sort();

    let mut expected = PRE_FREEZE_V0_ANCHORS
        .iter()
        .chain(&POST_FREEZE_V1_ANCHORS)
        .map(|file| (*file).to_owned())
        .collect::<Vec<_>>();
    expected.sort();

    assert_eq!(
        found, expected,
        "the golden corpus changed. A new anchor must be added to this \
         fence's list; an anchor minted at the frozen version belongs in a \
         post-freeze list, never by re-minting a pre-freeze file."
    );
}

fn corpus_directory() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/golden")
}

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

/// Self-cleaning scratch directory — the house pattern in this crate's tests,
/// which deliberately carry no `tempfile` dev-dependency.
struct TestDirectory {
    path: PathBuf,
}

impl TestDirectory {
    fn new() -> Self {
        let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "devondb-format-freeze-{}-{timestamp}-{sequence}",
            std::process::id()
        ));
        fs::create_dir(&path).unwrap();
        Self { path }
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn read_u32_at(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}

fn doctor_feature_flags(path: &Path, feature_flags: u64) {
    let mut bytes = fs::read(path).unwrap();
    for slot_offset in [0, 4096] {
        bytes[slot_offset + FEATURE_FLAGS_OFFSET..slot_offset + 24]
            .copy_from_slice(&feature_flags.to_le_bytes());
        let checksum = crc32c::crc32c(&bytes[slot_offset..slot_offset + CHECKSUM_OFFSET]);
        bytes[slot_offset + CHECKSUM_OFFSET..slot_offset + SUPERBLOCK_HEADER_LEN]
            .copy_from_slice(&checksum.to_le_bytes());
    }
    fs::write(path, bytes).unwrap();
}

#[test]
fn hnsw_mutation_bit_registry_is_frozen() {
    use devondb_storage::superblock::HNSW_MUTATION_WAL_FLAG;
    assert_eq!(HNSW_MUTATION_WAL_FLAG, 1 << 14);
    assert_eq!(READ_SAFE_FLAG_MASK & (1 << 14), 0);
    assert_ne!(SUPPORTED_FLAG_MASK & (1 << 14), 0);
}
