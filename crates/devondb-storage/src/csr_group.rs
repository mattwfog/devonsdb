//! CSR adjacency groups (`docs/FORMAT.md` § Rel table adjacency, binding).
//!
//! One CSR group holds every edge of a rel table, in one direction, whose
//! grouped endpoint lives in one node group of the endpoint table: an
//! `RCSR` directory page plus offsets / neighbors / property-column
//! payload runs, mirroring `node_group`'s page discipline.

use std::str;
use std::{mem::size_of, ops::Range};

use crc32c::crc32c;
use devondb_types::{DevonError, DevonResult, logical_type::LogicalType, value::Value};

use crate::pager::Pager;

const DIRECTORY_MAGIC: &[u8; 4] = b"RCSR";
const DIRECTORY_HEADER_LEN: usize = 16;
const DIRECTORY_ENTRY_LEN: usize = 16;
const REQUIRED_ARRAY_COUNT: usize = 2;

/// One direction of relationship adjacency for a single endpoint node group.
#[derive(Debug, Clone, PartialEq)]
pub struct CsrGroup {
    types: Vec<LogicalType>,
    offsets: Vec<u32>,
    neighbors: Vec<u64>,
    columns: Vec<Vec<Value>>,
}

/// Conservative scratch sizing for one decoded or replacement CSR group.
///
/// `resident_bytes` covers the decoded `CsrGroup`; `encoded_bytes` covers
/// the deterministic payload buffers allocated while writing it. Checkpoint
/// charges both before it decodes or builds a detach replacement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CsrCheckpointEstimate {
    /// Covered source slots.
    pub row_count: usize,
    /// Maximum resident edges.
    pub edge_count: usize,
    /// Estimated bytes held by the decoded group.
    pub resident_bytes: usize,
    /// Estimated bytes held by the encoded write payloads.
    pub encoded_bytes: usize,
    /// Encoded property payload bytes used to size a replacement.
    pub property_bytes: usize,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ArrayEntry {
    pub(crate) first_page: u64,
    pub(crate) byte_len: u32,
    pub(crate) checksum: u32,
}

impl CsrGroup {
    /// Creates an empty in-memory CSR group covering `row_count` node slots.
    pub fn new(row_count: usize, types: Vec<LogicalType>) -> DevonResult<Self> {
        if row_count == 0 {
            return Err(invalid_argument("a CSR group must cover at least one row"));
        }
        let offset_count = row_count
            .checked_add(1)
            .ok_or_else(|| invalid_argument("CSR offset count overflows usize"))?;
        u32::try_from(row_count).map_err(|_| invalid_argument("CSR row count exceeds u32"))?;
        let columns = types.iter().map(|_| Vec::new()).collect();
        Ok(Self {
            types,
            offsets: vec![0; offset_count],
            neighbors: Vec::new(),
            columns,
        })
    }

    /// Appends an edge to one covered node slot, preserving that slot's order.
    pub fn push_edge(&mut self, slot: usize, neighbor: u64, values: Vec<Value>) -> DevonResult<()> {
        if slot >= self.row_count() {
            return Err(invalid_argument(format!(
                "CSR slot {slot} is outside row_count {}",
                self.row_count()
            )));
        }
        validate_values(&self.types, &values)?;
        if self.neighbors.len() >= u32::MAX as usize {
            return Err(invalid_argument("CSR edge count exceeds u32"));
        }

        let insertion_index = self.offsets[slot + 1] as usize;
        self.neighbors.insert(insertion_index, neighbor);
        for (column, value) in self.columns.iter_mut().zip(values) {
            column.insert(insertion_index, value);
        }
        for offset in &mut self.offsets[slot + 1..] {
            *offset += 1;
        }
        Ok(())
    }

    /// Returns the number of covered node slots.
    #[must_use]
    pub fn row_count(&self) -> usize {
        self.offsets.len() - 1
    }

    /// Returns the number of stored edges.
    #[must_use]
    pub fn edge_count(&self) -> usize {
        self.neighbors.len()
    }

    /// Returns the number of relationship property columns.
    #[must_use]
    pub fn column_count(&self) -> usize {
        self.columns.len()
    }

    /// Returns the logical property types in declaration order.
    #[must_use]
    pub fn types(&self) -> &[LogicalType] {
        &self.types
    }

    /// Returns the flat edge-index range owned by `slot`.
    #[must_use]
    pub fn edge_range(&self, slot: usize) -> Option<Range<usize>> {
        let start = *self.offsets.get(slot)? as usize;
        let end = *self.offsets.get(slot + 1)? as usize;
        Some(start..end)
    }

    /// Returns the neighbor offset at a flat edge index.
    #[must_use]
    pub fn neighbor(&self, edge_index: usize) -> Option<u64> {
        self.neighbors.get(edge_index).copied()
    }

    /// Returns one property value by flat edge index and column index.
    #[must_use]
    pub fn value(&self, edge_index: usize, column_index: usize) -> Option<&Value> {
        self.columns.get(column_index)?.get(edge_index)
    }

    /// Persists all array payloads and the `RCSR` directory page.
    pub fn write(&self, pager: &Pager) -> DevonResult<u64> {
        let page_size = pager.superblock().page_size as usize;
        self.validate_for_write(page_size)?;
        let payloads = self.encode_payloads()?;
        let mut entries = Vec::with_capacity(payloads.len());
        for payload in &payloads {
            entries.push(ArrayEntry {
                first_page: write_payload(pager, payload, page_size)?,
                byte_len: u32::try_from(payload.len())
                    .map_err(|_| invalid_argument("CSR payload length exceeds u32"))?,
                checksum: crc32c(payload),
            });
        }

        let directory_page = pager.allocate_page()?;
        let directory = encode_directory(self.row_count(), self.edge_count(), &entries, page_size)?;
        pager.write_page(directory_page, &directory)?;
        pager.sync()?;
        Ok(directory_page)
    }

    /// Reads a CSR group without an external neighbor-offset bound.
    pub fn read(pager: &Pager, directory_page: u64, types: &[LogicalType]) -> DevonResult<Self> {
        Self::read_inner(pager, directory_page, types, None)
    }

