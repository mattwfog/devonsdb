//! Free-page management codecs: the superblock extension and retirement
//! ledger pages (`docs/FREE_PAGES.md`, BINDING; `docs/FORMAT.md`
//! § Superblock extension / § Free-page ledger pages).
//!
//! Allocation, retirement, and the pin horizon live in `pager.rs`;
//! `catalog.rs` computes the superseded set at publication. This module
//! contains only the byte layouts.
//!
//! Two layout laws both codecs enforce:
//!
//! - The extension occupies superblock-page bytes 64..92 and carries its
//!   OWN CRC — the 64-byte header CRC (bytes 0..60) never covers it, and
//!   slot arbitration never validates it. A CRC mismatch is degraded
//!   mode, never `Corrupt` (`docs/FREE_PAGES.md` § On-disk layout).
//! - A ledger chain is walked from `retire_ledger_head` following
//!   `next_page`, and the walk STOPS at `retire_ledger_tail`: pages
//!   linked past the tail are unreachable by definition. This is what
//!   keeps a publication's freshly written ledger pages invisible until
//!   the superblock flip that names them (the crash-window table's
//!   "new ledger pages unreachable" row).

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Mutex, OnceLock, PoisonError},
};

use crc32c::crc32c;
use devondb_types::{DevonError, DevonResult};

/// First byte of the superblock extension region claimed by `FREE_PAGES`.
pub const EXTENSION_OFFSET: usize = 64;
/// One past the last claimed extension byte; bytes 92+ stay writer-zeroed
/// under the zero-fence law.
pub const EXTENSION_END: usize = 92;
const EXTENSION_CRC_OFFSET: usize = 88;

/// Byte offset of the first ledger entry in a ledger page.
pub const LEDGER_ENTRIES_OFFSET: usize = 32;
/// Bytes per ledger entry: `(page_id u64, retired_lsn u64)`.
pub const LEDGER_ENTRY_LEN: usize = 16;
const LEDGER_MAGIC: &[u8; 8] = b"DEVONFPL";
const LEDGER_CRC_OFFSET: usize = 24;

static PROSPECTIVE_RETIREMENTS: OnceLock<Mutex<BTreeMap<[u8; 16], BTreeSet<u64>>>> =
    OnceLock::new();

/// Queues unreachable pages written by a failed prospective generation.
///
/// The next successful catalog publication for `db_id` incorporates these
/// page ids into its ordinary retirement ledger. A process crash may lose the
/// in-memory queue; those pages remain discoverable by the offline sweep.
pub fn queue_prospective_pages(db_id: [u8; 16], pages: impl IntoIterator<Item = u64>) {
    let mut queued = prospective_retirements()
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    let entry = queued.entry(db_id).or_default();
    entry.extend(pages.into_iter().filter(|page_id| *page_id >= 2));
}

/// Returns the number of prospective pages awaiting retirement for `db_id`.
///
/// This is a diagnostic surface; logical database reads do not consult the
/// in-memory queue.
#[doc(hidden)]
#[must_use]
pub fn prospective_page_count(db_id: [u8; 16]) -> usize {
    prospective_retirements()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(&db_id)
        .map_or(0, BTreeSet::len)
}

pub(crate) fn prospective_pages(db_id: [u8; 16]) -> Vec<u64> {
    prospective_retirements()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(&db_id)
        .map_or_else(Vec::new, |pages| pages.iter().copied().collect())
}

pub(crate) fn published_prospective_pages(db_id: [u8; 16], pages: &[u64]) {
    let mut queued = prospective_retirements()
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    let remove_entry = queued.get_mut(&db_id).is_some_and(|entry| {
        for page_id in pages {
            entry.remove(page_id);
        }
        entry.is_empty()
    });
    if remove_entry {
        queued.remove(&db_id);
    }
}

fn prospective_retirements() -> &'static Mutex<BTreeMap<[u8; 16], BTreeSet<u64>>> {
    PROSPECTIVE_RETIREMENTS.get_or_init(|| Mutex::new(BTreeMap::new()))
}

/// The `FREE_PAGES` fields of the superblock extension region
/// (bytes 64..92 of each slot page).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SuperblockExtension {
    /// Page id of the oldest ledger page; 0 = empty ledger.
    pub retire_ledger_head: u64,
    /// Page id of the newest ledger page; 0 = empty ledger. Chain walks
    /// stop here even when the tail page's `next_page` is nonzero.
    pub retire_ledger_tail: u64,
    /// Cumulative entries ever appended, strictly monotone. Live entries
    /// derive as this minus cumulative reuse (recoverable by walking the
    /// chain), never the other way around.
    pub retired_total: u64,
}

