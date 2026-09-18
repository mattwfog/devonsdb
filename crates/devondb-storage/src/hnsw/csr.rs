//! Bounded per-hop `RCSR` slot reader (`docs/HNSW.md` §3.4-§3.5).
//!
//! The relationship reader deliberately materializes complete payload runs.
//! This reader shares its directory codec, but pins only one payload page at
//! a time and retains at most the requested adjacency list.

use std::mem::size_of;
use std::ops::Deref;

use crc32c::{crc32c, crc32c_append};
use devondb_types::{DevonError, DevonResult};

use crate::budget::{ChargedBytes, MemoryBudget};
use crate::csr_group::{ArrayEntry, decode_directory};
use crate::pager::{PageRef, Pager};

const ALLOCATION_OVERHEAD: usize = 64;

/// HNSW-specific structural bounds for one CSR group.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CsrSlotReadOptions {
    /// Global node offset represented by local slot zero.
    pub group_start: u64,
    /// Exclusive upper bound for every stored neighbor.
    pub covered_rows: u64,
    /// Maximum adjacency length at the current HNSW layer.
    pub degree_cap: usize,
}

/// A budget-charged, caller-owned set of CSR groups verified in one query.
///
/// `capacity` is the query's bound from `docs/HNSW.md` §6.2. The set uses a
/// sorted preallocated vector so it never grows beyond its charged capacity.
#[derive(Debug)]
pub struct VerifiedCsrGroups<'budget> {
    budget: &'budget MemoryBudget,
    groups: Vec<VerifiedGroup>,
    capacity: usize,
    charge: ChargedBytes<'budget>,
}

impl<'budget> VerifiedCsrGroups<'budget> {
    /// Reserves a verified-group set with room for at most `capacity` groups.
    pub fn new(budget: &'budget MemoryBudget, capacity: usize) -> DevonResult<Self> {
        let bytes = allocation_bytes(capacity, size_of::<VerifiedGroup>(), "verified CSR set")?;
        let charge = ChargedBytes::try_new(budget, bytes, || {
            format!("verified CSR set reservation of {bytes} bytes")
        })?;
        let mut groups = Vec::new();
        groups.try_reserve_exact(capacity).map_err(|error| {
            budget_exceeded(format!(
                "failed to allocate verified CSR set capacity {capacity}: {error}"
            ))
        })?;
        Ok(Self {
            budget,
            groups,
            capacity,
            charge,
        })
    }

    /// Returns the number of successfully verified groups.
    #[must_use]
    pub fn len(&self) -> usize {
        self.groups.len()
    }

    /// Returns whether no group has been verified yet.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.groups.is_empty()
    }

    /// Returns the bytes reserved for this set.
    #[must_use]
    pub fn charged_bytes(&self) -> usize {
        self.charge.bytes()
    }

    fn contains(&self, group: VerifiedGroup) -> bool {
        self.groups.binary_search(&group).is_ok()
    }

    fn ensure_insert_capacity(&self, group: VerifiedGroup) -> DevonResult<()> {
        if self.contains(group) || self.groups.len() < self.capacity {
            return Ok(());
        }
        Err(budget_exceeded(format!(
            "verified CSR set capacity {} is exhausted",
            self.capacity
        )))
    }

    fn insert(&mut self, group: VerifiedGroup) {
        if let Err(index) = self.groups.binary_search(&group) {
            self.groups.insert(index, group);
        }
    }
}

/// One budget-charged adjacency list returned by [`CsrSlotReader`].
#[derive(Debug)]
pub struct CsrAdjacency<'budget> {
    neighbors: Vec<u64>,
    charge: ChargedBytes<'budget>,
}

impl CsrAdjacency<'_> {
    /// Returns the retained neighbor ids.
    #[must_use]
    pub fn neighbors(&self) -> &[u64] {
        &self.neighbors
    }

    /// Returns the bytes reserved for adjacency scratch.
    #[must_use]
    pub fn charged_bytes(&self) -> usize {
        self.charge.bytes()
    }

    /// Returns the neighbor-vector capacity in bytes.
    #[must_use]
    pub fn allocated_bytes(&self) -> usize {
        self.neighbors.capacity().saturating_mul(size_of::<u64>())
    }
}