    /// Reads a CSR group and rejects neighbors outside the endpoint row count.
    pub fn read_checked(
        pager: &Pager,
        directory_page: u64,
        types: &[LogicalType],
        neighbor_row_count: u64,
    ) -> DevonResult<Self> {
        Self::read_inner(pager, directory_page, types, Some(neighbor_row_count))
    }

    /// Reads only a CSR directory and conservatively sizes the full decoded
    /// group and its write-time payload buffers.
    ///
    /// This is the detach-checkpoint precharge seam: no offsets, neighbors,
    /// or property values are allocated before the caller reserves the
    /// returned amount.
    pub fn checkpoint_estimate(
        pager: &Pager,
        directory_page: u64,
        types: &[LogicalType],
    ) -> DevonResult<CsrCheckpointEstimate> {
        let page = pager.read_page(directory_page)?;
        let (row_count, edge_count, entries) = decode_directory(&page, types)?;
        let property_bytes =
            entries[REQUIRED_ARRAY_COUNT..]
                .iter()
                .try_fold(0_usize, |total, entry| {
                    total
                        .checked_add(entry.byte_len as usize)
                        .ok_or_else(|| corrupt("CSR property byte estimate overflows usize"))
                })?;
        checkpoint_estimate(row_count, edge_count, types, property_bytes)
    }

    /// Conservatively sizes a replacement builder with the supplied maximum
    /// edge count and property payload bytes.
    pub fn replacement_checkpoint_estimate(
        row_count: usize,
        edge_count: usize,
        types: &[LogicalType],
        property_bytes: usize,
    ) -> DevonResult<CsrCheckpointEstimate> {
        checkpoint_estimate(row_count, edge_count, types, property_bytes)
    }

    /// Estimates one edge's owned relationship-property values for a
    /// checkpoint replacement. The estimate is deliberately conservative
    /// and uses the shared `Value::approx_bytes` policy.
    pub fn checkpoint_property_bytes(values: &[Value]) -> DevonResult<usize> {
        values.iter().try_fold(0_usize, |total, value| {
            total
                .checked_add(value.approx_bytes())
                .ok_or_else(|| invalid_argument("CSR property byte estimate overflows usize"))
        })
    }

    fn read_inner(
        pager: &Pager,
        directory_page: u64,
        types: &[LogicalType],
        neighbor_row_count: Option<u64>,
    ) -> DevonResult<Self> {
        let directory = pager.read_page(directory_page)?;
        let (row_count, edge_count, entries) = decode_directory(&directory, types)?;
        let page_size = pager.superblock().page_size as usize;

        validate_fixed_payload_len(entries[0], offsets_payload_len(row_count), "offsets")?;
        let offsets_payload = read_checked_payload(pager, entries[0], page_size, "offsets")?;
        let offsets = decode_offsets(&offsets_payload, row_count, edge_count)?;

        validate_fixed_payload_len(entries[1], neighbors_payload_len(edge_count), "neighbors")?;
        let neighbors_payload = read_checked_payload(pager, entries[1], page_size, "neighbors")?;
        let neighbors = decode_neighbors(&neighbors_payload, neighbor_row_count)?;

        let mut columns = Vec::with_capacity(types.len());
        for (index, (entry, logical_type)) in entries[REQUIRED_ARRAY_COUNT..]
            .iter()
            .zip(types)
            .enumerate()
        {
            validate_column_payload_len(*entry, edge_count, logical_type, index)?;
            let name = format!("property column {index}");
            let payload = read_checked_payload(pager, *entry, page_size, &name)?;
            columns.push(decode_column(&payload, edge_count, logical_type, index)?);
        }

        Ok(Self {
            types: types.to_vec(),
            offsets,
            neighbors,
            columns,
        })
    }

    fn validate_for_write(&self, page_size: usize) -> DevonResult<()> {
        if self.edge_count() == 0 {
            return Err(invalid_argument("cannot write an empty CSR group"));
        }
        let entry_count = self
            .column_count()
            .checked_add(REQUIRED_ARRAY_COUNT)
            .ok_or_else(|| invalid_argument("CSR directory entry count overflows usize"))?;
        let max_entries = page_size
            .checked_sub(DIRECTORY_HEADER_LEN)
            .ok_or_else(|| invalid_argument("page is shorter than a CSR directory header"))?
            / DIRECTORY_ENTRY_LEN;
        if entry_count > max_entries {
            return Err(invalid_argument(format!(
                "CSR group needs {entry_count} directory entries but capacity is {max_entries}"
            )));
        }
        validate_structure(self)
    }

    fn encode_payloads(&self) -> DevonResult<Vec<Vec<u8>>> {
        let mut payloads = Vec::with_capacity(REQUIRED_ARRAY_COUNT + self.column_count());
        payloads.push(encode_offsets(&self.offsets));
        payloads.push(encode_neighbors(&self.neighbors));
        for (index, (column, logical_type)) in self.columns.iter().zip(&self.types).enumerate() {
            payloads.push(encode_column(column, logical_type, index)?);
        }
        Ok(payloads)
    }
}

fn checkpoint_estimate(
    row_count: usize,
    edge_count: usize,
    types: &[LogicalType],
    property_bytes: usize,
) -> DevonResult<CsrCheckpointEstimate> {
    let offset_count = row_count
        .checked_add(1)
        .ok_or_else(|| invalid_argument("CSR checkpoint offset count overflows usize"))?;
    let offsets_bytes = offset_count
        .checked_mul(size_of::<u32>())
        .ok_or_else(|| invalid_argument("CSR checkpoint offsets estimate overflows usize"))?;
    let neighbor_bytes = edge_count
        .checked_mul(size_of::<u64>())
        .ok_or_else(|| invalid_argument("CSR checkpoint neighbors estimate overflows usize"))?;
    let value_slots = edge_count
        .checked_mul(types.len())
        .and_then(|count| count.checked_mul(size_of::<Value>()))
        .ok_or_else(|| invalid_argument("CSR checkpoint value estimate overflows usize"))?;
    let container_bytes = size_of::<CsrGroup>()
        .checked_add(types.len().saturating_mul(size_of::<LogicalType>()))
        .and_then(|total| total.checked_add(types.len().saturating_mul(size_of::<Vec<Value>>())))
        .ok_or_else(|| invalid_argument("CSR checkpoint container estimate overflows usize"))?;
    let resident_bytes = container_bytes
        .checked_add(offsets_bytes)
        .and_then(|total| total.checked_add(neighbor_bytes))
        .and_then(|total| total.checked_add(value_slots))
        .and_then(|total| total.checked_add(property_bytes))
        .ok_or_else(|| invalid_argument("CSR checkpoint resident estimate overflows usize"))?;
    let encoded_bytes = offsets_bytes
        .checked_add(neighbor_bytes)
        .and_then(|total| total.checked_add(property_bytes))
        .and_then(|total| {
            total.checked_add(
                (types.len() + REQUIRED_ARRAY_COUNT).saturating_mul(size_of::<Vec<u8>>()),
            )
        })
        .ok_or_else(|| invalid_argument("CSR checkpoint encoding estimate overflows usize"))?;
    Ok(CsrCheckpointEstimate {
        row_count,
        edge_count,
        resident_bytes,
        encoded_bytes,
        property_bytes,
    })
}

