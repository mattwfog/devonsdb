//! Read-only physical storage accounting for the CLI statistics surface.
//!
//! This module deliberately uses only `std`: the CLI includes it directly so
//! the inspector cannot acquire a writer lease, create a WAL, or depend on a
//! mutable database handle.  All retained working memory is charged against
//! the caller's limit.

use std::{
    collections::BTreeMap,
    fmt,
    fs::{self, File},
    io::{self, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

const DATABASE_MAGIC: &[u8; 8] = b"DEVONDB\0";
const PACK_MAGIC: &[u8; 9] = b"DEVONPACK";
const SUPERBLOCK_HEADER_LEN: usize = 64;
const SUPERBLOCK_EXTENSION_END: usize = 92;
const PACK_HEADER_LEN: usize = 40;
const PACK_DIRECTORY_ENTRY_LEN: usize = 16;
const PACK_DIRECTORY_CRC_LEN: usize = 4;
const FIRST_DATA_PAGE: u64 = 2;
const FREE_PAGES_FLAG: u64 = 1 << 5;
const MULTIPAGE_CATALOG_FLAG: u64 = 1 << 9;

/// A failure to inspect a database without changing it.
#[derive(Debug)]
pub enum StatsError {
    /// A filesystem read or metadata operation failed.
    Io(io::Error),
    /// Reachable bytes do not obey the frozen on-disk format.
    Corrupt { context: String },
    /// The inspector's bounded working set exceeds the supplied limit.
    BudgetExceeded { context: String },
}

impl fmt::Display for StatsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => error.fmt(formatter),
            Self::Corrupt { context } => write!(formatter, "corrupt database: {context}"),
            Self::BudgetExceeded { context } => {
                write!(formatter, "memory budget exceeded: {context}")
            }
        }
    }
}

impl std::error::Error for StatsError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Corrupt { .. } | Self::BudgetExceeded { .. } => None,
        }
    }
}

impl From<io::Error> for StatsError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// The kind of file inspected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StorageKind {
    /// A page-aligned DevonDB main file.
    Database,
    /// A compressed read-only DEVONPACK container.
    Pack,
}

/// Exact page counts for a main database file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageCounts {
    /// Every page in the main file.
    pub total: u64,
    /// Catalog-reachable table and index pages.
    pub live: u64,
    /// Unconsumed page ids named by the active free-page ledger.
    pub free: u64,
    /// Superblock, active catalog, and active ledger pages.
    pub metadata: u64,
    /// The honest residual: `total - live - free - metadata`.
    pub unaccounted: u64,
}

/// Exact live storage attributed to one catalog table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableBytes {
    /// Catalog display spelling of the table name.
    pub name: String,
    /// Distinct live pages attributed to the table.
    pub pages: u64,
    /// `pages * page_size`.
    pub bytes: u64,
}

/// Whether the authoritative superblock exposes a usable free-page ledger.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FreeLedgerStatus {
    /// Feature bit 5 is clear or the extension names no chain.
    NotPresent,
    /// The extension CRC and reachable ledger chain are valid.
    Active,
    /// Feature bit 5 is set but the extension CRC is invalid.
    Degraded,
    /// DEVONPACK statistics do not inspect the compressed embedded pages.
    NotApplicable,
}

/// One immutable snapshot of physical storage accounting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorageStats {
    /// Main database or DEVONPACK container.
    pub kind: StorageKind,
    /// Physical bytes in the path supplied by the caller.
    pub file_bytes: u64,
    /// Main-file page size, absent for a compressed container.
    pub page_size: Option<u32>,
    /// Main-file page categories, absent for a compressed container.
    pub pages: Option<PageCounts>,
    /// Node tables in bytewise name order.
    pub node_tables: Option<Vec<TableBytes>>,
    /// Relationship tables in bytewise name order.
    pub rel_tables: Option<Vec<TableBytes>>,
    /// WAL sidecar bytes; absent when WAL has no meaning (DEVONPACK).
    pub wal_bytes: Option<u64>,
    /// Set superblock feature names in ascending bit order.
    pub feature_bits: Option<Vec<String>>,
    /// State of the authoritative free-page ledger.
    pub free_ledger: FreeLedgerStatus,
    /// Uncompressed main-file bytes declared by a DEVONPACK header.
    pub pack_logical_bytes: Option<u64>,
    /// Logical main-file pages declared by a DEVONPACK header.
    pub pack_logical_pages: Option<u64>,
}

/// Collects exact physical storage statistics without opening the database
/// for writes or creating sidecars.
pub fn collect(path: impl AsRef<Path>, memory_limit: usize) -> Result<StorageStats, StatsError> {
    let path = path.as_ref();
    let mut file = File::open(path)?;
    let file_bytes = file.metadata()?.len();
    let mut magic = [0_u8; 9];
    read_exact_at(&mut file, &mut magic, 0, "file magic")?;
    if magic == *PACK_MAGIC {
        return collect_pack(file, file_bytes, memory_limit);
    }
    collect_database(path, file, file_bytes, memory_limit)
}

/// Writes the stable one-screen text representation used by both CLI modes.
pub fn write_text(stats: &StorageStats, output: &mut impl Write) -> io::Result<()> {
    writeln!(output, "storage stats")?;
    writeln!(output, "kind: {}", kind_name(stats.kind))?;
    writeln!(output, "file bytes: {}", stats.file_bytes)?;
    match stats.kind {
        StorageKind::Database => write_database_text(stats, output),
        StorageKind::Pack => write_pack_text(stats, output),
    }
}

fn kind_name(kind: StorageKind) -> &'static str {
    match kind {
        StorageKind::Database => "database",
        StorageKind::Pack => "pack",
    }
}

fn write_database_text(stats: &StorageStats, output: &mut impl Write) -> io::Result<()> {
    let page_size = stats.page_size.unwrap_or(0);
    let pages = stats.pages.unwrap_or(PageCounts {
        total: 0,
        live: 0,
        free: 0,
        metadata: 0,
        unaccounted: 0,
    });
    writeln!(output, "page size: {page_size}")?;
    writeln!(output, "pages:")?;
    write_page_count(output, "total", pages.total, page_size)?;
    write_page_count(output, "live", pages.live, page_size)?;
    write_page_count(output, "free", pages.free, page_size)?;
    write_page_count(output, "metadata", pages.metadata, page_size)?;
    write_page_count(output, "unaccounted", pages.unaccounted, page_size)?;
    writeln!(output, "free ledger: {}", ledger_name(stats.free_ledger))?;
    write_tables(
        output,
        "node tables",
        stats.node_tables.as_deref().unwrap_or(&[]),
    )?;
    write_tables(
        output,
        "relationship tables",
        stats.rel_tables.as_deref().unwrap_or(&[]),
    )?;
    writeln!(output, "wal bytes: {}", stats.wal_bytes.unwrap_or(0))?;
    write_features(output, stats.feature_bits.as_deref().unwrap_or(&[]))
}

