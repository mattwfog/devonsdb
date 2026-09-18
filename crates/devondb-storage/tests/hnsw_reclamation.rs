//! Atomic HNSW subtree retirement, including shared cells and unpublished builds.
use devondb_storage::{
    budget::MemoryBudget,
    catalog::{Catalog, IndexEntry, IndexKind},
    hnsw::{
        index::{
            ConstructionVector, ConstructionVectorAccess, HnswNodeGroups,
            build_initial_index_partial, index_page_inventory, load_persisted_index,
            publish_checkpoint,
        },
        types::{HnswConfig, HnswMetric, NavigationEncoding},
    },
    pager::Pager,
};
use devondb_types::{
    DevonResult,
    logical_type::LogicalType,
    schema::{Column, NodeTableSchema},
};
use std::collections::BTreeSet;
use tempfile::tempdir;
struct Vectors;
impl ConstructionVectorAccess for Vectors {
    fn column_type(&self) -> &LogicalType {
        &LogicalType::Vector { dim: 2 }
    }
    fn with_vector<R, F>(&self, node: u64, read: F) -> DevonResult<R>
    where
        F: FnOnce(Option<ConstructionVector<'_>>) -> DevonResult<R>,
    {
        let decoded = [node as f32, (node % 3) as f32];
        let slot = decoded
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect::<Vec<_>>();
        read(Some(ConstructionVector {
            decoded: &decoded,
            navigation_slot: &slot,
        }))
    }
}
fn config() -> HnswConfig {
    HnswConfig {
        m: 4,
        m0: 8,
        ef_construction: 16,
        level_seed: 42,
        metric: HnswMetric::L2,
        navigation: NavigationEncoding::F32,
    }
}
fn catalog() -> Catalog {
    let mut c = Catalog::default();
    c.add_node_table(
        NodeTableSchema::new(
            "N".into(),
            vec![
                Column {
                    name: "id".into(),
                    ty: LogicalType::Int64,
                    primary_key: true,
                },
                Column {
                    name: "v".into(),
                    ty: LogicalType::Vector { dim: 2 },
                    primary_key: false,
                },
            ],
        )
        .unwrap(),
    )
    .unwrap();
    c
}
fn entry(root: u64) -> IndexEntry {
    IndexEntry {
        name: "idx".into(),
        kind: IndexKind::Hnsw,
        table: "N".into(),
        column: "v".into(),
        root,
    }
}
fn retired(pager: &Pager) -> BTreeSet<u64> {
    pager
        .free_pages_ledger_pages()
        .unwrap()
        .into_iter()
        .flat_map(|(_, p)| {
            p.entries
                .into_iter()
                .skip(p.consumed_count as usize)
                .map(|e| e.page_id)
        })
        .collect()
}

#[test]
fn identity_and_shared_csr_payloads_are_preserved_until_last_root_retires() {
    let dir = tempdir().unwrap();
    let pager = Pager::create(dir.path().join("db"), 4096, *b"hnsw-reclaim-01!").unwrap();
    let budget = MemoryBudget::unlimited();
    let groups = HnswNodeGroups::new(vec![8, 8]).unwrap();
    let first =
        build_initial_index_partial(&pager, &budget, &config(), &groups, 16, 4, &Vectors).unwrap();
    let old_pages = index_page_inventory(&pager, first.root_page_id).unwrap();
    let mut c = catalog();
    c.add_index(entry(first.root_page_id)).unwrap();
    c.save(&pager, 1).unwrap();
    pager.raise_min_pin(1);
    c.save(&pager, 2).unwrap();
    assert!(old_pages.is_disjoint(&retired(&pager)));
    let second = publish_checkpoint(
        &pager,
        &budget,
        &config(),
        Some(first.root_page_id),
        &[],
        &groups,
    )
    .unwrap()
    .index;
    let new_pages = index_page_inventory(&pager, second.root_page_id).unwrap();
    let shared = old_pages
        .intersection(&new_pages)
        .copied()
        .collect::<BTreeSet<_>>();
    assert!(!shared.is_empty());
    let baseline = c.clone();
    c.set_index_root("idx", second.root_page_id).unwrap();
    let prospective = c
        .prospective_pages(&pager, &baseline)
        .into_iter()
        .collect::<BTreeSet<_>>();
    assert!(prospective.is_disjoint(&old_pages));
    c.save(&pager, 3).unwrap();
    let ledger = retired(&pager);
    assert!(old_pages.difference(&new_pages).all(|p| ledger.contains(p)));
    assert!(shared.is_disjoint(&ledger));
    // Snapshot generation 1 still pins every old physical page despite retirement.
    assert_eq!(
        load_persisted_index(&pager, first.root_page_id).unwrap(),
        first
    );
    let third =
        build_initial_index_partial(&pager, &budget, &config(), &groups, 16, 4, &Vectors).unwrap();
    c.set_index_root("idx", third.root_page_id).unwrap();
    c.save(&pager, 4).unwrap();
    assert!(new_pages.iter().all(|p| retired(&pager).contains(p)));
    assert_eq!(
        load_persisted_index(&pager, first.root_page_id).unwrap(),
        first
    );
    pager.raise_min_pin(5);
    let before = pager.free_pages_session_reused();
    for _ in 0..10 {
        pager.allocate_page().unwrap();
    }
    assert!(pager.free_pages_session_reused() > before);
}

#[test]
fn repeated_fresh_rebuilds_reclaim_intermediate_and_old_topology() {
    let dir = tempdir().unwrap();
    let pager = Pager::create(dir.path().join("db"), 4096, *b"hnsw-reclaim-02!").unwrap();
    let budget = MemoryBudget::unlimited();
    let groups = HnswNodeGroups::new(vec![16]).unwrap();
    let mut c = catalog();
    let mut late_pages = Vec::new();
    for generation in 1..=18 {
        let index =
            build_initial_index_partial(&pager, &budget, &config(), &groups, 16, 4, &Vectors)
                .unwrap();
        if generation == 1 {
            c.add_index(entry(index.root_page_id)).unwrap();
        } else {
            c.set_index_root("idx", index.root_page_id).unwrap();
        }
        c.save(&pager, generation).unwrap();
        pager.raise_min_pin(generation);
        assert!(
            index_page_inventory(&pager, index.root_page_id)
                .unwrap()
                .is_disjoint(&retired(&pager))
        );
        if generation > 8 {
            late_pages.push(std::fs::metadata(dir.path().join("db")).unwrap().len() / 4096);
        }
    }
    assert!(pager.free_pages_session_reused() > 100);
    assert!(
        late_pages.iter().max().unwrap() - late_pages.iter().min().unwrap() < 100,
        "growth: {late_pages:?}"
    );
}

#[test]
fn failed_or_partial_attempt_pages_retire_without_retiring_live_root() {
    let dir = tempdir().unwrap();
    let pager = Pager::create(dir.path().join("db"), 4096, *b"hnsw-reclaim-03!").unwrap();
    let groups = HnswNodeGroups::new(vec![8, 8, 8, 8]).unwrap();
    let full = build_initial_index_partial(
        &pager,
        &MemoryBudget::unlimited(),
        &config(),
        &groups,
        32,
        4,
        &Vectors,
    )
    .unwrap();
    let mut c = catalog();
    c.add_index(entry(full.root_page_id)).unwrap();
    c.save(&pager, 1).unwrap();
    pager.raise_min_pin(1);
    let live = index_page_inventory(&pager, full.root_page_id).unwrap();
    let before = devondb_storage::free_pages::prospective_page_count(pager.superblock().db_id);
    // A small first cell flushes, then the 2048-row second cell exceeds its
    // builder reservation. No successful root can inventory those first pages.
    let attempt_groups = HnswNodeGroups::new(vec![2, 2048]).unwrap();
    let delta = devondb_storage::hnsw::types::HnswDelta {
        old_covered_rows: 0,
        new_covered_rows: 2050,
        entry: Some((0, 0)),
        replacements: (0..2050)
            .map(|node| {
                let neighbor = if node == 0 {
                    1
                } else if node == 1 {
                    0
                } else {
                    2 + (node - 1) % 2048
                };
                ((0, node), std::sync::Arc::from([neighbor]))
            })
            .collect(),
    };
    let first_page = std::fs::metadata(dir.path().join("db")).unwrap().len() / 4096;
    let attempt = publish_checkpoint(
        &pager,
        &MemoryBudget::new(64_000),
        &config(),
        None,
        &[delta],
        &attempt_groups,
    );
    assert!(
        matches!(
            attempt,
            Err(devondb_types::DevonError::BudgetExceeded { .. })
        ),
        "{attempt:?}"
    );
    let end_page = std::fs::metadata(dir.path().join("db")).unwrap().len() / 4096;
    let mut written_cells = BTreeSet::new();
    for page in first_page..end_page {
        if &pager.read_page(page).unwrap()[..4] == b"RCSR" {
            written_cells
                .extend(devondb_storage::csr_group::csr_page_inventory(&pager, page, &[]).unwrap());
        }
    }
    assert!(
        written_cells.len() >= 3,
        "first CSR directory and both payloads must flush before failure"
    );
    assert!(devondb_storage::free_pages::prospective_page_count(pager.superblock().db_id) > before);
    c.save(&pager, 2).unwrap();
    assert!(written_cells.is_subset(&retired(&pager)));
    assert!(live.is_disjoint(&retired(&pager)));
    assert_eq!(
        load_persisted_index(&pager, full.root_page_id).unwrap(),
        full
    );
    assert_eq!(
        devondb_storage::free_pages::prospective_page_count(pager.superblock().db_id),
        0
    );
}
