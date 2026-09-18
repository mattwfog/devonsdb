//! HNSW lifecycle: insert proposals, initial build, checkpoint
//! publication, recovery-tail state (`docs/HNSW.md` §5).

use std::collections::{BTreeMap, BTreeSet};
use std::mem::size_of;
use std::ops::Range;
use std::sync::Arc;

use devondb_types::{DevonError, DevonResult, logical_type::LogicalType};

use super::format::{HnswRoot, LayerDirectory};
use super::scoring::{ConstructionScorer, level_for_node};
use super::search::{
    ChargedReplacements, ScoredNode, SearchOptions, build_reciprocal_replacements, search,
};
use super::types::{
    GraphAccess, HnswConfig, HnswDelta, NavigationEncoding, NavigationScorer, supports_index,
};
use super::view::{HnswGroupLayout, HnswSnapshotView};
use crate::budget::{ChargedBytes, MemoryBudget};
use crate::csr_group::CsrGroup;
use crate::pager::Pager;

const ALLOCATION_OVERHEAD: usize = 64;

/// One eligible row's decoded construction vector and encoded navigation slot.
///
/// The decoded vector supplies the scalar construction query. The navigation
/// slot is scored by [`ConstructionScorer`] and must use the column's physical
/// encoding. A null vector is represented by `None` at the accessor boundary.
#[derive(Debug, Clone, Copy)]
pub struct ConstructionVector<'row> {
    /// Original decoded f32 vector used as a construction query.
    pub decoded: &'row [f32],
    /// Encoded value slot in the indexed column's navigation representation.
    pub navigation_slot: &'row [u8],
}

/// Snapshot-consistent vector access used by topology construction.
///
/// The callback permits pager-backed implementations to pin one row only for
/// the duration of a score. Implementations return `None` for null vectors.
pub trait ConstructionVectorAccess {
    /// Indexed vector column type, including its physical encoding.
    fn column_type(&self) -> &LogicalType;

    /// Invokes `read` with one row's vector, or `None` when the row is null.
    fn with_vector<R, F>(&self, node: u64, read: F) -> DevonResult<R>
    where
        F: FnOnce(Option<ConstructionVector<'_>>) -> DevonResult<R>;
}

/// A successful insertion proposal whose resident bytes remain charged.
#[derive(Debug)]
pub struct ChargedHnswDelta<'budget> {
    delta: HnswDelta,
    charge: ChargedBytes<'budget>,
}

impl<'budget> ChargedHnswDelta<'budget> {
    /// Returns the proposed immutable delta.
    #[must_use]
    pub fn delta(&self) -> &HnswDelta {
        &self.delta
    }

    /// Returns the proposal reservation retained against the shared budget.
    #[must_use]
    pub fn charged_bytes(&self) -> usize {
        self.charge.bytes()
    }

    /// Transfers the delta and its live reservation to the publication caller.
    #[must_use]
    pub fn into_parts(self) -> (HnswDelta, ChargedBytes<'budget>) {
        (self.delta, self.charge)
    }
}

/// Non-error outcome of preparing derived HNSW state for a base commit.
#[derive(Debug)]
pub enum InsertProposalOutcome<'budget> {
    /// The complete charged proposal is ready for atomic publication.
    Proposed(ChargedHnswDelta<'budget>),
    /// An older uncovered tail exists, so these rows simply join that tail.
    TailExists,
    /// Derived-state construction did not fit; the base commit may continue.
    BudgetExceeded,
}

/// Geometry of catalog-order node groups used by checkpoint publication.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HnswNodeGroups {
    row_counts: Vec<u64>,
    starts: Vec<u64>,
    total_rows: u64,
}

impl HnswNodeGroups {
    /// Builds geometry from nonzero catalog-order node-group row counts.
    pub fn new(row_counts: Vec<u64>) -> DevonResult<Self> {
        let mut starts = Vec::with_capacity(row_counts.len());
        let mut total_rows = 0_u64;
        for (group, row_count) in row_counts.iter().copied().enumerate() {
            if row_count == 0 {
                return Err(invalid_argument(format!(
                    "HNSW node-group {group} has zero rows"
                )));
            }
            starts.push(total_rows);
            total_rows = total_rows
                .checked_add(row_count)
                .ok_or_else(|| invalid_argument("HNSW node-group geometry overflows u64"))?;
        }
        Ok(Self {
            row_counts,
            starts,
            total_rows,
        })
    }

    /// Returns the total rows described by the geometry.
    #[must_use]
    pub const fn total_rows(&self) -> u64 {
        self.total_rows
    }

    /// Returns catalog-order node-group row counts.
    #[must_use]
    pub fn row_counts(&self) -> &[u64] {
        &self.row_counts
    }

    fn view_layout(&self) -> DevonResult<HnswGroupLayout> {
        HnswGroupLayout::new(self.row_counts.clone())
    }

    fn groups_intersecting(&self, covered_rows: u64) -> DevonResult<u32> {
        if covered_rows > self.total_rows {
            return Err(invalid_argument(format!(
                "HNSW coverage {covered_rows} exceeds node-group rows {}",
                self.total_rows
            )));
        }
        let count = self
            .starts
            .partition_point(|group_start| *group_start < covered_rows);
        u32::try_from(count).map_err(|_| invalid_argument("HNSW node-group count exceeds u32"))
    }

    fn locate(&self, node: u64) -> DevonResult<(u32, u64)> {
        if node >= self.total_rows {
            return Err(invalid_argument(format!(
                "HNSW node {node} is outside node-group geometry {}",
                self.total_rows
            )));
        }
        let group = self.starts.partition_point(|start| *start <= node) - 1;
        let group_u32 = u32::try_from(group)
            .map_err(|_| invalid_argument("HNSW node-group index exceeds u32"))?;
        Ok((group_u32, self.starts[group]))
    }

    fn covered_in_group(&self, group: u32, covered_rows: u64) -> DevonResult<usize> {
        let group = usize::try_from(group)
            .map_err(|_| invalid_argument("HNSW node-group index exceeds usize"))?;
        let start = *self
            .starts
            .get(group)
            .ok_or_else(|| invalid_argument("HNSW node-group index is out of bounds"))?;
        let capacity = self.row_counts[group];
        let covered = covered_rows.saturating_sub(start).min(capacity);
        usize::try_from(covered)
            .map_err(|_| invalid_argument("HNSW covered group rows exceed usize"))
    }
}

/// A decoded immutable index publication.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersistedHnswIndex {
    /// Page id of the immutable root.
    pub root_page_id: u64,
    /// Decoded root metadata.
    pub root: HnswRoot,
    /// Decoded layer-major CSR directory.
    pub directory: LayerDirectory,
}

/// Result of optional checkpoint tail catch-up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CatchUpStatus {
    /// This checkpoint did not request catch-up.
    NotRequested,
    /// Catch-up reached its requested target.
    Complete,
    /// Budget pressure stopped catch-up at this still-contiguous coverage.
    StoppedAtBudget(u64),
}

/// Newly written root publication plus optional catch-up status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckpointPublication {
    /// The new immutable root and directory.
    pub index: PersistedHnswIndex,
    /// Whether optional recovery-tail catch-up completed.
    pub catch_up: CatchUpStatus,
}

/// Prepares one deterministic insertion delta over a contiguous row range.
///
/// `rows` is canonical table-offset order. A range beginning after the view's
/// coverage proves an older exact tail exists and returns
/// [`InsertProposalOutcome::TailExists`] without reading vectors. Any
/// `BudgetExceeded` raised by construction becomes the distinct derived-state
/// outcome and is not returned as a transaction-failing error.
pub fn propose_insert<'budget, G, V>(
    graph: &G,
    config: &HnswConfig,
    rows: Range<u64>,
    vectors: &V,
    budget: &'budget MemoryBudget,
) -> DevonResult<InsertProposalOutcome<'budget>>
where
    G: GraphAccess,
    V: ConstructionVectorAccess,
{
    config.validate()?;
    validate_column(config, vectors.column_type())?;
    if rows.is_empty() {
        return Err(invalid_argument("HNSW proposal row range is empty"));
    }
    if rows.start > graph.covered_rows() {
        return Ok(InsertProposalOutcome::TailExists);
    }
    match propose_insert_inner(graph, config, rows, vectors, budget) {
        Ok(proposal) => Ok(InsertProposalOutcome::Proposed(proposal)),
        Err(DevonError::BudgetExceeded { .. }) => Ok(InsertProposalOutcome::BudgetExceeded),
        Err(error) => Err(error),
    }
}

