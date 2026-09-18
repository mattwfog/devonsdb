//! Bounded deterministic HNSW search (`docs/HNSW.md` §2.4, §4.2, §6.2).

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::mem::size_of;
use std::sync::Arc;

use devondb_types::{DevonError, DevonResult};

use super::scoring::compare_distance_then_offset;
use super::types::{GraphAccess, HnswConfig, MAX_LEVEL, NavigationEncoding};
use crate::budget::{ChargedBytes, MemoryBudget};

const ALLOCATION_OVERHEAD: usize = 64;
const EMPTY_VISITED_SLOT: u64 = u64::MAX;

/// Request-specific policy for one HNSW layer search.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SearchOptions {
    /// Layer at which best-first search runs after greedy descent.
    pub target_layer: u8,
    /// Requested final neighbor count.
    pub k: usize,
    /// Requested best-first candidate width.
    pub ef_search: usize,
}

/// One node scored against the search query.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScoredNode {
    /// Query-to-node navigation distance.
    pub distance: f32,
    /// Stable table node offset.
    pub node_offset: u64,
}

/// Sorted, budget-charged results from [`search`].
#[derive(Debug)]
pub struct SearchResults<'budget> {
    nodes: Vec<ScoredNode>,
    charge: ChargedBytes<'budget>,
}

impl SearchResults<'_> {
    /// Returns results in `(distance.total_cmp, node_offset)` order.
    #[must_use]
    pub fn nodes(&self) -> &[ScoredNode] {
        &self.nodes
    }

    /// Returns the retained result-heap reservation.
    #[must_use]
    pub fn charged_bytes(&self) -> usize {
        self.charge.bytes()
    }
}

/// A deterministic, degree-capped neighbor selection with a live charge.
#[derive(Debug)]
pub struct ChargedNeighbors<'budget> {
    nodes: Vec<u64>,
    charge: ChargedBytes<'budget>,
}

impl ChargedNeighbors<'_> {
    /// Returns selected node offsets in deterministic candidate order.
    #[must_use]
    pub fn nodes(&self) -> &[u64] {
        &self.nodes
    }

    /// Returns the retained neighbor-list reservation.
    #[must_use]
    pub fn charged_bytes(&self) -> usize {
        self.charge.bytes()
    }
}

/// Full ordered replacement lists produced by reciprocal insertion.
#[derive(Debug)]
pub struct ChargedReplacements<'budget> {
    replacements: BTreeMap<(u8, u64), Arc<[u64]>>,
    charge: ChargedBytes<'budget>,
}

impl ChargedReplacements<'_> {
    /// Returns replacements in the exact shape used by `HnswDelta`.
    #[must_use]
    pub fn replacements(&self) -> &BTreeMap<(u8, u64), Arc<[u64]>> {
        &self.replacements
    }

    /// Returns the retained replacement-map reservation.
    #[must_use]
    pub fn charged_bytes(&self) -> usize {
        self.charge.bytes()
    }
}

/// Greedily descends above `target_layer`, then searches that layer best-first.
///
/// `distance` fetches and scores one node's navigation slot. Callers may
/// compose it from their slot reader and `ConstructionScorer`. No callback or
/// graph adjacency is consulted until the complete search arena is reserved.
pub fn search<'budget, G, F>(
    graph: &G,
    config: &HnswConfig,
    options: SearchOptions,
    budget: &'budget MemoryBudget,
    mut distance: F,
) -> DevonResult<SearchResults<'budget>>
where
    G: GraphAccess,
    F: FnMut(u64) -> DevonResult<f32>,
{
    config.validate()?;
    let Some((entry_node, entry_level)) = validate_graph(graph, options)? else {
        return empty_results(budget);
    };
    let bounds = SearchBounds::new(graph, config, options)?;
    let mut arena = SearchArena::new(budget, bounds)?;
    let entry = arena.discover(entry_node, &mut distance)?;
    let start = greedy_descent(
        graph,
        config,
        options.target_layer,
        entry_level,
        entry,
        &mut arena,
        &mut distance,
    )?;
    best_first_layer(
        graph,
        config,
        options.target_layer,
        start,
        &mut arena,
        &mut distance,
    )?;
    Ok(arena.finish())
}