fn validate_values(types: &[LogicalType], values: &[Value]) -> DevonResult<()> {
    if values.len() != types.len() {
        return Err(invalid_argument(format!(
            "relationship property arity mismatch: expected {}, actual {}",
            types.len(),
            values.len()
        )));
    }
    for (index, (value, logical_type)) in values.iter().zip(types).enumerate() {
        if !value.matches_type(logical_type) {
            return Err(invalid_argument(format!(
                "property column {index} value {value} does not match expected type {logical_type}"
            )));
        }
    }
    Ok(())
}

fn validate_structure(group: &CsrGroup) -> DevonResult<()> {
    if group.offsets.first() != Some(&0) {
        return Err(invalid_argument("CSR offsets do not start at zero"));
    }
    if group.offsets.windows(2).any(|pair| pair[0] > pair[1]) {
        return Err(invalid_argument("CSR offsets are not monotonic"));
    }
    if group.offsets.last().copied().map(u64::from) != Some(group.edge_count() as u64) {
        return Err(invalid_argument(
            "CSR final offset does not equal edge count",
        ));
    }
    if group
        .columns
        .iter()
        .any(|column| column.len() != group.edge_count())
    {
        return Err(invalid_argument(
            "CSR property column length does not equal edge count",
        ));
    }
    Ok(())
}

fn encode_directory(
    row_count: usize,
    edge_count: usize,
    entries: &[ArrayEntry],
    page_size: usize,
) -> DevonResult<Vec<u8>> {
    let row_count =
        u32::try_from(row_count).map_err(|_| invalid_argument("CSR row count exceeds u32"))?;
    let edge_count =
        u32::try_from(edge_count).map_err(|_| invalid_argument("CSR edge count exceeds u32"))?;
    let column_count = entries
        .len()
        .checked_sub(REQUIRED_ARRAY_COUNT)
        .ok_or_else(|| invalid_argument("CSR directory lacks required arrays"))?;
    let column_count = u32::try_from(column_count)
        .map_err(|_| invalid_argument("CSR column count exceeds u32"))?;
    let mut page = vec![0_u8; page_size];
    page[..4].copy_from_slice(DIRECTORY_MAGIC);
    page[4..8].copy_from_slice(&row_count.to_le_bytes());
    page[8..12].copy_from_slice(&edge_count.to_le_bytes());
    page[12..16].copy_from_slice(&column_count.to_le_bytes());
    for (index, entry) in entries.iter().enumerate() {
        encode_entry(&mut page, index, *entry);
    }
    Ok(page)
}

fn encode_entry(page: &mut [u8], index: usize, entry: ArrayEntry) {
    let offset = DIRECTORY_HEADER_LEN + index * DIRECTORY_ENTRY_LEN;
    page[offset..offset + 8].copy_from_slice(&entry.first_page.to_le_bytes());
    page[offset + 8..offset + 12].copy_from_slice(&entry.byte_len.to_le_bytes());
    page[offset + 12..offset + 16].copy_from_slice(&entry.checksum.to_le_bytes());
}

pub(crate) fn decode_directory(
    page: &[u8],
    types: &[LogicalType],
) -> DevonResult<(usize, usize, Vec<ArrayEntry>)> {
    if page.len() < DIRECTORY_HEADER_LEN {
        return Err(corrupt("CSR directory is shorter than its header"));
    }
    if &page[..4] != DIRECTORY_MAGIC {
        return Err(corrupt("CSR directory magic is not RCSR"));
    }
    let row_count = read_u32(page, 4) as usize;
    if row_count == 0 {
        return Err(corrupt("CSR directory row_count is zero"));
    }
    let edge_count = read_u32(page, 8) as usize;
    if edge_count == 0 {
        return Err(corrupt("CSR directory edge_count is zero"));
    }
    let column_count = read_u32(page, 12) as usize;
    if column_count != types.len() {
        return Err(corrupt(format!(
            "CSR column_count is {column_count}, expected {} from schema",
            types.len()
        )));
    }
    let entry_count = column_count
        .checked_add(REQUIRED_ARRAY_COUNT)
        .ok_or_else(|| corrupt("CSR directory entry count overflows usize"))?;
    let entries_end = directory_entries_end(entry_count, page.len())?;
    if page[entries_end..].iter().any(|byte| *byte != 0) {
        return Err(corrupt("CSR directory padding is not zero"));
    }
    let entries = (0..entry_count)
        .map(|index| decode_entry(page, index))
        .collect();
    Ok((row_count, edge_count, entries))
}

/// Enumerates every page one persisted CSR group occupies: the directory
/// page plus each array payload run. Catalog publication uses this inventory
/// to expand the superseded set; see `node_group::group_page_inventory`.
pub fn csr_page_inventory(
    pager: &Pager,
    directory_page: u64,
    types: &[LogicalType],
) -> DevonResult<Vec<u64>> {
    let page = pager.read_page(directory_page)?;
    let (_row_count, _edge_count, entries) = decode_directory(&page, types)?;
    let page_size = pager.superblock().page_size as usize;
    let mut pages = vec![directory_page];
    for entry in &entries {
        let count = (entry.byte_len as usize).div_ceil(page_size).max(1);
        for index in 0..count as u64 {
            let page_id = entry
                .first_page
                .checked_add(index)
                .ok_or_else(|| corrupt("CSR payload page run overflows u64"))?;
            pages.push(page_id);
        }
    }
    Ok(pages)
}