impl Deref for CsrAdjacency<'_> {
    type Target = [u64];

    fn deref(&self) -> &Self::Target {
        self.neighbors()
    }
}

/// A decoded read-only view of one existing-format, zero-column `RCSR` group.
///
/// Construction reads and validates only the directory page. Payload CRC and
/// structural validation are deferred until the first query access recorded
/// in the caller's [`VerifiedCsrGroups`].
pub struct CsrSlotReader<'pager> {
    pager: &'pager Pager,
    directory_page: u64,
    page_size: usize,
    row_count: usize,
    edge_count: usize,
    offsets: ArrayEntry,
    neighbors: ArrayEntry,
}

impl<'pager> CsrSlotReader<'pager> {
    /// Decodes and validates one zero-property-column CSR directory page.
    pub fn open(pager: &'pager Pager, directory_page: u64) -> DevonResult<Self> {
        let directory = read_directory_page(pager, directory_page)?;
        let (row_count, edge_count, entries) = decode_directory(&directory, &[])?;
        let offsets = entries
            .first()
            .copied()
            .ok_or_else(|| corrupt("HNSW CSR directory lacks offsets entry"))?;
        let neighbors = entries
            .get(1)
            .copied()
            .ok_or_else(|| corrupt("HNSW CSR directory lacks neighbors entry"))?;
        validate_entry_lengths(row_count, edge_count, offsets, neighbors)?;
        Ok(Self {
            pager,
            directory_page,
            page_size: pager.superblock().page_size as usize,
            row_count,
            edge_count,
            offsets,
            neighbors,
        })
    }

    /// Returns the number of local node slots covered by this group.
    #[must_use]
    pub fn row_count(&self) -> usize {
        self.row_count
    }

    /// Returns the total number of neighbors in this group.
    #[must_use]
    pub fn edge_count(&self) -> usize {
        self.edge_count
    }

