use std::cell::RefCell;
use std::collections::BTreeMap;

use devondb_storage::budget::MemoryBudget;
use devondb_storage::hnsw::search::{
    ScoredNode, SearchOptions, build_reciprocal_replacements, search, select_diversified_neighbors,
};
use devondb_storage::hnsw::types::{GraphAccess, HnswConfig, HnswMetric, NavigationEncoding};
use devondb_types::{DevonError, DevonResult};

#[derive(Default)]
struct MapGraph {
    entry: Option<(u64, u8)>,
    layers: u8,
    covered_rows: u64,
    adjacency: BTreeMap<(u8, u64), Vec<u64>>,
    reads: RefCell<Vec<(u8, u64)>>,
}

impl MapGraph {
    fn with_entry(entry: u64, level: u8, covered_rows: u64) -> Self {
        Self {
            entry: Some((entry, level)),
            layers: level + 1,
            covered_rows,
            ..Self::default()
        }
    }

    fn add_neighbors(&mut self, layer: u8, node: u64, neighbors: &[u64]) {
        self.adjacency.insert((layer, node), neighbors.to_vec());
    }

    fn read_order(&self) -> Vec<(u8, u64)> {
        self.reads.borrow().clone()
    }
}

impl GraphAccess for MapGraph {
    fn entry(&self) -> Option<(u64, u8)> {
        self.entry
    }

    fn layer_count(&self) -> u8 {
        self.layers
    }

    fn covered_rows(&self) -> u64 {
        self.covered_rows
    }

    fn neighbors(&self, layer: u8, node: u64, scratch: &mut Vec<u64>) -> DevonResult<()> {
        self.reads.borrow_mut().push((layer, node));
        scratch.clear();
        if let Some(neighbors) = self.adjacency.get(&(layer, node)) {
            scratch.extend_from_slice(neighbors);
        }
        Ok(())
    }
}

fn config() -> HnswConfig {
    HnswConfig {
        m: 4,
        m0: 8,
        ef_construction: 8,
        level_seed: 7,
        metric: HnswMetric::L2,
        navigation: NavigationEncoding::F32,
    }
}

fn scored(distance: f32, node_offset: u64) -> ScoredNode {
    ScoredNode {
        distance,
        node_offset,
    }
}

#[test]
fn greedy_descent_and_best_first_have_hand_computable_visit_order() {
    let mut graph = MapGraph::with_entry(9, 2, 100);
    graph.add_neighbors(2, 9, &[7, 8]);
    graph.add_neighbors(2, 7, &[4]);
    graph.add_neighbors(1, 4, &[3, 5]);
    graph.add_neighbors(1, 3, &[2]);
    graph.add_neighbors(0, 2, &[0, 1, 3]);
    graph.add_neighbors(0, 0, &[4]);
    graph.add_neighbors(0, 1, &[5]);
    let distances = BTreeMap::from([
        (0, 0.0),
        (1, 1.0),
        (2, 2.0),
        (3, 3.0),
        (4, 4.0),
        (5, 5.0),
        (7, 7.0),
        (8, 8.0),
        (9, 9.0),
    ]);
    let budget = MemoryBudget::unlimited();
    let mut score_order = Vec::new();

    let results = search(
        &graph,
        &config(),
        SearchOptions {
            target_layer: 0,
            k: 3,
            ef_search: 3,
        },
        &budget,
        |node| {
            score_order.push(node);
            Ok(distances[&node])
        },
    )
    .unwrap();

    assert_eq!(score_order, vec![9, 7, 8, 4, 3, 5, 2, 0, 1, 3, 4, 5]);
    assert_eq!(
        graph.read_order(),
        vec![
            (2, 9),
            (2, 7),
            (2, 4),
            (1, 4),
            (1, 3),
            (1, 2),
            (0, 2),
            (0, 0),
            (0, 1),
        ]
    );
    assert_eq!(
        results
            .nodes()
            .iter()
            .map(|node| node.node_offset)
            .collect::<Vec<_>>(),
        vec![0, 1, 2]
    );
    assert!(results.charged_bytes() > 0);
    drop(results);
    assert_eq!(budget.charged(), 0);
}

#[test]
fn distance_ties_use_node_offset_for_results_expansion_and_selection() {
    let mut graph = MapGraph::with_entry(5, 0, 10);
    graph.add_neighbors(0, 5, &[4, 2, 3]);
    let budget = MemoryBudget::unlimited();
    let results = search(
        &graph,
        &config(),
        SearchOptions {
            target_layer: 0,
            k: 4,
            ef_search: 4,
        },
        &budget,
        |node| Ok(if node == 5 { 2.0 } else { 1.0 }),
    )
    .unwrap();
    assert_eq!(
        results
            .nodes()
            .iter()
            .map(|node| node.node_offset)
            .collect::<Vec<_>>(),
        vec![2, 3, 4, 5]
    );
    assert_eq!(graph.read_order(), vec![(0, 5), (0, 2), (0, 3), (0, 4)]);

    let selected = select_diversified_neighbors(
        &config(),
        1,
        9,
        &[scored(1.0, 4), scored(1.0, 2), scored(1.0, 3)],
        &budget,
        |_, _| Ok(100.0),
    )
    .unwrap();
    assert_eq!(selected.nodes(), &[2, 3, 4]);
}

