use std::ops::Range;
use std::sync::Arc;

use devondb_storage::budget::MemoryBudget;
use devondb_storage::csr_group::CsrGroup;
use devondb_storage::hnsw::format::{HnswRoot, LayerDirectory};
use devondb_storage::hnsw::scoring::level_for_node;
use devondb_storage::hnsw::types::{
    GraphAccess, HnswConfig, HnswDelta, HnswMetric, NavigationEncoding,
};
use devondb_storage::hnsw::view::{HnswGroupLayout, HnswSnapshotView};
use devondb_storage::pager::Pager;
use devondb_types::{DevonError, DevonResult};
use tempfile::{TempDir, tempdir};

const PAGE_SIZE: u32 = 4096;
const DB_ID: [u8; 16] = *b"hnsw-view-test!!";

struct PersistedFixture {
    _directory: TempDir,
    pager: Pager,
    budget: MemoryBudget,
    root: HnswRoot,
    layer_directory: LayerDirectory,
    groups: HnswGroupLayout,
}

impl PersistedFixture {
    fn populated(first_neighbors: &[u64]) -> Self {
        let directory = tempdir().unwrap();
        let path = directory.path().join("view.devondb");
        let pager = Pager::create(path, PAGE_SIZE, DB_ID).unwrap();

        let mut first_group = CsrGroup::new(2, Vec::new()).unwrap();
        for neighbor in first_neighbors {
            first_group.push_edge(0, *neighbor, Vec::new()).unwrap();
        }
        first_group.push_edge(1, 0, Vec::new()).unwrap();
        let first_page = first_group.write(&pager).unwrap();

        let mut second_group = CsrGroup::new(2, Vec::new()).unwrap();
        second_group.push_edge(0, 3, Vec::new()).unwrap();
        second_group.push_edge(1, 2, Vec::new()).unwrap();
        let second_page = second_group.write(&pager).unwrap();

        let layer_directory = LayerDirectory::new(1, 2, vec![first_page, second_page]).unwrap();
        let layer_page = persist_payload(&pager, &layer_directory.encode());
        let root = HnswRoot {
            config: HnswConfig::with_defaults(
                0x48_4e_53_57,
                HnswMetric::L2,
                NavigationEncoding::F32,
            ),
            covered_rows: 4,
            entry_node: Some(0),
            entry_level: 0,
            layer_count: 1,
            group_count: 2,
            layer_dir_first_page: layer_page,
            layer_dir_byte_len: layer_directory.byte_len(),
            layer_dir_crc32c: layer_directory.crc32c(),
        };
        let (root, layer_directory) = persist_and_decode_root(&pager, root);
        Self {
            _directory: directory,
            pager,
            budget: MemoryBudget::unlimited(),
            root,
            layer_directory,
            groups: HnswGroupLayout::new(vec![2, 2]).unwrap(),
        }
    }

    /// Root layout claims the first group covers 2 rows, but the persisted
    /// CSR group only has `row_count` 1 — the geometry contradiction the
    /// view must report as corruption instead of silent emptiness.
    fn short_first_group() -> Self {
        let directory = tempdir().unwrap();
        let path = directory.path().join("short-view.devondb");
        let pager = Pager::create(path, PAGE_SIZE, DB_ID).unwrap();

        let mut first_group = CsrGroup::new(1, Vec::new()).unwrap();
        first_group.push_edge(0, 1, Vec::new()).unwrap();
        let first_page = first_group.write(&pager).unwrap();

        let mut second_group = CsrGroup::new(2, Vec::new()).unwrap();
        second_group.push_edge(0, 3, Vec::new()).unwrap();
        second_group.push_edge(1, 2, Vec::new()).unwrap();
        let second_page = second_group.write(&pager).unwrap();

        let layer_directory = LayerDirectory::new(1, 2, vec![first_page, second_page]).unwrap();
        let layer_page = persist_payload(&pager, &layer_directory.encode());
        let root = HnswRoot {
            config: HnswConfig::with_defaults(
                0x48_4e_53_57,
                HnswMetric::L2,
                NavigationEncoding::F32,
            ),
            covered_rows: 4,
            entry_node: Some(0),
            entry_level: 0,
            layer_count: 1,
            group_count: 2,
            layer_dir_first_page: layer_page,
            layer_dir_byte_len: layer_directory.byte_len(),
            layer_dir_crc32c: layer_directory.crc32c(),
        };
        let (root, layer_directory) = persist_and_decode_root(&pager, root);
        Self {
            _directory: directory,
            pager,
            budget: MemoryBudget::unlimited(),
            root,
            layer_directory,
            groups: HnswGroupLayout::new(vec![2, 2]).unwrap(),
        }
    }