fn propose_insert_inner<'budget, G, V>(
    graph: &G,
    config: &HnswConfig,
    rows: Range<u64>,
    vectors: &V,
    budget: &'budget MemoryBudget,
) -> DevonResult<ChargedHnswDelta<'budget>>
where
    G: GraphAccess,
    V: ConstructionVectorAccess,
{
    validate_proposal_inputs(graph, config, &rows, vectors)?;
    let charge = ChargedBytes::try_new(budget, ALLOCATION_OVERHEAD, || {
        "HNSW proposal map reservation".to_owned()
    })?;
    let mut proposal = ProposalGraph::new(graph, charge);
    for node in rows.clone() {
        insert_row(&mut proposal, config, node, vectors, budget)?;
    }
    Ok(proposal.finish(rows.start))
}

fn validate_proposal_inputs<G, V>(
    graph: &G,
    config: &HnswConfig,
    rows: &Range<u64>,
    vectors: &V,
) -> DevonResult<()>
where
    G: GraphAccess,
    V: ConstructionVectorAccess,
{
    config.validate()?;
    validate_column(config, vectors.column_type())?;
    if rows.start < graph.covered_rows() {
        return Err(invalid_argument(format!(
            "HNSW proposal starts at {}, before view coverage {}",
            rows.start,
            graph.covered_rows()
        )));
    }
    Ok(())
}

struct ProposalGraph<'graph, 'budget, G> {
    base: &'graph G,
    replacements: BTreeMap<(u8, u64), Arc<[u64]>>,
    entry: Option<(u64, u8)>,
    layer_count: u8,
    covered_rows: u64,
    charge: ChargedBytes<'budget>,
}

impl<'graph, 'budget, G> ProposalGraph<'graph, 'budget, G>
where
    G: GraphAccess,
{
    fn new(base: &'graph G, charge: ChargedBytes<'budget>) -> Self {
        Self {
            base,
            replacements: BTreeMap::new(),
            entry: base.entry(),
            layer_count: base.layer_count(),
            covered_rows: base.covered_rows(),
            charge,
        }
    }

    fn apply_replacements(
        &mut self,
        replacements: &BTreeMap<(u8, u64), Arc<[u64]>>,
        budget: &MemoryBudget,
        cap: usize,
    ) -> DevonResult<()> {
        let removals = self.reciprocal_removals(replacements, budget, cap)?;
        for (slot, list) in replacements {
            self.reserve_replacement(*slot, list.len())?;
            self.replacements.insert(*slot, Arc::clone(list));
        }
        self.apply_reciprocal_removals(&removals.values, budget, cap)?;
        Ok(())
    }

    fn reciprocal_removals(
        &self,
        replacements: &BTreeMap<(u8, u64), Arc<[u64]>>,
        budget: &'budget MemoryBudget,
        cap: usize,
    ) -> DevonResult<ChargedRemovalList<'budget>> {
        let scratch_bytes = allocation_bytes::<u64>(cap, "HNSW pruning scratch")?;
        let _scratch_charge = ChargedBytes::try_new(budget, scratch_bytes, || {
            format!("HNSW pruning scratch reservation of {scratch_bytes} bytes")
        })?;
        let mut scratch = Vec::new();
        scratch.try_reserve_exact(cap).map_err(|error| {
            budget_exceeded(format!("failed to allocate HNSW pruning scratch: {error}"))
        })?;
        let count = self.count_reciprocal_removals(replacements, &mut scratch)?;
        let bytes = allocation_bytes::<(u8, u64, u64)>(count, "HNSW reciprocal removals")?;
        let charge = ChargedBytes::try_new(budget, bytes, || {
            format!("HNSW reciprocal-removal reservation of {bytes} bytes")
        })?;
        let mut removals = Vec::new();
        removals.try_reserve_exact(count).map_err(|error| {
            budget_exceeded(format!("failed to allocate reciprocal removals: {error}"))
        })?;
        self.collect_reciprocal_removals(replacements, &mut scratch, &mut removals)?;
        Ok(ChargedRemovalList {
            values: removals,
            _charge: charge,
        })
    }

    fn count_reciprocal_removals(
        &self,
        replacements: &BTreeMap<(u8, u64), Arc<[u64]>>,
        scratch: &mut Vec<u64>,
    ) -> DevonResult<usize> {
        let mut count = 0_usize;
        for (&(layer, node), replacement) in replacements {
            if node >= self.covered_rows {
                continue;
            }
            self.neighbors(layer, node, scratch)?;
            count = count
                .checked_add(
                    scratch
                        .iter()
                        .filter(|neighbor| !replacement.contains(neighbor))
                        .count(),
                )
                .ok_or_else(|| invalid_argument("HNSW reciprocal-removal count overflows"))?;
        }
        Ok(count)
    }

    fn collect_reciprocal_removals(
        &self,
        replacements: &BTreeMap<(u8, u64), Arc<[u64]>>,
        scratch: &mut Vec<u64>,
        removals: &mut Vec<(u8, u64, u64)>,
    ) -> DevonResult<()> {
        for (&(layer, node), replacement) in replacements {
            if node >= self.covered_rows {
                continue;
            }
            self.neighbors(layer, node, scratch)?;
            removals.extend(
                scratch
                    .iter()
                    .filter(|neighbor| !replacement.contains(neighbor))
                    .map(|neighbor| (layer, node, *neighbor)),
            );
        }
        Ok(())
    }

    fn apply_reciprocal_removals(
        &mut self,
        removals: &[(u8, u64, u64)],
        budget: &MemoryBudget,
        cap: usize,
    ) -> DevonResult<()> {
        let bytes = allocation_bytes::<u64>(cap, "HNSW reciprocal repair scratch")?;
        let _charge = ChargedBytes::try_new(budget, bytes, || {
            format!("HNSW reciprocal repair scratch reservation of {bytes} bytes")
        })?;
        let mut scratch = Vec::new();
        scratch.try_reserve_exact(cap).map_err(|error| {
            budget_exceeded(format!(
                "failed to allocate reciprocal repair scratch: {error}"
            ))
        })?;
        for &(layer, node, neighbor) in removals {
            self.neighbors(layer, neighbor, &mut scratch)?;
            if let Some(position) = scratch.iter().position(|candidate| *candidate == node) {
                scratch.remove(position);
                self.reserve_replacement((layer, neighbor), scratch.len())?;
                self.replacements
                    .insert((layer, neighbor), Arc::from(scratch.as_slice()));
            }
        }
        Ok(())
    }

    fn insert_empty(&mut self, layer: u8, node: u64) -> DevonResult<()> {
        self.reserve_replacement((layer, node), 0)?;
        self.replacements.insert((layer, node), Arc::from([]));
        Ok(())
    }

    fn reserve_replacement(&mut self, slot: (u8, u64), degree: usize) -> DevonResult<()> {
        if let Some(previous) = self.replacements.get(&slot) {
            let previous_bytes = replacement_charge(previous.len())?;
            self.charge.shrink(previous_bytes);
        }
        let bytes = replacement_charge(degree)?;
        self.charge.grow(bytes, || {
            format!("HNSW proposal replacement reservation of {bytes} bytes")
        })
    }

    fn advance(&mut self, node: u64, level: Option<u8>) -> DevonResult<()> {
        self.covered_rows = node
            .checked_add(1)
            .ok_or_else(|| invalid_argument("HNSW proposal coverage overflows u64"))?;
        if let Some(level) = level
            && self.entry.is_none_or(|(_, old_level)| level > old_level)
        {
            self.entry = Some((node, level));
            self.layer_count = level + 1;
        }
        Ok(())
    }

    fn finish(self, old_covered_rows: u64) -> ChargedHnswDelta<'budget> {
        let entry = (self.entry != self.base.entry())
            .then_some(self.entry)
            .flatten();
        ChargedHnswDelta {
            delta: HnswDelta {
                old_covered_rows,
                new_covered_rows: self.covered_rows,
                replacements: self.replacements,
                entry,
            },
            charge: self.charge,
        }
    }
}

struct ChargedRemovalList<'budget> {
    values: Vec<(u8, u64, u64)>,
    _charge: ChargedBytes<'budget>,
}