/// Applies the deterministic diversified heuristic with the layer's M/M0 cap.
///
/// `pair_distance` scores two existing nodes. Candidates are sorted before the
/// heuristic, and duplicate candidates and `query_node` are omitted.
pub fn select_diversified_neighbors<'budget, F>(
    config: &HnswConfig,
    layer: u8,
    query_node: u64,
    candidates: &[ScoredNode],
    budget: &'budget MemoryBudget,
    mut pair_distance: F,
) -> DevonResult<ChargedNeighbors<'budget>>
where
    F: FnMut(u64, u64) -> DevonResult<f32>,
{
    config.validate()?;
    let cap = degree_cap(config, layer)?;
    let (mut ordered, ordered_charge) =
        reserve_vec::<ScoredNode>(budget, candidates.len(), "HNSW selection candidates")?;
    ordered.extend_from_slice(candidates);
    ordered.sort_unstable_by(compare_scored);
    let (mut selected, selected_charge) =
        reserve_vec::<u64>(budget, cap, "HNSW selected neighbors")?;
    diversified_into(query_node, &ordered, cap, &mut selected, &mut pair_distance)?;
    drop(ordered_charge);
    Ok(ChargedNeighbors {
        nodes: selected,
        charge: selected_charge,
    })
}

/// Selects neighbors for a new node and builds reciprocal full replacements.
///
/// Existing lists that overflow the layer cap are deterministically re-pruned.
/// If re-pruning rejects the new edge, the corresponding edge is also omitted
/// from the new node's list so every output connection remains reciprocal.
pub fn build_reciprocal_replacements<'budget, G, F>(
    graph: &G,
    config: &HnswConfig,
    layer: u8,
    new_node: u64,
    candidates: &[ScoredNode],
    budget: &'budget MemoryBudget,
    mut pair_distance: F,
) -> DevonResult<ChargedReplacements<'budget>>
where
    G: GraphAccess,
    F: FnMut(u64, u64) -> DevonResult<f32>,
{
    let selected = select_diversified_neighbors(
        config,
        layer,
        new_node,
        candidates,
        budget,
        &mut pair_distance,
    )?;
    let cap = degree_cap(config, layer)?;
    let mut builder = ReplacementBuilder::new(budget, layer, new_node, cap)?;
    for neighbor in selected.nodes() {
        builder.add_reciprocal(graph, *neighbor, &mut pair_distance)?;
    }
    builder.finish()
}

fn validate_graph<G: GraphAccess>(
    graph: &G,
    options: SearchOptions,
) -> DevonResult<Option<(u64, u8)>> {
    if options.k == 0 {
        return Err(invalid_argument("HNSW search k must be greater than zero"));
    }
    match graph.entry() {
        None if graph.layer_count() == 0 => Ok(None),
        None => Err(corrupt("non-empty HNSW graph has no entry point")),
        Some((node, level)) => {
            validate_entry(graph, options.target_layer, node, level)?;
            Ok(Some((node, level)))
        }
    }
}

fn validate_entry<G: GraphAccess>(
    graph: &G,
    target_layer: u8,
    node: u64,
    level: u8,
) -> DevonResult<()> {
    if level > MAX_LEVEL || graph.layer_count() != level + 1 {
        return Err(corrupt("HNSW entry level does not match visible layers"));
    }
    if target_layer > level {
        return Err(invalid_argument(format!(
            "HNSW target layer {target_layer} is above entry level {level}"
        )));
    }
    if node >= graph.covered_rows() {
        return Err(corrupt("HNSW entry node is outside covered rows"));
    }
    Ok(())
}

fn empty_results(budget: &MemoryBudget) -> DevonResult<SearchResults<'_>> {
    let charge = ChargedBytes::try_new(budget, 0, || "empty HNSW result".to_owned())?;
    Ok(SearchResults {
        nodes: Vec::new(),
        charge,
    })
}

#[derive(Debug, Clone, Copy)]
struct SearchBounds {
    width: usize,
    discovery_limit: usize,
    visited_slots: usize,
    adjacency_capacity: usize,
}

