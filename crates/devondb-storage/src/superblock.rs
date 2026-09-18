//! Superblock encode/decode: the versioned header in pages 0 and 1.
//!
//! Byte layout is specified in `docs/FORMAT.md` § Superblock and is binding.

use crc32c::crc32c;
use devondb_types::{DevonError, DevonResult};

const MAGIC: &[u8; 8] = b"DEVONDB\0";
const HEADER_LEN: usize = 64;
const CHECKSUM_OFFSET: usize = 60;
const MIN_PAGE_SIZE: u32 = 4096;
const MAX_PAGE_SIZE: u32 = 65_536;

/// The newest on-disk format version understood by this reader.
///
/// **Frozen at 1.** `docs/FORMAT.md` § Compatibility rules are
/// PERMANENT from this version forward: a reader at version N opens every
/// file with `format_version ≤ N`, additive change rides a feature bit rather
/// than a version bump, and `min_reader_version` rises only on a breaking
/// layout change (major release plus an in-place upgrade path).
///
/// The pre-freeze v0 files remain in `tests/golden/` and MUST keep reading
/// forever. That corpus mechanically enforces rule 1.
pub const SUPPORTED_FORMAT_VERSION: u32 = 1;

/// Bit 0: the file contains at least one persistent HNSW index
/// (`docs/FORMAT.md` § Feature flag registry, `docs/HNSW.md` §8).
pub const HNSW_INDEX_FLAG: u64 = 1 << 0;

/// Bit 1: the catalog carries an `ontology` section
/// (`docs/FORMAT.md` § Feature flag registry, § Catalog `ontology`;
/// `docs/ONTOLOGY.md`). The flag is derived at catalog save and never
/// independently managed.
pub const ONTOLOGY_FLAG: u64 = 1 << 1;

/// Bit 2: the catalog carries a non-empty `pins` array
/// (`docs/FORMAT.md` § Feature flag registry, § Catalog `pins`). The flag is
/// derived at catalog save and never independently managed.
pub const PINNED_PLANS_FLAG: u64 = 1 << 2;

/// Bit 3: node-group directories may carry per-column zone-map statistics.
/// The feature is read-safe and sticky: once a writer publishes a stats-bearing
/// directory, later catalog publications preserve the bit.
pub const ZONE_MAPS_FLAG: u64 = 1 << 3;

/// Bit 4: at least one node table carries a `GeoPoint` column
/// (`docs/FORMAT.md` § Feature flag registry, § node groups; `docs/GEO.md`
/// §5). The flag is derived at catalog save and never independently
/// managed. It is not read-safe: interpreting a geo column payload requires the
/// codec, so a binary without it must refuse the file entirely rather than
/// open read-only.
pub const GEO_COLUMNS_FLAG: u64 = 1 << 4;

/// Bit 5: the file carries a free-page retirement ledger reachable from the
/// superblock extension region (`docs/FORMAT.md` § Superblock extension;
/// `docs/FREE_PAGES.md`). The extension uses the lowest reserved read-safe
/// bit, so earlier binaries accept `FREE_PAGES` files read-only. That
/// read-only rule keeps a pre-feature writer's zero-fence away from the
/// ledger roots. The first ledger-writing publication sets the flag, which
/// remains sticky afterward.
pub const FREE_PAGES_FLAG: u64 = 1 << 5;

/// Bit 12: the registry's current pre-allocated read-safe bit with no assigned
/// meaning (replacement reserve allocated when `FREE_PAGES` claimed bit 5;
/// bit 10 is `REL_TOMBSTONE_WAL` and bit 11 is the unknown-bit fixture's
/// relocation target). Writers never set it; a reader seeing it
/// opens read-only. The stable Rust name lets the permanent read-only-law
/// fixtures follow the registry slot whenever a feature claims the previous
/// bit.
pub const RESERVED_READ_SAFE_FLAG: u64 = 1 << 12;

