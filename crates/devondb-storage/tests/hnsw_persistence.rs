use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crc32c::crc32c;
use devondb_storage::budget::MemoryBudget;
use devondb_storage::csr_group::CsrGroup;
use devondb_storage::hnsw::csr::{CsrSlotReadOptions, CsrSlotReader, VerifiedCsrGroups};
use devondb_storage::hnsw::format::{HnswRoot, LayerDirectory};
use devondb_storage::hnsw::scoring::level_for_node;
use devondb_storage::hnsw::types::{GraphAccess, HnswConfig, HnswMetric, NavigationEncoding};
use devondb_storage::hnsw::view::{HnswGroupLayout, HnswSnapshotView};
use devondb_storage::node_group::NodeGroup;
use devondb_storage::pager::Pager;
use devondb_storage::superblock::{SUPPORTED_FORMAT_VERSION, ZONE_MAPS_FLAG};
use devondb_types::{DevonError, DevonResult, logical_type::LogicalType, value::Value};
use tempfile::{TempDir, tempdir};

const PAGE_SIZE: u32 = 4096;
const DB_ID: [u8; 16] = *b"hnsw-persist-v01";
const VECTOR_DIM: u32 = 3;
const COVERED_ROWS: u64 = 5;
const GROUP_ROW_COUNTS: [u64; 2] = [3, 2];
const LAYER_COUNT: u8 = 2;

// The vector corpus is deliberately smaller than the recall corpus, but uses
// HNSW.md section 9.1's binding LCG constants. Mapping the high 23 state bits
// directly into an f32 mantissa makes the fixture deterministic without any
// platform-dependent random source or floating-point conversion policy.
const LCG_MULTIPLIER: u64 = 6_364_136_223_846_793_005;
const LCG_INCREMENT: u64 = 1_442_695_040_888_963_407;
const CORPUS_SEED: u64 = 0x07_20_48_33_70_db;

// With m=4 this seed gives levels [0, 0, 0, 1, 1] for nodes 0..5. The two
// upper-layer nodes therefore form one valid reciprocal adjacency pair.
const LEVEL_SEED: u64 = 0x10;

const EXPECTED_FILE_LEN: usize = 17 * PAGE_SIZE as usize;
// Re-pinned for SCALE S-1: node groups now carry a `ZONE_MAP_STATS` directory
// section, so every freshly written group's bytes differ from the pre-S-1
// fixture. `EXPECTED_FILE_LEN` is deliberately unchanged — the section lives on
// the directory page that already existed, adding zero pages. This is a
// same-build determinism pin over a hand-built fixture, NOT a golden corpus
// anchor; the committed `tests/golden/*.devondb` files are untouched and still
// read byte-for-byte.
const EXPECTED_FILE_CRC32C: u32 = 0xcb4a_76c3;
const VERIFIED_GROUP_CAPACITY: usize = 3;
const TIGHT_SUCCESS_LIMIT: usize = 240;

type Topology = Vec<Vec<Vec<u64>>>;

#[derive(Debug, Clone, Copy)]
struct FixturePages {
    root: u64,
    layer_directory: u64,
    node_groups: [u64; 2],
    csr_groups: [[u64; 2]; 2],
}

struct PersistedFixture {
    _directory: TempDir,
    path: PathBuf,
    pages: FixturePages,
    root_before_reopen: HnswRoot,
    directory_before_reopen: LayerDirectory,
    topology_before_reopen: Topology,
    base_groups_before_reopen: Vec<NodeGroup>,
}

#[derive(Debug, Clone, Copy)]
enum Corruption {
    Root,
    LayerDirectory,
    CsrOffsets,
    CsrNeighbors,
}

impl Corruption {
    const ALL: [Self; 4] = [
        Self::Root,
        Self::LayerDirectory,
        Self::CsrOffsets,
        Self::CsrNeighbors,
    ];

    const fn expected_context(self) -> &'static str {
        match self {
            Self::Root => "HNSW root",
            Self::LayerDirectory => "HNSW layer-directory",
            Self::CsrOffsets => "CSR offsets",
            Self::CsrNeighbors => "CSR neighbors",
        }
    }
}