fn write_pack_text(stats: &StorageStats, output: &mut impl Write) -> io::Result<()> {
    writeln!(
        output,
        "logical main bytes: {}",
        stats.pack_logical_bytes.unwrap_or(0)
    )?;
    writeln!(
        output,
        "logical main pages: {}",
        stats.pack_logical_pages.unwrap_or(0)
    )?;
    writeln!(output, "page size: not applicable (DEVONPACK container)")?;
    writeln!(output, "pages: not applicable (compressed container)")?;
    writeln!(output, "free ledger: not applicable (compressed container)")?;
    writeln!(output, "node tables: not applicable (compressed container)")?;
    writeln!(
        output,
        "relationship tables: not applicable (compressed container)"
    )?;
    writeln!(output, "wal bytes: not applicable (checkpoint image)")?;
    writeln!(
        output,
        "feature bits: not applicable (stored inside compressed pages)"
    )
}

fn write_page_count(
    output: &mut impl Write,
    label: &str,
    pages: u64,
    page_size: u32,
) -> io::Result<()> {
    writeln!(
        output,
        "  {label}: {pages} ({} bytes)",
        pages.saturating_mul(u64::from(page_size))
    )
}

fn write_tables(output: &mut impl Write, label: &str, tables: &[TableBytes]) -> io::Result<()> {
    writeln!(output, "{label}:")?;
    if tables.is_empty() {
        return writeln!(output, "  (none)");
    }
    for table in tables {
        writeln!(
            output,
            "  {}: {} bytes ({} pages)",
            table.name, table.bytes, table.pages
        )?;
    }
    Ok(())
}

fn write_features(output: &mut impl Write, features: &[String]) -> io::Result<()> {
    writeln!(output, "feature bits:")?;
    if features.is_empty() {
        return writeln!(output, "  (none)");
    }
    for feature in features {
        writeln!(output, "  {feature}")?;
    }
    Ok(())
}

fn ledger_name(status: FreeLedgerStatus) -> &'static str {
    match status {
        FreeLedgerStatus::NotPresent => "not present",
        FreeLedgerStatus::Active => "active",
        FreeLedgerStatus::Degraded => "degraded (extension CRC mismatch)",
        FreeLedgerStatus::NotApplicable => "not applicable",
    }
}

fn collect_pack(
    mut file: File,
    file_bytes: u64,
    memory_limit: usize,
) -> Result<StorageStats, StatsError> {
    let mut budget = Budget::new(memory_limit);
    budget.charge(PACK_HEADER_LEN, "DEVONPACK header")?;
    let mut header = [0_u8; PACK_HEADER_LEN];
    read_exact_at(&mut file, &mut header, 0, "DEVONPACK header")?;
    validate_pack_header(&header)?;
    let page_size = read_u32(&header, 12);
    let page_count = read_u64(&header, 16);
    let frame_pages = read_u32(&header, 24);
    let frame_count = read_u64(&header, 28);
    validate_pack_geometry(page_count, frame_pages, frame_count)?;
    validate_pack_directory(&mut file, file_bytes, frame_count, &mut budget)?;
    let logical_bytes = page_count
        .checked_mul(u64::from(page_size))
        .ok_or_else(|| corrupt("DEVONPACK logical byte length overflows u64"))?;
    Ok(StorageStats {
        kind: StorageKind::Pack,
        file_bytes,
        page_size: None,
        pages: None,
        node_tables: None,
        rel_tables: None,
        wal_bytes: None,
        feature_bits: None,
        free_ledger: FreeLedgerStatus::NotApplicable,
        pack_logical_bytes: Some(logical_bytes),
        pack_logical_pages: Some(page_count),
    })
}

fn validate_pack_header(header: &[u8; PACK_HEADER_LEN]) -> Result<(), StatsError> {
    if header[..9] != *PACK_MAGIC {
        return Err(corrupt("bad DEVONPACK magic"));
    }
    if header[9] != 1 {
        return Err(corrupt(format!(
            "unsupported DEVONPACK container version {}",
            header[9]
        )));
    }
    if read_u16(header, 10) != 1 {
        return Err(corrupt("unsupported DEVONPACK codec"));
    }
    validate_page_size(read_u32(header, 12))?;
    if crc32c(&header[..36]) != read_u32(header, 36) {
        return Err(corrupt("DEVONPACK header CRC-32C does not match"));
    }
    Ok(())
}

fn validate_pack_geometry(
    page_count: u64,
    frame_pages: u32,
    frame_count: u64,
) -> Result<(), StatsError> {
    if frame_pages == 0 {
        return Err(corrupt("DEVONPACK frame_pages is zero"));
    }
    if page_count.div_ceil(u64::from(frame_pages)) != frame_count {
        return Err(corrupt("DEVONPACK page_count and frame_count disagree"));
    }
    Ok(())
}

fn validate_pack_directory(
    file: &mut File,
    file_bytes: u64,
    frame_count: u64,
    budget: &mut Budget,
) -> Result<(), StatsError> {
    let directory_len = usize::try_from(frame_count)
        .ok()
        .and_then(|count| count.checked_mul(PACK_DIRECTORY_ENTRY_LEN))
        .ok_or_else(|| corrupt("DEVONPACK directory length overflows usize"))?;
    budget.charge(directory_len, "DEVONPACK frame directory")?;
    let mut directory = vec![0_u8; directory_len];
    read_exact_at(
        file,
        &mut directory,
        PACK_HEADER_LEN as u64,
        "DEVONPACK frame directory",
    )?;
    let mut stored_crc = [0_u8; PACK_DIRECTORY_CRC_LEN];
    read_exact_at(
        file,
        &mut stored_crc,
        (PACK_HEADER_LEN + directory_len) as u64,
        "DEVONPACK directory CRC",
    )?;
    if crc32c(&directory) != u32::from_le_bytes(stored_crc) {
        return Err(corrupt("DEVONPACK directory CRC-32C does not match"));
    }
    validate_pack_frames(&directory, file_bytes, frame_count)?;
    budget.release(directory_len);
    Ok(())
}

fn validate_pack_frames(
    directory: &[u8],
    file_bytes: u64,
    frame_count: u64,
) -> Result<(), StatsError> {
    let frames_offset = (PACK_HEADER_LEN + PACK_DIRECTORY_CRC_LEN) as u64
        + frame_count
            .checked_mul(PACK_DIRECTORY_ENTRY_LEN as u64)
            .ok_or_else(|| corrupt("DEVONPACK frames offset overflows u64"))?;
    for entry in directory.as_chunks::<PACK_DIRECTORY_ENTRY_LEN>().0 {
        let offset = read_u64(entry, 0);
        let len = u64::from(read_u32(entry, 8));
        let end = offset
            .checked_add(len)
            .ok_or_else(|| corrupt("DEVONPACK frame end overflows u64"))?;
        if offset < frames_offset || end > file_bytes {
            return Err(corrupt("DEVONPACK frame lies outside the container"));
        }
    }
    Ok(())
}