impl<G> GraphAccess for ProposalGraph<'_, '_, G>
where
    G: GraphAccess,
{
    fn entry(&self) -> Option<(u64, u8)> {
        self.entry
    }

    fn layer_count(&self) -> u8 {
        self.layer_count
    }

    fn covered_rows(&self) -> u64 {
        self.covered_rows
    }

    fn neighbors(&self, layer: u8, node: u64, scratch: &mut Vec<u64>) -> DevonResult<()> {
        scratch.clear();
        validate_graph_lookup(self, layer, node)?;
        if let Some(replacement) = self.replacements.get(&(layer, node)) {
            scratch.extend_from_slice(replacement);
        } else if layer < self.base.layer_count() && node < self.base.covered_rows() {
            self.base.neighbors(layer, node, scratch)?;
        }
        Ok(())
    }
}

fn insert_row<G, V>(
    proposal: &mut ProposalGraph<'_, '_, G>,
    config: &HnswConfig,
    node: u64,
    vectors: &V,
    budget: &MemoryBudget,
) -> DevonResult<()>
where
    G: GraphAccess,
    V: ConstructionVectorAccess,
{
    if node != proposal.covered_rows() {
        return Err(invalid_argument(format!(
            "HNSW canonical insertion expected node {}, got {node}",
            proposal.covered_rows()
        )));
    }
    let eligible = vectors.with_vector(node, |vector| Ok(vector.is_some()))?;
    if !eligible {
        return proposal.advance(node, None);
    }
    let level = level_for_node(config, node)?;
    insert_eligible(proposal, config, node, level, vectors, budget)?;
    proposal.advance(node, Some(level))
}

fn insert_eligible<G, V>(
    proposal: &mut ProposalGraph<'_, '_, G>,
    config: &HnswConfig,
    node: u64,
    level: u8,
    vectors: &V,
    budget: &MemoryBudget,
) -> DevonResult<()>
where
    G: GraphAccess,
    V: ConstructionVectorAccess,
{
    let Some((_, entry_level)) = proposal.entry() else {
        for layer in 0..=level {
            proposal.insert_empty(layer, node)?;
        }
        return Ok(());
    };
    let participating_top = level.min(entry_level);
    let layer_count = usize::from(participating_top) + 1;
    let pending_bytes =
        allocation_bytes::<ChargedReplacements<'_>>(layer_count, "HNSW pending insertion layers")?;
    let _pending_charge = ChargedBytes::try_new(budget, pending_bytes, || {
        format!("HNSW pending insertion layers reservation of {pending_bytes} bytes")
    })?;
    let mut pending = Vec::new();
    pending.try_reserve_exact(layer_count).map_err(|error| {
        budget_exceeded(format!("failed to allocate pending HNSW layers: {error}"))
    })?;
    for layer in (0..=participating_top).rev() {
        pending.push(build_layer_replacements(
            proposal, config, layer, node, vectors, budget,
        )?);
    }
    for replacements in &pending {
        let layer = replacements
            .replacements()
            .keys()
            .next()
            .map(|(layer, _)| *layer)
            .ok_or_else(|| corrupt("HNSW insertion produced no replacements"))?;
        proposal.apply_replacements(
            replacements.replacements(),
            budget,
            degree_cap(config, layer),
        )?;
    }
    for layer in (participating_top + 1)..=level {
        proposal.insert_empty(layer, node)?;
    }
    Ok(())
}

fn build_layer_replacements<'budget, G, V>(
    proposal: &mut ProposalGraph<'_, '_, G>,
    config: &HnswConfig,
    layer: u8,
    node: u64,
    vectors: &V,
    budget: &'budget MemoryBudget,
) -> DevonResult<ChargedReplacements<'budget>>
where
    G: GraphAccess,
    V: ConstructionVectorAccess,
{
    with_scorer(config, vectors, node, budget, |scorer| {
        let candidates = search(
            proposal,
            config,
            SearchOptions {
                target_layer: layer,
                k: config.ef_construction as usize,
                ef_search: config.ef_construction as usize,
            },
            budget,
            |candidate| navigation_distance(vectors, candidate, scorer),
        )?;
        apply_reciprocal_layer(
            proposal,
            config,
            layer,
            node,
            candidates.nodes(),
            vectors,
            budget,
        )
    })
}

fn apply_reciprocal_layer<'budget, G, V>(
    proposal: &ProposalGraph<'_, '_, G>,
    config: &HnswConfig,
    layer: u8,
    node: u64,
    candidates: &[ScoredNode],
    vectors: &V,
    budget: &'budget MemoryBudget,
) -> DevonResult<ChargedReplacements<'budget>>
where
    G: GraphAccess,
    V: ConstructionVectorAccess,
{
    build_reciprocal_replacements(
        proposal,
        config,
        layer,
        node,
        candidates,
        budget,
        |left, right| pair_distance(config, vectors, left, right, budget),
    )
}

fn with_scorer<V, R, F>(
    config: &HnswConfig,
    vectors: &V,
    node: u64,
    budget: &MemoryBudget,
    use_scorer: F,
) -> DevonResult<R>
where
    V: ConstructionVectorAccess,
    F: FnOnce(&ConstructionScorer) -> DevonResult<R>,
{
    vectors.with_vector(node, |vector| {
        let vector = vector
            .ok_or_else(|| corrupt(format!("HNSW topology node {node} has a null vector")))?;
        let bytes = scorer_reservation(vectors.column_type())?;
        let _charge = ChargedBytes::try_new(budget, bytes, || {
            format!("HNSW scalar construction scorer reservation of {bytes} bytes")
        })?;
        let scorer = ConstructionScorer::new(config, vectors.column_type(), vector.decoded)?;
        use_scorer(&scorer)
    })
}

fn navigation_distance<V>(
    vectors: &V,
    candidate: u64,
    scorer: &ConstructionScorer,
) -> DevonResult<f32>
where
    V: ConstructionVectorAccess,
{
    vectors.with_vector(candidate, |vector| {
        let vector = vector
            .ok_or_else(|| corrupt(format!("HNSW candidate node {candidate} has a null vector")))?;
        scorer.navigation_distance(vector.navigation_slot)
    })
}

fn pair_distance<V>(
    config: &HnswConfig,
    vectors: &V,
    left: u64,
    right: u64,
    budget: &MemoryBudget,
) -> DevonResult<f32>
where
    V: ConstructionVectorAccess,
{
    with_scorer(config, vectors, left, budget, |scorer| {
        navigation_distance(vectors, right, scorer)
    })
}

fn scorer_reservation(column_type: &LogicalType) -> DevonResult<usize> {
    let dimension = column_type
        .vector_dim()
        .ok_or_else(|| invalid_argument("HNSW construction requires a vector column"))?;
    usize::try_from(dimension)
        .ok()
        .and_then(|dimension| dimension.checked_mul(64))
        .and_then(|bytes| bytes.checked_add(8 * ALLOCATION_OVERHEAD))
        .ok_or_else(|| invalid_argument("HNSW construction scorer reservation overflows"))
}

fn validate_column(config: &HnswConfig, column_type: &LogicalType) -> DevonResult<()> {
    if !supports_index(column_type, config.metric) {
        return Err(invalid_argument(format!(
            "unsupported HNSW construction column {column_type} for {:?}",
            config.metric
        )));
    }
    let navigation = NavigationEncoding::of_column(column_type)
        .ok_or_else(|| invalid_argument("HNSW construction requires a vector column"))?;
    if navigation != config.navigation {
        return Err(corrupt(format!(
            "HNSW config navigation {:?} does not match column navigation {navigation:?}",
            config.navigation
        )));
    }
    Ok(())
}

fn validate_graph_lookup<G: GraphAccess>(graph: &G, layer: u8, node: u64) -> DevonResult<()> {
    if layer >= graph.layer_count() {
        return Err(invalid_argument(format!(
            "HNSW layer {layer} is outside layer_count {}",
            graph.layer_count()
        )));
    }
    if node >= graph.covered_rows() {
        return Err(invalid_argument(format!(
            "HNSW node {node} is outside coverage {}",
            graph.covered_rows()
        )));
    }
    Ok(())
}

/// Loads and validates an immutable root and its contiguous layer directory.
pub fn load_persisted_index(pager: &Pager, root_page_id: u64) -> DevonResult<PersistedHnswIndex> {
    let root_page = read_index_page(pager, root_page_id, "HNSW root")?;
    let root = HnswRoot::decode(&root_page)?;
    let payload = read_layer_directory_payload(pager, &root)?;
    let directory = LayerDirectory::decode(&payload, &root)?;
    Ok(PersistedHnswIndex {
        root_page_id,
        root,
        directory,
    })
}

