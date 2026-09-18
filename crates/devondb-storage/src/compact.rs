//! Offline database compaction by copying only catalog-reachable pages.
//!
//! Compaction never edits the source inode. It takes the database writer
//! lease and publication gate, refuses a non-empty WAL, writes a fresh file
//! beside the source, syncs it, and atomically renames it over the source.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use devondb_types::logical_type::{B1Rescore, LogicalType, VectorEncoding};
use devondb_types::schema::fold;
use devondb_types::{DevonError, DevonResult};

use crate::catalog::{Catalog, IndexEntry, RelStorage, TableStorage};
use crate::csr_group::CsrGroup;
use crate::hnsw::format::{HnswRoot, LayerDirectory};
use crate::hnsw::index::load_persisted_index;
use crate::lock::{LockPaths, PublicationGate, WriterLease};
use crate::node_group::NodeGroup;
use crate::pager::Pager;
use crate::superblock::MULTIPROCESS_COORDINATION_FLAG;

const DIRECTORY_HEADER_LEN: usize = 16;
const DIRECTORY_ENTRY_LEN: usize = 16;
const NODE_MAGIC: &[u8; 4] = b"NGRP";
const CSR_MAGIC: &[u8; 4] = b"RCSR";
const ZONE_MAP_DIRECTORY_FLAG: u32 = 1;
const COLUMN_ENCODINGS_DIRECTORY_FLAG: u32 = 1 << 1;

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// The main-file sizes observed around one successful compaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompactStats {
    /// Source main-file length before the rewrite.
    pub before_bytes: u64,
    /// Replacement main-file length after the rewrite.
    pub after_bytes: u64,
}

/// Rewrites `path` into a minimal current-format file and atomically swaps it in.
///
/// The operation is offline and nonblocking: a held writer lease or publication
/// gate returns [`DevonError::Busy`]. A present, non-empty `<path>-wal` is also
/// refused; callers must checkpoint it before retrying.
pub fn compact(path: impl AsRef<Path>) -> DevonResult<CompactStats> {
    let main = fs::canonicalize(path.as_ref())?;
    let paths = LockPaths::for_main(&main)?;
    let _writer = WriterLease::try_acquire(&paths)?;
    let gate = PublicationGate::open(&paths)?;
    let _publication = gate.try_exclusive()?;
    refuse_nonempty_wal(&main)?;

    let before_bytes = fs::metadata(&main)?.len();
    let permissions = fs::metadata(&main)?.permissions();
    let (temporary, destination) = create_temporary(&main)?;
    let mut cleanup = TemporaryFile::new(temporary.clone());
    rewrite(&main, destination)?;
    fs::set_permissions(&temporary, permissions)?;
    File::options()
        .read(true)
        .write(true)
        .open(&temporary)?
        .sync_all()?;
    let after_bytes = fs::metadata(&temporary)?.len();

    fs::rename(&temporary, &main)?;
    cleanup.disarm();
    sync_parent(&main)?;
    Ok(CompactStats {
        before_bytes,
        after_bytes,
    })
}

fn rewrite(source_path: &Path, destination: Pager) -> DevonResult<()> {
    let source = Pager::open(source_path)?;
    if source.superblock().requires_read_only() {
        return Err(DevonError::ReadOnly {
            context: "compact refused: the source enables unsupported read-safe metadata"
                .to_owned(),
        });
    }
    let source_catalog = Catalog::load(&source)?;
    let mut catalog = copy_catalog_metadata(&source_catalog)?;
    preserve_coordination_flag(&source, &destination)?;

    let mut copier = PageCopier::new(&source, &destination);
    let layouts = copier.copy_nodes(&source_catalog, &mut catalog)?;
    copier.copy_relationships(&source_catalog, &mut catalog, &layouts)?;
    copier.copy_indexes(&source_catalog, &mut catalog, &layouts)?;
    publish_catalog(&source, &destination, &catalog)?;
    destination.sync()?;
    drop(copier);
    validate_rewrite(&destination, &catalog, &layouts)?;
    Ok(())
}