fn directory_entries_end(entry_count: usize, page_len: usize) -> DevonResult<usize> {
    let entries_len = entry_count
        .checked_mul(DIRECTORY_ENTRY_LEN)
        .ok_or_else(|| corrupt("CSR directory entry length overflows usize"))?;
    let end = DIRECTORY_HEADER_LEN
        .checked_add(entries_len)
        .ok_or_else(|| corrupt("CSR directory length overflows usize"))?;
    if end > page_len {
        return Err(corrupt("CSR column_count exceeds directory page capacity"));
    }
    Ok(end)
}

fn decode_entry(page: &[u8], index: usize) -> ArrayEntry {
    let offset = DIRECTORY_HEADER_LEN + index * DIRECTORY_ENTRY_LEN;
    ArrayEntry {
        first_page: read_u64(page, offset),
        byte_len: read_u32(page, offset + 8),
        checksum: read_u32(page, offset + 12),
    }
}

fn offsets_payload_len(row_count: usize) -> Option<usize> {
    row_count.checked_add(1)?.checked_mul(4)
}

fn neighbors_payload_len(edge_count: usize) -> Option<usize> {
    edge_count.checked_mul(8)
}

fn validate_fixed_payload_len(
    entry: ArrayEntry,
    expected: Option<usize>,
    name: &str,
) -> DevonResult<()> {
    let expected = expected.ok_or_else(|| corrupt(format!("{name} payload length overflows")))?;
    if entry.byte_len as usize != expected {
        return Err(corrupt(format!(
            "{name} byte_len is {}, expected exactly {expected}",
            entry.byte_len
        )));
    }
    Ok(())
}

fn validate_column_payload_len(
    entry: ArrayEntry,
    edge_count: usize,
    logical_type: &LogicalType,
    column_index: usize,
) -> DevonResult<()> {
    let actual = entry.byte_len as usize;
    if matches!(logical_type, LogicalType::String) {
        let minimum = string_payload_prefix_len(edge_count).ok_or_else(|| {
            corrupt(format!(
                "property column {column_index} string prefix length overflows"
            ))
        })?;
        if actual < minimum {
            return Err(corrupt(format!(
                "property column {column_index} byte_len is {actual}, smaller than string prefix {minimum}"
            )));
        }
        return Ok(());
    }
    let expected = fixed_payload_len(edge_count, logical_type).ok_or_else(|| {
        corrupt(format!(
            "property column {column_index} implied payload length overflows"
        ))
    })?;
    if actual != expected {
        return Err(corrupt(format!(
            "property column {column_index} byte_len is {actual}, expected exactly {expected} for {logical_type}"
        )));
    }
    Ok(())
}

fn encode_offsets(offsets: &[u32]) -> Vec<u8> {
    offsets
        .iter()
        .flat_map(|offset| offset.to_le_bytes())
        .collect()
}

fn decode_offsets(payload: &[u8], row_count: usize, edge_count: usize) -> DevonResult<Vec<u32>> {
    let offsets: Vec<u32> = payload
        .as_chunks::<4>()
        .0
        .iter()
        .map(|bytes| u32::from_le_bytes(copy_array(bytes)))
        .collect();
    if offsets.len() != row_count + 1 {
        return Err(corrupt("CSR offsets array has the wrong element count"));
    }
    if offsets.first() != Some(&0) {
        return Err(corrupt("CSR offsets do not start at zero"));
    }
    if offsets.windows(2).any(|pair| pair[0] > pair[1]) {
        return Err(corrupt("CSR offsets are not monotonic"));
    }
    if offsets.last().copied().map(u64::from) != Some(edge_count as u64) {
        return Err(corrupt(format!(
            "CSR final offset does not equal edge_count {edge_count}"
        )));
    }
    Ok(offsets)
}

fn encode_neighbors(neighbors: &[u64]) -> Vec<u8> {
    neighbors
        .iter()
        .flat_map(|neighbor| neighbor.to_le_bytes())
        .collect()
}

fn decode_neighbors(payload: &[u8], row_count: Option<u64>) -> DevonResult<Vec<u64>> {
    let neighbors: Vec<u64> = payload
        .as_chunks::<8>()
        .0
        .iter()
        .map(|bytes| u64::from_le_bytes(copy_array(bytes)))
        .collect();
    if let Some(row_count) = row_count
        && let Some((index, neighbor)) = neighbors
            .iter()
            .enumerate()
            .find(|(_, neighbor)| **neighbor >= row_count)
    {
        return Err(corrupt(format!(
            "CSR neighbor {index} offset {neighbor} is outside endpoint row count {row_count}"
        )));
    }
    Ok(neighbors)
}

fn write_payload(pager: &Pager, payload: &[u8], page_size: usize) -> DevonResult<u64> {
    let page_count = payload.len().div_ceil(page_size);
    let page_ids = allocate_contiguous_pages(pager, page_count)?;
    for (index, page_id) in page_ids.iter().enumerate() {
        let start = index * page_size;
        let end = payload.len().min(start + page_size);
        let mut page = vec![0_u8; page_size];
        page[..end - start].copy_from_slice(&payload[start..end]);
        pager.write_page(*page_id, &page)?;
    }
    Ok(page_ids[0])
}

fn allocate_contiguous_pages(pager: &Pager, page_count: usize) -> DevonResult<Vec<u64>> {
    if page_count == 0 {
        return Err(invalid_argument("CSR payload cannot occupy zero pages"));
    }
    // One atomic run allocation: the pager guarantees contiguity whether
    // the run is reused from the free-page ledger or appended.
    let first_page = pager.allocate_run(page_count)?;
    (0..page_count)
        .map(|index| {
            first_page
                .checked_add(index as u64)
                .ok_or_else(|| invalid_argument("CSR payload page run overflows u64"))
        })
        .collect()
}

fn read_checked_payload(
    pager: &Pager,
    entry: ArrayEntry,
    page_size: usize,
    name: &str,
) -> DevonResult<Vec<u8>> {
    let payload = read_payload(pager, entry, page_size, name)?;
    if crc32c(&payload) != entry.checksum {
        return Err(corrupt(format!("{name} payload CRC-32C does not match")));
    }
    Ok(payload)
}