impl SearchBounds {
    fn new<G: GraphAccess>(
        graph: &G,
        config: &HnswConfig,
        options: SearchOptions,
    ) -> DevonResult<Self> {
        let width = effective_width(config, options)?;
        let discovery_limit = discovery_limit(graph, config, width)?;
        if discovery_limit == 0 {
            return Err(corrupt("non-empty HNSW graph has zero discovery capacity"));
        }
        let visited_slots = visited_slot_count(discovery_limit)?;
        Ok(Self {
            width,
            discovery_limit,
            visited_slots,
            adjacency_capacity: usize::from(config.m0),
        })
    }
}

fn effective_width(config: &HnswConfig, options: SearchOptions) -> DevonResult<usize> {
    let mut width = options.k.max(options.ef_search);
    if config.navigation == NavigationEncoding::B1 {
        let oversampled = options
            .k
            .checked_mul(3)
            .ok_or_else(|| invalid_argument("b1 HNSW candidate width overflows usize"))?;
        width = width.max(oversampled);
    }
    Ok(width)
}

fn discovery_limit<G: GraphAccess>(
    graph: &G,
    config: &HnswConfig,
    width: usize,
) -> DevonResult<usize> {
    let width = u64::try_from(width)
        .map_err(|_| invalid_argument("HNSW candidate width does not fit u64"))?;
    let layer_zero = width
        .checked_mul(u64::from(config.m0) + 1)
        .ok_or_else(|| invalid_argument("HNSW layer-zero discovery bound overflows u64"))?;
    let upper = u64::from(graph.layer_count())
        .checked_mul(u64::from(config.m) + 1)
        .ok_or_else(|| invalid_argument("HNSW upper-layer discovery bound overflows u64"))?;
    let bound = layer_zero
        .checked_add(upper)
        .ok_or_else(|| invalid_argument("HNSW discovery bound overflows u64"))?;
    usize::try_from(graph.covered_rows().min(bound))
        .map_err(|_| invalid_argument("HNSW discovery bound does not fit usize"))
}

fn visited_slot_count(discovery_limit: usize) -> DevonResult<usize> {
    discovery_limit
        .checked_mul(2)
        .and_then(usize::checked_next_power_of_two)
        .ok_or_else(|| invalid_argument("HNSW visited-table capacity overflows usize"))
}

struct SearchArena<'budget> {
    candidates: MinHeap,
    candidate_charge: ChargedBytes<'budget>,
    results: MaxHeap,
    result_charge: ChargedBytes<'budget>,
    visited: VisitedTable,
    visited_charge: ChargedBytes<'budget>,
    adjacency: Vec<u64>,
    adjacency_charge: ChargedBytes<'budget>,
    discoveries: usize,
    discovery_limit: usize,
}

impl<'budget> SearchArena<'budget> {
    fn new(budget: &'budget MemoryBudget, bounds: SearchBounds) -> DevonResult<Self> {
        let (candidates, candidate_charge) =
            reserve_vec(budget, bounds.width, "HNSW candidate heap")?;
        let (results, result_charge) = reserve_vec(budget, bounds.width, "HNSW result heap")?;
        let (mut visited, visited_charge) =
            reserve_vec(budget, bounds.visited_slots, "HNSW visited table")?;
        visited.resize(bounds.visited_slots, EMPTY_VISITED_SLOT);
        let (adjacency, adjacency_charge) =
            reserve_vec(budget, bounds.adjacency_capacity, "HNSW adjacency scratch")?;
        Ok(Self {
            candidates: MinHeap::new(candidates, bounds.width),
            candidate_charge,
            results: MaxHeap::new(results, bounds.width),
            result_charge,
            visited: VisitedTable::new(visited, bounds.discovery_limit),
            visited_charge,
            adjacency,
            adjacency_charge,
            discoveries: 0,
            discovery_limit: bounds.discovery_limit,
        })
    }

    fn discover<F>(&mut self, node: u64, distance: &mut F) -> DevonResult<ScoredNode>
    where
        F: FnMut(u64) -> DevonResult<f32>,
    {
        if self.exhausted() {
            return Err(corrupt("HNSW discovery attempted after its bound"));
        }
        let scored = ScoredNode {
            distance: distance(node)?,
            node_offset: node,
        };
        self.discoveries += 1;
        Ok(scored)
    }