fn collect_database(
    path: &Path,
    mut file: File,
    file_bytes: u64,
    memory_limit: usize,
) -> Result<StorageStats, StatsError> {
    let mut budget = Budget::new(memory_limit);
    let selected = select_superblock(&mut file, file_bytes)?;
    let page_size = selected.superblock.page_size;
    validate_main_length(file_bytes, page_size)?;
    budget.charge(page_size as usize * 2, "two-page inspection scratch")?;
    let total_pages = file_bytes / u64::from(page_size);
    let class_len =
        usize::try_from(total_pages).map_err(|_| corrupt("main-file page count exceeds usize"))?;
    budget.charge(class_len, "page-accounting map")?;
    let mut accounting = Accounting::new(class_len, page_size);
    accounting.mark(0, PageClass::Superblock, "superblock slot 0")?;
    accounting.mark(1, PageClass::Superblock, "superblock slot 1")?;

    let catalog = load_catalog(
        &mut file,
        &selected.superblock,
        &mut accounting,
        &mut budget,
    )?;
    let ledger = account_free_ledger(&mut file, &selected, &mut accounting, &mut budget)?;
    let (mut node_tables, rel_tables) =
        account_tables(&mut file, &catalog, &mut accounting, &mut budget)?;
    account_indexes(
        &mut file,
        &catalog,
        &mut accounting,
        &mut budget,
        &mut node_tables,
    )?;
    let pages = accounting.counts();
    let wal_bytes = sidecar_len(&wal_path(path))?;
    budget.charge(64 * 64, "feature-bit names")?;
    Ok(StorageStats {
        kind: StorageKind::Database,
        file_bytes,
        page_size: Some(page_size),
        pages: Some(pages),
        node_tables: Some(finish_tables(node_tables, page_size)),
        rel_tables: Some(finish_tables(rel_tables, page_size)),
        wal_bytes: Some(wal_bytes),
        feature_bits: Some(feature_names(selected.superblock.feature_flags)),
        free_ledger: ledger,
        pack_logical_bytes: None,
        pack_logical_pages: None,
    })
}

#[derive(Clone, Copy)]
struct Superblock {
    feature_flags: u64,
    page_size: u32,
    checkpoint_lsn: u64,
    catalog_root: u64,
}

struct SelectedSuperblock {
    superblock: Superblock,
    slot: u8,
}

fn select_superblock(file: &mut File, file_bytes: u64) -> Result<SelectedSuperblock, StatsError> {
    let slot_zero = read_superblock_at(file, 0);
    let slot_one = match slot_zero {
        Ok(superblock) => read_superblock_at(file, u64::from(superblock.page_size))
            .and_then(|other| same_page_size(superblock, other)),
        Err(_) => locate_second_superblock(file, file_bytes),
    };
    choose_superblock(slot_zero, slot_one)
}

fn locate_second_superblock(file: &mut File, file_bytes: u64) -> Result<Superblock, StatsError> {
    for page_size in [4096_u32, 8192, 16_384, 32_768, 65_536] {
        if u64::from(page_size) + SUPERBLOCK_HEADER_LEN as u64 > file_bytes {
            continue;
        }
        if let Ok(superblock) = read_superblock_at(file, u64::from(page_size))
            && superblock.page_size == page_size
        {
            return Ok(superblock);
        }
    }
    Err(corrupt("could not locate a valid second superblock slot"))
}

fn read_superblock_at(file: &mut File, offset: u64) -> Result<Superblock, StatsError> {
    let mut header = [0_u8; SUPERBLOCK_HEADER_LEN];
    read_exact_at(file, &mut header, offset, "superblock header")?;
    if header[..8] != *DATABASE_MAGIC {
        return Err(corrupt("invalid superblock magic"));
    }
    if crc32c(&header[..60]) != read_u32(&header, 60) {
        return Err(corrupt("superblock CRC-32C does not match"));
    }
    let format_version = read_u32(&header, 8);
    let min_reader_version = read_u32(&header, 12);
    if min_reader_version > format_version || min_reader_version > 1 {
        return Err(corrupt("unsupported superblock version"));
    }
    let page_size = read_u32(&header, 24);
    validate_page_size(page_size)?;
    Ok(Superblock {
        feature_flags: read_u64(&header, 16),
        page_size,
        checkpoint_lsn: read_u64(&header, 44),
        catalog_root: read_u64(&header, 52),
    })
}

fn same_page_size(first: Superblock, second: Superblock) -> Result<Superblock, StatsError> {
    if first.page_size != second.page_size {
        return Err(corrupt("superblock slots disagree on page size"));
    }
    Ok(second)
}

fn choose_superblock(
    first: Result<Superblock, StatsError>,
    second: Result<Superblock, StatsError>,
) -> Result<SelectedSuperblock, StatsError> {
    match (first, second) {
        (Ok(a), Ok(b)) if b.checkpoint_lsn > a.checkpoint_lsn => Ok(SelectedSuperblock {
            superblock: b,
            slot: 1,
        }),
        (Ok(a), Ok(_)) | (Ok(a), Err(_)) => Ok(SelectedSuperblock {
            superblock: a,
            slot: 0,
        }),
        (Err(_), Ok(b)) => Ok(SelectedSuperblock {
            superblock: b,
            slot: 1,
        }),
        (Err(first_error), Err(_)) => Err(first_error),
    }
}

fn validate_page_size(page_size: u32) -> Result<(), StatsError> {
    if !(4096..=65_536).contains(&page_size) || !page_size.is_power_of_two() {
        return Err(corrupt("invalid page size"));
    }
    Ok(())
}