fn read_payload(
    pager: &Pager,
    entry: ArrayEntry,
    page_size: usize,
    name: &str,
) -> DevonResult<Vec<u8>> {
    let byte_len = entry.byte_len as usize;
    let page_count = byte_len.div_ceil(page_size);
    let mut payload = Vec::with_capacity(byte_len);
    for index in 0..page_count {
        let page_offset = u64::try_from(index)
            .map_err(|_| corrupt(format!("{name} payload page index exceeds u64")))?;
        let page_id = entry
            .first_page
            .checked_add(page_offset)
            .ok_or_else(|| corrupt(format!("{name} payload page run overflows u64")))?;
        let page = read_referenced_page(pager, page_id, name)?;
        let take = (byte_len - payload.len()).min(page_size);
        payload.extend_from_slice(&page[..take]);
        if take < page_size && page[take..].iter().any(|byte| *byte != 0) {
            return Err(corrupt(format!("{name} payload page padding is not zero")));
        }
    }
    Ok(payload)
}

fn read_referenced_page(pager: &Pager, page_id: u64, name: &str) -> DevonResult<Vec<u8>> {
    match pager.read_page(page_id) {
        Ok(page) => Ok(page),
        Err(DevonError::InvalidArgument { context }) => Err(corrupt(format!(
            "{name} references invalid payload page {page_id}: {context}"
        ))),
        Err(error) => Err(error),
    }
}

fn encode_column(
    column: &[Value],
    logical_type: &LogicalType,
    column_index: usize,
) -> DevonResult<Vec<u8>> {
    let mut payload = encode_validity(column);
    match logical_type {
        LogicalType::Bool => encode_bools(column, &mut payload, column_index)?,
        LogicalType::Int64 => encode_int64s(column, &mut payload, column_index)?,
        LogicalType::Float64 => encode_float64s(column, &mut payload, column_index)?,
        LogicalType::String => encode_strings(column, &mut payload, column_index)?,
        LogicalType::Vector { dim } => {
            encode_vectors(column, *dim as usize, &mut payload, column_index)?;
        }
        LogicalType::VectorEncoded { .. } => {
            return Err(invalid_argument(format!(
                "relationship property column {column_index} has type {logical_type}; \
                 VectorEncoded relationship properties are not supported"
            )));
        }
        // The schema floor rejects GeoPoint relationship properties;
        // defended like VectorEncoded above.
        LogicalType::GeoPoint => {
            return Err(invalid_argument(format!(
                "relationship property column {column_index} has type GeoPoint; \
                 GeoPoint relationship properties are not supported"
            )));
        }
        // Scalar-v2 types are rejected by the schema floor and defended
        // like the two arms above.
        LogicalType::Timestamp
        | LogicalType::Bytes
        | LogicalType::Decimal { .. }
        | LogicalType::Json => {
            return Err(invalid_argument(format!(
                "relationship property column {column_index} has type \
                 {logical_type}; the scalar-v2 storage codec is staged"
            )));
        }
    }
    if payload.len() > u32::MAX as usize {
        return Err(invalid_argument(format!(
            "property column {column_index} payload length exceeds u32"
        )));
    }
    Ok(payload)
}

fn encode_validity(column: &[Value]) -> Vec<u8> {
    let mut validity = vec![0_u8; bitmap_len(column.len())];
    for (row, value) in column.iter().enumerate() {
        if !matches!(value, Value::Null) {
            set_bit(&mut validity, row);
        }
    }
    validity
}

fn encode_bools(column: &[Value], payload: &mut Vec<u8>, index: usize) -> DevonResult<()> {
    let mut values = vec![0_u8; bitmap_len(column.len())];
    for (row, value) in column.iter().enumerate() {
        match value {
            Value::Null | Value::Bool(false) => {}
            Value::Bool(true) => set_bit(&mut values, row),
            _ => return Err(invalid_stored_value(index, row, value)),
        }
    }
    payload.extend(values);
    Ok(())
}

fn encode_int64s(column: &[Value], payload: &mut Vec<u8>, index: usize) -> DevonResult<()> {
    for (row, value) in column.iter().enumerate() {
        match value {
            Value::Null => payload.extend([0_u8; 8]),
            Value::Int64(value) => payload.extend(value.to_le_bytes()),
            _ => return Err(invalid_stored_value(index, row, value)),
        }
    }
    Ok(())
}

fn encode_float64s(column: &[Value], payload: &mut Vec<u8>, index: usize) -> DevonResult<()> {
    for (row, value) in column.iter().enumerate() {
        match value {
            Value::Null => payload.extend([0_u8; 8]),
            Value::Float64(value) => payload.extend(value.to_le_bytes()),
            _ => return Err(invalid_stored_value(index, row, value)),
        }
    }
    Ok(())
}

fn encode_vectors(
    column: &[Value],
    dim: usize,
    payload: &mut Vec<u8>,
    index: usize,
) -> DevonResult<()> {
    let slot_len = dim
        .checked_mul(4)
        .ok_or_else(|| invalid_argument("vector slot length overflows usize"))?;
    for (row, value) in column.iter().enumerate() {
        match value {
            Value::Null => payload.resize(payload.len() + slot_len, 0),
            Value::Vector(vector) if vector.len() == dim => {
                for element in vector {
                    payload.extend(element.to_le_bytes());
                }
            }
            _ => return Err(invalid_stored_value(index, row, value)),
        }
    }
    Ok(())
}

fn encode_strings(column: &[Value], payload: &mut Vec<u8>, index: usize) -> DevonResult<()> {
    let offsets_start = payload.len();
    let offsets_len = column
        .len()
        .checked_add(1)
        .and_then(|count| count.checked_mul(4))
        .ok_or_else(|| invalid_argument("string offsets length overflows usize"))?;
    payload.resize(offsets_start + offsets_len, 0);
    let mut heap_len = 0_u32;
    for (row, value) in column.iter().enumerate() {
        if let Value::String(value) = value {
            let value_len = u32::try_from(value.len())
                .map_err(|_| invalid_argument("string value length exceeds u32"))?;
            heap_len = heap_len
                .checked_add(value_len)
                .ok_or_else(|| invalid_argument("string heap length exceeds u32"))?;
            payload.extend(value.as_bytes());
        } else if !matches!(value, Value::Null) {
            return Err(invalid_stored_value(index, row, value));
        }
        let offset = offsets_start + (row + 1) * 4;
        payload[offset..offset + 4].copy_from_slice(&heap_len.to_le_bytes());
    }
    Ok(())
}