fn copy_catalog_metadata(source: &Catalog) -> DevonResult<Catalog> {
    let mut destination = Catalog::default();
    for schema in source.node_tables() {
        destination.add_node_table(schema.clone())?;
    }
    for schema in source.rel_tables() {
        destination.add_rel_table(schema.clone())?;
    }
    if let Some(ontology) = source.ontology() {
        for entry in &ontology.interfaces {
            destination.declare_interface(entry.clone())?;
        }
        for entry in &ontology.node_classes {
            destination.declare_node_class(entry.clone())?;
        }
        for entry in &ontology.rel_classes {
            destination.declare_rel_class(entry.clone())?;
        }
    }
    for pin in source.pins() {
        destination.pin(pin.clone())?;
    }
    Ok(destination)
}

fn preserve_coordination_flag(source: &Pager, destination: &Pager) -> DevonResult<()> {
    let flags = source.superblock().feature_flags & MULTIPROCESS_COORDINATION_FLAG;
    if flags != 0 {
        destination.commit_feature_flags(flags)?;
    }
    Ok(())
}

fn publish_catalog(source: &Pager, destination: &Pager, catalog: &Catalog) -> DevonResult<()> {
    if source.superblock().catalog_root == 0 {
        return Ok(());
    }
    let lsn = source.superblock().checkpoint_lsn;
    if lsn == 0 {
        return Err(corrupt("a non-empty catalog has checkpoint LSN zero"));
    }
    catalog.save(destination, lsn)
}

#[derive(Debug, Clone)]
struct NodeLayout {
    row_counts: Vec<usize>,
    total_rows: u64,
}

struct PageCopier<'a> {
    source: &'a Pager,
    destination: &'a Pager,
    directories: BTreeMap<u64, u64>,
    runs: BTreeMap<(u64, u32), u64>,
}

impl<'a> PageCopier<'a> {
    fn new(source: &'a Pager, destination: &'a Pager) -> Self {
        Self {
            source,
            destination,
            directories: BTreeMap::new(),
            runs: BTreeMap::new(),
        }
    }

    fn copy_nodes(
        &mut self,
        source_catalog: &Catalog,
        destination_catalog: &mut Catalog,
    ) -> DevonResult<BTreeMap<String, NodeLayout>> {
        let mut layouts = BTreeMap::new();
        for schema in source_catalog.node_tables().to_vec() {
            let types = schema
                .columns()
                .iter()
                .map(|column| column.ty)
                .collect::<Vec<_>>();
            let old_groups = source_catalog
                .table_storage(schema.name())
                .map_or(&[][..], |storage| storage.groups.as_slice());
            let mut new_groups = Vec::with_capacity(old_groups.len());
            let mut row_counts = Vec::with_capacity(old_groups.len());
            let mut total_rows = 0_u64;
            for old_group in old_groups {
                let group = NodeGroup::read(self.source, *old_group, &types)?;
                total_rows = add_rows(total_rows, group.row_count())?;
                row_counts.push(group.row_count());
                new_groups.push(self.copy_node_group(*old_group, &types)?);
            }
            if !new_groups.is_empty() {
                destination_catalog
                    .set_table_storage(schema.name(), TableStorage { groups: new_groups })?;
            }
            layouts.insert(
                fold(schema.name()).into_owned(),
                NodeLayout {
                    row_counts,
                    total_rows,
                },
            );
        }
        Ok(layouts)
    }

    fn copy_relationships(
        &mut self,
        source_catalog: &Catalog,
        destination_catalog: &mut Catalog,
        layouts: &BTreeMap<String, NodeLayout>,
    ) -> DevonResult<()> {
        for schema in source_catalog.rel_tables().to_vec() {
            let Some(storage) = source_catalog.rel_storage(schema.name()) else {
                continue;
            };
            let from = node_layout(layouts, schema.from())?;
            let to = node_layout(layouts, schema.to())?;
            validate_direction_count(&storage.fwd, from, schema.name(), "forward")?;
            validate_direction_count(&storage.bwd, to, schema.name(), "backward")?;
            let types = schema
                .columns()
                .iter()
                .map(|column| column.ty)
                .collect::<Vec<_>>();
            let fwd =
                self.copy_csr_direction(&storage.fwd, &types, &from.row_counts, to.total_rows)?;
            let bwd =
                self.copy_csr_direction(&storage.bwd, &types, &to.row_counts, from.total_rows)?;
            destination_catalog.set_rel_storage(schema.name(), RelStorage { fwd, bwd })?;
        }
        Ok(())
    }