/// Bit 6: the WAL may contain node update/delete records (`docs/UI.md`
/// §12.3; `docs/FORMAT.md` § WAL sidecar). Checkpoint-scoped lifetime:
/// set by the first DML commit since the last checkpoint, cleared by the
/// checkpoint that truncates the WAL — a cleanly checkpointed file never
/// carries it. NOT read-safe: a pre-DML binary cannot correctly recover
/// a WAL holding these records, so it must refuse the file entirely.
pub const DML_WAL_FLAG: u64 = 1 << 6;

/// Bit 7: at least one catalog table carries a `Timestamp`, `Bytes`,
/// `Decimal`, or `Json` column (`docs/FORMAT.md` § Feature flag registry,
/// § node groups). Derived at catalog save, never independently managed.
/// NOT read-safe: decoding these catalog spellings and column payloads
/// requires scalar-v2 support.
pub const SCALAR_TYPES_V2_FLAG: u64 = 1 << 7;

/// Bit 8: the file requires the cross-process writer lease and publication
/// gate protocol (`docs/FORMAT.md` § Feature flag registry;
/// `docs/MULTIPROCESS.md`). Sticky from offline activation onward and NOT
/// read-safe: a binary predating the bit does not participate in the protocol.
pub const MULTIPROCESS_COORDINATION_FLAG: u64 = 1 << 8;

/// Bit 9: the catalog payload spans multiple pages — `catalog_root` names
/// a directory page listing continuation pages instead of a v1 payload
/// page (`docs/FORMAT.md` § Feature flag registry, § Multi-page catalog).
/// Derived at catalog save and never independently managed: set iff the
/// published catalog payload exceeds one page's capacity. It is not read-safe: a binary
/// predating the bit would misread the directory page as a corrupt v1
/// catalog, so it must refuse the file entirely.
pub const MULTIPAGE_CATALOG_FLAG: u64 = 1 << 9;

/// Bit 10: the WAL may contain relationship endpoint-tombstone records
/// (`docs/DETACH_DELETE.md`; `docs/FORMAT.md` § WAL sidecar). This is a
/// checkpoint-scoped, NOT-read-safe allocation.
pub const REL_TOMBSTONE_WAL_FLAG: u64 = 1 << 10;

/// Bit 13: at least one node group carries a non-plain values-section
/// payload (`docs/FORMAT.md` § Feature flag registry; `docs/SCALE.md` §8 —
/// per-payload encodings declared by directory-flags bit 1). Derived at
/// catalog save (set iff any reachable node group carries a non-plain payload) and
/// sticky from then on. It is not read-safe: a build without the
/// codec cannot decode the payload, so it must refuse the file entirely.
pub const COLUMN_ENCODINGS_FLAG: u64 = 1 << 13;

/// Bit 14: committed WAL can mutate an HNSW-indexed node table.
/// Checkpoint-scoped and not read-safe: readers need mutation-aware exact
/// fallback until checkpoint publishes a rebuilt index and truncates the WAL.
pub const HNSW_MUTATION_WAL_FLAG: u64 = 1 << 14;

/// Feature flags that this reader may safely ignore while reading.
///
/// This registry must change only together with the feature flag table in
/// `docs/FORMAT.md`.
pub const READ_SAFE_FLAG_MASK: u64 = HNSW_INDEX_FLAG
    | ONTOLOGY_FLAG
    | PINNED_PLANS_FLAG
    | ZONE_MAPS_FLAG
    | FREE_PAGES_FLAG
    | RESERVED_READ_SAFE_FLAG;

/// Feature flags this build fully supports, including writes. A flag that
/// is read-safe but not supported forces read-only mode: base data stays
/// readable, but writes and checkpoints could drop or invalidate the
/// feature's state, so they are refused (`docs/HNSW.md` §8.2).
pub const SUPPORTED_FLAG_MASK: u64 = HNSW_INDEX_FLAG
    | ONTOLOGY_FLAG
    | PINNED_PLANS_FLAG
    | ZONE_MAPS_FLAG
    | GEO_COLUMNS_FLAG
    | FREE_PAGES_FLAG
    | DML_WAL_FLAG
    | SCALAR_TYPES_V2_FLAG
    | MULTIPROCESS_COORDINATION_FLAG
    | MULTIPAGE_CATALOG_FLAG
    | REL_TOMBSTONE_WAL_FLAG
    | COLUMN_ENCODINGS_FLAG
    | HNSW_MUTATION_WAL_FLAG;