fn fixed_payload_len(row_count: usize, logical_type: &LogicalType) -> Option<usize> {
    let validity_len = bitmap_len(row_count);
    let values_len = match logical_type {
        LogicalType::Bool => validity_len,
        LogicalType::Int64 | LogicalType::Float64 => row_count.checked_mul(8)?,
        LogicalType::Vector { dim } => row_count.checked_mul(*dim as usize)?.checked_mul(4)?,
        LogicalType::String
        | LogicalType::VectorEncoded { .. }
        | LogicalType::GeoPoint
        | LogicalType::Timestamp
        | LogicalType::Bytes
        | LogicalType::Decimal { .. }
        | LogicalType::Json => {
            return None;
        }
    };
    validity_len.checked_add(values_len)
}

fn string_payload_prefix_len(row_count: usize) -> Option<usize> {
    bitmap_len(row_count).checked_add(row_count.checked_add(1)?.checked_mul(4)?)
}

fn decode_column(
    payload: &[u8],
    row_count: usize,
    logical_type: &LogicalType,
    column_index: usize,
) -> DevonResult<Vec<Value>> {
    let validity_len = bitmap_len(row_count);
    let validity = &payload[..validity_len];
    validate_bitmap_padding(validity, row_count, column_index, "validity")?;
    let values = &payload[validity_len..];
    match logical_type {
        LogicalType::Bool => decode_bools(validity, values, row_count, column_index),
        LogicalType::Int64 => decode_int64s(validity, values, row_count, column_index),
        LogicalType::Float64 => decode_float64s(validity, values, row_count, column_index),
        LogicalType::String => decode_strings(validity, values, row_count, column_index),
        LogicalType::Vector { dim } => {
            decode_vectors(validity, values, row_count, *dim as usize, column_index)
        }
        LogicalType::VectorEncoded { .. } => Err(corrupt(format!(
            "column {column_index} has type {logical_type}, but no writer can \
             persist VectorEncoded payloads yet"
        ))),
        // Unreachable: the schema floor keeps GeoPoint out of catalogs
        // (docs/GEO.md §5 staging).
        LogicalType::GeoPoint => Err(corrupt(format!(
            "column {column_index} has type GeoPoint, which has no persisted layout"
        ))),
        // Unreachable: the schema floor keeps staged scalar-v2 types out of
        // catalogs.
        LogicalType::Timestamp
        | LogicalType::Bytes
        | LogicalType::Decimal { .. }
        | LogicalType::Json => Err(corrupt(format!(
            "column {column_index} has type {logical_type}, which has no \
             persisted layout (scalar-v2 is staged)"
        ))),
    }
}

fn decode_bools(
    validity: &[u8],
    values: &[u8],
    row_count: usize,
    index: usize,
) -> DevonResult<Vec<Value>> {
    validate_bitmap_padding(values, row_count, index, "Bool values")?;
    let mut column = Vec::with_capacity(row_count);
    for row in 0..row_count {
        if bit_is_set(validity, row) {
            column.push(Value::Bool(bit_is_set(values, row)));
        } else {
            if bit_is_set(values, row) {
                return Err(corrupt(format!(
                    "property column {index} row {row} null Bool slot is not zero"
                )));
            }
            column.push(Value::Null);
        }
    }
    Ok(column)
}

fn decode_int64s(
    validity: &[u8],
    values: &[u8],
    row_count: usize,
    index: usize,
) -> DevonResult<Vec<Value>> {
    decode_fixed_slots(validity, values, row_count, 8, index, |slot| {
        Value::Int64(i64::from_le_bytes(copy_array(slot)))
    })
}

fn decode_float64s(
    validity: &[u8],
    values: &[u8],
    row_count: usize,
    index: usize,
) -> DevonResult<Vec<Value>> {
    decode_fixed_slots(validity, values, row_count, 8, index, |slot| {
        Value::Float64(f64::from_le_bytes(copy_array(slot)))
    })
}

fn decode_vectors(
    validity: &[u8],
    values: &[u8],
    row_count: usize,
    dim: usize,
    index: usize,
) -> DevonResult<Vec<Value>> {
    let slot_len = dim.checked_mul(4).ok_or_else(|| {
        corrupt(format!(
            "property column {index} vector slot length overflows"
        ))
    })?;
    decode_fixed_slots(validity, values, row_count, slot_len, index, |slot| {
        Value::Vector(
            slot.as_chunks::<4>()
                .0
                .iter()
                .map(|bytes| f32::from_le_bytes(copy_array(bytes)))
                .collect(),
        )
    })
}

fn decode_fixed_slots<F>(
    validity: &[u8],
    values: &[u8],
    row_count: usize,
    slot_len: usize,
    index: usize,
    decode: F,
) -> DevonResult<Vec<Value>>
where
    F: Fn(&[u8]) -> Value,
{
    let mut column = Vec::with_capacity(row_count);
    for row in 0..row_count {
        let start = row * slot_len;
        let slot = &values[start..start + slot_len];
        if bit_is_set(validity, row) {
            column.push(decode(slot));
        } else {
            if slot.iter().any(|byte| *byte != 0) {
                return Err(corrupt(format!(
                    "property column {index} row {row} null value slot is not zero"
                )));
            }
            column.push(Value::Null);
        }
    }
    Ok(column)
}