    fn exhausted(&self) -> bool {
        self.discoveries >= self.discovery_limit
    }

    fn finish(self) -> SearchResults<'budget> {
        let Self {
            results,
            result_charge,
            candidate_charge,
            visited_charge,
            adjacency_charge,
            ..
        } = self;
        let mut nodes = results.into_vec();
        nodes.sort_unstable_by(compare_scored);
        drop((candidate_charge, visited_charge, adjacency_charge));
        SearchResults {
            nodes,
            charge: result_charge,
        }
    }
}

fn greedy_descent<G, F>(
    graph: &G,
    config: &HnswConfig,
    target_layer: u8,
    entry_level: u8,
    mut current: ScoredNode,
    arena: &mut SearchArena<'_>,
    distance: &mut F,
) -> DevonResult<ScoredNode>
where
    G: GraphAccess,
    F: FnMut(u64) -> DevonResult<f32>,
{
    for layer in ((target_layer + 1)..=entry_level).rev() {
        arena.visited.clear();
        arena.visited.insert(current.node_offset)?;
        current = descend_one_layer(graph, config, layer, current, arena, distance)?;
        if arena.exhausted() {
            break;
        }
    }
    Ok(current)
}

fn descend_one_layer<G, F>(
    graph: &G,
    config: &HnswConfig,
    layer: u8,
    mut current: ScoredNode,
    arena: &mut SearchArena<'_>,
    distance: &mut F,
) -> DevonResult<ScoredNode>
where
    G: GraphAccess,
    F: FnMut(u64) -> DevonResult<f32>,
{
    loop {
        load_neighbors(graph, config, layer, current.node_offset, arena)?;
        let best = best_neighbor(graph, current, arena, distance)?;
        if arena.exhausted() || compare_scored(&best, &current) != Ordering::Less {
            return Ok(current);
        }
        current = best;
    }
}

fn best_neighbor<G, F>(
    graph: &G,
    current: ScoredNode,
    arena: &mut SearchArena<'_>,
    distance: &mut F,
) -> DevonResult<ScoredNode>
where
    G: GraphAccess,
    F: FnMut(u64) -> DevonResult<f32>,
{
    let mut best = current;
    for index in 0..arena.adjacency.len() {
        if arena.exhausted() {
            break;
        }
        let neighbor = arena.adjacency[index];
        validate_neighbor(graph, neighbor)?;
        if !arena.visited.insert(neighbor)? {
            continue;
        }
        let scored = arena.discover(neighbor, distance)?;
        if compare_scored(&scored, &best) == Ordering::Less {
            best = scored;
        }
    }
    Ok(best)
}

fn best_first_layer<G, F>(
    graph: &G,
    config: &HnswConfig,
    layer: u8,
    start: ScoredNode,
    arena: &mut SearchArena<'_>,
    distance: &mut F,
) -> DevonResult<()>
where
    G: GraphAccess,
    F: FnMut(u64) -> DevonResult<f32>,
{
    arena.visited.clear();
    arena.visited.insert(start.node_offset)?;
    arena.candidates.push(start);
    arena.results.push(start);
    while !arena.exhausted() {
        let Some(current) = arena.candidates.pop() else {
            break;
        };
        if should_stop_best_first(current, &arena.results) {
            break;
        }
        load_neighbors(graph, config, layer, current.node_offset, arena)?;
        discover_best_first_neighbors(graph, arena, distance)?;
    }
    Ok(())
}

fn should_stop_best_first(current: ScoredNode, results: &MaxHeap) -> bool {
    results.is_full()
        && results
            .worst()
            .is_some_and(|worst| compare_scored(&current, &worst) == Ordering::Greater)
}