#[test]
fn deterministic_fixture_is_byte_stable() {
    let first = write_fixture("stable-first");
    let second = write_fixture("stable-second");
    let first_bytes = fs::read(&first.path).unwrap();
    let second_bytes = fs::read(&second.path).unwrap();

    assert_eq!(first_bytes, second_bytes);
    assert_eq!(first_bytes.len(), EXPECTED_FILE_LEN);
    assert_eq!(crc32c(&first_bytes), EXPECTED_FILE_CRC32C);

    // The whole-file CRC above is STRUCTURALLY BLIND to every superblock
    // header field. Each slot ends in its own CRC-32C over its first 60
    // bytes, and CRC(data || CRC(data)) is a constant residue — so changing
    // `format_version`, `min_reader_version`, `feature_flags`, `page_size`,
    // `db_id`, `checkpoint_lsn`, or `catalog_root` leaves the file CRC
    // untouched once the writer restamps the slot checksum. Demonstrated
    // during the 052 freeze: bumping SUPPORTED_FORMAT_VERSION 0 -> 1 changed
    // these bytes and this pin did not move. Assert the header fields
    // directly, on BOTH slots, or the pin silently stops fencing them.
    //
    // These compare against the build's own constant, so they catch a WRITER
    // that stops stamping the header correctly — not a deliberate change to
    // the constant itself. The frozen value is pinned by its own literal
    // fence in `crates/devondb/tests/format_freeze.rs`.
    for slot in 0..2 {
        let base = slot * PAGE_SIZE as usize;
        assert_eq!(
            read_u32_at(&first_bytes, base + 8),
            SUPPORTED_FORMAT_VERSION,
            "slot {slot} format_version"
        );
        assert_eq!(
            read_u32_at(&first_bytes, base + 12),
            SUPPORTED_FORMAT_VERSION,
            "slot {slot} min_reader_version"
        );
        assert_eq!(
            read_u32_at(&first_bytes, base + 24),
            PAGE_SIZE,
            "slot {slot} page_size"
        );
    }
}

fn read_u32_at(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}

#[test]
fn full_stack_reopen_preserves_topology_entry_coverage_and_base_columns() {
    let fixture = write_fixture("reopen");

    // write_fixture has already dropped its writer Pager and every view. This
    // is a fresh file handle, cache, decoded root, directory, and snapshot view.
    let pager = Pager::open(&fixture.path).unwrap();
    let (root, directory) = load_index(&pager, fixture.pages.root).unwrap();
    let budget = MemoryBudget::unlimited();
    let groups = HnswGroupLayout::new(GROUP_ROW_COUNTS.to_vec()).unwrap();
    let view = HnswSnapshotView::new(
        &pager,
        &budget,
        root,
        directory.clone(),
        &[],
        COVERED_ROWS,
        groups,
        VERIFIED_GROUP_CAPACITY,
    )
    .unwrap();

    assert_eq!(root, fixture.root_before_reopen);
    assert_eq!(directory, fixture.directory_before_reopen);
    assert_eq!(view.entry(), Some((3, 1)));
    assert_eq!(
        view.entry(),
        fixture
            .root_before_reopen
            .entry_node
            .map(|node| (node, fixture.root_before_reopen.entry_level))
    );
    assert_eq!(view.covered_rows(), COVERED_ROWS);
    assert_eq!(
        walk_topology(&view).unwrap(),
        fixture.topology_before_reopen
    );
    assert_eq!(
        read_base_groups(&pager, fixture.pages.node_groups).unwrap(),
        fixture.base_groups_before_reopen
    );
}

#[test]
fn root_referencing_an_unwritten_directory_is_corruption() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("unwritten-directory.devondb");
    let pager = Pager::create(&path, PAGE_SIZE, DB_ID).unwrap();
    let root_page = pager.allocate_page().unwrap();
    let absent_directory_page = root_page + 1;
    let empty_cell_payload = 0_u64.to_le_bytes();
    let root = HnswRoot {
        config: fixture_config(),
        covered_rows: 1,
        entry_node: Some(0),
        entry_level: 0,
        layer_count: 1,
        group_count: 1,
        layer_dir_first_page: absent_directory_page,
        layer_dir_byte_len: empty_cell_payload.len() as u32,
        layer_dir_crc32c: crc32c(&empty_cell_payload),
    };
    write_root(&pager, root_page, &root);
    drop(pager);

    let reopened = Pager::open(&path).unwrap();
    assert_corrupt_mentions(load_index(&reopened, root_page), "HNSW layer-directory");
}