fn decode_strings(
    validity: &[u8],
    values: &[u8],
    row_count: usize,
    index: usize,
) -> DevonResult<Vec<Value>> {
    let offsets_len = (row_count + 1) * 4;
    let offsets: Vec<usize> = values[..offsets_len]
        .as_chunks::<4>()
        .0
        .iter()
        .map(|bytes| u32::from_le_bytes(copy_array(bytes)) as usize)
        .collect();
    let heap = &values[offsets_len..];
    validate_string_offsets(&offsets, heap.len(), index)?;
    let mut column = Vec::with_capacity(row_count);
    for row in 0..row_count {
        let start = offsets[row];
        let end = offsets[row + 1];
        if bit_is_set(validity, row) {
            let value = str::from_utf8(&heap[start..end]).map_err(|error| {
                corrupt(format!(
                    "property column {index} row {row} string is not valid UTF-8: {error}"
                ))
            })?;
            column.push(Value::String(value.to_owned()));
        } else {
            if start != end {
                return Err(corrupt(format!(
                    "property column {index} row {row} null String slot is not zero length"
                )));
            }
            column.push(Value::Null);
        }
    }
    Ok(column)
}

fn validate_string_offsets(offsets: &[usize], heap_len: usize, index: usize) -> DevonResult<()> {
    if offsets.first() != Some(&0) {
        return Err(corrupt(format!(
            "property column {index} string offsets do not start at zero"
        )));
    }
    if offsets.windows(2).any(|pair| pair[0] > pair[1]) {
        return Err(corrupt(format!(
            "property column {index} string offsets are not monotonic"
        )));
    }
    if offsets.last() != Some(&heap_len) {
        return Err(corrupt(format!(
            "property column {index} final string offset does not equal heap length {heap_len}"
        )));
    }
    Ok(())
}

fn validate_bitmap_padding(
    bitmap: &[u8],
    row_count: usize,
    index: usize,
    name: &str,
) -> DevonResult<()> {
    let used_bits = row_count % 8;
    if used_bits == 0 {
        return Ok(());
    }
    let used_mask = (1_u8 << used_bits) - 1;
    if bitmap.last().is_some_and(|byte| byte & !used_mask != 0) {
        return Err(corrupt(format!(
            "property column {index} {name} bitmap trailing bits are not zero"
        )));
    }
    Ok(())
}

fn bitmap_len(row_count: usize) -> usize {
    row_count.div_ceil(8)
}

fn set_bit(bitmap: &mut [u8], row: usize) {
    bitmap[row / 8] |= 1 << (row % 8);
}

fn bit_is_set(bitmap: &[u8], row: usize) -> bool {
    bitmap[row / 8] & (1 << (row % 8)) != 0
}

fn copy_array<const N: usize>(bytes: &[u8]) -> [u8; N] {
    let mut array = [0_u8; N];
    array.copy_from_slice(bytes);
    array
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(copy_array(&bytes[offset..offset + 4]))
}

fn read_u64(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(copy_array(&bytes[offset..offset + 8]))
}