/// The versioned database header stored in both superblock slots.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Superblock {
    /// The format version that wrote the database file.
    pub format_version: u32,
    /// The oldest reader version capable of opening the database file.
    pub min_reader_version: u32,
    /// Format feature flags enabled for the database file.
    pub feature_flags: u64,
    /// The fixed database page size in bytes.
    pub page_size: u32,
    /// The database's persistent 128-bit identifier.
    pub db_id: [u8; 16],
    /// The log sequence number of the last completed checkpoint.
    pub checkpoint_lsn: u64,
    /// The page identifier of the catalog root, or zero for an empty database.
    pub catalog_root: u64,
}

impl Superblock {
    /// Encodes this superblock into a page using the v0 on-disk layout.
    pub fn encode(&self, page: &mut [u8]) -> DevonResult<()> {
        if page.len() < HEADER_LEN {
            return Err(DevonError::InvalidArgument {
                context: "superblock page must be at least 64 bytes".to_owned(),
            });
        }
        if !valid_page_size(self.page_size) {
            return Err(DevonError::InvalidArgument {
                context: format!(
                    "page size {} must be a power of two between 4096 and 65536 bytes",
                    self.page_size
                ),
            });
        }
        if self.min_reader_version > self.format_version {
            return Err(DevonError::InvalidArgument {
                context: format!(
                    "minimum reader version {} cannot exceed format version {}",
                    self.min_reader_version, self.format_version
                ),
            });
        }

        page.fill(0);
        page[..8].copy_from_slice(MAGIC);
        page[8..12].copy_from_slice(&self.format_version.to_le_bytes());
        page[12..16].copy_from_slice(&self.min_reader_version.to_le_bytes());
        page[16..24].copy_from_slice(&self.feature_flags.to_le_bytes());
        page[24..28].copy_from_slice(&self.page_size.to_le_bytes());
        page[28..44].copy_from_slice(&self.db_id);
        page[44..52].copy_from_slice(&self.checkpoint_lsn.to_le_bytes());
        page[52..60].copy_from_slice(&self.catalog_root.to_le_bytes());
        let checksum = crc32c(&page[..CHECKSUM_OFFSET]);
        page[CHECKSUM_OFFSET..HEADER_LEN].copy_from_slice(&checksum.to_le_bytes());
        Ok(())
    }

    /// Decodes and validates a superblock from its on-disk representation.
    pub fn decode(page: &[u8]) -> DevonResult<Self> {
        Self::decode_with_masks(page, SUPPORTED_FLAG_MASK, READ_SAFE_FLAG_MASK)
    }

    fn decode_with_masks(page: &[u8], supported: u64, read_safe: u64) -> DevonResult<Self> {
        if page.len() < HEADER_LEN {
            return Err(corrupt("superblock page is shorter than 64 bytes"));
        }
        if &page[..8] != MAGIC {
            return Err(corrupt("invalid superblock magic"));
        }

        let expected_checksum = read_u32(page, CHECKSUM_OFFSET);
        let actual_checksum = crc32c(&page[..CHECKSUM_OFFSET]);
        if actual_checksum != expected_checksum {
            return Err(corrupt("superblock checksum does not match"));
        }
        // NOTE: `decode` deliberately validates only the 64-byte header.
        // Readers never load the rest of the superblock page — `read_slot`
        // reads exactly `SUPERBLOCK_HEADER_LEN` bytes (pager.rs) — so the
        // page's extension region is written zero but is neither checksummed
        // (the slot CRC covers bytes 0..60) nor validated. That is a
        // deliberate format property recorded in `docs/FORMAT.md`
        // § Superblock: a future extension there is fenced by its feature bit
        // under the compatibility rules, exactly as every other extension
        // is, so
        // rejecting a nonzero region would buy no unambiguity while adding a
        // way for a file to become unreadable. The writer-side guarantee is
        // fenced in `crates/devondb/tests/format_freeze.rs`.

        let superblock = Self {
            format_version: read_u32(page, 8),
            min_reader_version: read_u32(page, 12),
            feature_flags: read_u64(page, 16),
            page_size: read_u32(page, 24),
            db_id: read_db_id(page),
            checkpoint_lsn: read_u64(page, 44),
            catalog_root: read_u64(page, 52),
        };
        superblock.validate_decoded(supported, read_safe)?;
        Ok(superblock)
    }