impl SuperblockExtension {
    /// Writes the extension fields and their CRC into a superblock page.
    pub fn encode_into(&self, page: &mut [u8]) -> DevonResult<()> {
        if page.len() < EXTENSION_END {
            return Err(DevonError::InvalidArgument {
                context: format!(
                    "superblock page of {} bytes cannot hold the {EXTENSION_END}-byte extension",
                    page.len()
                ),
            });
        }
        page[64..72].copy_from_slice(&self.retire_ledger_head.to_le_bytes());
        page[72..80].copy_from_slice(&self.retire_ledger_tail.to_le_bytes());
        page[80..88].copy_from_slice(&self.retired_total.to_le_bytes());
        let checksum = crc32c(&page[EXTENSION_OFFSET..EXTENSION_CRC_OFFSET]);
        page[EXTENSION_CRC_OFFSET..EXTENSION_END].copy_from_slice(&checksum.to_le_bytes());
        Ok(())
    }

    /// Decodes the extension from a superblock page prefix.
    ///
    /// `None` means the extension CRC does not match: degraded mode — the
    /// ledger is treated as absent, allocation appends as today, and the
    /// next publication rewrites a valid extension. Never an error and
    /// never a slot-arbitration input (`docs/FREE_PAGES.md` § On-disk
    /// layout).
    #[must_use]
    pub fn decode_from(page: &[u8]) -> Option<Self> {
        if page.len() < EXTENSION_END {
            return None;
        }
        let expected = u32::from_le_bytes([
            page[EXTENSION_CRC_OFFSET],
            page[EXTENSION_CRC_OFFSET + 1],
            page[EXTENSION_CRC_OFFSET + 2],
            page[EXTENSION_CRC_OFFSET + 3],
        ]);
        if crc32c(&page[EXTENSION_OFFSET..EXTENSION_CRC_OFFSET]) != expected {
            return None;
        }
        Some(Self {
            retire_ledger_head: read_u64(page, 64),
            retire_ledger_tail: read_u64(page, 72),
            retired_total: read_u64(page, 80),
        })
    }

    /// Whether the extension names a ledger chain.
    #[must_use]
    pub const fn has_ledger(&self) -> bool {
        self.retire_ledger_head != 0
    }
}

/// One retirement: a page id and the LSN of the publication that made it
/// unreachable. Reclaimable when `retired_lsn < min_pin`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LedgerEntry {
    /// The retired data page. Never a superblock page.
    pub page_id: u64,
    /// The publication LSN whose catalog first no longer names the page.
    pub retired_lsn: u64,
}

/// A decoded retirement ledger page.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct LedgerPage {
    /// The next-older... next-newer page in the chain; 0 = none written.
    /// Only meaningful up to the extension's tail bound.
    pub next_page: u64,
    /// Entries before this index are already reused. Advances in place
    /// under the consumption durability law; never decreases on disk.
    pub consumed_count: u32,
    /// Append-ordered retirement entries.
    pub entries: Vec<LedgerEntry>,
}

impl LedgerPage {
    /// Entries one ledger page can hold at the given page size.
    #[must_use]
    pub const fn capacity(page_size: usize) -> usize {
        (page_size - LEDGER_ENTRIES_OFFSET) / LEDGER_ENTRY_LEN
    }

    /// Encodes this ledger page, computing the CRC over bytes 0..24 and
    /// the entry array (`docs/FREE_PAGES.md` § Ledger pages).
    pub fn encode(&self, page_size: usize) -> DevonResult<Vec<u8>> {
        if self.entries.len() > Self::capacity(page_size) {
            return Err(DevonError::InvalidArgument {
                context: format!(
                    "{} ledger entries exceed the page capacity {}",
                    self.entries.len(),
                    Self::capacity(page_size)
                ),
            });
        }
        let entry_count =
            u32::try_from(self.entries.len()).map_err(|_| DevonError::InvalidArgument {
                context: "ledger entry count exceeds u32".to_owned(),
            })?;
        if self.consumed_count > entry_count {
            return Err(DevonError::InvalidArgument {
                context: format!(
                    "ledger consumed_count {} exceeds entry_count {entry_count}",
                    self.consumed_count
                ),
            });
        }
        let mut page = vec![0_u8; page_size];
        page[..8].copy_from_slice(LEDGER_MAGIC);
        page[8..16].copy_from_slice(&self.next_page.to_le_bytes());
        page[16..20].copy_from_slice(&entry_count.to_le_bytes());
        page[20..24].copy_from_slice(&self.consumed_count.to_le_bytes());
        for (index, entry) in self.entries.iter().enumerate() {
            let offset = LEDGER_ENTRIES_OFFSET + index * LEDGER_ENTRY_LEN;
            page[offset..offset + 8].copy_from_slice(&entry.page_id.to_le_bytes());
            page[offset + 8..offset + 16].copy_from_slice(&entry.retired_lsn.to_le_bytes());
        }
        let checksum = self.checksum(&page);
        page[LEDGER_CRC_OFFSET..LEDGER_CRC_OFFSET + 4].copy_from_slice(&checksum.to_le_bytes());
        Ok(page)
    }