fn invalid_stored_value(column: usize, row: usize, value: &Value) -> DevonError {
    invalid_argument(format!(
        "property column {column} edge {row} has invalid stored value {value}"
    ))
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

#[cfg(test)]
mod tests {
    use std::fs::{self, OpenOptions};
    use std::io::{Read, Seek, SeekFrom, Write};
    use std::path::{Path, PathBuf};

    use crc32c::crc32c;
    use devondb_types::{DevonError, logical_type::LogicalType, value::Value};
    use tempfile::{TempDir, tempdir};

    use super::{ArrayEntry, CsrGroup, DIRECTORY_HEADER_LEN, Pager, decode_entry, read_u32};

    const PAGE_SIZE: u32 = 4096;
    const DB_ID: [u8; 16] = *b"csr-group-db-id!";

    fn property_types() -> Vec<LogicalType> {
        vec![
            LogicalType::Int64,
            LogicalType::String,
            LogicalType::Vector { dim: 2 },
        ]
    }

    fn property_group() -> CsrGroup {
        let mut group = CsrGroup::new(4, property_types()).unwrap();
        group
            .push_edge(
                0,
                8,
                vec![
                    Value::Int64(11),
                    Value::String("first".to_owned()),
                    Value::Vector(vec![1.0, 2.0]),
                ],
            )
            .unwrap();
        group
            .push_edge(0, 9, vec![Value::Null, Value::Null, Value::Null])
            .unwrap();
        group
            .push_edge(
                3,
                2,
                vec![
                    Value::Int64(-4),
                    Value::String(String::new()),
                    Value::Vector(vec![-0.0, 3.5]),
                ],
            )
            .unwrap();
        group
    }

    fn corruption_group() -> CsrGroup {
        let mut group = CsrGroup::new(3, Vec::new()).unwrap();
        group.push_edge(0, 1, Vec::new()).unwrap();
        group.push_edge(2, 3, Vec::new()).unwrap();
        group
    }

    #[test]
    fn property_columns_and_nulls_round_trip() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("properties.devondb");
        let pager = Pager::create(&path, PAGE_SIZE, DB_ID).unwrap();
        let group = property_group();
        let directory_page = group.write(&pager).unwrap();
        let page = pager.read_page(directory_page).unwrap();

        assert_eq!(&page[..4], b"RCSR");
        assert_eq!(read_u32(&page, 4), 4);
        assert_eq!(read_u32(&page, 8), 3);
        assert_eq!(read_u32(&page, 12), 3);
        drop(pager);

        let pager = Pager::open(path).unwrap();
        let decoded =
            CsrGroup::read_checked(&pager, directory_page, &property_types(), 10).unwrap();
        assert_eq!(decoded, group);
        assert_eq!(decoded.edge_range(0), Some(0..2));
        assert_eq!(decoded.edge_range(1), Some(2..2));
        assert_eq!(decoded.edge_range(3), Some(2..3));
        assert_eq!(decoded.neighbor(1), Some(9));
        assert_eq!(decoded.value(1, 0), Some(&Value::Null));
    }

    #[test]
    fn zero_property_relationship_round_trips() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("zero-properties.devondb");
        let pager = Pager::create(path, PAGE_SIZE, DB_ID).unwrap();
        let mut group = CsrGroup::new(2, Vec::new()).unwrap();
        group.push_edge(1, 42, Vec::new()).unwrap();

        let directory_page = group.write(&pager).unwrap();
        let page = pager.read_page(directory_page).unwrap();
        assert_eq!(read_u32(&page, 12), 0);
        assert_eq!(CsrGroup::read(&pager, directory_page, &[]).unwrap(), group);
    }

    #[test]
    fn identical_groups_produce_identical_database_files() {
        let first_directory = tempdir().unwrap();
        let second_directory = tempdir().unwrap();
        let first_path = first_directory.path().join("first.devondb");
        let second_path = second_directory.path().join("second.devondb");
        write_group_file(&first_path, &property_group());
        write_group_file(&second_path, &property_group());

        assert_eq!(
            fs::read(first_path).unwrap(),
            fs::read(second_path).unwrap()
        );
    }

    #[test]
    fn empty_group_is_never_written() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("empty.devondb");
        let pager = Pager::create(path, PAGE_SIZE, DB_ID).unwrap();
        let group = CsrGroup::new(2, Vec::new()).unwrap();

        assert_invalid(group.write(&pager));
        assert_invalid(CsrGroup::new(0, Vec::new()));
    }

    #[test]
    fn bad_magic_is_corruption() {
        let (_directory, path, directory_page) = write_corruption_group("bad-magic");
        write_file_bytes(&path, directory_page * u64::from(PAGE_SIZE), b"NOPE");

        assert_group_corrupt(&path, directory_page, None);
    }

    #[test]
    fn wrong_array_byte_len_is_corruption() {
        let (_directory, path, directory_page) = write_corruption_group("bad-length");
        let byte_len_offset =
            directory_page * u64::from(PAGE_SIZE) + (DIRECTORY_HEADER_LEN + 8) as u64;
        write_file_bytes(&path, byte_len_offset, &11_u32.to_le_bytes());

        assert_group_corrupt(&path, directory_page, None);
    }

    #[test]
    fn payload_crc_failure_is_corruption() {
        let (_directory, path, directory_page) = write_corruption_group("bad-crc");
        let entry = read_directory_entry(&path, directory_page, 1);
        flip_file_byte(&path, entry.first_page * u64::from(PAGE_SIZE));

        assert_group_corrupt(&path, directory_page, None);
    }

    #[test]
    fn non_monotonic_offsets_are_corruption() {
        let (_directory, path, directory_page) = write_corruption_group("bad-monotonic");
        let entry = read_directory_entry(&path, directory_page, 0);
        write_file_bytes(
            &path,
            entry.first_page * u64::from(PAGE_SIZE) + 4,
            &2_u32.to_le_bytes(),
        );
        refresh_payload_checksum(&path, directory_page, 0);

        assert_group_corrupt(&path, directory_page, None);
    }

    #[test]
    fn final_offset_must_equal_edge_count() {
        let (_directory, path, directory_page) = write_corruption_group("bad-final");
        let entry = read_directory_entry(&path, directory_page, 0);
        write_file_bytes(
            &path,
            entry.first_page * u64::from(PAGE_SIZE) + 12,
            &1_u32.to_le_bytes(),
        );
        refresh_payload_checksum(&path, directory_page, 0);

        assert_group_corrupt(&path, directory_page, None);
    }

    #[test]
    fn neighbor_outside_endpoint_row_count_is_corruption() {
        let (_directory, path, directory_page) = write_corruption_group("bad-neighbor");
        let entry = read_directory_entry(&path, directory_page, 1);
        write_file_bytes(
            &path,
            entry.first_page * u64::from(PAGE_SIZE),
            &99_u64.to_le_bytes(),
        );
        refresh_payload_checksum(&path, directory_page, 1);

        assert_group_corrupt(&path, directory_page, Some(4));
    }

    fn write_group_file(path: &Path, group: &CsrGroup) {
        let pager = Pager::create(path, PAGE_SIZE, DB_ID).unwrap();
        group.write(&pager).unwrap();
    }

    fn write_corruption_group(name: &str) -> (TempDir, PathBuf, u64) {
        let directory = tempdir().unwrap();
        let path = directory.path().join(format!("{name}.devondb"));
        let pager = Pager::create(&path, PAGE_SIZE, DB_ID).unwrap();
        let directory_page = corruption_group().write(&pager).unwrap();
        drop(pager);
        (directory, path, directory_page)
    }

    fn read_directory_entry(path: &Path, directory_page: u64, index: usize) -> ArrayEntry {
        let pager = Pager::open(path).unwrap();
        decode_entry(&pager.read_page(directory_page).unwrap(), index)
    }

    fn refresh_payload_checksum(path: &Path, directory_page: u64, index: usize) {
        let entry = read_directory_entry(path, directory_page, index);
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .unwrap();
        file.seek(SeekFrom::Start(entry.first_page * u64::from(PAGE_SIZE)))
            .unwrap();
        let mut payload = vec![0_u8; entry.byte_len as usize];
        file.read_exact(&mut payload).unwrap();
        let checksum_offset =
            directory_page * u64::from(PAGE_SIZE) + (DIRECTORY_HEADER_LEN + index * 16 + 12) as u64;
        file.seek(SeekFrom::Start(checksum_offset)).unwrap();
        file.write_all(&crc32c(&payload).to_le_bytes()).unwrap();
        file.sync_all().unwrap();
    }

    fn flip_file_byte(path: &Path, offset: u64) {
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .unwrap();
        file.seek(SeekFrom::Start(offset)).unwrap();
        let mut byte = [0_u8; 1];
        file.read_exact(&mut byte).unwrap();
        byte[0] ^= 0xff;
        file.seek(SeekFrom::Start(offset)).unwrap();
        file.write_all(&byte).unwrap();
        file.sync_all().unwrap();
    }

    fn write_file_bytes(path: &Path, offset: u64, bytes: &[u8]) {
        let mut file = OpenOptions::new().write(true).open(path).unwrap();
        file.seek(SeekFrom::Start(offset)).unwrap();
        file.write_all(bytes).unwrap();
        file.sync_all().unwrap();
    }

    fn assert_group_corrupt(path: &Path, directory_page: u64, neighbor_limit: Option<u64>) {
        let pager = Pager::open(path).unwrap();
        let result = if let Some(limit) = neighbor_limit {
            CsrGroup::read_checked(&pager, directory_page, &[], limit)
        } else {
            CsrGroup::read(&pager, directory_page, &[])
        };
        assert!(matches!(result, Err(DevonError::Corrupt { .. })));
    }

    fn assert_invalid<T>(result: Result<T, DevonError>) {
        assert!(matches!(result, Err(DevonError::InvalidArgument { .. })));
    }
}