/// Publishes reachable deltas into fresh CSR groups, a directory, and a root.
///
/// Deltas must be oldest-first. Mandatory merge budget failure returns
/// `BudgetExceeded`; all pages written by the attempt remain unreachable and
/// the old root remains readable.
pub fn publish_checkpoint(
    pager: &Pager,
    budget: &MemoryBudget,
    config: &HnswConfig,
    base_root_page: Option<u64>,
    deltas: &[HnswDelta],
    groups: &HnswNodeGroups,
) -> DevonResult<CheckpointPublication> {
    let mut state = prepare_checkpoint(pager, budget, config, base_root_page, deltas, groups)?;
    let index = state.publish(pager, budget, config, groups)?;
    Ok(CheckpointPublication {
        index,
        catch_up: CatchUpStatus::NotRequested,
    })
}

/// Publishes deltas and optionally advances an existing recovery tail.
///
/// Catch-up inserts rows one at a time through [`propose_insert`]. Budget
/// pressure while proposing or merging the next row stops at the last
/// contiguous success and still publishes the checkpoint.
#[allow(clippy::too_many_arguments)]
pub fn publish_checkpoint_with_catch_up<V>(
    pager: &Pager,
    budget: &MemoryBudget,
    config: &HnswConfig,
    base_root_page: Option<u64>,
    deltas: &[HnswDelta],
    groups: &HnswNodeGroups,
    target_rows: u64,
    vectors: &V,
) -> DevonResult<CheckpointPublication>
where
    V: ConstructionVectorAccess,
{
    validate_column(config, vectors.column_type())?;
    if target_rows > groups.total_rows() {
        return Err(invalid_argument(format!(
            "HNSW catch-up target {target_rows} exceeds visible rows {}",
            groups.total_rows()
        )));
    }
    let mut state = prepare_checkpoint(pager, budget, config, base_root_page, deltas, groups)?;
    if target_rows < state.covered_rows {
        return Err(invalid_argument(format!(
            "HNSW catch-up target {target_rows} is before coverage {}",
            state.covered_rows
        )));
    }
    state.reserve_publication(pager, budget, groups)?;
    let catch_up = catch_up_tail(
        pager,
        budget,
        config,
        deltas,
        groups,
        target_rows,
        vectors,
        &mut state,
    )?;
    let index = state.publish(pager, budget, config, groups)?;
    Ok(CheckpointPublication { index, catch_up })
}

/// Builds an unreachable index over `0..prefix` in bounded row batches.
///
/// Each batch uses the same insertion proposal path and is flushed to fresh
/// immutable pages before the next batch. Newly written cells, directories and
/// roots enter the prospective queue; catalog save protects the final subtree
/// and retires the unused intermediate pages.
#[allow(clippy::too_many_arguments)]
pub fn build_initial_index<V>(
    pager: &Pager,
    budget: &MemoryBudget,
    config: &HnswConfig,
    groups: &HnswNodeGroups,
    prefix: u64,
    batch_rows: usize,
    vectors: &V,
) -> DevonResult<PersistedHnswIndex>
where
    V: ConstructionVectorAccess,
{
    config.validate()?;
    validate_column(config, vectors.column_type())?;
    if prefix > groups.total_rows() {
        return Err(invalid_argument(format!(
            "HNSW build prefix {prefix} exceeds visible rows {}",
            groups.total_rows()
        )));
    }
    if batch_rows == 0 {
        return Err(invalid_argument("HNSW initial-build batch size is zero"));
    }
    build_batches(pager, budget, config, groups, prefix, batch_rows, vectors)
}

/// Builds a fresh valid prefix, reducing batches under pressure and leaving an
/// exact tail if a single insertion cannot fit. Published old roots are never
/// used: every intermediate publication here is prospective until catalog save.
#[allow(clippy::too_many_arguments)]
pub fn build_initial_index_partial<V: ConstructionVectorAccess>(
    pager: &Pager,
    budget: &MemoryBudget,
    config: &HnswConfig,
    groups: &HnswNodeGroups,
    prefix: u64,
    batch_rows: usize,
    vectors: &V,
) -> DevonResult<PersistedHnswIndex> {
    config.validate()?;
    validate_column(config, vectors.column_type())?;
    if prefix > groups.total_rows() || batch_rows == 0 {
        return Err(invalid_argument("invalid HNSW partial build bounds"));
    }
    let mut publication = publish_checkpoint(pager, budget, config, None, &[], groups)?.index;
    let mut batch = batch_rows;
    loop {
        let start = publication.root.covered_rows;
        if start == prefix {
            return Ok(publication);
        }
        let end = start.saturating_add(batch as u64).min(prefix);
        match build_one_batch(
            pager,
            budget,
            config,
            groups,
            &publication,
            start..end,
            batch,
            vectors,
        ) {
            Ok(next) => publication = next,
            Err(DevonError::BudgetExceeded { .. }) if batch > 1 => batch = (batch / 2).max(1),
            Err(DevonError::BudgetExceeded { .. }) => return Ok(publication),
            Err(error) => return Err(error),
        }
    }
}

/// Enumerates the immutable root, contiguous layer directory and complete CSR
/// subtrees. Errors prevent a catalog publication from retiring uncertain pages.
pub fn index_page_inventory(pager: &Pager, root: u64) -> DevonResult<BTreeSet<u64>> {
    let index = load_persisted_index(pager, root)?;
    let mut pages = BTreeSet::from([root]);
    let count =
        u64::from(index.root.layer_dir_byte_len).div_ceil(u64::from(pager.superblock().page_size));
    for offset in 0..count {
        pages.insert(
            index
                .root
                .layer_dir_first_page
                .checked_add(offset)
                .ok_or_else(|| corrupt("HNSW directory page range overflow"))?,
        );
    }
    for cell in index
        .directory
        .page_ids()
        .iter()
        .copied()
        .filter(|id| *id != 0)
    {
        pages.extend(crate::csr_group::csr_page_inventory(pager, cell, &[])?);
    }
    Ok(pages)
}

fn build_batches<V>(
    pager: &Pager,
    budget: &MemoryBudget,
    config: &HnswConfig,
    groups: &HnswNodeGroups,
    prefix: u64,
    batch_rows: usize,
    vectors: &V,
) -> DevonResult<PersistedHnswIndex>
where
    V: ConstructionVectorAccess,
{
    let mut publication = publish_checkpoint(pager, budget, config, None, &[], groups)?.index;
    while publication.root.covered_rows < prefix {
        let start = publication.root.covered_rows;
        let batch = u64::try_from(batch_rows)
            .map_err(|_| invalid_argument("HNSW batch size exceeds u64"))?;
        let end = start.saturating_add(batch).min(prefix);
        publication = build_one_batch(
            pager,
            budget,
            config,
            groups,
            &publication,
            start..end,
            batch_rows,
            vectors,
        )?;
    }
    Ok(publication)
}

#[allow(clippy::too_many_arguments)]
fn build_one_batch<V>(
    pager: &Pager,
    budget: &MemoryBudget,
    config: &HnswConfig,
    groups: &HnswNodeGroups,
    base: &PersistedHnswIndex,
    rows: Range<u64>,
    batch_rows: usize,
    vectors: &V,
) -> DevonResult<PersistedHnswIndex>
where
    V: ConstructionVectorAccess,
{
    let verified_capacity = build_verified_capacity(&base.root, config, batch_rows)?;
    let layout = groups.view_layout()?;
    let view = HnswSnapshotView::new(
        pager,
        budget,
        base.root,
        base.directory.clone(),
        &[],
        groups.total_rows(),
        layout,
        verified_capacity,
    )?;
    let proposal = match propose_insert(&view, config, rows, vectors, budget)? {
        InsertProposalOutcome::Proposed(proposal) => proposal,
        InsertProposalOutcome::BudgetExceeded => {
            return Err(budget_exceeded(
                "HNSW initial-build proposal exceeded its budget",
            ));
        }
        InsertProposalOutcome::TailExists => {
            return Err(corrupt(
                "HNSW initial build unexpectedly found an exact tail",
            ));
        }
    };
    let publication = publish_checkpoint(
        pager,
        budget,
        config,
        Some(base.root_page_id),
        std::slice::from_ref(proposal.delta()),
        groups,
    )?;
    Ok(publication.index)
}

