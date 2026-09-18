//! Snapshot index view: persistent root + immutable delta chain
//! (`docs/HNSW.md` §4.2-§4.3, §5.1).

use std::cell::RefCell;
use std::ops::Range;

use devondb_types::{DevonError, DevonResult};

use super::csr::{CsrSlotReadOptions, CsrSlotReader, VerifiedCsrGroups};
use super::format::{HnswRoot, LayerDirectory};
use super::scoring;
use super::types::{GraphAccess, HnswDelta, MAX_LEVEL};
use crate::budget::MemoryBudget;
use crate::pager::Pager;

/// Geometry of the persisted node groups used by an HNSW root.
///
/// `row_counts` are in catalog order. Their prefix sums map a global node
/// offset to the layer directory's group index and that CSR group's local
/// slot. The layout may include groups or rows newer than the root, but its
/// prefix intersecting the root coverage must match `root.group_count`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HnswGroupLayout {
    row_counts: Vec<u64>,
    starts: Vec<u64>,
    total_rows: u64,
}

impl HnswGroupLayout {
    /// Builds catalog-order group geometry from nonzero row counts.
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
            total_rows = total_rows.checked_add(row_count).ok_or_else(|| {
                invalid_argument("HNSW node-group layout row count overflows u64")
            })?;
        }
        Ok(Self {
            row_counts,
            starts,
            total_rows,
        })
    }

    fn locate(&self, node: u64) -> Option<(usize, u64, usize)> {
        if node >= self.total_rows {
            return None;
        }
        let group = self.starts.partition_point(|start| *start <= node) - 1;
        let slot = usize::try_from(node - self.starts[group]).ok()?;
        Some((group, self.starts[group], slot))
    }

    fn groups_intersecting(&self, covered_rows: u64) -> usize {
        self.starts
            .partition_point(|group_start| *group_start < covered_rows)
    }

    fn validate_root_geometry(&self, root: &HnswRoot) -> DevonResult<()> {
        if root.covered_rows > self.total_rows {
            return Err(corrupt(format!(
                "HNSW root covers {} rows, but its node-group layout covers only {}",
                root.covered_rows, self.total_rows
            )));
        }
        let intersecting = self.groups_intersecting(root.covered_rows);
        if intersecting != root.group_count as usize {
            return Err(corrupt(format!(
                "HNSW root group_count is {}, but {intersecting} layout groups intersect coverage {}",
                root.group_count, root.covered_rows
            )));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy)]
struct EffectiveState {
    entry: Option<(u64, u8)>,
    layer_count: u8,
    covered_rows: u64,
}

/// One immutable snapshot's HNSW topology view.
///
/// The ordered `deltas` slice passed to [`Self::new`] must contain exactly
/// the deltas reachable from the snapshot, oldest first. The view never
/// consults a mutable publication head. Its only interior mutation is the
/// query-local set of CSR payloads already verified by [`CsrSlotReader`].
pub struct HnswSnapshotView<'view> {
    pager: &'view Pager,
    root: HnswRoot,
    directory: LayerDirectory,
    deltas: &'view [HnswDelta],
    row_count: u64,
    groups: HnswGroupLayout,
    effective: EffectiveState,
    verified: RefCell<VerifiedCsrGroups<'view>>,
}