    /// Returns whether this file may only be opened read-only: it enables
    /// a read-safe feature flag this build does not fully support.
    #[must_use]
    pub fn requires_read_only(&self) -> bool {
        self.feature_flags & READ_SAFE_FLAG_MASK & !SUPPORTED_FLAG_MASK != 0
    }

    fn validate_decoded(&self, supported: u64, read_safe: u64) -> DevonResult<()> {
        if self.min_reader_version > SUPPORTED_FORMAT_VERSION {
            return Err(DevonError::VersionMismatch {
                file_version: self.format_version,
                min_reader_version: self.min_reader_version,
                supported: SUPPORTED_FORMAT_VERSION,
            });
        }
        // A set bit is acceptable when this build either fully supports it
        // or can at least read around it (read-safe → read-only mode).
        // Bits outside BOTH masks are unknown future features: refuse.
        if self.feature_flags & !(read_safe | supported) != 0 {
            return Err(DevonError::VersionMismatch {
                file_version: self.format_version,
                min_reader_version: self.min_reader_version,
                supported: SUPPORTED_FORMAT_VERSION,
            });
        }
        if !valid_page_size(self.page_size) {
            return Err(corrupt("invalid page size in superblock"));
        }
        Ok(())
    }
}

/// Chooses the valid superblock slot with the highest checkpoint LSN.
pub fn choose_authoritative(
    a: DevonResult<Superblock>,
    b: DevonResult<Superblock>,
) -> DevonResult<Superblock> {
    match (a, b) {
        (Err(error @ DevonError::VersionMismatch { .. }), _) => Err(error),
        (_, Err(error @ DevonError::VersionMismatch { .. })) => Err(error),
        (Ok(a), Ok(b)) if b.checkpoint_lsn > a.checkpoint_lsn => Ok(b),
        (Ok(a), Ok(_)) | (Ok(a), Err(_)) => Ok(a),
        (Err(_), Ok(b)) => Ok(b),
        (Err(first), Err(_)) => Err(first),
    }
}

fn valid_page_size(page_size: u32) -> bool {
    (MIN_PAGE_SIZE..=MAX_PAGE_SIZE).contains(&page_size) && page_size.is_power_of_two()
}

fn read_u32(page: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        page[offset],
        page[offset + 1],
        page[offset + 2],
        page[offset + 3],
    ])
}

fn read_u64(page: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes([
        page[offset],
        page[offset + 1],
        page[offset + 2],
        page[offset + 3],
        page[offset + 4],
        page[offset + 5],
        page[offset + 6],
        page[offset + 7],
    ])
}

fn read_db_id(page: &[u8]) -> [u8; 16] {
    let mut db_id = [0_u8; 16];
    db_id.copy_from_slice(&page[28..44]);
    db_id
}