fn build_verified_capacity(
    root: &HnswRoot,
    config: &HnswConfig,
    batch_rows: usize,
) -> DevonResult<usize> {
    let per_search = construction_discovery_bound(root.covered_rows, root.layer_count, config)?;
    let cells = usize::from(root.layer_count)
        .checked_mul(root.group_count as usize)
        .ok_or_else(|| invalid_argument("HNSW verified-cell count overflows usize"))?;
    per_search
        .checked_mul(batch_rows)
        .map(|bound| cells.min(bound))
        .ok_or_else(|| invalid_argument("HNSW build verified-group bound overflows usize"))
}

struct CheckpointState<'budget> {
    base_root: HnswRoot,
    base_directory: LayerDirectory,
    directory: DirectoryState,
    entry: Option<(u64, u8)>,
    layer_count: u8,
    covered_rows: u64,
    overlay: OverlayState,
    catch_up_charges: Vec<ChargedBytes<'budget>>,
    publication_charge: Option<ChargedBytes<'budget>>,
}

impl<'budget> CheckpointState<'budget> {
    fn publish(
        &mut self,
        pager: &Pager,
        budget: &MemoryBudget,
        config: &HnswConfig,
        groups: &HnswNodeGroups,
    ) -> DevonResult<PersistedHnswIndex> {
        let group_count = groups.groups_intersecting(self.covered_rows)?;
        self.directory = self.directory.expanded(self.layer_count, group_count)?;
        let directory =
            LayerDirectory::new(self.layer_count, group_count, self.directory.cells.clone())?;
        if self.publication_charge.is_some() {
            return persist_publication_reserved(
                pager,
                config,
                self.covered_rows,
                self.entry,
                directory,
            );
        }
        persist_publication(
            pager,
            budget,
            config,
            self.covered_rows,
            self.entry,
            directory,
        )
    }

    fn reserve_publication(
        &mut self,
        pager: &Pager,
        budget: &'budget MemoryBudget,
        groups: &HnswNodeGroups,
    ) -> DevonResult<()> {
        let group_count = groups.groups_intersecting(self.covered_rows)?;
        let payload_len = directory_payload_len(self.layer_count, group_count)?;
        let bytes = publication_reservation(pager, payload_len)?;
        self.publication_charge = Some(ChargedBytes::try_new(budget, bytes, || {
            format!("HNSW checkpoint publication reservation of {bytes} bytes")
        })?);
        Ok(())
    }

    fn grow_publication_reservation(
        &mut self,
        pager: &Pager,
        layer_count: u8,
        group_count: u32,
    ) -> DevonResult<()> {
        let payload_len = directory_payload_len(layer_count, group_count)?;
        let needed = publication_reservation(pager, payload_len)?;
        let charge = self
            .publication_charge
            .as_mut()
            .ok_or_else(|| corrupt("HNSW catch-up publication is not reserved"))?;
        if needed > charge.bytes() {
            let additional = needed - charge.bytes();
            charge.grow(additional, || {
                format!("HNSW catch-up publication growth of {additional} bytes")
            })?;
        }
        Ok(())
    }
}

#[derive(Default)]
struct OverlayState {
    replacements: BTreeMap<(u8, u64), Arc<[u64]>>,
    entry: Option<(u64, u8)>,
    layer_count: u8,
    covered_rows: u64,
}

impl OverlayState {
    fn apply(&mut self, delta: HnswDelta) {
        self.covered_rows = delta.new_covered_rows;
        if let Some(entry) = delta.entry {
            self.entry = Some(entry);
            self.layer_count = entry.1 + 1;
        }
        for (slot, replacement) in delta.replacements {
            self.replacements.insert(slot, replacement);
        }
    }
}

#[derive(Clone)]
struct DirectoryState {
    layer_count: u8,
    group_count: u32,
    cells: Vec<u64>,
}

impl DirectoryState {
    fn from_base(directory: &LayerDirectory) -> Self {
        Self {
            layer_count: directory.layer_count(),
            group_count: directory.group_count(),
            cells: directory.page_ids().to_vec(),
        }
    }

    fn expanded(&self, layer_count: u8, group_count: u32) -> DevonResult<Self> {
        if layer_count < self.layer_count || group_count < self.group_count {
            return Err(corrupt(
                "HNSW checkpoint directory dimensions moved backwards",
            ));
        }
        let len = usize::from(layer_count)
            .checked_mul(group_count as usize)
            .ok_or_else(|| invalid_argument("HNSW directory dimensions overflow usize"))?;
        let mut cells = vec![0_u64; len];
        for layer in 0..self.layer_count {
            for group in 0..self.group_count {
                let old = self.index(layer, group);
                let new = usize::from(layer) * group_count as usize + group as usize;
                cells[new] = self.cells[old];
            }
        }
        Ok(Self {
            layer_count,
            group_count,
            cells,
        })
    }

    fn set(&mut self, layer: u8, group: u32, page_id: u64) -> DevonResult<()> {
        if layer >= self.layer_count || group >= self.group_count {
            return Err(invalid_argument("HNSW directory update is out of bounds"));
        }
        let index = self.index(layer, group);
        self.cells[index] = page_id;
        Ok(())
    }

    fn index(&self, layer: u8, group: u32) -> usize {
        usize::from(layer) * self.group_count as usize + group as usize
    }
}

fn prepare_checkpoint<'budget>(
    pager: &Pager,
    budget: &'budget MemoryBudget,
    config: &HnswConfig,
    base_root_page: Option<u64>,
    deltas: &[HnswDelta],
    groups: &HnswNodeGroups,
) -> DevonResult<CheckpointState<'budget>> {
    config.validate()?;
    let base = load_or_empty_base(pager, config, base_root_page)?;
    validate_base_config(config, &base.root)?;
    let metadata = effective_metadata(pager, budget, &base, deltas, groups, 0)?;
    let group_count = groups.groups_intersecting(metadata.covered_rows)?;
    let mut directory =
        DirectoryState::from_base(&base.directory).expanded(metadata.layer_count, group_count)?;
    let cells = changed_cells(
        &base.root,
        deltas,
        groups,
        metadata.covered_rows,
        metadata.layer_count,
    )?;
    merge_mandatory_cells(
        pager,
        budget,
        config,
        &base,
        deltas,
        groups,
        metadata.covered_rows,
        &cells,
        &mut directory,
    )?;
    Ok(CheckpointState {
        base_root: base.root,
        base_directory: base.directory,
        directory,
        entry: metadata.entry,
        layer_count: metadata.layer_count,
        covered_rows: metadata.covered_rows,
        overlay: OverlayState {
            entry: metadata.entry,
            layer_count: metadata.layer_count,
            covered_rows: metadata.covered_rows,
            ..OverlayState::default()
        },
        catch_up_charges: Vec::new(),
        publication_charge: None,
    })
}

fn load_or_empty_base(
    pager: &Pager,
    config: &HnswConfig,
    root_page: Option<u64>,
) -> DevonResult<PersistedHnswIndex> {
    if let Some(root_page) = root_page {
        return load_persisted_index(pager, root_page);
    }
    let root = empty_root(*config);
    let directory = LayerDirectory::new(0, 0, Vec::new())?;
    Ok(PersistedHnswIndex {
        root_page_id: 0,
        root,
        directory,
    })
}

fn empty_root(config: HnswConfig) -> HnswRoot {
    HnswRoot {
        config,
        covered_rows: 0,
        entry_node: None,
        entry_level: 0,
        layer_count: 0,
        group_count: 0,
        layer_dir_first_page: 0,
        layer_dir_byte_len: 0,
        layer_dir_crc32c: 0,
    }
}

fn validate_base_config(config: &HnswConfig, root: &HnswRoot) -> DevonResult<()> {
    if root.config != *config {
        return Err(corrupt(
            "HNSW checkpoint config does not match the base root",
        ));
    }
    Ok(())
}

#[derive(Clone, Copy)]
struct EffectiveMetadata {
    entry: Option<(u64, u8)>,
    layer_count: u8,
    covered_rows: u64,
}

fn effective_metadata(
    pager: &Pager,
    budget: &MemoryBudget,
    base: &PersistedHnswIndex,
    deltas: &[HnswDelta],
    groups: &HnswNodeGroups,
    verified_capacity: usize,
) -> DevonResult<EffectiveMetadata> {
    let layout = groups.view_layout()?;
    let view = HnswSnapshotView::new(
        pager,
        budget,
        base.root,
        base.directory.clone(),
        deltas,
        groups.total_rows(),
        layout,
        verified_capacity,
    )?;
    Ok(EffectiveMetadata {
        entry: view.entry(),
        layer_count: view.layer_count(),
        covered_rows: view.covered_rows(),
    })
}

