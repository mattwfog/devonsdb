use std::collections::BTreeMap;
use std::fs;
use std::mem::size_of;
use std::sync::Arc;

use devondb_storage::budget::MemoryBudget;
use devondb_storage::hnsw::index::{
    CatchUpStatus, ConstructionVector, ConstructionVectorAccess, HnswNodeGroups,
    InsertProposalOutcome, PersistedHnswIndex, build_initial_index, load_persisted_index,
    propose_insert, publish_checkpoint, publish_checkpoint_with_catch_up,
};
use devondb_storage::hnsw::scoring::level_for_node;
use devondb_storage::hnsw::types::{
    GraphAccess, HnswConfig, HnswDelta, HnswMetric, NavigationEncoding,
};
use devondb_storage::hnsw::view::HnswSnapshotView;
use devondb_storage::pager::Pager;
use devondb_types::{DevonError, DevonResult, logical_type::LogicalType};
use tempfile::{TempDir, tempdir};

const PAGE_SIZE: u32 = 4096;
const DB_ID: [u8; 16] = *b"hnsw-life-test!!";
const DIMENSION: usize = 8;

struct VectorRows {
    column_type: LogicalType,
    decoded: Vec<Option<Vec<f32>>>,
    slots: Vec<Option<Vec<u8>>>,
}

impl VectorRows {
    fn generated(row_count: usize) -> Self {
        let mut state = 0xd3_70_db_20_00_u64;
        let decoded = (0..row_count)
            .map(|_| {
                Some(
                    (0..DIMENSION)
                        .map(|_| {
                            state = state
                                .wrapping_mul(6_364_136_223_846_793_005)
                                .wrapping_add(1_442_695_040_888_963_407);
                            ((state >> 40) as i32 - (1 << 23)) as f32 / (1 << 23) as f32
                        })
                        .collect::<Vec<_>>(),
                )
            })
            .collect::<Vec<_>>();
        let slots = decoded
            .iter()
            .map(|vector| {
                vector.as_ref().map(|vector| {
                    vector
                        .iter()
                        .flat_map(|value| value.to_le_bytes())
                        .collect()
                })
            })
            .collect();
        Self {
            column_type: LogicalType::Vector {
                dim: DIMENSION as u32,
            },
            decoded,
            slots,
        }
    }
}

impl ConstructionVectorAccess for VectorRows {
    fn column_type(&self) -> &LogicalType {
        &self.column_type
    }

    fn with_vector<R, F>(&self, node: u64, read: F) -> DevonResult<R>
    where
        F: FnOnce(Option<ConstructionVector<'_>>) -> DevonResult<R>,
    {
        let index = usize::try_from(node).map_err(|_| DevonError::InvalidArgument {
            context: "test vector node exceeds usize".to_owned(),
        })?;
        let decoded = self.decoded.get(index).and_then(Option::as_deref);
        let slot = self.slots.get(index).and_then(Option::as_deref);
        let vector = decoded
            .zip(slot)
            .map(|(decoded, navigation_slot)| ConstructionVector {
                decoded,
                navigation_slot,
            });
        read(vector)
    }
}

struct EmptyGraph;

impl GraphAccess for EmptyGraph {
    fn entry(&self) -> Option<(u64, u8)> {
        None
    }

    fn layer_count(&self) -> u8 {
        0
    }

    fn covered_rows(&self) -> u64 {
        0
    }