#[test]
fn cross_object_corruption_is_named_local_and_base_data_independent() {
    for corruption in Corruption::ALL {
        let fixture = write_fixture(&format!("corrupt-{corruption:?}"));
        apply_corruption(&fixture, corruption);
        let pager = Pager::open(&fixture.path).unwrap();

        // Index pages are optional derived state: corrupting any one of them
        // must not poison the authoritative node-group vector columns.
        assert_eq!(
            read_base_groups(&pager, fixture.pages.node_groups).unwrap(),
            fixture.base_groups_before_reopen,
            "base data changed after {corruption:?} corruption"
        );

        match corruption {
            Corruption::Root | Corruption::LayerDirectory => {
                assert_corrupt_mentions(
                    load_index(&pager, fixture.pages.root),
                    corruption.expected_context(),
                );
                assert_unrelated_csr_readable(&pager, fixture.pages.csr_groups[0][1]);
            }
            Corruption::CsrOffsets | Corruption::CsrNeighbors => {
                assert_corrupt_csr_is_local(&pager, &fixture, corruption);
            }
        }
    }
}

#[test]
fn tight_budget_completes_within_ceiling_or_fails_without_leaking_charge() {
    let fixture = write_fixture("budget");

    let success_budget = Arc::new(MemoryBudget::new(TIGHT_SUCCESS_LIMIT));
    let success_pager = Pager::open(&fixture.path)
        .unwrap()
        .with_budget(Arc::clone(&success_budget));
    let (root, directory) = load_index(&success_pager, fixture.pages.root).unwrap();
    let success_view = HnswSnapshotView::new(
        &success_pager,
        &success_budget,
        root,
        directory,
        &[],
        COVERED_ROWS,
        HnswGroupLayout::new(GROUP_ROW_COUNTS.to_vec()).unwrap(),
        VERIFIED_GROUP_CAPACITY,
    )
    .unwrap();
    assert_eq!(
        walk_topology(&success_view).unwrap(),
        fixture.topology_before_reopen
    );
    assert!(success_budget.charged() <= TIGHT_SUCCESS_LIMIT);
    drop(success_view);
    drop(success_pager);
    assert_eq!(success_budget.charged(), 0);

    // Capacity one allows the first CSR group to be returned, then forces a
    // hard BudgetExceeded at the second group. walk_topology returns no partial
    // topology, and dropping the failed view releases the retained set charge.
    let failure_budget = Arc::new(MemoryBudget::new(TIGHT_SUCCESS_LIMIT));
    let failure_pager = Pager::open(&fixture.path)
        .unwrap()
        .with_budget(Arc::clone(&failure_budget));
    let (root, directory) = load_index(&failure_pager, fixture.pages.root).unwrap();
    let failure_view = HnswSnapshotView::new(
        &failure_pager,
        &failure_budget,
        root,
        directory,
        &[],
        COVERED_ROWS,
        HnswGroupLayout::new(GROUP_ROW_COUNTS.to_vec()).unwrap(),
        1,
    )
    .unwrap();
    assert!(matches!(
        walk_topology(&failure_view),
        Err(DevonError::BudgetExceeded { .. })
    ));
    drop(failure_view);
    drop(failure_pager);
    assert_eq!(failure_budget.charged(), 0);

    let construction_budget = MemoryBudget::new(79);
    let construction_pager = Pager::open(&fixture.path).unwrap();
    assert!(matches!(
        HnswSnapshotView::new(
            &construction_pager,
            &construction_budget,
            fixture.root_before_reopen,
            fixture.directory_before_reopen.clone(),
            &[],
            COVERED_ROWS,
            HnswGroupLayout::new(GROUP_ROW_COUNTS.to_vec()).unwrap(),
            1,
        ),
        Err(DevonError::BudgetExceeded { .. })
    ));
    drop(construction_pager);
    assert_eq!(construction_budget.charged(), 0);
}