impl<'view> HnswSnapshotView<'view> {
    /// Builds a view over a decoded root/directory and one reachable delta chain.
    ///
    /// `groups` describes persisted node groups in catalog order. Overlay-only
    /// rows may increase `row_count` without appearing in that layout.
    /// `verified_group_capacity` is the caller's §6.2 per-query bound for CSR
    /// groups whose payload checksums may be memoized by this view.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        pager: &'view Pager,
        budget: &'view MemoryBudget,
        root: HnswRoot,
        directory: LayerDirectory,
        deltas: &'view [HnswDelta],
        row_count: u64,
        groups: HnswGroupLayout,
        verified_group_capacity: usize,
    ) -> DevonResult<Self> {
        validate_root_and_directory(&root, &directory, row_count, &groups)?;
        let effective = derive_effective_state(&root, deltas, row_count)?;
        let verified = VerifiedCsrGroups::new(budget, verified_group_capacity)?;
        Ok(Self {
            pager,
            root,
            directory,
            deltas,
            row_count,
            groups,
            effective,
            verified: RefCell::new(verified),
        })
    }

    /// Returns the exact-scan tail for this snapshot.
    #[must_use]
    pub fn exact_tail(&self) -> Range<u64> {
        self.effective.covered_rows..self.row_count
    }

    /// Returns the snapshot's visible table row count.
    #[must_use]
    pub const fn row_count(&self) -> u64 {
        self.row_count
    }

    fn newest_replacement(&self, layer: u8, node: u64) -> Option<(&HnswDelta, &[u64])> {
        self.deltas.iter().rev().find_map(|delta| {
            delta
                .replacements
                .get(&(layer, node))
                .map(|neighbors| (delta, neighbors.as_ref()))
        })
    }

    fn read_base_neighbors(&self, layer: u8, node: u64, scratch: &mut Vec<u64>) -> DevonResult<()> {
        if layer >= self.root.layer_count || node >= self.root.covered_rows {
            return Ok(());
        }
        let (group, group_start, slot) = self.groups.locate(node).ok_or_else(|| {
            corrupt(format!(
                "HNSW root node {node} is absent from its group layout"
            ))
        })?;
        let group =
            u32::try_from(group).map_err(|_| corrupt("HNSW node-group index exceeds u32"))?;
        let Some(page_id) = self.directory.page_id(layer, group)? else {
            return Ok(());
        };
        let reader = CsrSlotReader::open(self.pager, page_id)?;
        if slot >= reader.row_count() {
            return Err(corrupt(format!(
                "HNSW CSR group row_count {} does not cover node {node} at slot {slot}: \
                 group geometry contradicts the root layout",
                reader.row_count()
            )));
        }
        let options = CsrSlotReadOptions {
            group_start,
            covered_rows: self.root.covered_rows,
            degree_cap: degree_cap(&self.root, layer),
        };
        let mut verified = self
            .verified
            .try_borrow_mut()
            .map_err(|_| invalid_argument("HNSW verified-group set is already in use"))?;
        let adjacency = reader.read_slot_with_eligibility(
            slot,
            options,
            &mut verified,
            |neighbor| {
                let derived_level = scoring::level_for_node(&self.root.config, neighbor)?;
                if derived_level < layer {
                    return Err(corrupt(format!(
                        "HNSW neighbor {neighbor} has derived level {derived_level}, below layer {layer}"
                    )));
                }
                Ok(true)
            },
        )?;
        scratch.extend_from_slice(adjacency.neighbors());
        Ok(())
    }

    fn validate_lookup(&self, layer: u8, node: u64) -> DevonResult<()> {
        if layer >= self.effective.layer_count {
            return Err(invalid_argument(format!(
                "HNSW layer {layer} is outside effective layer_count {}",
                self.effective.layer_count
            )));
        }
        if node >= self.effective.covered_rows {
            return Err(invalid_argument(format!(
                "HNSW node {node} is outside effective covered_rows {}",
                self.effective.covered_rows
            )));
        }
        Ok(())
    }
}

impl GraphAccess for HnswSnapshotView<'_> {
    fn entry(&self) -> Option<(u64, u8)> {
        self.effective.entry
    }

    fn layer_count(&self) -> u8 {
        self.effective.layer_count
    }

    fn covered_rows(&self) -> u64 {
        self.effective.covered_rows
    }

    fn neighbors(&self, layer: u8, node: u64, scratch: &mut Vec<u64>) -> DevonResult<()> {
        scratch.clear();
        self.validate_lookup(layer, node)?;
        if let Some((delta, replacement)) = self.newest_replacement(layer, node) {
            validate_replacement(
                layer,
                node,
                replacement,
                delta.new_covered_rows,
                degree_cap(&self.root, layer),
            )?;
            scratch.extend_from_slice(replacement);
            return Ok(());
        }
        self.read_base_neighbors(layer, node, scratch)
    }
}

fn validate_root_and_directory(
    root: &HnswRoot,
    directory: &LayerDirectory,
    row_count: u64,
    groups: &HnswGroupLayout,
) -> DevonResult<()> {
    root.config.validate()?;
    if root.covered_rows > row_count {
        return Err(corrupt(format!(
            "HNSW root coverage {} exceeds snapshot row count {row_count}",
            root.covered_rows
        )));
    }
    if root.layer_count != directory.layer_count()
        || root.group_count != directory.group_count()
        || root.layer_dir_byte_len != directory.byte_len()
        || root.layer_dir_crc32c != directory.crc32c()
    {
        return Err(corrupt(
            "HNSW root and decoded layer-directory metadata disagree",
        ));
    }
    validate_root_entry(root)?;
    groups.validate_root_geometry(root)
}

fn validate_root_entry(root: &HnswRoot) -> DevonResult<()> {
    match root.entry_node {
        None if root.entry_level != 0 || root.layer_count != 0 => Err(corrupt(
            "HNSW root without an entry must have zero entry_level and layer_count",
        )),
        None => Ok(()),
        Some(node) if node >= root.covered_rows => Err(corrupt(format!(
            "HNSW root entry {node} is outside root coverage {}",
            root.covered_rows
        ))),
        Some(_) if root.entry_level > MAX_LEVEL => Err(corrupt(format!(
            "HNSW root entry level {} exceeds {MAX_LEVEL}",
            root.entry_level
        ))),
        Some(_) if root.layer_count != root.entry_level + 1 => Err(corrupt(
            "HNSW root layer_count does not equal entry_level + 1",
        )),
        Some(_) => Ok(()),
    }
}