    fn ineligible_upper_layer_neighbor() -> Self {
        let directory = tempdir().unwrap();
        let path = directory.path().join("ineligible-upper-layer.devondb");
        let pager = Pager::create(path, PAGE_SIZE, DB_ID).unwrap();
        let config = HnswConfig {
            m: 4,
            m0: 8,
            ef_construction: 16,
            level_seed: 0x10,
            metric: HnswMetric::L2,
            navigation: NavigationEncoding::F32,
        };
        assert_eq!(level_for_node(&config, 0).unwrap(), 0);
        assert_eq!(level_for_node(&config, 3).unwrap(), 1);

        let mut upper_group = CsrGroup::new(2, Vec::new()).unwrap();
        upper_group.push_edge(0, 0, Vec::new()).unwrap();
        let upper_page = upper_group.write(&pager).unwrap();

        let layer_directory = LayerDirectory::new(2, 2, vec![0, 0, 0, upper_page]).unwrap();
        let layer_page = persist_payload(&pager, &layer_directory.encode());
        let root = HnswRoot {
            config,
            covered_rows: 5,
            entry_node: Some(3),
            entry_level: 1,
            layer_count: 2,
            group_count: 2,
            layer_dir_first_page: layer_page,
            layer_dir_byte_len: layer_directory.byte_len(),
            layer_dir_crc32c: layer_directory.crc32c(),
        };
        let (root, layer_directory) = persist_and_decode_root(&pager, root);
        Self {
            _directory: directory,
            pager,
            budget: MemoryBudget::unlimited(),
            root,
            layer_directory,
            groups: HnswGroupLayout::new(vec![3, 2]).unwrap(),
        }
    }

    fn empty() -> Self {
        let directory = tempdir().unwrap();
        let path = directory.path().join("empty-view.devondb");
        let pager = Pager::create(path, PAGE_SIZE, DB_ID).unwrap();
        let root = HnswRoot {
            config: HnswConfig::with_defaults(
                0x48_4e_53_57,
                HnswMetric::L2,
                NavigationEncoding::F32,
            ),
            covered_rows: 0,
            entry_node: None,
            entry_level: 0,
            layer_count: 0,
            group_count: 0,
            layer_dir_first_page: 0,
            layer_dir_byte_len: 0,
            layer_dir_crc32c: 0,
        };
        let (root, layer_directory) = persist_and_decode_root(&pager, root);
        Self {
            _directory: directory,
            pager,
            budget: MemoryBudget::unlimited(),
            root,
            layer_directory,
            groups: HnswGroupLayout::new(Vec::new()).unwrap(),
        }
    }

    fn view<'fixture>(
        &'fixture self,
        deltas: &'fixture [HnswDelta],
        row_count: u64,
    ) -> DevonResult<HnswSnapshotView<'fixture>> {
        HnswSnapshotView::new(
            &self.pager,
            &self.budget,
            self.root,
            self.layer_directory.clone(),
            deltas,
            row_count,
            self.groups.clone(),
            2,
        )
    }
}

#[test]
fn root_only_and_empty_views_implement_graph_access_and_tail_boundary() {
    let fixture = PersistedFixture::populated(&[1]);
    let view = fixture.view(&[], 6).unwrap();
    assert_eq!(view.entry(), Some((0, 0)));
    assert_eq!(view.layer_count(), 1);
    assert_eq!(view.covered_rows(), 4);
    assert_eq!(view.row_count(), 6);
    assert_eq!(view.exact_tail(), Range { start: 4, end: 6 });

    let mut scratch = vec![99];
    view.neighbors(0, 0, &mut scratch).unwrap();
    assert_eq!(scratch, [1]);

    let empty = PersistedFixture::empty();
    let empty_view = empty.view(&[], 3).unwrap();
    assert_eq!(empty_view.entry(), None);
    assert_eq!(empty_view.layer_count(), 0);
    assert_eq!(empty_view.covered_rows(), 0);
    assert_eq!(empty_view.exact_tail(), 0..3);
}

#[test]
fn newest_reachable_replacement_wins() {
    let fixture = PersistedFixture::populated(&[1]);
    let deltas = [
        replacement_delta(4, 5, 0, &[2]),
        replacement_delta(5, 6, 0, &[3]),
    ];
    let view = fixture.view(&deltas, 6).unwrap();
    let mut scratch = Vec::new();

    view.neighbors(0, 0, &mut scratch).unwrap();

    assert_eq!(scratch, [3]);
    assert_eq!(view.covered_rows(), 6);
    assert_eq!(view.exact_tail(), 6..6);
}

#[test]
fn snapshot_sees_only_deltas_in_its_reachable_slice() {
    let fixture = PersistedFixture::populated(&[1]);
    let deltas = [
        replacement_delta(4, 5, 0, &[2]),
        replacement_delta(5, 6, 0, &[3]),
    ];
    let old_snapshot = fixture.view(&deltas[..1], 5).unwrap();
    let new_snapshot = fixture.view(&deltas, 6).unwrap();
    let mut old_neighbors = Vec::new();
    let mut new_neighbors = Vec::new();

    old_snapshot.neighbors(0, 0, &mut old_neighbors).unwrap();
    new_snapshot.neighbors(0, 0, &mut new_neighbors).unwrap();

    assert_eq!(old_neighbors, [2]);
    assert_eq!(new_neighbors, [3]);
    assert_eq!(old_snapshot.covered_rows(), 5);
    assert_eq!(new_snapshot.covered_rows(), 6);
}