fn discover_best_first_neighbors<G, F>(
    graph: &G,
    arena: &mut SearchArena<'_>,
    distance: &mut F,
) -> DevonResult<()>
where
    G: GraphAccess,
    F: FnMut(u64) -> DevonResult<f32>,
{
    for index in 0..arena.adjacency.len() {
        if arena.exhausted() {
            break;
        }
        let neighbor = arena.adjacency[index];
        validate_neighbor(graph, neighbor)?;
        if !arena.visited.insert(neighbor)? {
            continue;
        }
        let scored = arena.discover(neighbor, distance)?;
        if arena.results.would_accept(scored) {
            arena.results.push(scored);
            arena.candidates.push(scored);
        }
    }
    Ok(())
}

fn load_neighbors<G: GraphAccess>(
    graph: &G,
    config: &HnswConfig,
    layer: u8,
    node: u64,
    arena: &mut SearchArena<'_>,
) -> DevonResult<()> {
    graph.neighbors(layer, node, &mut arena.adjacency)?;
    let cap = degree_cap(config, layer)?;
    if arena.adjacency.len() > cap {
        return Err(corrupt(format!(
            "HNSW layer {layer} adjacency degree {} exceeds cap {cap}",
            arena.adjacency.len()
        )));
    }
    Ok(())
}

fn validate_neighbor<G: GraphAccess>(graph: &G, neighbor: u64) -> DevonResult<()> {
    if neighbor >= graph.covered_rows() {
        return Err(corrupt(format!(
            "HNSW neighbor {neighbor} is outside covered rows {}",
            graph.covered_rows()
        )));
    }
    Ok(())
}

fn degree_cap(config: &HnswConfig, layer: u8) -> DevonResult<usize> {
    if layer > MAX_LEVEL {
        return Err(invalid_argument(format!(
            "HNSW layer {layer} exceeds maximum {MAX_LEVEL}"
        )));
    }
    Ok(usize::from(if layer == 0 { config.m0 } else { config.m }))
}

struct MinHeap {
    nodes: Vec<ScoredNode>,
    capacity: usize,
}

impl MinHeap {
    const fn new(nodes: Vec<ScoredNode>, capacity: usize) -> Self {
        Self { nodes, capacity }
    }

    fn push(&mut self, node: ScoredNode) {
        if self.nodes.len() == self.capacity {
            self.replace_worst(node);
            return;
        }
        self.nodes.push(node);
        sift_up_min(&mut self.nodes);
    }

    fn replace_worst(&mut self, node: ScoredNode) {
        let worst = self
            .nodes
            .iter()
            .enumerate()
            .max_by(|(_, left), (_, right)| compare_scored(left, right));
        let Some((index, current)) = worst else {
            return;
        };
        if compare_scored(&node, current) != Ordering::Less {
            return;
        }
        self.nodes[index] = node;
        heapify_min(&mut self.nodes);
    }

    fn pop(&mut self) -> Option<ScoredNode> {
        if self.nodes.is_empty() {
            return None;
        }
        let result = self.nodes.swap_remove(0);
        sift_down_min(&mut self.nodes, 0);
        Some(result)
    }
}

struct MaxHeap {
    nodes: Vec<ScoredNode>,
    capacity: usize,
}

impl MaxHeap {
    const fn new(nodes: Vec<ScoredNode>, capacity: usize) -> Self {
        Self { nodes, capacity }
    }

    fn is_full(&self) -> bool {
        self.nodes.len() == self.capacity
    }

    fn worst(&self) -> Option<ScoredNode> {
        self.nodes.first().copied()
    }

    fn would_accept(&self, node: ScoredNode) -> bool {
        !self.is_full()
            || self
                .worst()
                .is_some_and(|worst| compare_scored(&node, &worst) == Ordering::Less)
    }

    fn push(&mut self, node: ScoredNode) {
        if self.is_full() {
            self.nodes[0] = node;
            sift_down_max(&mut self.nodes, 0);
        } else {
            self.nodes.push(node);
            sift_up_max(&mut self.nodes);
        }
    }

    fn into_vec(self) -> Vec<ScoredNode> {
        self.nodes
    }
}

fn sift_up_min(nodes: &mut [ScoredNode]) {
    let mut child = nodes.len() - 1;
    while child > 0 {
        let parent = (child - 1) / 2;
        if compare_scored(&nodes[child], &nodes[parent]) != Ordering::Less {
            break;
        }
        nodes.swap(child, parent);
        child = parent;
    }
}