fn validate_main_length(file_bytes: u64, page_size: u32) -> Result<(), StatsError> {
    let page_size = u64::from(page_size);
    if file_bytes < page_size * 2 || !file_bytes.is_multiple_of(page_size) {
        return Err(corrupt("main-file length is not a whole number of pages"));
    }
    Ok(())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PageClass {
    Unknown,
    Superblock,
    Catalog,
    Ledger,
    Live,
    Free,
}

struct Accounting {
    classes: Vec<PageClass>,
    page_size: u32,
}

impl Accounting {
    fn new(page_count: usize, page_size: u32) -> Self {
        Self {
            classes: vec![PageClass::Unknown; page_count],
            page_size,
        }
    }

    fn mark(&mut self, page_id: u64, class: PageClass, context: &str) -> Result<(), StatsError> {
        let index = usize::try_from(page_id)
            .ok()
            .filter(|index| *index < self.classes.len())
            .ok_or_else(|| corrupt(format!("{context} names page {page_id} beyond EOF")))?;
        if self.classes[index] != PageClass::Unknown {
            return Err(corrupt(format!(
                "{context} page {page_id} overlaps another accounting category"
            )));
        }
        self.classes[index] = class;
        Ok(())
    }

    fn counts(&self) -> PageCounts {
        let count = |wanted: fn(PageClass) -> bool| {
            self.classes.iter().filter(|class| wanted(**class)).count() as u64
        };
        PageCounts {
            total: self.classes.len() as u64,
            live: count(|class| class == PageClass::Live),
            free: count(|class| class == PageClass::Free),
            metadata: count(|class| {
                matches!(
                    class,
                    PageClass::Superblock | PageClass::Catalog | PageClass::Ledger
                )
            }),
            unaccounted: count(|class| class == PageClass::Unknown),
        }
    }
}

fn read_page(
    file: &mut File,
    accounting: &Accounting,
    page_id: u64,
    context: &str,
) -> Result<Vec<u8>, StatsError> {
    let offset = page_id
        .checked_mul(u64::from(accounting.page_size))
        .ok_or_else(|| corrupt(format!("{context} page offset overflows u64")))?;
    let mut page = vec![0_u8; accounting.page_size as usize];
    read_exact_at(file, &mut page, offset, context)?;
    Ok(page)
}

#[derive(Default)]
struct CatalogFacts {
    nodes: BTreeMap<String, TableDefinition>,
    relationships: BTreeMap<String, TableDefinition>,
    storage: BTreeMap<String, StorageRoots>,
    indexes: Vec<IndexRoot>,
}

#[derive(Default)]
struct TableDefinition {
    name: String,
    column_count: usize,
    rescore_count: usize,
}

#[derive(Default)]
struct StorageRoots {
    groups: Vec<u64>,
    forward: Vec<u64>,
    backward: Vec<u64>,
}

struct IndexRoot {
    table: String,
    root: u64,
}

fn load_catalog(
    file: &mut File,
    superblock: &Superblock,
    accounting: &mut Accounting,
    budget: &mut Budget,
) -> Result<CatalogFacts, StatsError> {
    if superblock.catalog_root == 0 {
        return Ok(CatalogFacts::default());
    }
    accounting.mark(superblock.catalog_root, PageClass::Catalog, "catalog root")?;
    let root = read_page(file, accounting, superblock.catalog_root, "catalog root")?;
    let payload = if superblock.feature_flags & MULTIPAGE_CATALOG_FLAG != 0 {
        load_multipage_catalog(file, accounting, budget, &root)?
    } else {
        load_single_catalog(budget, &root)?
    };
    budget.charge(payload.len(), "parsed catalog accounting")?;
    let parsed = JsonParser::new(&payload).parse_catalog();
    budget.release(payload.len());
    parsed
}

fn load_single_catalog(budget: &mut Budget, root: &[u8]) -> Result<Vec<u8>, StatsError> {
    let payload_len = read_u32(root, 0) as usize;
    let end = 8_usize
        .checked_add(payload_len)
        .filter(|end| *end <= root.len())
        .ok_or_else(|| corrupt("catalog payload exceeds its page"))?;
    budget.charge(payload_len, "catalog JSON payload")?;
    let payload = root[8..end].to_vec();
    if crc32c(&payload) != read_u32(root, 4) {
        return Err(corrupt("catalog payload CRC-32C does not match"));
    }
    Ok(payload)
}

fn load_multipage_catalog(
    file: &mut File,
    accounting: &mut Accounting,
    budget: &mut Budget,
    root: &[u8],
) -> Result<Vec<u8>, StatsError> {
    let payload_len = read_u32(root, 0) as usize;
    let continuation_count = read_u32(root, 8) as usize;
    if continuation_count != payload_len.div_ceil(accounting.page_size as usize) {
        return Err(corrupt("catalog continuation count is inconsistent"));
    }
    let directory_end = 12_usize
        .checked_add(
            continuation_count
                .checked_mul(8)
                .ok_or_else(|| corrupt("catalog directory length overflows"))?,
        )
        .filter(|end| *end <= root.len())
        .ok_or_else(|| corrupt("catalog continuation directory exceeds its page"))?;
    if root[directory_end..].iter().any(|byte| *byte != 0) {
        return Err(corrupt("catalog directory padding is not zero"));
    }
    budget.charge(payload_len, "multi-page catalog JSON payload")?;
    let mut payload = Vec::with_capacity(payload_len);
    for index in 0..continuation_count {
        let page_id = read_u64(root, 12 + index * 8);
        accounting.mark(page_id, PageClass::Catalog, "catalog continuation")?;
        let page = read_page(file, accounting, page_id, "catalog continuation")?;
        let take = (payload_len - payload.len()).min(page.len());
        payload.extend_from_slice(&page[..take]);
        if page[take..].iter().any(|byte| *byte != 0) {
            return Err(corrupt("catalog continuation padding is not zero"));
        }
    }
    if crc32c(&payload) != read_u32(root, 4) {
        return Err(corrupt("catalog payload CRC-32C does not match"));
    }
    Ok(payload)
}

fn account_free_ledger(
    file: &mut File,
    selected: &SelectedSuperblock,
    accounting: &mut Accounting,
    budget: &mut Budget,
) -> Result<FreeLedgerStatus, StatsError> {
    if selected.superblock.feature_flags & FREE_PAGES_FLAG == 0 {
        return Ok(FreeLedgerStatus::NotPresent);
    }
    let extension = read_extension(file, selected)?;
    let Some((head, tail)) = extension else {
        return Ok(FreeLedgerStatus::Degraded);
    };
    if head == 0 {
        return Ok(FreeLedgerStatus::NotPresent);
    }
    if tail == 0 {
        return Err(corrupt("free-page ledger head is set but tail is zero"));
    }
    budget.charge(accounting.classes.len(), "free-ledger cycle map")?;
    let mut visited = vec![false; accounting.classes.len()];
    let result = walk_free_ledger(file, head, tail, accounting, &mut visited);
    budget.release(visited.len());
    result?;
    Ok(FreeLedgerStatus::Active)
}

fn read_extension(
    file: &mut File,
    selected: &SelectedSuperblock,
) -> Result<Option<(u64, u64)>, StatsError> {
    let offset = u64::from(selected.slot) * u64::from(selected.superblock.page_size);
    let mut prefix = [0_u8; SUPERBLOCK_EXTENSION_END];
    read_exact_at(file, &mut prefix, offset, "superblock extension")?;
    if crc32c(&prefix[64..88]) != read_u32(&prefix, 88) {
        return Ok(None);
    }
    Ok(Some((read_u64(&prefix, 64), read_u64(&prefix, 72))))
}

fn walk_free_ledger(
    file: &mut File,
    mut cursor: u64,
    tail: u64,
    accounting: &mut Accounting,
    visited: &mut [bool],
) -> Result<(), StatsError> {
    loop {
        let index = usize::try_from(cursor)
            .ok()
            .filter(|index| *index < visited.len())
            .ok_or_else(|| corrupt("free-page ledger page lies beyond EOF"))?;
        if std::mem::replace(&mut visited[index], true) {
            return Err(corrupt("free-page ledger chain contains a cycle"));
        }
        accounting.mark(cursor, PageClass::Ledger, "free-page ledger")?;
        let page = read_page(file, accounting, cursor, "free-page ledger")?;
        let decoded = decode_ledger_page(&page)?;
        for page_id in decoded.free_pages {
            accounting.mark(page_id, PageClass::Free, "free-page ledger entry")?;
        }
        if cursor == tail {
            if decoded.next_page != 0 {
                return Err(corrupt("free-page ledger tail names a successor"));
            }
            return Ok(());
        }
        if decoded.next_page == 0 {
            return Ok(());
        }
        cursor = decoded.next_page;
    }
}

struct LedgerPage {
    next_page: u64,
    free_pages: Vec<u64>,
}

fn decode_ledger_page(page: &[u8]) -> Result<LedgerPage, StatsError> {
    if page.len() < 32 || &page[..8] != b"DEVONFPL" {
        return Err(corrupt("free-page ledger magic is invalid"));
    }
    let entry_count = read_u32(page, 16) as usize;
    let consumed_count = read_u32(page, 20) as usize;
    let entries_end = 32_usize
        .checked_add(
            entry_count
                .checked_mul(16)
                .ok_or_else(|| corrupt("ledger entry length overflows"))?,
        )
        .filter(|end| *end <= page.len())
        .ok_or_else(|| corrupt("ledger entry_count exceeds page capacity"))?;
    if consumed_count > entry_count {
        return Err(corrupt("ledger consumed_count exceeds entry_count"));
    }
    let mut computed = crc32c_append(0, &page[..24]);
    computed = crc32c_append(computed, &page[32..entries_end]);
    if computed != read_u32(page, 24) {
        return Err(corrupt("free-page ledger CRC-32C does not match"));
    }
    let free_pages = (consumed_count..entry_count)
        .map(|index| read_u64(page, 32 + index * 16))
        .collect();
    Ok(LedgerPage {
        next_page: read_u64(page, 8),
        free_pages,
    })
}

type TablePageCounts = BTreeMap<String, u64>;

fn account_tables(
    file: &mut File,
    catalog: &CatalogFacts,
    accounting: &mut Accounting,
    budget: &mut Budget,
) -> Result<(TablePageCounts, TablePageCounts), StatsError> {
    let table_bytes = catalog
        .nodes
        .values()
        .chain(catalog.relationships.values())
        .try_fold(0_usize, |total, definition| {
            total.checked_add(64 + definition.name.len())
        })
        .ok_or_else(|| corrupt("per-table accounting size overflows usize"))?;
    budget.charge(table_bytes, "per-table accounting")?;
    let mut nodes = zero_table_counts(&catalog.nodes);
    let mut relationships = zero_table_counts(&catalog.relationships);
    for (folded, definition) in &catalog.nodes {
        let Some(storage) = catalog.storage.get(folded) else {
            continue;
        };
        for root in &storage.groups {
            account_group(
                file,
                *root,
                GroupLayout {
                    columns: definition.column_count,
                    entries: definition.column_count + definition.rescore_count,
                    magic: *b"NGRP",
                },
                accounting,
                table_counter(&mut nodes, &definition.name)?,
            )?;
        }
    }
    for (folded, definition) in &catalog.relationships {
        let Some(storage) = catalog.storage.get(folded) else {
            continue;
        };
        for root in storage.forward.iter().chain(&storage.backward) {
            if *root == 0 {
                continue;
            }
            account_group(
                file,
                *root,
                GroupLayout {
                    columns: definition.column_count,
                    entries: definition.column_count + 2,
                    magic: *b"RCSR",
                },
                accounting,
                table_counter(&mut relationships, &definition.name)?,
            )?;
        }
    }
    Ok((nodes, relationships))
}

fn zero_table_counts(definitions: &BTreeMap<String, TableDefinition>) -> TablePageCounts {
    definitions
        .values()
        .map(|definition| (definition.name.clone(), 0))
        .collect()
}

fn table_counter<'a>(
    tables: &'a mut TablePageCounts,
    name: &str,
) -> Result<&'a mut u64, StatsError> {
    tables
        .get_mut(name)
        .ok_or_else(|| corrupt(format!("missing accounting slot for table `{name}`")))
}