    /// Decodes and validates a ledger page.
    pub fn decode(page: &[u8]) -> DevonResult<Self> {
        if page.len() < LEDGER_ENTRIES_OFFSET {
            return Err(corrupt("ledger page is shorter than its header"));
        }
        if &page[..8] != LEDGER_MAGIC {
            return Err(corrupt("ledger page magic is not DEVONFPL"));
        }
        let entry_count = read_u32(page, 16) as usize;
        let consumed_count = read_u32(page, 20);
        if entry_count > Self::capacity(page.len()) {
            return Err(corrupt("ledger entry_count exceeds the page capacity"));
        }
        if consumed_count as usize > entry_count {
            return Err(corrupt("ledger consumed_count exceeds entry_count"));
        }
        let mut decoded = Self {
            next_page: read_u64(page, 8),
            consumed_count,
            entries: Vec::with_capacity(entry_count),
        };
        for index in 0..entry_count {
            let offset = LEDGER_ENTRIES_OFFSET + index * LEDGER_ENTRY_LEN;
            decoded.entries.push(LedgerEntry {
                page_id: read_u64(page, offset),
                retired_lsn: read_u64(page, offset + 8),
            });
        }
        let expected = read_u32(page, LEDGER_CRC_OFFSET);
        if decoded.checksum(page) != expected {
            return Err(corrupt("ledger page checksum does not match"));
        }
        Ok(decoded)
    }

    /// CRC-32C over bytes 0..24 and the entry array, excluding the CRC
    /// field itself and the reserved word.
    fn checksum(&self, page: &[u8]) -> u32 {
        let entries_end = LEDGER_ENTRIES_OFFSET + self.entries.len() * LEDGER_ENTRY_LEN;
        let mut hasher = 0_u32;
        hasher = crc32c::crc32c_append(hasher, &page[..LEDGER_CRC_OFFSET]);
        crc32c::crc32c_append(hasher, &page[LEDGER_ENTRIES_OFFSET..entries_end])
    }
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

fn corrupt(context: &str) -> DevonError {
    DevonError::Corrupt {
        context: context.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::{LedgerEntry, LedgerPage, SuperblockExtension};

    #[test]
    fn extension_round_trips_and_rejects_flips() {
        let extension = SuperblockExtension {
            retire_ledger_head: 7,
            retire_ledger_tail: 12,
            retired_total: 900,
        };
        let mut page = vec![0_u8; 4096];
        extension.encode_into(&mut page).unwrap();

        assert_eq!(SuperblockExtension::decode_from(&page), Some(extension));
        page[70] ^= 0xff;
        assert_eq!(SuperblockExtension::decode_from(&page), None);
    }

    #[test]
    fn zeroed_extension_region_fails_decode() {
        // An all-zero region has a zero CRC field, which does not match the
        // CRC of zero bytes — pre-FREE_PAGES pages decode as None, which is
        // why open gates the read on the feature bit rather than sniffing.
        let page = vec![0_u8; 4096];
        assert_eq!(SuperblockExtension::decode_from(&page), None);
    }

    #[test]
    fn ledger_page_round_trips() {
        let ledger = LedgerPage {
            next_page: 44,
            consumed_count: 1,
            entries: vec![
                LedgerEntry {
                    page_id: 9,
                    retired_lsn: 3,
                },
                LedgerEntry {
                    page_id: 10,
                    retired_lsn: 3,
                },
            ],
        };
        let page = ledger.encode(4096).unwrap();
        assert_eq!(LedgerPage::decode(&page).unwrap(), ledger);
    }

    #[test]
    fn ledger_rejects_bad_magic_count_and_crc() {
        let ledger = LedgerPage {
            next_page: 0,
            consumed_count: 0,
            entries: vec![LedgerEntry {
                page_id: 5,
                retired_lsn: 1,
            }],
        };
        let good = ledger.encode(4096).unwrap();

        let mut bad_magic = good.clone();
        bad_magic[0] ^= 0xff;
        assert!(LedgerPage::decode(&bad_magic).is_err());

        let mut bad_entry = good.clone();
        bad_entry[super::LEDGER_ENTRIES_OFFSET] ^= 0xff;
        assert!(LedgerPage::decode(&bad_entry).is_err());

        let mut bad_consumed = good;
        bad_consumed[20..24].copy_from_slice(&9_u32.to_le_bytes());
        assert!(LedgerPage::decode(&bad_consumed).is_err());
    }

    #[test]
    fn capacity_matches_the_layout() {
        assert_eq!(LedgerPage::capacity(4096), 254);
        assert_eq!(LedgerPage::capacity(65_536), 4094);
    }
}