fn sift_down_min(nodes: &mut [ScoredNode], mut parent: usize) {
    while let Some(left) = parent.checked_mul(2).and_then(|value| value.checked_add(1)) {
        if left >= nodes.len() {
            break;
        }
        let right = left + 1;
        let child = if right < nodes.len()
            && compare_scored(&nodes[right], &nodes[left]) == Ordering::Less
        {
            right
        } else {
            left
        };
        if compare_scored(&nodes[child], &nodes[parent]) != Ordering::Less {
            break;
        }
        nodes.swap(parent, child);
        parent = child;
    }
}

fn heapify_min(nodes: &mut [ScoredNode]) {
    for parent in (0..(nodes.len() / 2)).rev() {
        sift_down_min(nodes, parent);
    }
}

fn sift_up_max(nodes: &mut [ScoredNode]) {
    let mut child = nodes.len() - 1;
    while child > 0 {
        let parent = (child - 1) / 2;
        if compare_scored(&nodes[child], &nodes[parent]) != Ordering::Greater {
            break;
        }
        nodes.swap(child, parent);
        child = parent;
    }
}

fn sift_down_max(nodes: &mut [ScoredNode], mut parent: usize) {
    loop {
        let left = parent * 2 + 1;
        if left >= nodes.len() {
            break;
        }
        let right = left + 1;
        let child = if right < nodes.len()
            && compare_scored(&nodes[right], &nodes[left]) == Ordering::Greater
        {
            right
        } else {
            left
        };
        if compare_scored(&nodes[child], &nodes[parent]) != Ordering::Greater {
            break;
        }
        nodes.swap(parent, child);
        parent = child;
    }
}

struct VisitedTable {
    slots: Vec<u64>,
    len: usize,
    max_ids: usize,
}

impl VisitedTable {
    const fn new(slots: Vec<u64>, max_ids: usize) -> Self {
        Self {
            slots,
            len: 0,
            max_ids,
        }
    }

    fn clear(&mut self) {
        self.slots.fill(EMPTY_VISITED_SLOT);
        self.len = 0;
    }

    fn insert(&mut self, node: u64) -> DevonResult<bool> {
        let mask = self.slots.len() - 1;
        let mut index = hash_node(node) & mask;
        loop {
            match self.slots[index] {
                EMPTY_VISITED_SLOT => return self.insert_empty(index, node),
                existing if existing == node => return Ok(false),
                _ => index = (index + 1) & mask,
            }
        }
    }

    fn insert_empty(&mut self, index: usize, node: u64) -> DevonResult<bool> {
        if self.len == self.max_ids {
            return Err(corrupt("HNSW visited-table id bound exceeded"));
        }
        self.slots[index] = node;
        self.len += 1;
        Ok(true)
    }
}

fn hash_node(node: u64) -> usize {
    let mut value = node;
    value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    (value ^ (value >> 31)) as usize
}

struct ReplacementBuilder<'budget> {
    layer: u8,
    new_node: u64,
    cap: usize,
    replacements: BTreeMap<(u8, u64), Arc<[u64]>>,
    replacement_charge: ChargedBytes<'budget>,
    scratch: Vec<u64>,
    scratch_charge: ChargedBytes<'budget>,
    scored: Vec<ScoredNode>,
    scored_charge: ChargedBytes<'budget>,
    retained: Vec<u64>,
    retained_charge: ChargedBytes<'budget>,
    confirmed: Vec<u64>,
    confirmed_charge: ChargedBytes<'budget>,
}