#[derive(Clone, Copy)]
struct GroupLayout {
    columns: usize,
    entries: usize,
    magic: [u8; 4],
}

fn account_group(
    file: &mut File,
    root: u64,
    layout: GroupLayout,
    accounting: &mut Accounting,
    table_pages: &mut u64,
) -> Result<(), StatsError> {
    accounting.mark(root, PageClass::Live, "table directory")?;
    *table_pages = table_pages
        .checked_add(1)
        .ok_or_else(|| corrupt("per-table page count overflows u64"))?;
    let page = read_page(file, accounting, root, "table directory")?;
    if page[..4] != layout.magic {
        return Err(corrupt("catalog root names the wrong table directory kind"));
    }
    validate_directory_column_count(&page, &layout.magic, layout.columns)?;
    let entries_end = 16_usize
        .checked_add(
            layout
                .entries
                .checked_mul(16)
                .ok_or_else(|| corrupt("directory entry length overflows"))?,
        )
        .filter(|end| *end <= page.len())
        .ok_or_else(|| corrupt("directory entries exceed their page"))?;
    for offset in (16..entries_end).step_by(16) {
        let first_page = read_u64(&page, offset);
        let byte_len = read_u32(&page, offset + 8);
        account_payload_run(accounting, first_page, byte_len, table_pages)?;
    }
    Ok(())
}

fn validate_directory_column_count(
    page: &[u8],
    magic: &[u8; 4],
    expected_columns: usize,
) -> Result<(), StatsError> {
    let stored_columns = if magic == b"NGRP" {
        read_u32(page, 8) as usize
    } else {
        read_u32(page, 12) as usize
    };
    if stored_columns != expected_columns {
        return Err(corrupt(
            "table directory column_count disagrees with schema",
        ));
    }
    Ok(())
}

fn account_payload_run(
    accounting: &mut Accounting,
    first_page: u64,
    byte_len: u32,
    table_pages: &mut u64,
) -> Result<(), StatsError> {
    if first_page < FIRST_DATA_PAGE {
        return Err(corrupt("payload run names a reserved page"));
    }
    let count = (u64::from(byte_len))
        .div_ceil(u64::from(accounting.page_size))
        .max(1);
    for index in 0..count {
        let page_id = first_page
            .checked_add(index)
            .ok_or_else(|| corrupt("payload page run overflows u64"))?;
        accounting.mark(page_id, PageClass::Live, "table payload")?;
        *table_pages = table_pages
            .checked_add(1)
            .ok_or_else(|| corrupt("per-table page count overflows u64"))?;
    }
    Ok(())
}