    fn neighbors(&self, _: u8, _: u64, scratch: &mut Vec<u64>) -> DevonResult<()> {
        scratch.clear();
        Ok(())
    }
}

struct Fixture {
    _directory: TempDir,
    path: std::path::PathBuf,
    pager: Pager,
}

impl Fixture {
    fn new(name: &str) -> Self {
        let directory = tempdir().unwrap();
        let path = directory.path().join(name);
        let pager = Pager::create(&path, PAGE_SIZE, DB_ID).unwrap();
        Self {
            _directory: directory,
            path,
            pager,
        }
    }
}

fn config() -> HnswConfig {
    HnswConfig {
        m: 4,
        m0: 8,
        ef_construction: 16,
        level_seed: 0x48_4e_53_57_20_26_08_03,
        metric: HnswMetric::L2,
        navigation: NavigationEncoding::F32,
    }
}

#[test]
fn proposal_tail_budget_caps_reciprocity_and_repruning() {
    let vectors = VectorRows::generated(32);
    let config = config();
    let unlimited = MemoryBudget::unlimited();

    assert!(matches!(
        propose_insert(&EmptyGraph, &config, 2..4, &vectors, &unlimited).unwrap(),
        InsertProposalOutcome::TailExists
    ));

    let tiny = MemoryBudget::new(1);
    assert!(matches!(
        propose_insert(&EmptyGraph, &config, 0..1, &vectors, &tiny).unwrap(),
        InsertProposalOutcome::BudgetExceeded
    ));
    assert_eq!(tiny.charged(), 0);

    let proposal = match propose_insert(&EmptyGraph, &config, 0..32, &vectors, &unlimited).unwrap()
    {
        InsertProposalOutcome::Proposed(proposal) => proposal,
        other => panic!("unexpected proposal outcome: {other:?}"),
    };
    assert_eq!(proposal.delta().old_covered_rows, 0);
    assert_eq!(proposal.delta().new_covered_rows, 32);
    assert!(proposal.charged_bytes() > 0);
    assert_eq!(unlimited.charged(), proposal.charged_bytes());

    let replacements = &proposal.delta().replacements;
    assert_degree_caps_and_reciprocity(replacements, &config);
    assert!(replacements.iter().any(|(&(layer, node), neighbors)| {
        layer == 0 && node < 31 && neighbors.iter().any(|neighbor| *neighbor > node)
    }));
    assert!(replacements.values().any(|neighbors| neighbors.len() >= 4));
    drop(proposal);
    assert_eq!(unlimited.charged(), 0);
}

#[test]
fn proposal_charge_is_bounded_by_final_live_replacements() {
    let vectors = VectorRows::generated(64);
    let budget = MemoryBudget::unlimited();
    let proposal = match propose_insert(&EmptyGraph, &config(), 0..64, &vectors, &budget).unwrap() {
        InsertProposalOutcome::Proposed(proposal) => proposal,
        other => panic!("unexpected proposal outcome: {other:?}"),
    };

    let live_map_upper_bound = 64
        + proposal
            .delta()
            .replacements
            .values()
            .map(|neighbors| 64 + neighbors.len() * size_of::<u64>())
            .sum::<usize>();
    assert!(
        proposal.charged_bytes() <= live_map_upper_bound,
        "proposal charge {} exceeds final live-map bound {live_map_upper_bound}",
        proposal.charged_bytes()
    );
}

fn assert_degree_caps_and_reciprocity(
    replacements: &BTreeMap<(u8, u64), Arc<[u64]>>,
    config: &HnswConfig,
) {
    for (&(layer, node), neighbors) in replacements {
        let cap = usize::from(if layer == 0 { config.m0 } else { config.m });
        assert!(neighbors.len() <= cap);
        for neighbor in neighbors.iter().copied() {
            let reverse = replacements.get(&(layer, neighbor)).unwrap();
            assert!(
                reverse.contains(&node),
                "missing reciprocal {neighbor}->{node}"
            );
        }
    }
}

#[test]
fn initial_build_is_byte_deterministic() {
    let vectors = VectorRows::generated(24);
    let groups = HnswNodeGroups::new(vec![8, 8, 8]).unwrap();
    let first = Fixture::new("first.devondb");
    let second = Fixture::new("second.devondb");

    let first_index = build_initial_index(
        &first.pager,
        &MemoryBudget::unlimited(),
        &config(),
        &groups,
        24,
        6,
        &vectors,
    )
    .unwrap();
    let second_index = build_initial_index(
        &second.pager,
        &MemoryBudget::unlimited(),
        &config(),
        &groups,
        24,
        6,
        &vectors,
    )
    .unwrap();

    assert_eq!(first_index, second_index);
    assert_eq!(
        first.pager.read_page(first_index.root_page_id).unwrap(),
        second.pager.read_page(second_index.root_page_id).unwrap()
    );
    assert_eq!(
        fs::read(&first.path).unwrap(),
        fs::read(&second.path).unwrap()
    );
}

#[test]
fn checkpoint_matches_effective_view_and_preserves_unchanged_pages() {
    let vectors = VectorRows::generated(16);
    let groups = HnswNodeGroups::new(vec![8, 8]).unwrap();
    let fixture = Fixture::new("checkpoint.devondb");
    let budget = MemoryBudget::unlimited();
    let base =
        build_initial_index(&fixture.pager, &budget, &config(), &groups, 16, 4, &vectors).unwrap();
    let delta = replacement_delta(base.root.covered_rows, 0, &[]);

    let publication = publish_checkpoint(
        &fixture.pager,
        &budget,
        &config(),
        Some(base.root_page_id),
        std::slice::from_ref(&delta),
        &groups,
    )
    .unwrap();
    let merged = publication.index;

    assert_ne!(
        base.directory.page_id(0, 0).unwrap(),
        merged.directory.page_id(0, 0).unwrap()
    );
    assert_eq!(
        base.directory.page_id(0, 1).unwrap(),
        merged.directory.page_id(0, 1).unwrap()
    );
    assert_topology_equal(
        &fixture.pager,
        &budget,
        &groups,
        &base,
        std::slice::from_ref(&delta),
        &merged,
    );
}

fn replacement_delta(covered_rows: u64, node: u64, neighbors: &[u64]) -> HnswDelta {
    let mut delta = HnswDelta {
        old_covered_rows: covered_rows,
        new_covered_rows: covered_rows,
        ..HnswDelta::default()
    };
    delta.replacements.insert((0, node), Arc::from(neighbors));
    delta
}

fn assert_topology_equal(
    pager: &Pager,
    budget: &MemoryBudget,
    groups: &HnswNodeGroups,
    base: &PersistedHnswIndex,
    deltas: &[HnswDelta],
    merged: &PersistedHnswIndex,
) {
    let before = HnswSnapshotView::new(
        pager,
        budget,
        base.root,
        base.directory.clone(),
        deltas,
        groups.total_rows(),
        devondb_storage::hnsw::view::HnswGroupLayout::new(groups.row_counts().to_vec()).unwrap(),
        32,
    )
    .unwrap();
    let after = HnswSnapshotView::new(
        pager,
        budget,
        merged.root,
        merged.directory.clone(),
        &[],
        groups.total_rows(),
        devondb_storage::hnsw::view::HnswGroupLayout::new(groups.row_counts().to_vec()).unwrap(),
        32,
    )
    .unwrap();
    assert_eq!(before.entry(), after.entry());
    assert_eq!(before.layer_count(), after.layer_count());
    assert_eq!(before.covered_rows(), after.covered_rows());
    for layer in 0..before.layer_count() {
        for node in 0..before.covered_rows() {
            let mut expected = Vec::new();
            let mut actual = Vec::new();
            before.neighbors(layer, node, &mut expected).unwrap();
            after.neighbors(layer, node, &mut actual).unwrap();
            assert_eq!(actual, expected, "topology differs at ({layer}, {node})");
        }
    }
}

#[test]
fn mandatory_budget_abort_keeps_old_root_readable() {
    let vectors = VectorRows::generated(16);
    let groups = HnswNodeGroups::new(vec![8, 8]).unwrap();
    let fixture = Fixture::new("abort.devondb");
    let base = build_initial_index(
        &fixture.pager,
        &MemoryBudget::unlimited(),
        &config(),
        &groups,
        16,
        4,
        &vectors,
    )
    .unwrap();
    let old_root_bytes = fixture.pager.read_page(base.root_page_id).unwrap();
    let delta = replacement_delta(16, 0, &[]);
    let constrained = MemoryBudget::new(100);

    let result = publish_checkpoint(
        &fixture.pager,
        &constrained,
        &config(),
        Some(base.root_page_id),
        &[delta],
        &groups,
    );
    assert!(matches!(result, Err(DevonError::BudgetExceeded { .. })));
    assert_eq!(constrained.charged(), 0);
    assert_eq!(
        fixture.pager.read_page(base.root_page_id).unwrap(),
        old_root_bytes
    );
    assert_eq!(
        load_persisted_index(&fixture.pager, base.root_page_id).unwrap(),
        base
    );
}

#[test]
fn catch_up_stops_contiguously_and_resumes_with_more_budget() {
    let vectors = VectorRows::generated(16);
    let groups = HnswNodeGroups::new(vec![16]).unwrap();
    let fixture = Fixture::new("catch-up.devondb");
    let base = build_initial_index(
        &fixture.pager,
        &MemoryBudget::unlimited(),
        &config(),
        &groups,
        4,
        4,
        &vectors,
    )
    .unwrap();

    let constrained = MemoryBudget::new(22_000);
    let partial = publish_checkpoint_with_catch_up(
        &fixture.pager,
        &constrained,
        &config(),
        Some(base.root_page_id),
        &[],
        &groups,
        16,
        &vectors,
    )
    .unwrap();
    let stopped = match partial.catch_up {
        CatchUpStatus::StoppedAtBudget(covered) => covered,
        other => panic!("catch-up unexpectedly did not stop: {other:?}"),
    };
    assert!((4..16).contains(&stopped));
    assert_eq!(partial.index.root.covered_rows, stopped);
    assert_eq!(constrained.charged(), 0);

    let resumed = publish_checkpoint_with_catch_up(
        &fixture.pager,
        &MemoryBudget::unlimited(),
        &config(),
        Some(partial.index.root_page_id),
        &[],
        &groups,
        16,
        &vectors,
    )
    .unwrap();
    assert_eq!(resumed.catch_up, CatchUpStatus::Complete);
    assert_eq!(resumed.index.root.covered_rows, 16);
}

#[test]
fn generated_fixture_has_an_upper_layer_for_directory_gates() {
    assert!((0..32).any(|node| level_for_node(&config(), node).unwrap() > 0));
}