impl<'budget> ReplacementBuilder<'budget> {
    fn new(
        budget: &'budget MemoryBudget,
        layer: u8,
        new_node: u64,
        cap: usize,
    ) -> DevonResult<Self> {
        let replacement_bytes = replacement_reservation(cap)?;
        let replacement_charge = ChargedBytes::try_new(budget, replacement_bytes, || {
            format!("HNSW replacement-map reservation of {replacement_bytes} bytes")
        })?;
        let (scratch, scratch_charge) =
            reserve_vec(budget, cap, "HNSW reciprocal adjacency scratch")?;
        let scored_capacity = cap
            .checked_add(1)
            .ok_or_else(|| invalid_argument("HNSW reciprocal candidate capacity overflows"))?;
        let (scored, scored_charge) =
            reserve_vec(budget, scored_capacity, "HNSW reciprocal candidates")?;
        let (retained, retained_charge) =
            reserve_vec(budget, cap, "HNSW reciprocal retained list")?;
        let (confirmed, confirmed_charge) =
            reserve_vec(budget, cap, "HNSW reciprocal new-node list")?;
        Ok(Self {
            layer,
            new_node,
            cap,
            replacements: BTreeMap::new(),
            replacement_charge,
            scratch,
            scratch_charge,
            scored,
            scored_charge,
            retained,
            retained_charge,
            confirmed,
            confirmed_charge,
        })
    }

    fn add_reciprocal<G, F>(
        &mut self,
        graph: &G,
        existing: u64,
        pair_distance: &mut F,
    ) -> DevonResult<()>
    where
        G: GraphAccess,
        F: FnMut(u64, u64) -> DevonResult<f32>,
    {
        validate_neighbor(graph, existing)?;
        graph.neighbors(self.layer, existing, &mut self.scratch)?;
        validate_existing_list(graph, existing, &self.scratch, self.cap)?;
        self.score_reciprocal_candidates(existing, pair_distance)?;
        self.choose_existing_replacement(existing, pair_distance)?;
        if !self.retained.contains(&self.new_node) {
            return Ok(());
        }
        self.confirmed.push(existing);
        if self.retained != self.scratch {
            self.replacements
                .insert((self.layer, existing), Arc::from(self.retained.as_slice()));
        }
        Ok(())
    }

    fn score_reciprocal_candidates<F>(
        &mut self,
        existing: u64,
        pair_distance: &mut F,
    ) -> DevonResult<()>
    where
        F: FnMut(u64, u64) -> DevonResult<f32>,
    {
        self.scored.clear();
        for node in &self.scratch {
            self.scored.push(ScoredNode {
                distance: pair_distance(existing, *node)?,
                node_offset: *node,
            });
        }
        if !self.scratch.contains(&self.new_node) {
            self.scored.push(ScoredNode {
                distance: pair_distance(existing, self.new_node)?,
                node_offset: self.new_node,
            });
        }
        self.scored.sort_unstable_by(compare_scored);
        Ok(())
    }

    fn choose_existing_replacement<F>(
        &mut self,
        existing: u64,
        pair_distance: &mut F,
    ) -> DevonResult<()>
    where
        F: FnMut(u64, u64) -> DevonResult<f32>,
    {
        self.retained.clear();
        if self.scored.len() <= self.cap {
            self.retained
                .extend(self.scored.iter().map(|candidate| candidate.node_offset));
            return Ok(());
        }
        diversified_into(
            existing,
            &self.scored,
            self.cap,
            &mut self.retained,
            pair_distance,
        )
    }

    fn finish(mut self) -> DevonResult<ChargedReplacements<'budget>> {
        validate_selected(self.new_node, &self.confirmed, self.cap)?;
        self.replacements.insert(
            (self.layer, self.new_node),
            Arc::from(self.confirmed.as_slice()),
        );
        drop((
            self.scratch_charge,
            self.scored_charge,
            self.retained_charge,
            self.confirmed_charge,
        ));
        Ok(ChargedReplacements {
            replacements: self.replacements,
            charge: self.replacement_charge,
        })
    }
}

fn validate_existing_list<G: GraphAccess>(
    graph: &G,
    node: u64,
    neighbors: &[u64],
    cap: usize,
) -> DevonResult<()> {
    if neighbors.len() > cap {
        return Err(corrupt(format!(
            "HNSW existing node {node} degree {} exceeds cap {cap}",
            neighbors.len()
        )));
    }
    for (index, neighbor) in neighbors.iter().enumerate() {
        if *neighbor == node || *neighbor >= graph.covered_rows() {
            return Err(corrupt(format!(
                "HNSW existing node {node} has invalid neighbor {neighbor}"
            )));
        }
        if neighbors[..index].contains(neighbor) {
            return Err(corrupt(format!(
                "HNSW existing node {node} repeats neighbor {neighbor}"
            )));
        }
    }
    Ok(())
}