fn account_indexes(
    file: &mut File,
    catalog: &CatalogFacts,
    accounting: &mut Accounting,
    budget: &mut Budget,
    node_tables: &mut TablePageCounts,
) -> Result<(), StatsError> {
    for index in &catalog.indexes {
        let folded = fold(&index.table);
        let definition = catalog
            .nodes
            .get(&folded)
            .ok_or_else(|| corrupt("index names an unknown node table"))?;
        let table_pages = table_counter(node_tables, &definition.name)?;
        account_one_index(file, index.root, accounting, budget, table_pages)?;
    }
    Ok(())
}

fn account_one_index(
    file: &mut File,
    root: u64,
    accounting: &mut Accounting,
    budget: &mut Budget,
    index_pages: &mut u64,
) -> Result<(), StatsError> {
    accounting.mark(root, PageClass::Live, "HNSW root")?;
    *index_pages += 1;
    let page = read_page(file, accounting, root, "HNSW root")?;
    if &page[..4] != b"HNSW" {
        return Err(corrupt("index root magic is not HNSW"));
    }
    let first_page = read_u64(&page, 48);
    let byte_len = read_u32(&page, 56);
    let expected_byte_len = u64::from(page[41])
        .checked_mul(u64::from(read_u32(&page, 44)))
        .and_then(|cells| cells.checked_mul(8))
        .ok_or_else(|| corrupt("HNSW layer-directory dimensions overflow"))?;
    if expected_byte_len != u64::from(byte_len) {
        return Err(corrupt("HNSW layer-directory dimensions disagree"));
    }
    if byte_len == 0 {
        if first_page != 0 {
            return Err(corrupt("empty HNSW layer directory has a page id"));
        }
        return Ok(());
    }
    let payload = read_live_payload(
        file,
        accounting,
        budget,
        first_page,
        byte_len,
        index_pages,
        "HNSW layer directory",
    )?;
    if crc32c(&payload) != read_u32(&page, 60) {
        return Err(corrupt("HNSW layer-directory CRC-32C does not match"));
    }
    if !payload.len().is_multiple_of(8) {
        return Err(corrupt("HNSW layer-directory length is not cell-aligned"));
    }
    for cell in payload.as_chunks::<8>().0 {
        let csr_root = read_u64(cell, 0);
        if csr_root != 0 {
            account_group(
                file,
                csr_root,
                GroupLayout {
                    columns: 0,
                    entries: 2,
                    magic: *b"RCSR",
                },
                accounting,
                index_pages,
            )?;
        }
    }
    budget.release(payload.len());
    Ok(())
}

fn read_live_payload(
    file: &mut File,
    accounting: &mut Accounting,
    budget: &mut Budget,
    first_page: u64,
    byte_len: u32,
    pages: &mut u64,
    context: &str,
) -> Result<Vec<u8>, StatsError> {
    budget.charge(byte_len as usize, context)?;
    let page_count = u64::from(byte_len).div_ceil(u64::from(accounting.page_size));
    let mut payload = Vec::with_capacity(byte_len as usize);
    for index in 0..page_count {
        let page_id = first_page
            .checked_add(index)
            .ok_or_else(|| corrupt(format!("{context} page run overflows")))?;
        accounting.mark(page_id, PageClass::Live, context)?;
        *pages += 1;
        let page = read_page(file, accounting, page_id, context)?;
        let take = (byte_len as usize - payload.len()).min(page.len());
        payload.extend_from_slice(&page[..take]);
        if page[take..].iter().any(|byte| *byte != 0) {
            return Err(corrupt(format!("{context} padding is not zero")));
        }
    }
    Ok(payload)
}

fn finish_tables(tables: TablePageCounts, page_size: u32) -> Vec<TableBytes> {
    tables
        .into_iter()
        .map(|(name, pages)| TableBytes {
            name,
            pages,
            bytes: pages.saturating_mul(u64::from(page_size)),
        })
        .collect()
}

fn feature_names(flags: u64) -> Vec<String> {
    (0_u32..64)
        .filter(|bit| flags & (1_u64 << bit) != 0)
        .map(|bit| match feature_name(bit) {
            Some(name) => format!("{name} (bit {bit})"),
            None => format!("UNKNOWN (bit {bit})"),
        })
        .collect()
}

fn feature_name(bit: u32) -> Option<&'static str> {
    match bit {
        0 => Some("HNSW_INDEX"),
        1 => Some("ONTOLOGY"),
        2 => Some("PINNED_PLANS"),
        3 => Some("ZONE_MAPS"),
        4 => Some("GEO_COLUMNS"),
        5 => Some("FREE_PAGES"),
        6 => Some("DML_WAL"),
        7 => Some("SCALAR_TYPES_V2"),
        8 => Some("MULTIPROCESS_COORDINATION"),
        9 => Some("MULTIPAGE_CATALOG"),
        10 => Some("REL_TOMBSTONE_WAL"),
        12 => Some("RESERVED_READ_SAFE_12"),
        13 => Some("COLUMN_ENCODINGS"),
        _ => None,
    }
}

fn sidecar_len(path: &Path) -> Result<u64, StatsError> {
    match fs::metadata(path) {
        Ok(metadata) => Ok(metadata.len()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(0),
        Err(error) => Err(error.into()),
    }
}

fn wal_path(path: &Path) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push("-wal");
    value.into()
}

struct Budget {
    limit: usize,
    charged: usize,
}

impl Budget {
    fn new(limit: usize) -> Self {
        Self { limit, charged: 0 }
    }

    fn charge(&mut self, bytes: usize, purpose: &str) -> Result<(), StatsError> {
        let next = self.charged.checked_add(bytes).ok_or_else(|| {
            budget_exceeded(format!("{purpose} allocation overflows the host size"))
        })?;
        if next > self.limit {
            return Err(budget_exceeded(format!(
                "{purpose} needs {bytes} bytes with {} of {} bytes already charged",
                self.charged, self.limit
            )));
        }
        self.charged = next;
        Ok(())
    }

    fn release(&mut self, bytes: usize) {
        self.charged = self.charged.saturating_sub(bytes);
    }
}

struct JsonParser<'a> {
    bytes: &'a [u8],
    position: usize,
    depth: usize,
}