    fn copy_csr_direction(
        &mut self,
        groups: &[u64],
        types: &[LogicalType],
        grouped_rows: &[usize],
        neighbor_rows: u64,
    ) -> DevonResult<Vec<u64>> {
        groups
            .iter()
            .enumerate()
            .map(|(index, page_id)| {
                if *page_id == 0 {
                    return Ok(0);
                }
                let group = CsrGroup::read_checked(self.source, *page_id, types, neighbor_rows)?;
                if group.row_count() > grouped_rows[index] {
                    return Err(corrupt("CSR group covers more rows than its node group"));
                }
                self.copy_directory(*page_id, CSR_MAGIC, 2 + types.len())
            })
            .collect()
    }

    fn copy_indexes(
        &mut self,
        source_catalog: &Catalog,
        destination_catalog: &mut Catalog,
        layouts: &BTreeMap<String, NodeLayout>,
    ) -> DevonResult<()> {
        for entry in source_catalog.indexes().to_vec() {
            let layout = node_layout(layouts, &entry.table)?;
            let root = self.copy_index(entry.root, layout)?;
            destination_catalog.add_index(IndexEntry { root, ..entry })?;
        }
        Ok(())
    }

    fn copy_index(&mut self, old_root: u64, layout: &NodeLayout) -> DevonResult<u64> {
        let persisted = load_persisted_index(self.source, old_root)?;
        validate_index_geometry(&persisted.root, layout)?;
        let mut cells = Vec::with_capacity(persisted.directory.page_ids().len());
        for old_group in persisted.directory.page_ids() {
            if *old_group == 0 {
                cells.push(0);
                continue;
            }
            let _ =
                CsrGroup::read_checked(self.source, *old_group, &[], persisted.root.covered_rows)?;
            cells.push(self.copy_directory(*old_group, CSR_MAGIC, 2)?);
        }
        self.write_index_root(persisted.root, cells)
    }

    fn write_index_root(&mut self, mut root: HnswRoot, cells: Vec<u64>) -> DevonResult<u64> {
        let directory = LayerDirectory::new(root.layer_count, root.group_count, cells)?;
        let payload = directory.encode();
        root.layer_dir_first_page = self.write_payload(&payload)?;
        root.layer_dir_byte_len = directory.byte_len();
        root.layer_dir_crc32c = directory.crc32c();
        let page_id = self.destination.allocate_page()?;
        let mut page = vec![0_u8; self.page_size()];
        root.encode(&mut page)?;
        self.destination.write_page(page_id, &page)?;
        Ok(page_id)
    }

    fn copy_node_group(&mut self, page_id: u64, types: &[LogicalType]) -> DevonResult<u64> {
        let page = self.source.read_page(page_id)?;
        let flags = read_u32(&page, 12)?;
        if flags & ZONE_MAP_DIRECTORY_FLAG != 0 {
            self.destination.note_zone_maps_written();
        }
        if flags & COLUMN_ENCODINGS_DIRECTORY_FLAG != 0 {
            self.destination.note_column_encodings_written();
        }
        self.copy_directory(page_id, NODE_MAGIC, types.len() + b1_rescore_count(types))
    }