fn diversified_into<F>(
    query_node: u64,
    ordered: &[ScoredNode],
    cap: usize,
    selected: &mut Vec<u64>,
    pair_distance: &mut F,
) -> DevonResult<()>
where
    F: FnMut(u64, u64) -> DevonResult<f32>,
{
    selected.clear();
    for candidate in ordered {
        if selected.len() == cap {
            break;
        }
        if candidate.node_offset == query_node || selected.contains(&candidate.node_offset) {
            continue;
        }
        if is_diversified(candidate, selected, pair_distance)? {
            selected.push(candidate.node_offset);
        }
    }
    validate_selected(query_node, selected, cap)
}

fn is_diversified<F>(
    candidate: &ScoredNode,
    selected: &[u64],
    pair_distance: &mut F,
) -> DevonResult<bool>
where
    F: FnMut(u64, u64) -> DevonResult<f32>,
{
    for retained in selected {
        let separation = pair_distance(candidate.node_offset, *retained)?;
        if separation.total_cmp(&candidate.distance) == Ordering::Less {
            return Ok(false);
        }
    }
    Ok(true)
}

fn validate_selected(query_node: u64, selected: &[u64], cap: usize) -> DevonResult<()> {
    if selected.len() > cap {
        return Err(corrupt("HNSW selected neighbor list exceeds degree cap"));
    }
    for (index, node) in selected.iter().enumerate() {
        if *node == query_node || selected[..index].contains(node) {
            return Err(corrupt(
                "HNSW selected neighbor list has a self/duplicate edge",
            ));
        }
    }
    Ok(())
}

fn replacement_reservation(cap: usize) -> DevonResult<usize> {
    let replacement_count = cap
        .checked_add(1)
        .ok_or_else(|| invalid_argument("HNSW replacement count overflows usize"))?;
    let list_bytes = cap
        .checked_mul(size_of::<u64>())
        .and_then(|bytes| bytes.checked_add(ALLOCATION_OVERHEAD))
        .ok_or_else(|| invalid_argument("HNSW replacement list size overflows usize"))?;
    replacement_count
        .checked_mul(list_bytes)
        .and_then(|bytes| bytes.checked_add(ALLOCATION_OVERHEAD))
        .ok_or_else(|| invalid_argument("HNSW replacement-map size overflows usize"))
}

fn reserve_vec<'budget, T>(
    budget: &'budget MemoryBudget,
    capacity: usize,
    name: &str,
) -> DevonResult<(Vec<T>, ChargedBytes<'budget>)> {
    let bytes = allocation_bytes::<T>(capacity, name)?;
    let charge = ChargedBytes::try_new(budget, bytes, || {
        format!("{name} reservation of {bytes} bytes")
    })?;
    let mut values = Vec::new();
    values.try_reserve_exact(capacity).map_err(|error| {
        budget_exceeded(format!(
            "failed to allocate {name} capacity {capacity}: {error}"
        ))
    })?;
    Ok((values, charge))
}

fn allocation_bytes<T>(capacity: usize, name: &str) -> DevonResult<usize> {
    let payload = capacity
        .checked_mul(size_of::<T>())
        .ok_or_else(|| invalid_argument(format!("{name} reservation overflows usize")))?;
    if capacity == 0 {
        return Ok(0);
    }
    payload
        .checked_add(ALLOCATION_OVERHEAD)
        .ok_or_else(|| invalid_argument(format!("{name} reservation overflows usize")))
}

fn compare_scored(left: &ScoredNode, right: &ScoredNode) -> Ordering {
    compare_distance_then_offset(
        left.distance,
        left.node_offset,
        right.distance,
        right.node_offset,
    )
}

fn invalid_argument(context: impl Into<String>) -> DevonError {
    DevonError::InvalidArgument {
        context: context.into(),
    }
}

fn budget_exceeded(context: impl Into<String>) -> DevonError {
    DevonError::BudgetExceeded {
        context: context.into(),
    }
}

fn corrupt(context: impl Into<String>) -> DevonError {
    DevonError::Corrupt {
        context: context.into(),
    }
}