fn changed_cells(
    base: &HnswRoot,
    deltas: &[HnswDelta],
    groups: &HnswNodeGroups,
    covered_rows: u64,
    layer_count: u8,
) -> DevonResult<BTreeSet<(u8, u32)>> {
    let mut cells = BTreeSet::new();
    for delta in deltas {
        for &(layer, node) in delta.replacements.keys() {
            let (group, _) = groups.locate(node)?;
            cells.insert((layer, group));
        }
    }
    add_coverage_changed_cells(
        &mut cells,
        groups,
        base.covered_rows,
        covered_rows,
        layer_count,
    )?;
    Ok(cells)
}

fn add_coverage_changed_cells(
    cells: &mut BTreeSet<(u8, u32)>,
    groups: &HnswNodeGroups,
    old_covered_rows: u64,
    new_covered_rows: u64,
    layer_count: u8,
) -> DevonResult<()> {
    let group_count = groups.groups_intersecting(new_covered_rows)?;
    for group in 0..group_count {
        let old = groups.covered_in_group(group, old_covered_rows)?;
        let new = groups.covered_in_group(group, new_covered_rows)?;
        if old != new {
            for layer in 0..layer_count {
                cells.insert((layer, group));
            }
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn merge_mandatory_cells(
    pager: &Pager,
    budget: &MemoryBudget,
    config: &HnswConfig,
    base: &PersistedHnswIndex,
    deltas: &[HnswDelta],
    groups: &HnswNodeGroups,
    covered_rows: u64,
    cells: &BTreeSet<(u8, u32)>,
    directory: &mut DirectoryState,
) -> DevonResult<()> {
    for &(layer, group) in cells {
        let layout = groups.view_layout()?;
        let view = HnswSnapshotView::new(
            pager,
            budget,
            base.root,
            base.directory.clone(),
            deltas,
            groups.total_rows(),
            layout,
            1,
        )?;
        let page_id = merge_one_cell(
            pager,
            budget,
            config,
            &view,
            groups,
            covered_rows,
            layer,
            group,
        )?;
        directory.set(layer, group, page_id)?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn merge_one_cell<G>(
    pager: &Pager,
    budget: &MemoryBudget,
    config: &HnswConfig,
    graph: &G,
    groups: &HnswNodeGroups,
    covered_rows: u64,
    layer: u8,
    group: u32,
) -> DevonResult<u64>
where
    G: GraphAccess,
{
    let row_count = groups.covered_in_group(group, covered_rows)?;
    if row_count == 0 {
        return Ok(0);
    }
    let group_index = group as usize;
    let group_start = groups.starts[group_index];
    let cap = degree_cap(config, layer);
    let scratch_bytes = allocation_bytes::<u64>(cap, "HNSW checkpoint adjacency scratch")?;
    let _scratch_charge = ChargedBytes::try_new(budget, scratch_bytes, || {
        format!("HNSW checkpoint adjacency reservation of {scratch_bytes} bytes")
    })?;
    let mut scratch = Vec::new();
    scratch.try_reserve_exact(cap).map_err(|error| {
        budget_exceeded(format!(
            "failed to allocate checkpoint adjacency scratch: {error}"
        ))
    })?;
    let edge_count = count_cell_edges(
        graph,
        layer,
        group_start,
        row_count,
        covered_rows,
        cap,
        &mut scratch,
    )?;
    if edge_count == 0 {
        return Ok(0);
    }
    let builder_bytes = csr_builder_reservation(pager, row_count, edge_count)?;
    let _builder_charge = ChargedBytes::try_new(budget, builder_bytes, || {
        format!("HNSW checkpoint CSR builder reservation of {builder_bytes} bytes")
    })?;
    write_cell_group(
        pager,
        graph,
        layer,
        group_start,
        row_count,
        covered_rows,
        cap,
        &mut scratch,
    )
}

#[allow(clippy::too_many_arguments)]
fn count_cell_edges<G: GraphAccess>(
    graph: &G,
    layer: u8,
    group_start: u64,
    row_count: usize,
    covered_rows: u64,
    cap: usize,
    scratch: &mut Vec<u64>,
) -> DevonResult<usize> {
    let mut edge_count = 0_usize;
    for slot in 0..row_count {
        let node = group_start
            .checked_add(slot as u64)
            .ok_or_else(|| invalid_argument("HNSW group node offset overflows u64"))?;
        graph.neighbors(layer, node, scratch)?;
        validate_checkpoint_adjacency(node, layer, scratch, covered_rows, cap)?;
        edge_count = edge_count
            .checked_add(scratch.len())
            .ok_or_else(|| invalid_argument("HNSW checkpoint edge count overflows usize"))?;
    }
    Ok(edge_count)
}

#[allow(clippy::too_many_arguments)]
fn write_cell_group<G: GraphAccess>(
    pager: &Pager,
    graph: &G,
    layer: u8,
    group_start: u64,
    row_count: usize,
    covered_rows: u64,
    cap: usize,
    scratch: &mut Vec<u64>,
) -> DevonResult<u64> {
    let mut csr = CsrGroup::new(row_count, Vec::new())?;
    for slot in 0..row_count {
        let node = group_start
            .checked_add(slot as u64)
            .ok_or_else(|| invalid_argument("HNSW group node offset overflows u64"))?;
        graph.neighbors(layer, node, scratch)?;
        validate_checkpoint_adjacency(node, layer, scratch, covered_rows, cap)?;
        for neighbor in scratch.iter().copied() {
            csr.push_edge(slot, neighbor, Vec::new())?;
        }
    }
    let page = csr.write(pager)?;
    // Queue each successfully written cell before later cells/publication can
    // fail. This never includes the base's immutable shared cells.
    crate::free_pages::queue_prospective_pages(
        pager.superblock().db_id,
        crate::csr_group::csr_page_inventory(pager, page, &[])?,
    );
    Ok(page)
}

fn validate_checkpoint_adjacency(
    node: u64,
    layer: u8,
    neighbors: &[u64],
    covered_rows: u64,
    degree_cap: usize,
) -> DevonResult<()> {
    if neighbors.len() > degree_cap {
        return Err(corrupt(format!(
            "HNSW checkpoint node {node} layer {layer} adjacency degree {} exceeds layer cap {degree_cap}",
            neighbors.len()
        )));
    }
    for (index, &neighbor) in neighbors.iter().enumerate() {
        if neighbor >= covered_rows {
            return Err(corrupt(format!(
                "HNSW checkpoint node {node} layer {layer} neighbor {neighbor} is outside covered_rows {covered_rows}"
            )));
        }
        if neighbor == node {
            return Err(corrupt(format!(
                "HNSW checkpoint node {node} layer {layer} has a self-neighbor"
            )));
        }
        if neighbors[..index].contains(&neighbor) {
            return Err(corrupt(format!(
                "HNSW checkpoint node {node} layer {layer} has duplicate neighbor {neighbor}"
            )));
        }
    }
    Ok(())
}

fn csr_builder_reservation(
    pager: &Pager,
    row_count: usize,
    edge_count: usize,
) -> DevonResult<usize> {
    let offsets = row_count
        .checked_add(1)
        .and_then(|count| count.checked_mul(size_of::<u32>()))
        .ok_or_else(|| invalid_argument("HNSW CSR offsets reservation overflows"))?;
    let neighbor_capacity = edge_count
        .checked_next_power_of_two()
        .ok_or_else(|| invalid_argument("HNSW CSR neighbor capacity overflows"))?;
    let neighbors = neighbor_capacity
        .checked_mul(size_of::<u64>())
        .ok_or_else(|| invalid_argument("HNSW CSR neighbors reservation overflows"))?;
    let raw = offsets
        .checked_add(neighbors)
        .ok_or_else(|| invalid_argument("HNSW CSR raw reservation overflows"))?;
    raw.checked_mul(4)
        .and_then(|bytes| bytes.checked_add(2 * pager.superblock().page_size as usize))
        .and_then(|bytes| bytes.checked_add(8 * ALLOCATION_OVERHEAD))
        .ok_or_else(|| invalid_argument("HNSW CSR builder reservation overflows"))
}

fn degree_cap(config: &HnswConfig, layer: u8) -> usize {
    usize::from(if layer == 0 { config.m0 } else { config.m })
}

fn persist_publication(
    pager: &Pager,
    budget: &MemoryBudget,
    config: &HnswConfig,
    covered_rows: u64,
    entry: Option<(u64, u8)>,
    directory: LayerDirectory,
) -> DevonResult<PersistedHnswIndex> {
    let payload = directory.encode();
    let bytes = publication_reservation(pager, payload.len())?;
    let _charge = ChargedBytes::try_new(budget, bytes, || {
        format!("HNSW directory/root publication reservation of {bytes} bytes")
    })?;
    persist_publication_reserved(pager, config, covered_rows, entry, directory)
}

fn persist_publication_reserved(
    pager: &Pager,
    config: &HnswConfig,
    covered_rows: u64,
    entry: Option<(u64, u8)>,
    directory: LayerDirectory,
) -> DevonResult<PersistedHnswIndex> {
    let payload = directory.encode();
    let first_page = write_contiguous_payload(pager, &payload)?;
    let (entry_node, entry_level) = entry.map_or((None, 0), |(node, level)| (Some(node), level));
    let root = HnswRoot {
        config: *config,
        covered_rows,
        entry_node,
        entry_level,
        layer_count: directory.layer_count(),
        group_count: directory.group_count(),
        layer_dir_first_page: first_page,
        layer_dir_byte_len: directory.byte_len(),
        layer_dir_crc32c: directory.crc32c(),
    };
    let root_page_id = write_root(pager, &root)?;
    pager.sync()?;
    Ok(PersistedHnswIndex {
        root_page_id,
        root,
        directory,
    })
}

fn directory_payload_len(layer_count: u8, group_count: u32) -> DevonResult<usize> {
    usize::from(layer_count)
        .checked_mul(group_count as usize)
        .and_then(|cells| cells.checked_mul(size_of::<u64>()))
        .ok_or_else(|| invalid_argument("HNSW directory payload length overflows usize"))
}

fn publication_reservation(pager: &Pager, payload_len: usize) -> DevonResult<usize> {
    payload_len
        .checked_add(2 * pager.superblock().page_size as usize)
        .and_then(|bytes| bytes.checked_add(4 * ALLOCATION_OVERHEAD))
        .ok_or_else(|| invalid_argument("HNSW publication reservation overflows"))
}

fn write_contiguous_payload(pager: &Pager, payload: &[u8]) -> DevonResult<u64> {
    if payload.is_empty() {
        return Ok(0);
    }
    let page_size = pager.superblock().page_size as usize;
    let page_count = payload.len().div_ceil(page_size);
    // One atomic run allocation: the pager guarantees contiguity whether
    // the run is reused from the free-page ledger or appended.
    let first_page = pager.allocate_run(page_count)?;
    crate::free_pages::queue_prospective_pages(
        pager.superblock().db_id,
        (0..page_count).map(|offset| first_page + offset as u64),
    );
    for page_index in 0..page_count {
        let page_id = first_page
            .checked_add(page_index as u64)
            .ok_or_else(|| invalid_argument("HNSW directory page run overflows u64"))?;
        let start = page_index * page_size;
        let end = payload.len().min(start + page_size);
        let mut page = vec![0_u8; page_size];
        page[..end - start].copy_from_slice(&payload[start..end]);
        pager.write_page(page_id, &page)?;
    }
    Ok(first_page)
}

fn write_root(pager: &Pager, root: &HnswRoot) -> DevonResult<u64> {
    let page_id = pager.allocate_page()?;
    crate::free_pages::queue_prospective_pages(pager.superblock().db_id, [page_id]);
    let mut page = vec![0_u8; pager.superblock().page_size as usize];
    root.encode(&mut page)?;
    pager.write_page(page_id, &page)?;
    Ok(page_id)
}

#[allow(clippy::too_many_arguments)]
fn catch_up_tail<'budget, V>(
    pager: &Pager,
    budget: &'budget MemoryBudget,
    config: &HnswConfig,
    mandatory_deltas: &[HnswDelta],
    groups: &HnswNodeGroups,
    target_rows: u64,
    vectors: &V,
    state: &mut CheckpointState<'budget>,
) -> DevonResult<CatchUpStatus>
where
    V: ConstructionVectorAccess,
{
    while state.covered_rows < target_rows {
        let proposal = propose_catch_up_row(
            pager,
            budget,
            config,
            mandatory_deltas,
            groups,
            vectors,
            state,
        )?;
        let Some(proposal) = proposal else {
            return Ok(CatchUpStatus::StoppedAtBudget(state.covered_rows));
        };
        if !merge_catch_up_row(
            pager,
            budget,
            config,
            mandatory_deltas,
            groups,
            state,
            proposal,
        )? {
            return Ok(CatchUpStatus::StoppedAtBudget(state.covered_rows));
        }
    }
    Ok(CatchUpStatus::Complete)
}

#[allow(clippy::too_many_arguments)]
fn propose_catch_up_row<'budget, V>(
    pager: &Pager,
    budget: &'budget MemoryBudget,
    config: &HnswConfig,
    mandatory_deltas: &[HnswDelta],
    groups: &HnswNodeGroups,
    vectors: &V,
    state: &CheckpointState<'budget>,
) -> DevonResult<Option<ChargedHnswDelta<'budget>>>
where
    V: ConstructionVectorAccess,
{
    let capacity = catch_up_verified_capacity(&state.base_root, state, config)?;
    let view = checkpoint_view(pager, budget, state, mandatory_deltas, groups, capacity)?;
    let graph = OverlayGraph {
        base: &view,
        overlay: &state.overlay,
    };
    let row = state.covered_rows;
    match propose_insert(&graph, config, row..row + 1, vectors, budget)? {
        InsertProposalOutcome::Proposed(proposal) => Ok(Some(proposal)),
        InsertProposalOutcome::BudgetExceeded => Ok(None),
        InsertProposalOutcome::TailExists => Err(corrupt(
            "HNSW checkpoint catch-up unexpectedly found an older tail",
        )),
    }
}

fn catch_up_verified_capacity(
    base_root: &HnswRoot,
    state: &CheckpointState<'_>,
    config: &HnswConfig,
) -> DevonResult<usize> {
    let discovery = construction_discovery_bound(state.covered_rows, state.layer_count, config)?;
    let base_cells = usize::from(base_root.layer_count)
        .checked_mul(base_root.group_count as usize)
        .ok_or_else(|| invalid_argument("HNSW base cell count overflows usize"))?;
    Ok(discovery.min(base_cells))
}

#[allow(clippy::too_many_arguments)]
fn merge_catch_up_row<'budget>(
    pager: &Pager,
    budget: &'budget MemoryBudget,
    config: &HnswConfig,
    mandatory_deltas: &[HnswDelta],
    groups: &HnswNodeGroups,
    state: &mut CheckpointState<'budget>,
    proposal: ChargedHnswDelta<'budget>,
) -> DevonResult<bool> {
    let candidate = proposal.delta();
    let layer_count = candidate
        .entry
        .map_or(state.layer_count, |(_, level)| level + 1);
    let group_count = groups.groups_intersecting(candidate.new_covered_rows)?;
    match state.grow_publication_reservation(pager, layer_count, group_count) {
        Ok(()) => {}
        Err(DevonError::BudgetExceeded { .. }) => return Ok(false),
        Err(error) => return Err(error),
    }
    let mut directory = state.directory.expanded(layer_count, group_count)?;
    let cells = delta_changed_cells(candidate, groups, state.covered_rows, layer_count)?;
    for &(layer, group) in &cells {
        match merge_catch_up_cell(
            pager,
            budget,
            config,
            mandatory_deltas,
            groups,
            state,
            candidate,
            layer,
            group,
        ) {
            Ok(page_id) => directory.set(layer, group, page_id)?,
            Err(DevonError::BudgetExceeded { .. }) => return Ok(false),
            Err(error) => return Err(error),
        }
    }
    let (delta, charge) = proposal.into_parts();
    state.overlay.apply(delta);
    state.entry = state.overlay.entry;
    state.layer_count = state.overlay.layer_count;
    state.covered_rows = state.overlay.covered_rows;
    state.directory = directory;
    state.catch_up_charges.push(charge);
    Ok(true)
}

fn delta_changed_cells(
    delta: &HnswDelta,
    groups: &HnswNodeGroups,
    old_covered_rows: u64,
    layer_count: u8,
) -> DevonResult<BTreeSet<(u8, u32)>> {
    let mut cells = BTreeSet::new();
    for &(layer, node) in delta.replacements.keys() {
        cells.insert((layer, groups.locate(node)?.0));
    }
    add_coverage_changed_cells(
        &mut cells,
        groups,
        old_covered_rows,
        delta.new_covered_rows,
        layer_count,
    )?;
    Ok(cells)
}

#[allow(clippy::too_many_arguments)]
fn merge_catch_up_cell(
    pager: &Pager,
    budget: &MemoryBudget,
    config: &HnswConfig,
    mandatory_deltas: &[HnswDelta],
    groups: &HnswNodeGroups,
    state: &CheckpointState<'_>,
    candidate: &HnswDelta,
    layer: u8,
    group: u32,
) -> DevonResult<u64> {
    let view = checkpoint_view(pager, budget, state, mandatory_deltas, groups, 1)?;
    let accepted = OverlayGraph {
        base: &view,
        overlay: &state.overlay,
    };
    let graph = DeltaGraph {
        base: &accepted,
        delta: candidate,
    };
    merge_one_cell(
        pager,
        budget,
        config,
        &graph,
        groups,
        candidate.new_covered_rows,
        layer,
        group,
    )
}

fn checkpoint_view<'view>(
    pager: &'view Pager,
    budget: &'view MemoryBudget,
    state: &CheckpointState<'_>,
    deltas: &'view [HnswDelta],
    groups: &HnswNodeGroups,
    capacity: usize,
) -> DevonResult<HnswSnapshotView<'view>> {
    HnswSnapshotView::new(
        pager,
        budget,
        state.base_root,
        state.base_directory.clone(),
        deltas,
        groups.total_rows(),
        groups.view_layout()?,
        capacity,
    )
}