fn derive_effective_state(
    root: &HnswRoot,
    deltas: &[HnswDelta],
    row_count: u64,
) -> DevonResult<EffectiveState> {
    let mut state = EffectiveState {
        entry: root.entry_node.map(|node| (node, root.entry_level)),
        layer_count: root.layer_count,
        covered_rows: root.covered_rows,
    };
    for (index, delta) in deltas.iter().enumerate() {
        validate_delta_coverage(index, delta, state.covered_rows, row_count)?;
        state.covered_rows = delta.new_covered_rows;
        if let Some(entry) = delta.entry {
            validate_delta_entry(index, entry, state.covered_rows)?;
            state.entry = Some(entry);
            state.layer_count = entry.1 + 1;
        }
        validate_delta_replacements(index, root, delta, state)?;
    }
    Ok(state)
}

fn validate_delta_coverage(
    index: usize,
    delta: &HnswDelta,
    previous: u64,
    row_count: u64,
) -> DevonResult<()> {
    if delta.old_covered_rows != previous {
        return Err(corrupt(format!(
            "HNSW delta {index} starts at {}, expected contiguous coverage {previous}",
            delta.old_covered_rows
        )));
    }
    if delta.new_covered_rows < delta.old_covered_rows {
        return Err(corrupt(format!(
            "HNSW delta {index} moves coverage backwards from {} to {}",
            delta.old_covered_rows, delta.new_covered_rows
        )));
    }
    if delta.new_covered_rows > row_count {
        return Err(corrupt(format!(
            "HNSW delta {index} coverage {} exceeds snapshot row count {row_count}",
            delta.new_covered_rows
        )));
    }
    Ok(())
}

fn validate_delta_entry(index: usize, entry: (u64, u8), covered_rows: u64) -> DevonResult<()> {
    if entry.0 >= covered_rows {
        return Err(corrupt(format!(
            "HNSW delta {index} entry {} is outside effective coverage {covered_rows}",
            entry.0
        )));
    }
    if entry.1 > MAX_LEVEL {
        return Err(corrupt(format!(
            "HNSW delta {index} entry level {} exceeds {MAX_LEVEL}",
            entry.1
        )));
    }
    Ok(())
}

fn validate_delta_replacements(
    index: usize,
    root: &HnswRoot,
    delta: &HnswDelta,
    state: EffectiveState,
) -> DevonResult<()> {
    for (&(layer, node), replacement) in &delta.replacements {
        if layer >= state.layer_count {
            return Err(corrupt(format!(
                "HNSW delta {index} replacement layer {layer} is outside effective layer_count {}",
                state.layer_count
            )));
        }
        validate_replacement(
            layer,
            node,
            replacement,
            state.covered_rows,
            degree_cap(root, layer),
        )?;
    }
    Ok(())
}

fn validate_replacement(
    layer: u8,
    node: u64,
    replacement: &[u64],
    covered_rows: u64,
    cap: usize,
) -> DevonResult<()> {
    if node >= covered_rows {
        return Err(corrupt(format!(
            "HNSW replacement node {node} at layer {layer} is outside coverage {covered_rows}"
        )));
    }
    if replacement.len() > cap {
        return Err(corrupt(format!(
            "HNSW replacement degree {} exceeds layer {layer} cap {cap}",
            replacement.len()
        )));
    }
    for (position, neighbor) in replacement.iter().copied().enumerate() {
        validate_replacement_neighbor(node, neighbor, &replacement[..position], covered_rows)?;
    }
    Ok(())
}

fn validate_replacement_neighbor(
    node: u64,
    neighbor: u64,
    previous: &[u64],
    covered_rows: u64,
) -> DevonResult<()> {
    if neighbor >= covered_rows {
        return Err(corrupt(format!(
            "HNSW replacement neighbor {neighbor} is outside coverage {covered_rows}"
        )));
    }
    if neighbor == node {
        return Err(corrupt(format!(
            "HNSW replacement for node {node} contains a self-neighbor"
        )));
    }
    if previous.contains(&neighbor) {
        return Err(corrupt(format!(
            "HNSW replacement for node {node} contains duplicate neighbor {neighbor}"
        )));
    }
    Ok(())
}

fn degree_cap(root: &HnswRoot, layer: u8) -> usize {
    if layer == 0 {
        usize::from(root.config.m0)
    } else {
        usize::from(root.config.m)
    }
}

fn invalid_argument(context: impl Into<String>) -> DevonError {
    DevonError::InvalidArgument {
        context: context.into(),
    }
}

fn corrupt(context: impl Into<String>) -> DevonError {
    DevonError::Corrupt {
        context: context.into(),
    }
}