fn write_fixture(name: &str) -> PersistedFixture {
    let directory = tempdir().unwrap();
    let path = directory.path().join(format!("{name}.devondb"));
    let pager = Pager::create(&path, PAGE_SIZE, DB_ID).unwrap();
    let vectors = seeded_vectors();
    let node_groups = write_node_groups(&pager, &vectors);

    let layer_zero_group_zero = write_csr(&pager, &[&[1, 2], &[0, 3], &[0, 4]]);
    let layer_zero_group_one = write_csr(&pager, &[&[1, 4], &[2, 3]]);
    let layer_one_group_one = write_csr(&pager, &[&[4], &[3]]);
    let csr_groups = [
        [layer_zero_group_zero, layer_zero_group_one],
        [0, layer_one_group_one],
    ];

    let directory_codec = LayerDirectory::new(
        LAYER_COUNT,
        GROUP_ROW_COUNTS.len() as u32,
        csr_groups.into_iter().flatten().collect(),
    )
    .unwrap();
    let layer_directory_page = persist_payload(&pager, &directory_codec.encode());
    let root = HnswRoot {
        config: fixture_config(),
        covered_rows: COVERED_ROWS,
        entry_node: Some(3),
        entry_level: 1,
        layer_count: LAYER_COUNT,
        group_count: GROUP_ROW_COUNTS.len() as u32,
        layer_dir_first_page: layer_directory_page,
        layer_dir_byte_len: directory_codec.byte_len(),
        layer_dir_crc32c: directory_codec.crc32c(),
    };
    let root_page = pager.allocate_page().unwrap();
    write_root(&pager, root_page, &root);
    pager.sync().unwrap();

    // This fixture writes node groups directly and never publishes a catalog,
    // so `Catalog::save` — the only code that claims `ZONE_MAPS_FLAG` — never
    // runs here. Since SCALE S-1 the groups written above carry a
    // `ZONE_MAP_STATS` directory section, and a set section bit whose
    // governing feature bit is clear is corruption by format law
    // (`docs/FORMAT.md` § node groups). Publish the bit the way a real
    // checkpoint would, so the fixture is self-consistent on reopen.
    // The pager enforces checkpoint-LSN monotonicity on every superblock
    // commit, so advance it exactly as a real publication does.
    let mut superblock = pager.superblock();
    superblock.feature_flags |= ZONE_MAPS_FLAG;
    superblock.checkpoint_lsn += 1;
    pager.commit_superblock(superblock).unwrap();

    let levels = (0..COVERED_ROWS)
        .map(|node| level_for_node(&root.config, node).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(levels, [0, 0, 0, 1, 1]);

    let (decoded_root, decoded_directory) = load_index(&pager, root_page).unwrap();
    let budget = MemoryBudget::unlimited();
    let view = HnswSnapshotView::new(
        &pager,
        &budget,
        decoded_root,
        decoded_directory.clone(),
        &[],
        COVERED_ROWS,
        HnswGroupLayout::new(GROUP_ROW_COUNTS.to_vec()).unwrap(),
        VERIFIED_GROUP_CAPACITY,
    )
    .unwrap();
    assert_eq!(view.entry(), Some((3, 1)));
    let topology = walk_topology(&view).unwrap();
    assert_expected_topology(&topology);
    let base_groups = read_base_groups(&pager, node_groups).unwrap();
    drop(view);
    drop(pager);

    PersistedFixture {
        _directory: directory,
        path,
        pages: FixturePages {
            root: root_page,
            layer_directory: layer_directory_page,
            node_groups,
            csr_groups,
        },
        root_before_reopen: decoded_root,
        directory_before_reopen: decoded_directory,
        topology_before_reopen: topology,
        base_groups_before_reopen: base_groups,
    }
}

fn fixture_config() -> HnswConfig {
    HnswConfig {
        m: 4,
        m0: 8,
        ef_construction: 16,
        level_seed: LEVEL_SEED,
        metric: HnswMetric::L2,
        navigation: NavigationEncoding::F32,
    }
}

fn seeded_vectors() -> Vec<Vec<f32>> {
    let mut state = CORPUS_SEED;
    (0..COVERED_ROWS)
        .map(|_| {
            (0..VECTOR_DIM)
                .map(|_| {
                    state = state
                        .wrapping_mul(LCG_MULTIPLIER)
                        .wrapping_add(LCG_INCREMENT);
                    let mantissa = ((state >> 41) as u32) & 0x007f_ffff;
                    f32::from_bits(0x3f00_0000 | mantissa)
                })
                .collect()
        })
        .collect()
}

fn write_node_groups(pager: &Pager, vectors: &[Vec<f32>]) -> [u64; 2] {
    let mut pages = [0_u64; 2];
    let mut row_start = 0_usize;
    for (group_index, row_count) in GROUP_ROW_COUNTS.into_iter().enumerate() {
        let mut group = NodeGroup::new(vec![LogicalType::Vector { dim: VECTOR_DIM }]).unwrap();
        let row_end = row_start + row_count as usize;
        for vector in &vectors[row_start..row_end] {
            group.push_row(vec![Value::Vector(vector.clone())]).unwrap();
        }
        pages[group_index] = group.write(pager).unwrap();
        row_start = row_end;
    }
    pages
}

fn write_csr(pager: &Pager, adjacency: &[&[u64]]) -> u64 {
    let mut group = CsrGroup::new(adjacency.len(), Vec::new()).unwrap();
    for (slot, neighbors) in adjacency.iter().enumerate() {
        for neighbor in *neighbors {
            group.push_edge(slot, *neighbor, Vec::new()).unwrap();
        }
    }
    group.write(pager).unwrap()
}

fn persist_payload(pager: &Pager, payload: &[u8]) -> u64 {
    let page_size = PAGE_SIZE as usize;
    let page_count = payload.len().div_ceil(page_size);
    let mut first_page = None;
    for (page_index, chunk) in payload.chunks(page_size).enumerate() {
        let page_id = pager.allocate_page().unwrap();
        let expected = *first_page.get_or_insert(page_id) + page_index as u64;
        assert_eq!(page_id, expected);
        let mut page = vec![0_u8; page_size];
        page[..chunk.len()].copy_from_slice(chunk);
        pager.write_page(page_id, &page).unwrap();
    }
    assert_eq!(page_count, 1);
    first_page.unwrap()
}

fn write_root(pager: &Pager, page_id: u64, root: &HnswRoot) {
    let mut page = vec![0_u8; PAGE_SIZE as usize];
    root.encode(&mut page).unwrap();
    pager.write_page(page_id, &page).unwrap();
    pager.sync().unwrap();
}

fn load_index(pager: &Pager, root_page: u64) -> DevonResult<(HnswRoot, LayerDirectory)> {
    let root_bytes = read_index_page(pager, root_page, "HNSW root")?;
    let root = HnswRoot::decode(&root_bytes)?;
    let payload = read_layer_directory_payload(pager, &root)?;
    let directory = LayerDirectory::decode(&payload, &root)?;
    Ok((root, directory))
}

fn read_layer_directory_payload(pager: &Pager, root: &HnswRoot) -> DevonResult<Vec<u8>> {
    let byte_len = root.layer_dir_byte_len as usize;
    if byte_len == 0 {
        return Ok(Vec::new());
    }
    let page_size = PAGE_SIZE as usize;
    let mut payload = Vec::with_capacity(byte_len);
    for page_offset in 0..byte_len.div_ceil(page_size) {
        let page_id = root
            .layer_dir_first_page
            .checked_add(page_offset as u64)
            .ok_or_else(|| corrupt("HNSW layer-directory page run overflows u64"))?;
        let page = read_index_page(pager, page_id, "HNSW layer-directory")?;
        let take = (byte_len - payload.len()).min(page_size);
        payload.extend_from_slice(&page[..take]);
        if take < page_size && page[take..].iter().any(|byte| *byte != 0) {
            return Err(corrupt(
                "HNSW layer-directory final-page padding is not zero",
            ));
        }
    }
    Ok(payload)
}

fn read_index_page(pager: &Pager, page_id: u64, object: &str) -> DevonResult<Vec<u8>> {
    match pager.read_page(page_id) {
        Ok(page) => Ok(page),
        Err(DevonError::InvalidArgument { context }) => Err(corrupt(format!(
            "{object} references invalid page {page_id}: {context}"
        ))),
        Err(error) => Err(error),
    }
}

fn walk_topology(view: &HnswSnapshotView<'_>) -> DevonResult<Topology> {
    let mut topology = Vec::with_capacity(view.layer_count() as usize);
    let mut scratch = Vec::new();
    for layer in 0..view.layer_count() {
        let mut nodes = Vec::with_capacity(view.covered_rows() as usize);
        for node in 0..view.covered_rows() {
            view.neighbors(layer, node, &mut scratch)?;
            nodes.push(scratch.clone());
        }
        topology.push(nodes);
    }
    Ok(topology)
}

fn assert_expected_topology(topology: &Topology) {
    assert_eq!(topology.len(), LAYER_COUNT as usize);
    for (layer, nodes) in topology.iter().enumerate() {
        assert_eq!(nodes.len(), COVERED_ROWS as usize);
        for (node, actual) in nodes.iter().enumerate() {
            assert_eq!(actual, expected_neighbors(layer as u8, node as u64));
        }
    }
}

fn expected_neighbors(layer: u8, node: u64) -> &'static [u64] {
    match (layer, node) {
        (0, 0) => &[1, 2],
        (0, 1) => &[0, 3],
        (0, 2) => &[0, 4],
        (0, 3) => &[1, 4],
        (0, 4) => &[2, 3],
        (1, 3) => &[4],
        (1, 4) => &[3],
        (1, 0..=2) => &[],
        _ => panic!("unexpected topology coordinate ({layer}, {node})"),
    }
}