#[test]
fn coverage_gap_is_corruption() {
    let fixture = PersistedFixture::populated(&[1]);
    let deltas = [replacement_delta(5, 6, 0, &[2])];

    assert_corrupt(fixture.view(&deltas, 6));
}

#[test]
fn base_neighbor_must_stay_below_root_coverage() {
    let fixture = PersistedFixture::populated(&[5]);
    let deltas = [coverage_delta(4, 6)];
    let view = fixture.view(&deltas, 6).unwrap();
    let mut scratch = Vec::new();

    assert_corrupt(view.neighbors(0, 0, &mut scratch));
}

#[test]
fn upper_layer_base_neighbor_must_reach_the_derived_layer() {
    let fixture = PersistedFixture::ineligible_upper_layer_neighbor();
    let view = fixture.view(&[], 5).unwrap();
    let mut scratch = Vec::new();

    let error = view.neighbors(1, 3, &mut scratch).unwrap_err();

    let DevonError::Corrupt { context } = error else {
        panic!("expected corrupt upper-layer neighbor, got {error:?}");
    };
    assert!(context.contains("neighbor 0"), "{context}");
    assert!(context.contains("derived level 0"), "{context}");
    assert!(context.contains("layer 1"), "{context}");
    assert!(scratch.is_empty());
}

#[test]
fn short_csr_group_is_corruption_not_emptiness() {
    let fixture = PersistedFixture::short_first_group();
    let view = fixture.view(&[], 4).unwrap();
    let mut scratch = Vec::new();

    assert_corrupt(view.neighbors(0, 1, &mut scratch));
    assert!(scratch.is_empty());
}

#[test]
fn entry_replacement_advances_effective_layers_atomically() {
    let fixture = PersistedFixture::populated(&[1]);
    let mut delta = coverage_delta(4, 5);
    delta.entry = Some((4, 1));
    let deltas = [delta];
    let view = fixture.view(&deltas, 6).unwrap();

    assert_eq!(view.entry(), Some((4, 1)));
    assert_eq!(view.layer_count(), 2);
    assert_eq!(view.covered_rows(), 5);
    assert_eq!(view.exact_tail(), 5..6);
}

#[test]
fn delta_lists_are_checked_against_degree_and_effective_coverage() {
    let fixture = PersistedFixture::populated(&[1]);
    let mut out_of_bounds = coverage_delta(4, 5);
    out_of_bounds
        .replacements
        .insert((0, 0), Arc::from([5_u64]));
    assert_corrupt(fixture.view(&[out_of_bounds], 5));

    let mut too_many = coverage_delta(4, 35);
    too_many.replacements.insert(
        (0, 34),
        Arc::from((0_u64..33).collect::<Vec<_>>().into_boxed_slice()),
    );
    assert_corrupt(fixture.view(&[too_many], 35));
}

fn replacement_delta(
    old_covered_rows: u64,
    new_covered_rows: u64,
    node: u64,
    neighbors: &[u64],
) -> HnswDelta {
    let mut delta = coverage_delta(old_covered_rows, new_covered_rows);
    delta.replacements.insert((0, node), Arc::from(neighbors));
    delta
}

fn coverage_delta(old_covered_rows: u64, new_covered_rows: u64) -> HnswDelta {
    HnswDelta {
        old_covered_rows,
        new_covered_rows,
        ..HnswDelta::default()
    }
}

fn persist_payload(pager: &Pager, payload: &[u8]) -> u64 {
    assert!(payload.len() <= PAGE_SIZE as usize);
    let page_id = pager.allocate_page().unwrap();
    let mut page = vec![0_u8; PAGE_SIZE as usize];
    page[..payload.len()].copy_from_slice(payload);
    pager.write_page(page_id, &page).unwrap();
    page_id
}

fn persist_and_decode_root(pager: &Pager, root: HnswRoot) -> (HnswRoot, LayerDirectory) {
    let root_page = pager.allocate_page().unwrap();
    let mut encoded = vec![0_u8; PAGE_SIZE as usize];
    root.encode(&mut encoded).unwrap();
    pager.write_page(root_page, &encoded).unwrap();
    pager.sync().unwrap();

    let decoded_root = HnswRoot::decode(&pager.read_page(root_page).unwrap()).unwrap();
    let payload = if decoded_root.layer_dir_byte_len == 0 {
        Vec::new()
    } else {
        let page = pager.read_page(decoded_root.layer_dir_first_page).unwrap();
        page[..decoded_root.layer_dir_byte_len as usize].to_vec()
    };
    let decoded_directory = LayerDirectory::decode(&payload, &decoded_root).unwrap();
    (decoded_root, decoded_directory)
}

fn assert_corrupt<T>(result: DevonResult<T>) {
    assert!(matches!(result, Err(DevonError::Corrupt { .. })));
}