    fn copy_directory(
        &mut self,
        old_page: u64,
        magic: &[u8; 4],
        entry_count: usize,
    ) -> DevonResult<u64> {
        if let Some(page) = self.directories.get(&old_page) {
            return Ok(*page);
        }
        let mut directory = self.source.read_page(old_page)?;
        validate_directory_header(&directory, magic, entry_count)?;
        for index in 0..entry_count {
            let offset = DIRECTORY_HEADER_LEN + index * DIRECTORY_ENTRY_LEN;
            let first_page = read_u64(&directory, offset)?;
            let byte_len = read_u32(&directory, offset + 8)?;
            let replacement = self.copy_run(first_page, byte_len)?;
            directory[offset..offset + 8].copy_from_slice(&replacement.to_le_bytes());
        }
        let new_page = self.destination.allocate_page()?;
        self.destination.write_page(new_page, &directory)?;
        self.directories.insert(old_page, new_page);
        Ok(new_page)
    }

    fn copy_run(&mut self, first_page: u64, byte_len: u32) -> DevonResult<u64> {
        if let Some(page) = self.runs.get(&(first_page, byte_len)) {
            return Ok(*page);
        }
        let page_count = (byte_len as usize).div_ceil(self.page_size());
        if page_count == 0 {
            return Err(corrupt("a live payload run has zero bytes"));
        }
        let new_first = self.destination.allocate_run(page_count)?;
        for offset in 0..page_count {
            let old = add_page_offset(first_page, offset)?;
            let new = add_page_offset(new_first, offset)?;
            self.destination
                .write_page(new, &self.source.read_page(old)?)?;
        }
        self.runs.insert((first_page, byte_len), new_first);
        Ok(new_first)
    }

    fn write_payload(&self, payload: &[u8]) -> DevonResult<u64> {
        if payload.is_empty() {
            return Ok(0);
        }
        let page_count = payload.len().div_ceil(self.page_size());
        let first_page = self.destination.allocate_run(page_count)?;
        for (index, chunk) in payload.chunks(self.page_size()).enumerate() {
            let mut page = vec![0_u8; self.page_size()];
            page[..chunk.len()].copy_from_slice(chunk);
            self.destination
                .write_page(add_page_offset(first_page, index)?, &page)?;
        }
        Ok(first_page)
    }

    fn page_size(&self) -> usize {
        self.source.superblock().page_size as usize
    }
}

fn validate_rewrite(
    pager: &Pager,
    expected: &Catalog,
    layouts: &BTreeMap<String, NodeLayout>,
) -> DevonResult<()> {
    let actual = Catalog::load(pager)?;
    if &actual != expected {
        return Err(corrupt(
            "compacted catalog does not match the rewritten catalog",
        ));
    }
    for index in actual.indexes() {
        let persisted = load_persisted_index(pager, index.root)?;
        validate_index_geometry(&persisted.root, node_layout(layouts, &index.table)?)?;
    }
    Ok(())
}

fn validate_index_geometry(root: &HnswRoot, layout: &NodeLayout) -> DevonResult<()> {
    if root.covered_rows > layout.total_rows {
        return Err(corrupt("HNSW coverage exceeds its compacted node table"));
    }
    let mut start = 0_u64;
    let mut expected_groups = 0_usize;
    for rows in &layout.row_counts {
        if start < root.covered_rows {
            expected_groups += 1;
        }
        start = add_rows(start, *rows)?;
    }
    if root.group_count as usize != expected_groups {
        return Err(corrupt(
            "HNSW group count disagrees with node-group geometry",
        ));
    }
    Ok(())
}

fn node_layout<'a>(
    layouts: &'a BTreeMap<String, NodeLayout>,
    table: &str,
) -> DevonResult<&'a NodeLayout> {
    layouts
        .get(fold(table).as_ref())
        .ok_or_else(|| corrupt("catalog relationship or index endpoint table is missing"))
}

fn validate_direction_count(
    groups: &[u64],
    layout: &NodeLayout,
    relationship: &str,
    direction: &str,
) -> DevonResult<()> {
    if groups.len() > layout.row_counts.len() {
        return Err(corrupt(format!(
            "relationship `{relationship}` {direction} storage has more groups than its endpoint"
        )));
    }
    Ok(())
}