impl<'a> JsonParser<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self {
            bytes,
            position: 0,
            depth: 0,
        }
    }

    fn parse_catalog(mut self) -> Result<CatalogFacts, StatsError> {
        let mut facts = CatalogFacts::default();
        self.expect(b'{')?;
        while !self.consume(b'}') {
            let key = self.string()?;
            self.expect(b':')?;
            match key.as_str() {
                "node_tables" => self.table_definitions(&mut facts.nodes, true)?,
                "rel_tables" => self.table_definitions(&mut facts.relationships, false)?,
                "storage" => self.storage(&mut facts.storage)?,
                "indexes" => self.indexes(&mut facts.indexes)?,
                _ => self.skip_value()?,
            }
            if !self.consume(b',') {
                self.expect(b'}')?;
                break;
            }
        }
        self.whitespace();
        if self.position != self.bytes.len() {
            return Err(corrupt("catalog JSON has trailing bytes"));
        }
        Ok(facts)
    }

    fn table_definitions(
        &mut self,
        output: &mut BTreeMap<String, TableDefinition>,
        node: bool,
    ) -> Result<(), StatsError> {
        self.expect(b'[')?;
        while !self.consume(b']') {
            let definition = self.table_definition(node)?;
            let folded = fold(&definition.name);
            if output.insert(folded, definition).is_some() {
                return Err(corrupt("catalog contains fold-equal table names"));
            }
            if !self.consume(b',') {
                self.expect(b']')?;
                break;
            }
        }
        Ok(())
    }

    fn table_definition(&mut self, node: bool) -> Result<TableDefinition, StatsError> {
        let mut definition = TableDefinition::default();
        self.expect(b'{')?;
        while !self.consume(b'}') {
            let key = self.string()?;
            self.expect(b':')?;
            match key.as_str() {
                "name" => definition.name = self.string()?,
                "columns" => {
                    let (columns, rescores) = self.columns()?;
                    definition.column_count = columns;
                    definition.rescore_count = if node { rescores } else { 0 };
                }
                _ => self.skip_value()?,
            }
            if !self.consume(b',') {
                self.expect(b'}')?;
                break;
            }
        }
        if definition.name.is_empty() {
            return Err(corrupt("catalog table has an empty name"));
        }
        Ok(definition)
    }

    fn columns(&mut self) -> Result<(usize, usize), StatsError> {
        let mut columns = 0_usize;
        let mut rescores = 0_usize;
        self.expect(b'[')?;
        while !self.consume(b']') {
            rescores += usize::from(self.column_has_rescore()?);
            columns += 1;
            if !self.consume(b',') {
                self.expect(b']')?;
                break;
            }
        }
        Ok((columns, rescores))
    }

    fn column_has_rescore(&mut self) -> Result<bool, StatsError> {
        let mut rescore = false;
        self.expect(b'{')?;
        while !self.consume(b'}') {
            let key = self.string()?;
            self.expect(b':')?;
            if key == "ty" {
                rescore = self.type_has_rescore()?;
            } else {
                self.skip_value()?;
            }
            if !self.consume(b',') {
                self.expect(b'}')?;
                break;
            }
        }
        Ok(rescore)
    }

    fn type_has_rescore(&mut self) -> Result<bool, StatsError> {
        if self.peek() == Some(b'"') {
            self.string()?;
            return Ok(false);
        }
        let mut rescore = false;
        self.expect(b'{')?;
        while !self.consume(b'}') {
            let key = self.string()?;
            self.expect(b':')?;
            if key == "VectorEncoded" {
                rescore = self.vector_encoded_has_rescore()?;
            } else {
                self.skip_value()?;
            }
            if !self.consume(b',') {
                self.expect(b'}')?;
                break;
            }
        }
        Ok(rescore)
    }

    fn vector_encoded_has_rescore(&mut self) -> Result<bool, StatsError> {
        let mut result = false;
        self.expect(b'{')?;
        while !self.consume(b'}') {
            let key = self.string()?;
            self.expect(b':')?;
            if key == "encoding" {
                result = self.encoding_has_rescore()?;
            } else {
                self.skip_value()?;
            }
            if !self.consume(b',') {
                self.expect(b'}')?;
                break;
            }
        }
        Ok(result)
    }

    fn encoding_has_rescore(&mut self) -> Result<bool, StatsError> {
        let mut kind = String::new();
        let mut rescore = String::new();
        self.expect(b'{')?;
        while !self.consume(b'}') {
            let key = self.string()?;
            self.expect(b':')?;
            match key.as_str() {
                "kind" => kind = self.string()?,
                "rescore" => rescore = self.string()?,
                _ => self.skip_value()?,
            }
            if !self.consume(b',') {
                self.expect(b'}')?;
                break;
            }
        }
        Ok(kind == "b1" && !rescore.is_empty() && rescore != "none")
    }

    fn storage(&mut self, output: &mut BTreeMap<String, StorageRoots>) -> Result<(), StatsError> {
        self.expect(b'{')?;
        while !self.consume(b'}') {
            let table = self.string()?;
            self.expect(b':')?;
            let roots = self.storage_roots()?;
            if output.insert(fold(&table), roots).is_some() {
                return Err(corrupt("catalog storage has fold-equal table keys"));
            }
            if !self.consume(b',') {
                self.expect(b'}')?;
                break;
            }
        }
        Ok(())
    }

    fn storage_roots(&mut self) -> Result<StorageRoots, StatsError> {
        let mut roots = StorageRoots::default();
        self.expect(b'{')?;
        while !self.consume(b'}') {
            let key = self.string()?;
            self.expect(b':')?;
            match key.as_str() {
                "groups" => roots.groups = self.page_ids()?,
                "fwd" => roots.forward = self.page_ids()?,
                "bwd" => roots.backward = self.page_ids()?,
                _ => self.skip_value()?,
            }
            if !self.consume(b',') {
                self.expect(b'}')?;
                break;
            }
        }
        Ok(roots)
    }

    fn page_ids(&mut self) -> Result<Vec<u64>, StatsError> {
        let mut ids = Vec::new();
        self.expect(b'[')?;
        while !self.consume(b']') {
            ids.push(self.u64()?);
            if !self.consume(b',') {
                self.expect(b']')?;
                break;
            }
        }
        Ok(ids)
    }

    fn indexes(&mut self, output: &mut Vec<IndexRoot>) -> Result<(), StatsError> {
        self.expect(b'[')?;
        while !self.consume(b']') {
            output.push(self.index()?);
            if !self.consume(b',') {
                self.expect(b']')?;
                break;
            }
        }
        Ok(())
    }

    fn index(&mut self) -> Result<IndexRoot, StatsError> {
        let mut table = String::new();
        let mut root = 0_u64;
        self.expect(b'{')?;
        while !self.consume(b'}') {
            let key = self.string()?;
            self.expect(b':')?;
            match key.as_str() {
                "table" => table = self.string()?,
                "root" => root = self.u64()?,
                _ => self.skip_value()?,
            }
            if !self.consume(b',') {
                self.expect(b'}')?;
                break;
            }
        }
        if table.is_empty() || root < FIRST_DATA_PAGE {
            return Err(corrupt("catalog index entry is incomplete"));
        }
        Ok(IndexRoot { table, root })
    }

    fn skip_value(&mut self) -> Result<(), StatsError> {
        if self.depth >= 256 {
            return Err(corrupt("catalog JSON nesting exceeds 256 levels"));
        }
        self.depth += 1;
        self.whitespace();
        let result = match self.peek() {
            Some(b'{') => self.skip_object(),
            Some(b'[') => self.skip_array(),
            Some(b'"') => self.string().map(|_| ()),
            Some(b't') => self.literal(b"true"),
            Some(b'f') => self.literal(b"false"),
            Some(b'n') => self.literal(b"null"),
            Some(b'-' | b'0'..=b'9') => self.skip_number(),
            _ => Err(corrupt("catalog JSON contains an invalid value")),
        };
        self.depth -= 1;
        result
    }

    fn skip_object(&mut self) -> Result<(), StatsError> {
        self.expect(b'{')?;
        while !self.consume(b'}') {
            self.string()?;
            self.expect(b':')?;
            self.skip_value()?;
            if !self.consume(b',') {
                self.expect(b'}')?;
                break;
            }
        }
        Ok(())
    }

    fn skip_array(&mut self) -> Result<(), StatsError> {
        self.expect(b'[')?;
        while !self.consume(b']') {
            self.skip_value()?;
            if !self.consume(b',') {
                self.expect(b']')?;
                break;
            }
        }
        Ok(())
    }

    fn skip_number(&mut self) -> Result<(), StatsError> {
        let start = self.position;
        while matches!(
            self.peek(),
            Some(b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9')
        ) {
            self.position += 1;
        }
        if self.position == start {
            return Err(corrupt("catalog JSON number is empty"));
        }
        Ok(())
    }

    fn u64(&mut self) -> Result<u64, StatsError> {
        self.whitespace();
        let start = self.position;
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.position += 1;
        }
        let bytes = self
            .bytes
            .get(start..self.position)
            .filter(|bytes| !bytes.is_empty())
            .ok_or_else(|| corrupt("catalog page id is not an unsigned integer"))?;
        let text = std::str::from_utf8(bytes)
            .map_err(|_| corrupt("catalog page id is not UTF-8 digits"))?;
        text.parse()
            .map_err(|_| corrupt("catalog page id exceeds u64"))
    }

    fn string(&mut self) -> Result<String, StatsError> {
        self.whitespace();
        self.expect_raw(b'"')?;
        let mut decoded = Vec::new();
        loop {
            let byte = self
                .next()
                .ok_or_else(|| corrupt("catalog JSON string is unterminated"))?;
            match byte {
                b'"' => break,
                b'\\' => self.escape(&mut decoded)?,
                0..=31 => return Err(corrupt("catalog JSON string contains a control byte")),
                other => decoded.push(other),
            }
        }
        String::from_utf8(decoded).map_err(|_| corrupt("catalog JSON string is not UTF-8"))
    }

    fn escape(&mut self, decoded: &mut Vec<u8>) -> Result<(), StatsError> {
        match self
            .next()
            .ok_or_else(|| corrupt("catalog JSON escape is incomplete"))?
        {
            b'"' => decoded.push(b'"'),
            b'\\' => decoded.push(b'\\'),
            b'/' => decoded.push(b'/'),
            b'b' => decoded.push(8),
            b'f' => decoded.push(12),
            b'n' => decoded.push(b'\n'),
            b'r' => decoded.push(b'\r'),
            b't' => decoded.push(b'\t'),
            b'u' => self.unicode_escape(decoded)?,
            _ => return Err(corrupt("catalog JSON escape is invalid")),
        }
        Ok(())
    }

    fn unicode_escape(&mut self, decoded: &mut Vec<u8>) -> Result<(), StatsError> {
        let first = self.hex_quad()?;
        let scalar = if (0xd800..=0xdbff).contains(&first) {
            self.expect_raw(b'\\')?;
            self.expect_raw(b'u')?;
            let second = self.hex_quad()?;
            if !(0xdc00..=0xdfff).contains(&second) {
                return Err(corrupt("catalog JSON has an invalid surrogate pair"));
            }
            0x1_0000 + ((u32::from(first) - 0xd800) << 10) + (u32::from(second) - 0xdc00)
        } else if (0xdc00..=0xdfff).contains(&first) {
            return Err(corrupt("catalog JSON has an unpaired low surrogate"));
        } else {
            u32::from(first)
        };
        let character = char::from_u32(scalar)
            .ok_or_else(|| corrupt("catalog JSON Unicode escape is invalid"))?;
        let mut buffer = [0_u8; 4];
        decoded.extend_from_slice(character.encode_utf8(&mut buffer).as_bytes());
        Ok(())
    }

    fn hex_quad(&mut self) -> Result<u16, StatsError> {
        let mut value = 0_u16;
        for _ in 0..4 {
            let digit = self
                .next()
                .and_then(|byte| (byte as char).to_digit(16))
                .ok_or_else(|| corrupt("catalog JSON Unicode escape is incomplete"))?;
            value = value * 16 + digit as u16;
        }
        Ok(value)
    }

    fn literal(&mut self, literal: &[u8]) -> Result<(), StatsError> {
        let end = self.position.saturating_add(literal.len());
        if self.bytes.get(self.position..end) != Some(literal) {
            return Err(corrupt("catalog JSON literal is invalid"));
        }
        self.position = end;
        Ok(())
    }

    fn expect(&mut self, expected: u8) -> Result<(), StatsError> {
        self.whitespace();
        self.expect_raw(expected)
    }

    fn expect_raw(&mut self, expected: u8) -> Result<(), StatsError> {
        if self.next() != Some(expected) {
            return Err(corrupt(format!(
                "catalog JSON expected byte `{}` at offset {}",
                expected as char,
                self.position.saturating_sub(1)
            )));
        }
        Ok(())
    }

    fn consume(&mut self, expected: u8) -> bool {
        self.whitespace();
        if self.peek() == Some(expected) {
            self.position += 1;
            true
        } else {
            false
        }
    }

    fn whitespace(&mut self) {
        while self.peek().is_some_and(|byte| byte.is_ascii_whitespace()) {
            self.position += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.position).copied()
    }

    fn next(&mut self) -> Option<u8> {
        let byte = self.peek()?;
        self.position += 1;
        Some(byte)
    }
}