    /// Reads one adjacency list, accepting every in-bounds neighbor as eligible.
    pub fn read_slot<'budget>(
        &self,
        slot: usize,
        options: CsrSlotReadOptions,
        verified: &mut VerifiedCsrGroups<'budget>,
    ) -> DevonResult<CsrAdjacency<'budget>> {
        self.read_slot_with_eligibility(slot, options, verified, |_| Ok(true))
    }

    /// Reads one adjacency list and validates layer eligibility of every neighbor.
    pub fn read_slot_with_eligibility<'budget, F>(
        &self,
        slot: usize,
        options: CsrSlotReadOptions,
        verified: &mut VerifiedCsrGroups<'budget>,
        mut is_eligible: F,
    ) -> DevonResult<CsrAdjacency<'budget>>
    where
        F: FnMut(u64) -> DevonResult<bool>,
    {
        self.validate_request(slot, options)?;
        let group = self.verification_key();
        if !verified.contains(group) {
            verified.ensure_insert_capacity(group)?;
            self.verify_group(options, verified.budget, &mut is_eligible)?;
            verified.insert(group);
        }
        let (start, end) = self.read_offset_pair(slot)?;
        let adjacency =
            self.read_adjacency(start, end, options.degree_cap, verified.budget, None)?;
        validate_adjacency(
            adjacency.neighbors(),
            options.group_start + slot as u64,
            options,
            &mut is_eligible,
        )?;
        Ok(adjacency)
    }

    fn verification_key(&self) -> VerifiedGroup {
        VerifiedGroup {
            directory_page: self.directory_page,
            offsets_checksum: self.offsets.checksum,
            neighbors_checksum: self.neighbors.checksum,
        }
    }

    fn validate_request(&self, slot: usize, options: CsrSlotReadOptions) -> DevonResult<()> {
        if slot >= self.row_count {
            return Err(invalid_argument(format!(
                "CSR slot {slot} is outside row_count {}",
                self.row_count
            )));
        }
        if options.degree_cap == 0 {
            return Err(invalid_argument("HNSW CSR degree cap must be positive"));
        }
        let group_end = options
            .group_start
            .checked_add(self.row_count as u64)
            .ok_or_else(|| corrupt("HNSW CSR group node range overflows u64"))?;
        if group_end > options.covered_rows {
            return Err(corrupt(format!(
                "HNSW CSR group ends at {group_end}, beyond covered_rows {}",
                options.covered_rows
            )));
        }
        Ok(())
    }

    fn verify_group<F>(
        &self,
        options: CsrSlotReadOptions,
        budget: &MemoryBudget,
        is_eligible: &mut F,
    ) -> DevonResult<()>
    where
        F: FnMut(u64) -> DevonResult<bool>,
    {
        self.verify_offsets(options.degree_cap)?;
        let mut checksum = crc32c(&[]);
        for slot in 0..self.row_count {
            let (start, end) = self.read_offset_pair(slot)?;
            let adjacency =
                self.read_adjacency(start, end, options.degree_cap, budget, Some(&mut checksum))?;
            validate_adjacency(
                adjacency.neighbors(),
                options.group_start + slot as u64,
                options,
                is_eligible,
            )?;
        }
        validate_checksum(checksum, self.neighbors.checksum, "CSR neighbors")
    }

    fn verify_offsets(&self, degree_cap: usize) -> DevonResult<()> {
        let mut checksum = crc32c(&[]);
        let mut previous = None;
        let mut offset_index = 0_usize;
        self.for_each_payload_page(self.offsets, "CSR offsets", |bytes| {
            checksum = crc32c_append(checksum, bytes);
            for chunk in bytes.as_chunks::<4>().0 {
                let value = u32::from_le_bytes(copy_array(chunk));
                validate_offset(value, previous, offset_index, degree_cap)?;
                previous = Some(value);
                offset_index += 1;
            }
            Ok(())
        })?;
        if previous != Some(self.edge_count as u32) {
            return Err(corrupt(format!(
                "CSR final offset does not equal edge_count {}",
                self.edge_count
            )));
        }
        validate_checksum(checksum, self.offsets.checksum, "CSR offsets")
    }

    fn for_each_payload_page<F>(
        &self,
        entry: ArrayEntry,
        name: &str,
        mut inspect: F,
    ) -> DevonResult<()>
    where
        F: FnMut(&[u8]) -> DevonResult<()>,
    {
        let byte_len = entry.byte_len as usize;
        let page_count = byte_len.div_ceil(self.page_size);
        for page_index in 0..page_count {
            let page = self.read_payload_page(entry, page_index, name)?;
            let start = page_index * self.page_size;
            let take = (byte_len - start).min(self.page_size);
            inspect(&page[..take])?;
            validate_page_padding(&page, take, name)?;
        }
        Ok(())
    }

    fn read_offset_pair(&self, slot: usize) -> DevonResult<(usize, usize)> {
        let start = self.read_offset(slot)? as usize;
        let end = self.read_offset(slot + 1)? as usize;
        if start > end || end > self.edge_count {
            return Err(corrupt(format!(
                "CSR slot {slot} has invalid edge range {start}..{end}"
            )));
        }
        Ok((start, end))
    }

    fn read_offset(&self, index: usize) -> DevonResult<u32> {
        let byte_offset = index
            .checked_mul(4)
            .ok_or_else(|| corrupt("CSR offset byte position overflows usize"))?;
        let page_index = byte_offset / self.page_size;
        let in_page = byte_offset % self.page_size;
        let page = self.read_payload_page(self.offsets, page_index, "CSR offsets")?;
        Ok(u32::from_le_bytes(copy_array(&page[in_page..in_page + 4])))
    }

    fn read_adjacency<'budget>(
        &self,
        start: usize,
        end: usize,
        degree_cap: usize,
        budget: &'budget MemoryBudget,
        checksum: Option<&mut u32>,
    ) -> DevonResult<CsrAdjacency<'budget>> {
        let degree = end - start;
        if degree > degree_cap {
            return Err(corrupt(format!(
                "HNSW adjacency degree {degree} exceeds layer cap {degree_cap}"
            )));
        }
        let bytes = allocation_bytes(degree_cap, size_of::<u64>(), "HNSW adjacency scratch")?;
        let charge = ChargedBytes::try_new(budget, bytes, || {
            format!("HNSW adjacency scratch reservation of {bytes} bytes")
        })?;
        let mut neighbors = Vec::new();
        neighbors.try_reserve_exact(degree).map_err(|error| {
            budget_exceeded(format!("failed to allocate HNSW adjacency list: {error}"))
        })?;
        self.decode_neighbor_range(start, end, &mut neighbors, checksum)?;
        Ok(CsrAdjacency { neighbors, charge })
    }

    fn decode_neighbor_range(
        &self,
        start: usize,
        end: usize,
        output: &mut Vec<u64>,
        mut checksum: Option<&mut u32>,
    ) -> DevonResult<()> {
        if start == end {
            return Ok(());
        }
        let byte_start = start * size_of::<u64>();
        let byte_end = end * size_of::<u64>();
        let first_page = byte_start / self.page_size;
        let page_end = byte_end.div_ceil(self.page_size);
        for page_index in first_page..page_end {
            let page = self.read_payload_page(self.neighbors, page_index, "CSR neighbors")?;
            let page_start = page_index * self.page_size;
            let local_start = byte_start.saturating_sub(page_start);
            let local_end = (byte_end - page_start).min(self.page_size);
            let bytes = &page[local_start..local_end];
            if let Some(actual) = checksum.as_deref_mut() {
                *actual = crc32c_append(*actual, bytes);
            }
            output.extend(
                bytes
                    .as_chunks::<8>()
                    .0
                    .iter()
                    .map(|chunk| u64::from_le_bytes(copy_array(chunk))),
            );
            if byte_end == self.neighbors.byte_len as usize {
                validate_page_padding(&page, local_end, "CSR neighbors")?;
            }
        }
        Ok(())
    }

    fn read_payload_page(
        &self,
        entry: ArrayEntry,
        page_index: usize,
        name: &str,
    ) -> DevonResult<PageRef> {
        let page_offset = u64::try_from(page_index)
            .map_err(|_| corrupt(format!("{name} page index exceeds u64")))?;
        let page_id = entry
            .first_page
            .checked_add(page_offset)
            .ok_or_else(|| corrupt(format!("{name} page run overflows u64")))?;
        read_referenced_page(self.pager, page_id, name)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct VerifiedGroup {
    directory_page: u64,
    offsets_checksum: u32,
    neighbors_checksum: u32,
}

fn validate_entry_lengths(
    row_count: usize,
    edge_count: usize,
    offsets: ArrayEntry,
    neighbors: ArrayEntry,
) -> DevonResult<()> {
    let offsets_len = row_count
        .checked_add(1)
        .and_then(|count| count.checked_mul(4))
        .ok_or_else(|| corrupt("CSR offsets payload length overflows"))?;
    let neighbors_len = edge_count
        .checked_mul(8)
        .ok_or_else(|| corrupt("CSR neighbors payload length overflows"))?;
    validate_entry_length(offsets, offsets_len, "CSR offsets")?;
    validate_entry_length(neighbors, neighbors_len, "CSR neighbors")
}

fn validate_entry_length(entry: ArrayEntry, expected: usize, name: &str) -> DevonResult<()> {
    if entry.byte_len as usize != expected {
        return Err(corrupt(format!(
            "{name} byte_len is {}, expected exactly {expected}",
            entry.byte_len
        )));
    }
    Ok(())
}

fn validate_offset(
    value: u32,
    previous: Option<u32>,
    index: usize,
    degree_cap: usize,
) -> DevonResult<()> {
    let Some(previous) = previous else {
        return if value == 0 {
            Ok(())
        } else {
            Err(corrupt("CSR offsets do not start at zero"))
        };
    };
    if value < previous {
        return Err(corrupt(format!(
            "CSR offsets are not monotonic at endpoint {index}"
        )));
    }
    let degree = (value - previous) as usize;
    if degree > degree_cap {
        return Err(corrupt(format!(
            "HNSW adjacency degree {degree} exceeds layer cap {degree_cap}"
        )));
    }
    Ok(())
}

fn validate_neighbor<F>(
    neighbor: u64,
    slot: usize,
    seen: &[u64],
    options: CsrSlotReadOptions,
    is_eligible: &mut F,
) -> DevonResult<()>
where
    F: FnMut(u64) -> DevonResult<bool>,
{
    if neighbor >= options.covered_rows {
        return Err(corrupt(format!(
            "HNSW neighbor {neighbor} is outside covered_rows {}",
            options.covered_rows
        )));
    }
    let node = options.group_start + slot as u64;
    if neighbor == node {
        return Err(corrupt(format!("HNSW node {node} has a self-neighbor")));
    }
    if seen.contains(&neighbor) {
        return Err(corrupt(format!(
            "HNSW node {node} has duplicate neighbor {neighbor}"
        )));
    }
    if !is_eligible(neighbor)? {
        return Err(corrupt(format!(
            "HNSW neighbor {neighbor} is not eligible in this layer"
        )));
    }
    Ok(())
}

fn validate_adjacency<F>(
    neighbors: &[u64],
    node: u64,
    options: CsrSlotReadOptions,
    is_eligible: &mut F,
) -> DevonResult<()>
where
    F: FnMut(u64) -> DevonResult<bool>,
{
    if neighbors.len() > options.degree_cap {
        return Err(corrupt(format!(
            "HNSW adjacency degree {} exceeds layer cap {}",
            neighbors.len(),
            options.degree_cap
        )));
    }
    let slot = (node - options.group_start) as usize;
    for (index, neighbor) in neighbors.iter().enumerate() {
        validate_neighbor(*neighbor, slot, &neighbors[..index], options, is_eligible)?;
    }
    Ok(())
}

fn validate_checksum(actual: u32, expected: u32, name: &str) -> DevonResult<()> {
    if actual != expected {
        return Err(corrupt(format!("{name} payload CRC-32C does not match")));
    }
    Ok(())
}

fn validate_page_padding(page: &[u8], used: usize, name: &str) -> DevonResult<()> {
    if used < page.len() && page[used..].iter().any(|byte| *byte != 0) {
        return Err(corrupt(format!("{name} payload page padding is not zero")));
    }
    Ok(())
}

fn read_directory_page(pager: &Pager, page_id: u64) -> DevonResult<PageRef> {
    read_referenced_page(pager, page_id, "HNSW CSR directory")
}

fn read_referenced_page(pager: &Pager, page_id: u64, name: &str) -> DevonResult<PageRef> {
    match pager.read_page_ref(page_id) {
        Ok(page) => Ok(page),
        Err(DevonError::InvalidArgument { context }) => Err(corrupt(format!(
            "{name} references invalid page {page_id}: {context}"
        ))),
        Err(error) => Err(error),
    }
}

fn allocation_bytes(count: usize, width: usize, name: &str) -> DevonResult<usize> {
    let payload = count
        .checked_mul(width)
        .ok_or_else(|| invalid_argument(format!("{name} reservation overflows usize")))?;
    if count == 0 {
        return Ok(0);
    }
    payload
        .checked_add(ALLOCATION_OVERHEAD)
        .ok_or_else(|| invalid_argument(format!("{name} reservation overflows usize")))
}

fn copy_array<const N: usize>(bytes: &[u8]) -> [u8; N] {
    let mut array = [0_u8; N];
    array.copy_from_slice(bytes);
    array
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