fn read_base_groups(pager: &Pager, pages: [u64; 2]) -> DevonResult<Vec<NodeGroup>> {
    pages
        .into_iter()
        .map(|page| NodeGroup::read(pager, page, &[LogicalType::Vector { dim: VECTOR_DIM }]))
        .collect()
}

fn apply_corruption(fixture: &PersistedFixture, corruption: Corruption) {
    match corruption {
        Corruption::Root => flip_file_byte(&fixture.path, fixture.pages.root, 0, 0xff),
        Corruption::LayerDirectory => {
            flip_file_byte(&fixture.path, fixture.pages.layer_directory, 0, 0x01);
        }
        Corruption::CsrOffsets => {
            let directory = fixture.pages.csr_groups[0][0];
            let payload = csr_payload_first_page(&fixture.path, directory, 0);
            flip_file_byte(&fixture.path, payload, 4, 0x01);
        }
        Corruption::CsrNeighbors => {
            let directory = fixture.pages.csr_groups[0][0];
            let payload = csr_payload_first_page(&fixture.path, directory, 1);
            flip_file_byte(&fixture.path, payload, 0, 0x02);
        }
    }
}

fn assert_corrupt_csr_is_local(pager: &Pager, fixture: &PersistedFixture, corruption: Corruption) {
    let (root, directory) = load_index(pager, fixture.pages.root).unwrap();
    let budget = MemoryBudget::unlimited();
    let view = HnswSnapshotView::new(
        pager,
        &budget,
        root,
        directory,
        &[],
        COVERED_ROWS,
        HnswGroupLayout::new(GROUP_ROW_COUNTS.to_vec()).unwrap(),
        VERIFIED_GROUP_CAPACITY,
    )
    .unwrap();
    let mut scratch = Vec::new();

    view.neighbors(0, 3, &mut scratch).unwrap();
    assert_eq!(scratch, expected_neighbors(0, 3));
    assert_corrupt_mentions(
        view.neighbors(0, 0, &mut scratch),
        corruption.expected_context(),
    );
    view.neighbors(0, 3, &mut scratch).unwrap();
    assert_eq!(scratch, expected_neighbors(0, 3));
}