struct OverlayGraph<'graph, G> {
    base: &'graph G,
    overlay: &'graph OverlayState,
}

impl<G: GraphAccess> GraphAccess for OverlayGraph<'_, G> {
    fn entry(&self) -> Option<(u64, u8)> {
        self.overlay.entry
    }

    fn layer_count(&self) -> u8 {
        self.overlay.layer_count
    }

    fn covered_rows(&self) -> u64 {
        self.overlay.covered_rows
    }

    fn neighbors(&self, layer: u8, node: u64, scratch: &mut Vec<u64>) -> DevonResult<()> {
        scratch.clear();
        validate_graph_lookup(self, layer, node)?;
        if let Some(replacement) = self.overlay.replacements.get(&(layer, node)) {
            scratch.extend_from_slice(replacement);
        } else if layer < self.base.layer_count() && node < self.base.covered_rows() {
            self.base.neighbors(layer, node, scratch)?;
        }
        Ok(())
    }
}

struct DeltaGraph<'graph, G> {
    base: &'graph G,
    delta: &'graph HnswDelta,
}

impl<G: GraphAccess> GraphAccess for DeltaGraph<'_, G> {
    fn entry(&self) -> Option<(u64, u8)> {
        self.delta.entry.or_else(|| self.base.entry())
    }

    fn layer_count(&self) -> u8 {
        self.delta
            .entry
            .map_or_else(|| self.base.layer_count(), |(_, level)| level + 1)
    }

    fn covered_rows(&self) -> u64 {
        self.delta.new_covered_rows
    }

    fn neighbors(&self, layer: u8, node: u64, scratch: &mut Vec<u64>) -> DevonResult<()> {
        scratch.clear();
        validate_graph_lookup(self, layer, node)?;
        if let Some(replacement) = self.delta.replacements.get(&(layer, node)) {
            scratch.extend_from_slice(replacement);
        } else if layer < self.base.layer_count() && node < self.base.covered_rows() {
            self.base.neighbors(layer, node, scratch)?;
        }
        Ok(())
    }
}