#[test]
fn layer_degree_caps_cannot_be_severed() {
    let candidates: Vec<_> = (0..10).map(|node| scored(node as f32, node)).collect();
    let budget = MemoryBudget::unlimited();

    let upper =
        select_diversified_neighbors(&config(), 1, 99, &candidates, &budget, |_, _| Ok(1_000.0))
            .unwrap();
    let layer_zero =
        select_diversified_neighbors(&config(), 0, 99, &candidates, &budget, |_, _| Ok(1_000.0))
            .unwrap();

    assert_eq!(upper.nodes(), &[0, 1, 2, 3]);
    assert_eq!(layer_zero.nodes(), &[0, 1, 2, 3, 4, 5, 6, 7]);
}

#[test]
fn discovery_stops_at_the_checked_visited_bound() {
    let mut graph = MapGraph::with_entry(0, 0, 100);
    for node in 0..99 {
        graph.add_neighbors(0, node, &[node + 1]);
    }
    let budget = MemoryBudget::unlimited();
    let mut score_order = Vec::new();

    let results = search(
        &graph,
        &config(),
        SearchOptions {
            target_layer: 0,
            k: 1,
            ef_search: 1,
        },
        &budget,
        |node| {
            score_order.push(node);
            Ok((100 - node) as f32)
        },
    )
    .unwrap();

    // V = 1 * (M0 + 1) + 1 * (M + 1) = 14.
    assert_eq!(score_order, (0..14).collect::<Vec<_>>());
    assert_eq!(results.nodes(), &[scored(87.0, 13)]);
    assert_eq!(
        graph.read_order(),
        (0..13).map(|node| (0, node)).collect::<Vec<_>>()
    );
}

#[test]
fn arena_reservation_failure_aborts_before_graph_access_and_releases_charges() {
    let mut graph = MapGraph::with_entry(0, 0, 2);
    graph.add_neighbors(0, 0, &[1]);
    let budget = MemoryBudget::new(100);
    assert!(budget.try_charge(30));
    let before = budget.charged();
    let mut score_calls = 0;

    let error = search(
        &graph,
        &config(),
        SearchOptions {
            target_layer: 0,
            k: 1,
            ef_search: 1,
        },
        &budget,
        |_| {
            score_calls += 1;
            Ok(0.0)
        },
    )
    .unwrap_err();

    assert!(matches!(error, DevonError::BudgetExceeded { .. }));
    assert_eq!(score_calls, 0);
    assert!(graph.read_order().is_empty());
    assert_eq!(budget.charged(), before);
    budget.release(before);
}

#[test]
fn reciprocal_overflow_reprunes_full_ordered_replacements() {
    let mut graph = MapGraph::with_entry(1, 1, 10);
    graph.add_neighbors(1, 1, &[2, 3, 4, 5]);
    let budget = MemoryBudget::unlimited();
    let candidates = [scored(0.5, 1)];

    let replacements = build_reciprocal_replacements(
        &graph,
        &config(),
        1,
        10,
        &candidates,
        &budget,
        reciprocal_distance,
    )
    .unwrap();

    assert_eq!(&*replacements.replacements()[&(1, 1)], &[10, 2, 3, 4]);
    assert_eq!(&*replacements.replacements()[&(1, 10)], &[1]);
    assert!(replacements.charged_bytes() > 0);
    drop(replacements);
    assert_eq!(budget.charged(), 0);
}

#[test]
fn selection_omits_self_and_duplicate_candidates() {
    let budget = MemoryBudget::unlimited();
    let selected = select_diversified_neighbors(
        &config(),
        1,
        10,
        &[
            scored(0.0, 10),
            scored(2.0, 1),
            scored(1.0, 1),
            scored(1.0, 2),
        ],
        &budget,
        |_, _| Ok(100.0),
    )
    .unwrap();

    assert_eq!(selected.nodes(), &[1, 2]);
}

fn reciprocal_distance(left: u64, right: u64) -> DevonResult<f32> {
    let pair = if left <= right {
        (left, right)
    } else {
        (right, left)
    };
    let distance = match pair {
        (1, 10) => 0.5,
        (1, 2) => 1.0,
        (1, 3) => 2.0,
        (1, 4) => 3.0,
        (1, 5) => 4.0,
        _ => 100.0,
    };
    Ok(distance)
}