fn assert_unrelated_csr_readable(pager: &Pager, directory_page: u64) {
    let reader = CsrSlotReader::open(pager, directory_page).unwrap();
    let budget = MemoryBudget::unlimited();
    let mut verified = VerifiedCsrGroups::new(&budget, 1).unwrap();
    let adjacency = reader
        .read_slot(
            0,
            CsrSlotReadOptions {
                group_start: 3,
                covered_rows: COVERED_ROWS,
                degree_cap: fixture_config().m0 as usize,
            },
            &mut verified,
        )
        .unwrap();
    assert_eq!(adjacency.neighbors(), expected_neighbors(0, 3));
}

fn csr_payload_first_page(path: &Path, directory_page: u64, entry_index: usize) -> u64 {
    let byte_offset = directory_page * u64::from(PAGE_SIZE) + 16 + entry_index as u64 * 16;
    let mut file = File::open(path).unwrap();
    file.seek(SeekFrom::Start(byte_offset)).unwrap();
    let mut bytes = [0_u8; 8];
    file.read_exact(&mut bytes).unwrap();
    u64::from_le_bytes(bytes)
}

fn flip_file_byte(path: &Path, page_id: u64, in_page_offset: u64, mask: u8) {
    let offset = page_id * u64::from(PAGE_SIZE) + in_page_offset;
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .unwrap();
    file.seek(SeekFrom::Start(offset)).unwrap();
    let mut byte = [0_u8; 1];
    file.read_exact(&mut byte).unwrap();
    byte[0] ^= mask;
    file.seek(SeekFrom::Start(offset)).unwrap();
    file.write_all(&byte).unwrap();
    file.sync_all().unwrap();
}

fn assert_corrupt_mentions<T>(result: DevonResult<T>, expected: &str) {
    let error = match result {
        Ok(_) => panic!("expected Corrupt naming {expected}"),
        Err(error) => error,
    };
    let DevonError::Corrupt { context } = error else {
        panic!("expected Corrupt naming {expected}, got {error}");
    };
    assert!(
        context.contains(expected),
        "corruption context {context:?} did not name {expected:?}"
    );
}

fn corrupt(context: impl Into<String>) -> DevonError {
    DevonError::Corrupt {
        context: context.into(),
    }
}