fn construction_discovery_bound(
    covered_rows: u64,
    layer_count: u8,
    config: &HnswConfig,
) -> DevonResult<usize> {
    let width = u64::from(config.ef_construction);
    let layer_zero = width
        .checked_mul(u64::from(config.m0) + 1)
        .ok_or_else(|| invalid_argument("HNSW construction discovery bound overflows"))?;
    let upper = u64::from(layer_count)
        .checked_mul(u64::from(config.m) + 1)
        .ok_or_else(|| invalid_argument("HNSW construction upper bound overflows"))?;
    let bound = layer_zero
        .checked_add(upper)
        .ok_or_else(|| invalid_argument("HNSW construction discovery bound overflows"))?;
    usize::try_from(covered_rows.min(bound))
        .map_err(|_| invalid_argument("HNSW construction discovery bound exceeds usize"))
}

fn read_layer_directory_payload(pager: &Pager, root: &HnswRoot) -> DevonResult<Vec<u8>> {
    let byte_len = root.layer_dir_byte_len as usize;
    if byte_len == 0 {
        return Ok(Vec::new());
    }
    let page_size = pager.superblock().page_size as usize;
    let page_count = byte_len.div_ceil(page_size);
    let mut payload = Vec::with_capacity(byte_len);
    for page_index in 0..page_count {
        let offset = u64::try_from(page_index)
            .map_err(|_| corrupt("HNSW directory page index exceeds u64"))?;
        let page_id = root
            .layer_dir_first_page
            .checked_add(offset)
            .ok_or_else(|| corrupt("HNSW directory page run overflows u64"))?;
        let page = read_index_page(pager, page_id, "HNSW layer directory")?;
        let take = (byte_len - payload.len()).min(page_size);
        payload.extend_from_slice(&page[..take]);
        if take < page_size && page[take..].iter().any(|byte| *byte != 0) {
            return Err(corrupt("HNSW layer-directory final-page tail is not zero"));
        }
    }
    Ok(payload)
}

fn read_index_page(pager: &Pager, page_id: u64, name: &str) -> DevonResult<Vec<u8>> {
    match pager.read_page(page_id) {
        Ok(page) => Ok(page),
        Err(DevonError::InvalidArgument { context }) => Err(corrupt(format!(
            "{name} references invalid page {page_id}: {context}"
        ))),
        Err(error) => Err(error),
    }
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

fn replacement_charge(degree: usize) -> DevonResult<usize> {
    degree
        .checked_mul(size_of::<u64>())
        .and_then(|payload| payload.checked_add(ALLOCATION_OVERHEAD))
        .ok_or_else(|| invalid_argument("HNSW proposal replacement charge overflows"))
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

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;
    use crate::hnsw::types::HnswMetric;

    struct OverCapGraph;

    impl GraphAccess for OverCapGraph {
        fn entry(&self) -> Option<(u64, u8)> {
            Some((0, 0))
        }

        fn layer_count(&self) -> u8 {
            1
        }

        fn covered_rows(&self) -> u64 {
            10
        }

        fn neighbors(&self, _: u8, node: u64, scratch: &mut Vec<u64>) -> DevonResult<()> {
            scratch.clear();
            if node == 0 {
                scratch.extend(1..=9);
            }
            Ok(())
        }
    }

    #[test]
    fn checkpoint_rejects_over_cap_graph_without_publishing_root() {
        let directory = tempdir().unwrap();
        let pager = Pager::create(
            directory.path().join("over-cap.devondb"),
            4096,
            *b"hnsw-write-test!",
        )
        .unwrap();
        let budget = MemoryBudget::unlimited();
        let config = HnswConfig {
            m: 4,
            m0: 8,
            ef_construction: 16,
            level_seed: 7,
            metric: HnswMetric::L2,
            navigation: NavigationEncoding::F32,
        };
        let groups = HnswNodeGroups::new(vec![10]).unwrap();
        let old = publish_checkpoint(&pager, &budget, &config, None, &[], &groups)
            .unwrap()
            .index;
        let old_root_bytes = pager.read_page(old.root_page_id).unwrap();

        let error =
            merge_one_cell(&pager, &budget, &config, &OverCapGraph, &groups, 10, 0, 0).unwrap_err();
        assert!(matches!(
            error,
            DevonError::Corrupt { context }
                if context == "HNSW checkpoint node 0 layer 0 adjacency degree 9 exceeds layer cap 8"
        ));
        assert_eq!(pager.read_page(old.root_page_id).unwrap(), old_root_bytes);
        assert_eq!(load_persisted_index(&pager, old.root_page_id).unwrap(), old);
    }
}