fn fold(name: &str) -> String {
    name.to_ascii_lowercase()
}

fn read_exact_at(
    file: &mut File,
    destination: &mut [u8],
    offset: u64,
    context: &str,
) -> Result<(), StatsError> {
    file.seek(SeekFrom::Start(offset))?;
    file.read_exact(destination).map_err(|error| {
        if error.kind() == io::ErrorKind::UnexpectedEof {
            corrupt(format!("{context} is truncated"))
        } else {
            error.into()
        }
    })
}

fn read_u16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([bytes[offset], bytes[offset + 1]])
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ])
}

fn read_u64(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
        bytes[offset + 4],
        bytes[offset + 5],
        bytes[offset + 6],
        bytes[offset + 7],
    ])
}

fn crc32c(bytes: &[u8]) -> u32 {
    crc32c_append(0, bytes)
}

fn crc32c_append(previous: u32, bytes: &[u8]) -> u32 {
    let mut crc = !previous;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            let mask = 0_u32.wrapping_sub(crc & 1);
            crc = (crc >> 1) ^ (0x82f6_3b78 & mask);
        }
    }
    !crc
}

fn corrupt(context: impl Into<String>) -> StatsError {
    StatsError::Corrupt {
        context: context.into(),
    }
}

fn budget_exceeded(context: impl Into<String>) -> StatsError {
    StatsError::BudgetExceeded {
        context: context.into(),
    }
}