fn b1_rescore_count(types: &[LogicalType]) -> usize {
    types
        .iter()
        .filter(|ty| {
            matches!(
                ty,
                LogicalType::VectorEncoded {
                    encoding: VectorEncoding::B1 {
                        rescore: B1Rescore::F16 | B1Rescore::I8 | B1Rescore::F32,
                        ..
                    },
                    ..
                }
            )
        })
        .count()
}

fn validate_directory_header(page: &[u8], magic: &[u8; 4], entries: usize) -> DevonResult<()> {
    let required = DIRECTORY_HEADER_LEN
        .checked_add(
            entries
                .checked_mul(DIRECTORY_ENTRY_LEN)
                .ok_or_else(|| corrupt("directory entry count overflows its page"))?,
        )
        .ok_or_else(|| corrupt("directory length overflows its page"))?;
    if page.len() < required || page.get(..4) != Some(magic.as_slice()) {
        return Err(corrupt("live group directory has an invalid envelope"));
    }
    Ok(())
}

fn read_u32(bytes: &[u8], offset: usize) -> DevonResult<u32> {
    let value = bytes
        .get(offset..offset + 4)
        .ok_or_else(|| corrupt("page field exceeds its page"))?;
    Ok(u32::from_le_bytes([value[0], value[1], value[2], value[3]]))
}

fn read_u64(bytes: &[u8], offset: usize) -> DevonResult<u64> {
    let value = bytes
        .get(offset..offset + 8)
        .ok_or_else(|| corrupt("page field exceeds its page"))?;
    Ok(u64::from_le_bytes([
        value[0], value[1], value[2], value[3], value[4], value[5], value[6], value[7],
    ]))
}

fn add_rows(total: u64, rows: usize) -> DevonResult<u64> {
    let rows = u64::try_from(rows).map_err(|_| corrupt("row count exceeds u64"))?;
    total
        .checked_add(rows)
        .ok_or_else(|| corrupt("row count exceeds u64"))
}

fn add_page_offset(first: u64, offset: usize) -> DevonResult<u64> {
    let offset = u64::try_from(offset).map_err(|_| corrupt("page run exceeds u64"))?;
    first
        .checked_add(offset)
        .ok_or_else(|| corrupt("page run exceeds u64"))
}

fn refuse_nonempty_wal(main: &Path) -> DevonResult<()> {
    let wal = append_suffix(main, "-wal");
    match fs::metadata(&wal) {
        Ok(metadata) if metadata.len() != 0 => Err(DevonError::Busy {
            context: format!(
                "compact refused: WAL {} is non-empty; checkpoint the database first",
                wal.display()
            ),
        }),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn create_temporary(main: &Path) -> DevonResult<(PathBuf, Pager)> {
    for _ in 0..100 {
        let path = temporary_path(main);
        match Pager::create(&path, page_size(main)?, db_id(main)?) {
            Ok(pager) => return Ok((path, pager)),
            Err(DevonError::Io(error)) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
    Err(DevonError::Busy {
        context: "compact could not reserve a unique temporary file".to_owned(),
    })
}

fn page_size(main: &Path) -> DevonResult<u32> {
    Ok(Pager::open(main)?.superblock().page_size)
}

fn db_id(main: &Path) -> DevonResult<[u8; 16]> {
    Ok(Pager::open(main)?.superblock().db_id)
}

fn temporary_path(main: &Path) -> PathBuf {
    let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    append_suffix(
        main,
        &format!(".compact-{}-{nanos}-{sequence}.tmp", std::process::id()),
    )
}

fn append_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut value = OsString::from(path.as_os_str());
    value.push(suffix);
    value.into()
}

fn sync_parent(path: &Path) -> DevonResult<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let Ok(directory) = File::open(parent) else {
        return Ok(());
    };
    directory.sync_all()?;
    Ok(())
}

struct TemporaryFile {
    path: PathBuf,
    armed: bool,
}

impl TemporaryFile {
    fn new(path: PathBuf) -> Self {
        Self { path, armed: true }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for TemporaryFile {
    fn drop(&mut self) {
        if self.armed {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn corrupt(context: impl Into<String>) -> DevonError {
    DevonError::Corrupt {
        context: context.into(),
    }
}