fn corrupt(context: &str) -> DevonError {
    DevonError::Corrupt {
        context: context.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CHECKSUM_OFFSET, HEADER_LEN, SUPPORTED_FORMAT_VERSION, Superblock, choose_authoritative,
    };
    use crc32c::crc32c;
    use devondb_types::{DevonError, DevonResult};

    #[test]
    fn mutation_fence_rejects_old_mask_even_at_unchanged_checkpoint_lsn() {
        let mut header = sample_superblock(7);
        let old = super::SUPPORTED_FLAG_MASK & !super::HNSW_MUTATION_WAL_FLAG;
        assert!(
            Superblock::decode_with_masks(&encoded(&header), old, super::READ_SAFE_FLAG_MASK)
                .is_ok()
        );
        header.feature_flags |= super::HNSW_MUTATION_WAL_FLAG;
        let bytes = encoded(&header);
        assert!(Superblock::decode(&bytes).is_ok());
        assert!(matches!(
            Superblock::decode_with_masks(&bytes, old, super::READ_SAFE_FLAG_MASK),
            Err(DevonError::VersionMismatch { .. })
        ));
        header.feature_flags &= !super::HNSW_MUTATION_WAL_FLAG;
        assert!(
            Superblock::decode_with_masks(&encoded(&header), old, super::READ_SAFE_FLAG_MASK)
                .is_ok()
        );
    }

    fn sample_superblock(checkpoint_lsn: u64) -> Superblock {
        Superblock {
            format_version: SUPPORTED_FORMAT_VERSION,
            min_reader_version: SUPPORTED_FORMAT_VERSION,
            feature_flags: 0,
            page_size: 4096,
            db_id: *b"0123456789abcdef",
            checkpoint_lsn,
            catalog_root: 73,
        }
    }

    fn encoded(superblock: &Superblock) -> Vec<u8> {
        let mut page = vec![0xff; superblock.page_size as usize];
        superblock.encode(&mut page).unwrap();
        page
    }

    fn corrupt_result() -> DevonResult<Superblock> {
        Err(DevonError::Corrupt {
            context: "invalid slot".to_owned(),
        })
    }

    fn newer_format_result() -> DevonResult<Superblock> {
        let mut newer = sample_superblock(43);
        newer.format_version = SUPPORTED_FORMAT_VERSION + 1;
        newer.min_reader_version = SUPPORTED_FORMAT_VERSION + 1;
        Superblock::decode(&encoded(&newer))
    }

    #[test]
    fn encode_decode_round_trip_and_zeroes_padding() {
        let expected = sample_superblock(42);
        let page = encoded(&expected);

        assert_eq!(Superblock::decode(&page).unwrap(), expected);
        assert!(page[HEADER_LEN..].iter().all(|byte| *byte == 0));
    }

    #[test]
    fn bad_magic_is_corrupt() {
        let mut page = encoded(&sample_superblock(42));
        page[0] ^= 0xff;

        assert!(matches!(
            Superblock::decode(&page),
            Err(DevonError::Corrupt { .. })
        ));
    }

    #[test]
    fn flipped_byte_is_corrupt() {
        let mut page = encoded(&sample_superblock(42));
        page[32] ^= 0xff;

        assert!(matches!(
            Superblock::decode(&page),
            Err(DevonError::Corrupt { .. })
        ));
    }

    #[test]
    fn unsupported_min_reader_version_is_version_mismatch() {
        let mut page = encoded(&sample_superblock(42));
        let min_reader_version = SUPPORTED_FORMAT_VERSION + 1;
        page[12..16].copy_from_slice(&min_reader_version.to_le_bytes());
        let checksum = crc32c(&page[..CHECKSUM_OFFSET]);
        page[CHECKSUM_OFFSET..HEADER_LEN].copy_from_slice(&checksum.to_le_bytes());

        assert!(matches!(
            Superblock::decode(&page),
            Err(DevonError::VersionMismatch {
                file_version: SUPPORTED_FORMAT_VERSION,
                min_reader_version: version,
                supported: SUPPORTED_FORMAT_VERSION,
            }) if version == min_reader_version
        ));
    }

    #[test]
    fn unknown_feature_flag_is_version_mismatch() {
        let mut flagged = sample_superblock(42);
        // Bit 11 is outside both masks (bit 10 is REL_TOMBSTONE_WAL and bit
        // 12 is the current reserved read-safe fixture slot).
        flagged.feature_flags = 1 << 11;

        assert!(matches!(
            Superblock::decode(&encoded(&flagged)),
            Err(DevonError::VersionMismatch {
                file_version: SUPPORTED_FORMAT_VERSION,
                min_reader_version: SUPPORTED_FORMAT_VERSION,
                supported: SUPPORTED_FORMAT_VERSION,
            })
        ));
    }

    #[test]
    fn rel_tombstone_wal_flag_is_supported_and_not_read_safe() {
        const BIT_10: u64 = 1 << 10;
        assert_eq!(super::REL_TOMBSTONE_WAL_FLAG, BIT_10);
        assert_eq!(super::SUPPORTED_FLAG_MASK & BIT_10, BIT_10);
        assert_eq!(super::READ_SAFE_FLAG_MASK & BIT_10, 0);

        let mut flagged = sample_superblock(42);
        flagged.feature_flags = super::REL_TOMBSTONE_WAL_FLAG;
        let decoded = Superblock::decode(&encoded(&flagged)).unwrap();
        assert!(!decoded.requires_read_only());
    }

    #[test]
    fn supported_not_read_safe_flag_decodes_writable() {
        let mut flagged = sample_superblock(42);
        flagged.feature_flags = super::GEO_COLUMNS_FLAG
            | super::SCALAR_TYPES_V2_FLAG
            | super::MULTIPROCESS_COORDINATION_FLAG;

        let decoded = Superblock::decode(&encoded(&flagged)).unwrap();
        assert!(!decoded.requires_read_only());
    }

    #[test]
    fn multiprocess_flag_survives_superblock_round_trip() {
        let mut flagged = sample_superblock(42);
        flagged.feature_flags = super::MULTIPROCESS_COORDINATION_FLAG;

        let decoded = Superblock::decode(&encoded(&flagged)).unwrap();

        assert_eq!(decoded.feature_flags, super::MULTIPROCESS_COORDINATION_FLAG);
        assert!(!decoded.requires_read_only());
    }

    #[test]
    fn read_safe_unsupported_flag_decodes_but_requires_read_only() {
        let mut flagged = sample_superblock(42);
        flagged.feature_flags = super::RESERVED_READ_SAFE_FLAG;

        let decoded = Superblock::decode(&encoded(&flagged)).unwrap();
        assert!(decoded.requires_read_only());
        // Fully supported flags never force read-only in this build.
        let mut supported = sample_superblock(42);
        supported.feature_flags =
            super::HNSW_INDEX_FLAG | super::ONTOLOGY_FLAG | super::PINNED_PLANS_FLAG;
        assert!(!supported.requires_read_only());
        assert!(!sample_superblock(42).requires_read_only());
    }

    #[test]
    fn encode_rejects_min_reader_version_above_format_version() {
        let mut superblock = sample_superblock(42);
        superblock.min_reader_version = superblock.format_version + 1;
        let mut page = vec![0_u8; superblock.page_size as usize];

        assert!(matches!(
            superblock.encode(&mut page),
            Err(DevonError::InvalidArgument { .. })
        ));
    }

    #[test]
    fn arbitration_picks_higher_checkpoint_lsn() {
        let lower = sample_superblock(41);
        let higher = sample_superblock(42);

        assert_eq!(
            choose_authoritative(Ok(lower), Ok(higher.clone())).unwrap(),
            higher
        );
    }

    #[test]
    fn arbitration_uses_valid_slot_when_other_is_corrupt() {
        let valid = sample_superblock(42);

        assert_eq!(
            choose_authoritative(corrupt_result(), Ok(valid.clone())).unwrap(),
            valid
        );
    }

    #[test]
    fn arbitration_rejects_newer_format_instead_of_using_stale_valid_slot() {
        let valid = sample_superblock(42);

        assert!(matches!(
            choose_authoritative(Ok(valid), newer_format_result()),
            Err(DevonError::VersionMismatch { .. })
        ));
    }

    #[test]
    fn arbitration_prefers_version_mismatch_over_corrupt_in_both_orders() {
        assert!(matches!(
            choose_authoritative(newer_format_result(), corrupt_result()),
            Err(DevonError::VersionMismatch { .. })
        ));
        assert!(matches!(
            choose_authoritative(corrupt_result(), newer_format_result()),
            Err(DevonError::VersionMismatch { .. })
        ));
    }
}
