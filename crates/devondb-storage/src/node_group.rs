//! Node groups: columnar property chunks for a contiguous run of a node
//! table's rows.
//!
//! On-disk layout per `docs/FORMAT.md` § Node group pages (binding): one
//! directory page referencing, per column, a contiguous run of payload
//! pages. Column types are never recorded here — the catalog schema is the
//! single source of truth.

use std::cell::RefCell;
use std::str;

use crc32c::crc32c;
use devondb_types::{
    Decimal128, DevonError, DevonResult,
    column::{Bitmap, Column},
    decimal::MAX_PRECISION,
    logical_type::{B1Rescore, LogicalType, VectorEncoding},
    value::Value,
};

use crate::{
    pager::Pager,
    vector_encoding::{decode_b1, decode_f16, decode_i8, encode_b1, encode_f16, encode_i8},
};

pub(crate) mod encodings;

use encodings::Encoding;

const DIRECTORY_MAGIC: &[u8; 4] = b"NGRP";
const DIRECTORY_HEADER_LEN: usize = 16;
const DIRECTORY_ENTRY_LEN: usize = 16;
const ZONE_MAP_DIRECTORY_FLAG: u32 = 1 << 0;
const COLUMN_ENCODINGS_DIRECTORY_FLAG: u32 = 1 << 1;
const RESERVED_READ_SAFE_DIRECTORY_FLAG: u32 = 1 << 2;
const DIRECTORY_FLAG_GOVERNORS: [(u32, u64); 3] = [
    (ZONE_MAP_DIRECTORY_FLAG, crate::superblock::ZONE_MAPS_FLAG),
    (
        COLUMN_ENCODINGS_DIRECTORY_FLAG,
        crate::superblock::COLUMN_ENCODINGS_FLAG,
    ),
    (
        RESERVED_READ_SAFE_DIRECTORY_FLAG,
        crate::superblock::RESERVED_READ_SAFE_FLAG,
    ),
];
const ZONE_MAP_RECORD_LEN: usize = 24;
const ENCODING_RECORD_LEN: usize = 4;
const SECTION_HEADER_LEN: usize = 8;
const STATS_MIN_MAX_PRESENT: u32 = 1 << 0;

#[derive(Debug, PartialEq, Eq)]
struct PendingDirectory {
    db_id: [u8; 16],
    page_id: u64,
    bytes: Vec<u8>,
}

// The pager's existing atomics say which feature kinds its writer has emitted,
// but not which directory pages it emitted. The engine's publication pipeline
// is synchronous, so thread-local exact bytes scope the exception to that
// writer call stack and cannot authorize a follower on another thread. The
// list normally lives only through a file's first feature-bearing publication;
// an exact governed directory read prunes its entry.
thread_local! {
    static PENDING_DIRECTORIES: RefCell<Vec<PendingDirectory>> = const { RefCell::new(Vec::new()) };
}

/// Maximum number of rows written in one node group by v0 writers.
pub const NODE_GROUP_CAPACITY: usize = 2048;

/// A typed, column-major group of node property values.
#[derive(Debug, Clone, PartialEq)]
pub struct NodeGroup {
    types: Vec<LogicalType>,
    columns: Vec<Vec<Value>>,
    row_count: usize,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct ColumnEntry {
    first_page: u64,
    byte_len: u32,
    checksum: u32,
}

/// One encoded zone-map endpoint, interpreted according to the catalog type.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ZoneMapValue {
    /// An `Int64` endpoint.
    Int64(i64),
    /// A `Float64` endpoint selected with [`f64::total_cmp`].
    Float64(f64),
    /// A `GeoPoint` endpoint represented by its resolution-15 atom key.
    GeoPoint(u64),
}

/// Directory-resident statistics for one main property column.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ZoneMapStats {
    /// Number of null rows in the column.
    pub null_count: u32,
    /// Least non-null value, or `None` when this type/group has no min/max.
    pub min: Option<ZoneMapValue>,
    /// Greatest non-null value, paired with [`Self::min`].
    pub max: Option<ZoneMapValue>,
}

/// A decoded node-group directory whose extension sections have been
/// validated without reading any payload page.
#[derive(Debug, Clone, PartialEq)]
pub struct NodeGroupDirectory {
    row_count: usize,
    entries: Vec<ColumnEntry>,
    zone_maps: Option<Vec<ZoneMapStats>>,
    encodings: Option<Vec<(Encoding, [u8; 3])>>,
}

impl NodeGroupDirectory {
    /// Returns the group's persisted row count.
    #[must_use]
    pub const fn row_count(&self) -> usize {
        self.row_count
    }

    /// Upper bound on requested heap bytes while decoding one String column.
    ///
    /// Includes encoded payload, decoded values, offset/validity scratch and
    /// encoding tables. Charge before `read_column_typed`, then shrink to the
    /// returned column's actual capacities. Only String columns may use this
    /// bound; it preserves the existing decoder's corruption validation.
    pub fn string_column_decode_peak_bytes(&self, column: usize) -> DevonResult<usize> {
        let entry = self
            .entries
            .get(column)
            .ok_or_else(|| invalid_argument("String decode column is out of bounds"))?;
        let payload = usize::try_from(entry.byte_len)
            .map_err(|_| corrupt("String payload exceeds address space"))?;
        let (encoding, _) = column_encoding(self.column_encodings(), column);
        let multiplier = match encoding {
            Encoding::Plain => 2,
            // Constant temporarily owns its shared String besides each row.
            Encoding::Constant => self.row_count.saturating_add(2),
            // Dictionary entries/offsets borrow the payload; each row may
            // repeat the largest entry, with six payload-widths of scratch.
            Encoding::Dictionary => self.row_count.saturating_add(7),
            // Every FSST code expands to at most eight bytes, preallocated
            // exactly by decode_row, plus the encoded payload itself.
            Encoding::Fsst => 9,
            _ => return Err(corrupt("non-String encoding in String decode bound")),
        };
        payload
            .checked_mul(multiplier)
            .and_then(|bytes| {
                bytes.checked_add((self.row_count + 1) * (std::mem::size_of::<Value>() + 16))
            })
            .and_then(|bytes| bytes.checked_add(255 * std::mem::size_of::<&[u8]>()))
            .ok_or_else(|| DevonError::BudgetExceeded {
                context: "String decode working set exceeds address space".into(),
            })
    }

    /// Bounds the transient decode heap for one selected logical column.
    ///
    /// Pass the complete main-column schema so auxiliary b1 rescore payloads
    /// can be included. The bound covers encoded payloads, decoded row slots,
    /// and temporary codec buffers; callers release it after decoding and
    /// retain a separate charge for the actual returned values.
    pub fn column_decode_peak_bytes(
        &self,
        column: usize,
        types: &[LogicalType],
    ) -> DevonResult<usize> {
        let ty = types
            .get(column)
            .ok_or_else(|| invalid_argument("decode column is out of bounds"))?;
        if *ty == LogicalType::String {
            return self.string_column_decode_peak_bytes(column);
        }
        let entry = self
            .entries
            .get(column)
            .ok_or_else(|| corrupt("decode directory column is missing"))?;
        let mut payload = entry.byte_len as usize;
        if matches!(
            ty,
            LogicalType::VectorEncoded {
                encoding: VectorEncoding::B1 { .. },
                ..
            }
        ) {
            // Including all derived entries is conservative when more than
            // one b1 column exists and avoids omitting rescore scratch.
            for entry in self.entries.iter().skip(types.len()) {
                payload = payload
                    .checked_add(entry.byte_len as usize)
                    .ok_or_else(|| corrupt("decode auxiliary size overflows"))?;
            }
        }
        let vector = ty.vector_dim().map_or(0, |dim| dim as usize);
        let row = vector
            .checked_mul(4)
            .and_then(|n| n.checked_add(size_of::<Value>() + 32))
            .ok_or_else(|| corrupt("decode row size overflows"))?;
        // Json decoding validates nested values as well as retaining text.
        // Sixty-four payload widths cover its temporary serde value tree.
        let payload_factor = if *ty == LogicalType::Json { 64 } else { 4 };
        payload
            .checked_mul(payload_factor)
            .and_then(|n| {
                row.checked_mul(self.row_count)?
                    .checked_mul(8)?
                    .checked_add(n)
            })
            .ok_or_else(|| DevonError::BudgetExceeded {
                context: "column decode peak exceeds address space".into(),
            })
    }

    /// Returns zone maps in main-column declaration order when section bit 0
    /// is present, or `None` for a legacy directory.
    #[must_use]
    pub fn zone_maps(&self) -> Option<&[ZoneMapStats]> {
        self.zone_maps.as_deref()
    }

    /// Returns the per-main-column values-section encodings declared by
    /// section bit 1 (`docs/SCALE.md` §8.1), or `None` when every payload
    /// is plain and the section is absent.
    pub(crate) fn column_encodings(&self) -> Option<&[(Encoding, [u8; 3])]> {
        self.encodings.as_deref()
    }
}

impl NodeGroup {
    /// Creates an empty node group with columns in catalog declaration order.
    pub fn new(types: Vec<LogicalType>) -> DevonResult<Self> {
        if types.is_empty() {
            return Err(invalid_argument(
                "a node group must contain at least one column",
            ));
        }
        let columns = types.iter().map(|_| Vec::new()).collect();
        Ok(Self {
            types,
            columns,
            row_count: 0,
        })
    }

    /// Appends a row after validating its arity, values, and group capacity.
    pub fn push_row(&mut self, row: Vec<Value>) -> DevonResult<()> {
        if row.len() != self.types.len() {
            return Err(invalid_argument(format!(
                "row arity mismatch: expected {} values, actual {}",
                self.types.len(),
                row.len()
            )));
        }
        if self.row_count >= NODE_GROUP_CAPACITY {
            return Err(invalid_argument(format!(
                "node group is full at capacity {NODE_GROUP_CAPACITY} rows"
            )));
        }
        for (index, (value, logical_type)) in row.iter().zip(&self.types).enumerate() {
            if !value.matches_type(logical_type) {
                return Err(invalid_argument(format!(
                    "column {index} value {value} does not match expected type {logical_type}"
                )));
            }
        }
        for (column, value) in self.columns.iter_mut().zip(row) {
            column.push(value);
        }
        self.row_count += 1;
        Ok(())
    }

    /// Returns the number of rows in this node group.
    #[must_use]
    pub const fn row_count(&self) -> usize {
        self.row_count
    }

    /// Returns the number of property columns in this node group.
    #[must_use]
    pub fn column_count(&self) -> usize {
        self.columns.len()
    }

    /// Returns the logical column types in catalog declaration order.
    #[must_use]
    pub fn types(&self) -> &[LogicalType] {
        &self.types
    }

    /// Returns a value by row and column, or `None` when either is out of bounds.
    #[must_use]
    pub fn value(&self, row: usize, col: usize) -> Option<&Value> {
        self.columns.get(col)?.get(row)
    }

    /// Persists payload runs and their directory, returning the directory page id.
    ///
    /// Each main column's values-section encoding is chosen by the sampled
    /// adaptive selection policy (`docs/SCALE.md` §8.4; writer policy,
    /// never format law): the cheapest admissible encoding whose sampled
    /// estimate beats plain by at least 25 %, with plain as the fallback when nothing
    /// qualifies. A group whose columns all select plain stays
    /// byte-identical to a pre-feature write (the `COLUMN_ENCODINGS`
    /// directory section is absent by law, `docs/SCALE.md` §8.1).
    pub fn write(&self, pager: &Pager) -> DevonResult<u64> {
        let payloads = self.encode_columns(None)?;
        let declares_column_encodings = payloads[..self.column_count()]
            .iter()
            .any(|payload| payload.encoding != Encoding::Plain);
        let directory_page = self.persist_payloads(pager, payloads)?;
        if declares_column_encodings {
            pager.note_column_encodings_written();
        }
        Ok(directory_page)
    }

    /// Test-only interface (`docs/SCALE.md` §8.2/§8.4): persists
    /// the group with the given main columns forced to a non-default
    /// values-section encoding, bypassing the adaptive selection the
    /// production writer ([`Self::write`]) runs, so goldens and fences pin
    /// exact bytes per encoding. `forced` is
    /// `(column index, encoding id)` pairs.
    ///
    /// The caller must publish the group through a catalog save (which
    /// derives superblock feature bit 13) or set `COLUMN_ENCODINGS_FLAG`
    /// directly before reading the group back — a flags-bit-1 directory
    /// whose feature bit is clear is corruption by law.
    #[doc(hidden)]
    pub fn write_forcing_encodings(
        &self,
        pager: &Pager,
        forced: &[(usize, u8)],
    ) -> DevonResult<u64> {
        let mut selections = Vec::with_capacity(forced.len());
        for &(column, id) in forced {
            let encoding = Encoding::from_id(id)
                .ok_or_else(|| invalid_argument(format!("encoding id {id} is not registered")))?;
            if column >= self.column_count() {
                return Err(invalid_argument(format!(
                    "forced encoding column {column} is out of bounds for {} columns",
                    self.column_count()
                )));
            }
            selections.push((column, encoding));
        }
        let payloads = self.encode_columns(Some(&selections))?;
        self.persist_payloads(pager, payloads)
    }

    /// The shared tail of both write paths: payload runs, the directory
    /// (with the `COLUMN_ENCODINGS` section exactly when any selection is
    /// non-plain), and the zone-map note for the next publication.
    fn persist_payloads(&self, pager: &Pager, payloads: Vec<EncodedPayload>) -> DevonResult<u64> {
        let page_size = pager.superblock().page_size as usize;
        let selections: Vec<(Encoding, [u8; 3])> = payloads[..self.column_count()]
            .iter()
            .map(|payload| (payload.encoding, payload.params))
            .collect();
        // The section is absent when every payload is plain (docs/SCALE.md
        // §8.1): an all-plain group stays byte-identical to a pre-feature
        // write, which the goldens prove.
        let section = selections
            .iter()
            .any(|(encoding, _)| *encoding != Encoding::Plain)
            .then_some(selections);
        self.validate_for_write(page_size, section.is_some())?;
        let stats = self.compute_zone_maps()?;
        let mut entries = Vec::with_capacity(payloads.len());
        for payload in &payloads {
            let first_page = write_payload(pager, &payload.bytes, page_size)?;
            let byte_len = u32::try_from(payload.bytes.len())
                .map_err(|_| invalid_argument("column payload length exceeds u32"))?;
            entries.push(ColumnEntry {
                first_page,
                byte_len,
                checksum: crc32c(&payload.bytes),
            });
        }

        let directory_page = pager.allocate_page()?;
        let directory = encode_directory(
            self.row_count,
            self.column_count(),
            &entries,
            &stats,
            section.as_deref(),
            page_size,
        )?;
        pager.write_page(directory_page, &directory)?;
        pager.sync()?;
        register_pending_directory(pager, directory_page, &directory);
        pager.note_zone_maps_written();
        Ok(directory_page)
    }

    /// Reads and validates a node group from its directory page.
    /// Reads one group's row count from its directory page alone.
    ///
    /// This is the metadata-only path for counting checkpointed rows
    /// (recovery validation, key resolution): it validates the directory
    /// header — magic, nonzero rows, logical `column_count`, reserved
    /// field — and never touches column payloads. `expected_columns`
    /// counts logical columns; derived b1 rescore entries are excluded,
    /// matching the persisted `column_count` field.
    pub fn read_row_count(
        pager: &Pager,
        directory_page: u64,
        expected_columns: usize,
    ) -> DevonResult<usize> {
        let page = pager.read_page(directory_page)?;
        if page.len() < DIRECTORY_HEADER_LEN || &page[..4] != DIRECTORY_MAGIC {
            return Err(corrupt("node-group directory magic is not NGRP"));
        }
        let row_count = read_u32(&page, 4) as usize;
        if row_count == 0 {
            return Err(corrupt("node-group directory row_count is zero"));
        }
        let column_count = read_u32(&page, 8) as usize;
        if column_count != expected_columns {
            return Err(corrupt(format!(
                "node-group column_count is {column_count}, expected {expected_columns}"
            )));
        }
        let allow_pending = is_pending_directory(pager, directory_page, &page);
        validate_directory_flags(read_u32(&page, 12), pager, allow_pending)?;
        Ok(row_count)
    }

    /// Reads and validates a directory and its extension sections without
    /// touching any column payload page.
    pub fn read_directory(
        pager: &Pager,
        directory_page: u64,
        types: &[LogicalType],
    ) -> DevonResult<NodeGroupDirectory> {
        let page = pager.read_page(directory_page)?;
        let allow_pending = is_pending_directory(pager, directory_page, &page);
        decode_directory(&page, types, pager, allow_pending)
    }

    /// Reads payloads using a directory previously returned by
    /// [`Self::read_directory`], avoiding a second directory-page read.
    pub fn read_from_directory(
        pager: &Pager,
        directory: NodeGroupDirectory,
        types: &[LogicalType],
    ) -> DevonResult<Self> {
        if directory.entries.len() != types.len() + b1_rescore_count(types) {
            return Err(corrupt(
                "node-group decoded directory entry count disagrees with schema",
            ));
        }
        let row_count = directory.row_count;
        let entries = directory.entries;
        let zone_maps = directory.zone_maps;
        let encodings = directory.encodings;
        Self::read_payloads(pager, row_count, entries, zone_maps, encodings, types)
    }

    /// Reads and decodes exactly one main column of a node group straight
    /// into typed [`Column`] storage (`docs/SCALE.md` §6.5), with no
    /// `Vec<Value>` between decode and chunk.
    ///
    /// The house idiom of [`Self::read_column`], unchanged: the directory is
    /// fully validated, only the selected column's payload pages are read,
    /// and every corruption check (`byte_len`, CRC-32C, bitmap padding,
    /// offsets, UTF-8, null-slot zero) runs exactly as on the boxed path.
    /// Fixed-width types decode into their typed variants with the FORMAT
    /// LSB-first validity bytes repacked into §6.3 bitmap words (null slots
    /// keep the zeros the payload law guarantees); String, Bytes, Json,
    /// Vector, GeoPoint, and VectorEncoded decode through the existing boxed
    /// decoders into [`Column::Boxed`] until arena variants are available.
    /// Columns carrying a b1-rescore sidecar are refused, as in
    /// [`Self::read_column`].
    pub fn read_column_typed(
        pager: &Pager,
        directory_page: u64,
        types: &[LogicalType],
        column: usize,
    ) -> DevonResult<(Column, usize)> {
        let directory = Self::read_directory(pager, directory_page, types)?;
        if directory.entries.len() != types.len() + b1_rescore_count(types) {
            return Err(corrupt(
                "node-group decoded directory entry count disagrees with schema",
            ));
        }
        let Some(logical_type) = types.get(column) else {
            return Err(invalid_argument(format!(
                "node-group column {column} is out of bounds for {} columns",
                types.len()
            )));
        };
        if b1_rescore_type(logical_type).is_some() {
            return Err(invalid_argument(format!(
                "node-group column {column} carries a rescore sidecar; use the full read"
            )));
        }
        let row_count = directory.row_count;
        let entry = directory.entries[column];
        let selection = column_encoding(directory.column_encodings(), column);
        let page_size = pager.superblock().page_size as usize;
        read_typed_payload(
            pager,
            entry,
            page_size,
            row_count,
            logical_type,
            column,
            selection,
        )
        .map(|typed| (typed, row_count))
    }

    /// Reads every main column of a node group straight into typed [`Column`]
    /// storage using a directory previously returned by
    /// [`Self::read_directory`] (`docs/SCALE.md` §6.5). This is the typed
    /// scan path's decode step.
    ///
    /// b1-rescore sidecars are decoded and paired exactly as in
    /// [`Self::read_from_directory`]. One difference, deliberate: zone-map
    /// stats are NOT re-validated against the decoded payloads here — stats
    /// are format law enforced by the corpus validator and the corruption
    /// tests, never re-checked on the scan hot path (docs/SCALE.md §4.3).
    /// Every payload-level check (`byte_len`, CRC-32C, offsets, UTF-8,
    /// null-slot zero, bitmap padding) runs exactly as on the boxed path.
    pub fn read_columns_typed_from_directory(
        pager: &Pager,
        directory: NodeGroupDirectory,
        types: &[LogicalType],
    ) -> DevonResult<Vec<Column>> {
        if directory.entries.len() != types.len() + b1_rescore_count(types) {
            return Err(corrupt(
                "node-group decoded directory entry count disagrees with schema",
            ));
        }
        let row_count = directory.row_count;
        let entries = directory.entries;
        let encodings = directory.encodings;
        let page_size = pager.superblock().page_size as usize;
        let mut columns = Vec::with_capacity(types.len());
        for (index, (entry, logical_type)) in entries[..types.len()].iter().zip(types).enumerate() {
            let selection = column_encoding(encodings.as_deref(), index);
            columns.push(read_typed_payload(
                pager,
                *entry,
                page_size,
                row_count,
                logical_type,
                index,
                selection,
            )?);
        }

        let mut rescore_entry = types.len();
        for (index, logical_type) in types.iter().enumerate() {
            let Some(rescore_type) = b1_rescore_type(logical_type) else {
                continue;
            };
            let entry = entries[rescore_entry];
            validate_payload_len(
                entry.byte_len,
                row_count,
                &rescore_type,
                index,
                Encoding::Plain,
            )?;
            let payload = read_payload(pager, entry, page_size, rescore_entry)?;
            if crc32c(&payload) != entry.checksum {
                return Err(corrupt(format!(
                    "column {index} rescore payload CRC-32C does not match"
                )));
            }
            let rescore = decode_column(
                &payload,
                row_count,
                &rescore_type,
                index,
                Encoding::Plain,
                [0; 3],
            )?;
            let Column::Boxed(main) = &columns[index] else {
                return Err(corrupt(format!(
                    "column {index} b1 main column is not boxed storage"
                )));
            };
            validate_rescore_rows(main, &rescore, index)?;
            columns[index] = Column::Boxed(rescore);
            rescore_entry += 1;
        }
        Ok(columns)
    }

    /// Reads and decodes exactly one main column of a node group.
    ///
    /// The directory is fully validated as in [`Self::read`]; only the
    /// selected column's payload pages are read, checksummed, and decoded.
    /// Unselected payloads are untouched and therefore unverified — exactly
    /// like unread pages, whose writers verified them at write time. Columns
    /// carrying a b1-rescore sidecar are refused (their sidecar pairing
    /// belongs to the full read); primary keys can never carry one. This is
    /// the pruned-decode primitive reused by the typed-chunk scan path
    /// (`docs/SCALE.md` §6.7).
    pub fn read_column(
        pager: &Pager,
        directory_page: u64,
        types: &[LogicalType],
        column: usize,
    ) -> DevonResult<(Vec<Value>, usize)> {
        let directory = Self::read_directory(pager, directory_page, types)?;
        if directory.entries.len() != types.len() + b1_rescore_count(types) {
            return Err(corrupt(
                "node-group decoded directory entry count disagrees with schema",
            ));
        }
        let Some(logical_type) = types.get(column) else {
            return Err(invalid_argument(format!(
                "node-group column {column} is out of bounds for {} columns",
                types.len()
            )));
        };
        if b1_rescore_type(logical_type).is_some() {
            return Err(invalid_argument(format!(
                "node-group column {column} carries a rescore sidecar; use the full read"
            )));
        }
        let row_count = directory.row_count;
        let entry = directory.entries[column];
        let (encoding, params) = column_encoding(directory.column_encodings(), column);
        let page_size = pager.superblock().page_size as usize;
        validate_payload_len(entry.byte_len, row_count, logical_type, column, encoding)?;
        let payload = read_payload(pager, entry, page_size, column)?;
        if crc32c(&payload) != entry.checksum {
            return Err(corrupt(format!(
                "column {column} payload CRC-32C does not match"
            )));
        }
        Ok((
            decode_column(&payload, row_count, logical_type, column, encoding, params)?,
            row_count,
        ))
    }

    /// Reads and validates a node group from its directory page.
    pub fn read(pager: &Pager, directory_page: u64, types: &[LogicalType]) -> DevonResult<Self> {
        let directory = Self::read_directory(pager, directory_page, types)?;
        Self::read_from_directory(pager, directory, types)
    }

    fn read_payloads(
        pager: &Pager,
        row_count: usize,
        entries: Vec<ColumnEntry>,
        zone_maps: Option<Vec<ZoneMapStats>>,
        encodings: Option<Vec<(Encoding, [u8; 3])>>,
        types: &[LogicalType],
    ) -> DevonResult<Self> {
        let page_size = pager.superblock().page_size as usize;
        let mut columns = Vec::with_capacity(types.len());

        for (index, (entry, logical_type)) in entries[..types.len()].iter().zip(types).enumerate() {
            let (encoding, params) = column_encoding(encodings.as_deref(), index);
            validate_payload_len(entry.byte_len, row_count, logical_type, index, encoding)?;
            let payload = read_payload(pager, *entry, page_size, index)?;
            if crc32c(&payload) != entry.checksum {
                return Err(corrupt(format!(
                    "column {index} payload CRC-32C does not match"
                )));
            }
            columns.push(decode_column(
                &payload,
                row_count,
                logical_type,
                index,
                encoding,
                params,
            )?);
        }

        let mut rescore_entry = types.len();
        for (index, logical_type) in types.iter().enumerate() {
            let Some(rescore_type) = b1_rescore_type(logical_type) else {
                continue;
            };
            let entry = entries[rescore_entry];
            validate_payload_len(
                entry.byte_len,
                row_count,
                &rescore_type,
                index,
                Encoding::Plain,
            )?;
            let payload = read_payload(pager, entry, page_size, rescore_entry)?;
            if crc32c(&payload) != entry.checksum {
                return Err(corrupt(format!(
                    "column {index} rescore payload CRC-32C does not match"
                )));
            }
            let rescore = decode_column(
                &payload,
                row_count,
                &rescore_type,
                index,
                Encoding::Plain,
                [0; 3],
            )?;
            validate_rescore_rows(&columns[index], &rescore, index)?;
            columns[index] = rescore;
            rescore_entry += 1;
        }

        if let Some(stats) = zone_maps {
            validate_stats_against_columns(&stats, &columns, types, row_count)?;
        }

        Ok(Self {
            types: types.to_vec(),
            columns,
            row_count,
        })
    }

    fn validate_for_write(&self, page_size: usize, encodings_section: bool) -> DevonResult<()> {
        if self.row_count == 0 {
            return Err(invalid_argument("cannot write an empty node group"));
        }
        let max_columns = page_size
            .checked_sub(DIRECTORY_HEADER_LEN)
            .ok_or_else(|| invalid_argument("page is shorter than a node-group header"))?
            / DIRECTORY_ENTRY_LEN;
        let entry_count = self
            .column_count()
            .checked_add(b1_rescore_count(&self.types))
            .ok_or_else(|| invalid_argument("node-group directory entry count overflows usize"))?;
        let stats_len = self
            .column_count()
            .checked_mul(ZONE_MAP_RECORD_LEN)
            .ok_or_else(|| invalid_argument("zone-map section length overflows usize"))?;
        let encodings_len = if encodings_section {
            self.column_count()
                .checked_mul(ENCODING_RECORD_LEN)
                .and_then(|length| length.checked_add(SECTION_HEADER_LEN))
                .ok_or_else(|| invalid_argument("encodings section length overflows usize"))?
        } else {
            0
        };
        let directory_len = DIRECTORY_HEADER_LEN
            .checked_add(
                entry_count
                    .checked_mul(DIRECTORY_ENTRY_LEN)
                    .ok_or_else(|| {
                        invalid_argument("node-group directory entry length overflows usize")
                    })?,
            )
            .and_then(|length| length.checked_add(SECTION_HEADER_LEN))
            .and_then(|length| length.checked_add(stats_len))
            .and_then(|length| length.checked_add(encodings_len))
            .ok_or_else(|| invalid_argument("node-group directory length overflows usize"))?;
        if entry_count > max_columns || directory_len > page_size {
            return Err(invalid_argument(format!(
                "node group directory needs {directory_len} bytes but page capacity is {page_size}"
            )));
        }
        Ok(())
    }

    /// Encodes every column payload. With `forced` the named main columns
    /// use their forced encoding and the rest plain (the test seam); with
    /// `None` each main column runs the §8.4 adaptive selection. b1-rescore
    /// sidecars are always plain.
    fn encode_columns(
        &self,
        forced: Option<&[(usize, Encoding)]>,
    ) -> DevonResult<Vec<EncodedPayload>> {
        let mut payloads = Vec::with_capacity(self.column_count() + b1_rescore_count(&self.types));
        for (index, (column, logical_type)) in self.columns.iter().zip(&self.types).enumerate() {
            let payload = match forced {
                Some(forced) => {
                    let encoding = forced
                        .iter()
                        .find(|(column, _)| *column == index)
                        .map_or(Encoding::Plain, |(_, encoding)| *encoding);
                    encode_column(column, logical_type, index, encoding)?
                }
                None => encode_column_selected(column, logical_type, index)?,
            };
            payloads.push(payload);
        }
        for (index, (column, logical_type)) in self.columns.iter().zip(&self.types).enumerate() {
            if let Some(rescore_type) = b1_rescore_type(logical_type) {
                payloads.push(encode_column(
                    column,
                    &rescore_type,
                    index,
                    Encoding::Plain,
                )?);
            }
        }
        Ok(payloads)
    }

    fn compute_zone_maps(&self) -> DevonResult<Vec<ZoneMapStats>> {
        self.columns
            .iter()
            .zip(&self.types)
            .enumerate()
            .map(|(index, (column, logical_type))| compute_zone_map(column, logical_type, index))
            .collect()
    }
}

/// One encoded column payload plus the values-section encoding it carries
/// (`docs/SCALE.md` §8.1: `byte_len`/`crc32c` cover the encoded payload;
/// the directory section records the encoding and its parameters).
struct EncodedPayload {
    bytes: Vec<u8>,
    encoding: Encoding,
    params: [u8; 3],
}

fn encode_column(
    column: &[Value],
    logical_type: &LogicalType,
    column_index: usize,
    encoding: Encoding,
) -> DevonResult<EncodedPayload> {
    if let Some(payload_len) = fixed_payload_len(column.len(), logical_type)
        && payload_len > u32::MAX as usize
    {
        return Err(invalid_argument(format!(
            "column {column_index} payload length exceeds u32"
        )));
    }
    let mut payload = encode_validity(column);
    // Plain short-circuits to the legacy byte path so writer error messages
    // keep their column context and all-plain files stay byte-identical to
    // pre-feature writes; every other encoding routes through the §8.2 seam
    // against typed `Column` storage.
    let (values, params) = match encoding {
        Encoding::Plain => (
            encode_values_section(column, logical_type, column_index)?,
            [0; 3],
        ),
        other => {
            let typed = Column::from_values(logical_type, column.to_vec());
            encodings::encode_values(other, &typed, logical_type)?
        }
    };
    payload.extend_from_slice(&values);
    if payload.len() > u32::MAX as usize {
        return Err(invalid_argument(format!(
            "column {column_index} payload length exceeds u32"
        )));
    }
    Ok(EncodedPayload {
        bytes: payload,
        encoding,
        params,
    })
}

/// Encodes one main column under the §8.4 adaptive selection: the ranked
/// candidates are tried in ascending estimated-cost order and the first
/// that accepts the full column wins. A candidate can refuse despite its
/// sampled estimate (a constant estimate a deviating unsampled row
/// disproves is the canonical case); the fallback chain — deterministic,
/// since it depends only on the column's bytes — ends at plain, which
/// encodes any valid column or surfaces the data error itself. Candidate
/// encoding builds bytes in memory only, so no I/O error is ever
/// swallowed by the fallback.
fn encode_column_selected(
    column: &[Value],
    logical_type: &LogicalType,
    column_index: usize,
) -> DevonResult<EncodedPayload> {
    for encoding in encodings::select::ranked_candidates(column, logical_type) {
        if let Ok(payload) = encode_column(column, logical_type, column_index, encoding) {
            return Ok(payload);
        }
    }
    encode_column(column, logical_type, column_index, Encoding::Plain)
}

/// The plain values section of one column payload (`docs/FORMAT.md` §
/// Column payload encoding) — the `Encoding::Plain` arm of the §8.2 seam.
fn encode_values_section(
    column: &[Value],
    logical_type: &LogicalType,
    column_index: usize,
) -> DevonResult<Vec<u8>> {
    let mut payload = Vec::new();
    match logical_type {
        LogicalType::Bool => encode_bools(column, &mut payload, column_index)?,
        LogicalType::Int64 => encode_int64s(column, &mut payload, column_index)?,
        LogicalType::Float64 => encode_float64s(column, &mut payload, column_index)?,
        LogicalType::String => encode_strings(column, &mut payload, column_index)?,
        LogicalType::Vector { dim } => {
            encode_vectors(column, *dim as usize, &mut payload, column_index)?;
        }
        LogicalType::VectorEncoded { dim, encoding } => {
            encode_vector_encoded(column, *dim as usize, *encoding, &mut payload, column_index)?;
        }
        LogicalType::GeoPoint => {
            crate::geo_column::encode_geo_points(column, &mut payload, column_index)?;
        }
        LogicalType::Timestamp => encode_timestamps(column, &mut payload, column_index)?,
        LogicalType::Bytes => {
            encode_heap_values(column, &mut payload, column_index, HeapValueKind::Bytes)?;
        }
        LogicalType::Decimal { precision, scale } => {
            encode_decimals(column, &mut payload, column_index, *precision, *scale)?;
        }
        LogicalType::Json => {
            encode_heap_values(column, &mut payload, column_index, HeapValueKind::Json)?;
        }
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

fn encode_bools(column: &[Value], payload: &mut Vec<u8>, column_index: usize) -> DevonResult<()> {
    let mut values = vec![0_u8; bitmap_len(column.len())];
    for (row, value) in column.iter().enumerate() {
        match value {
            Value::Null | Value::Bool(false) => {}
            Value::Bool(true) => set_bit(&mut values, row),
            _ => return Err(invalid_stored_value(column_index, row, value)),
        }
    }
    payload.extend(values);
    Ok(())
}

fn encode_int64s(column: &[Value], payload: &mut Vec<u8>, column_index: usize) -> DevonResult<()> {
    for (row, value) in column.iter().enumerate() {
        match value {
            Value::Null => payload.extend([0_u8; 8]),
            Value::Int64(value) => payload.extend(value.to_le_bytes()),
            _ => return Err(invalid_stored_value(column_index, row, value)),
        }
    }
    Ok(())
}

fn encode_float64s(
    column: &[Value],
    payload: &mut Vec<u8>,
    column_index: usize,
) -> DevonResult<()> {
    for (row, value) in column.iter().enumerate() {
        match value {
            Value::Null => payload.extend([0_u8; 8]),
            Value::Float64(value) => payload.extend(value.to_le_bytes()),
            _ => return Err(invalid_stored_value(column_index, row, value)),
        }
    }
    Ok(())
}

fn encode_timestamps(
    column: &[Value],
    payload: &mut Vec<u8>,
    column_index: usize,
) -> DevonResult<()> {
    for (row, value) in column.iter().enumerate() {
        match value {
            Value::Null => payload.extend([0_u8; 8]),
            Value::Timestamp(micros) => payload.extend(micros.to_le_bytes()),
            _ => return Err(invalid_stored_value(column_index, row, value)),
        }
    }
    Ok(())
}

fn encode_decimals(
    column: &[Value],
    payload: &mut Vec<u8>,
    column_index: usize,
    precision: u8,
    scale: u8,
) -> DevonResult<()> {
    if !(1..=devondb_types::decimal::MAX_PRECISION).contains(&precision) || scale > precision {
        return Err(invalid_argument(format!(
            "column {column_index} has invalid Decimal({precision}, {scale}) declaration"
        )));
    }
    for (row, value) in column.iter().enumerate() {
        match value {
            Value::Null => payload.extend([0_u8; 16]),
            Value::Decimal(value) if value.fits(precision, scale) => {
                payload.extend(value.digits().to_le_bytes());
            }
            _ => return Err(invalid_stored_value(column_index, row, value)),
        }
    }
    Ok(())
}

fn encode_vectors(
    column: &[Value],
    dim: usize,
    payload: &mut Vec<u8>,
    column_index: usize,
) -> DevonResult<()> {
    let zero_slot_len = dim
        .checked_mul(4)
        .ok_or_else(|| invalid_argument("vector slot length overflows usize"))?;
    for (row, value) in column.iter().enumerate() {
        match value {
            Value::Null => payload.resize(payload.len() + zero_slot_len, 0),
            Value::Vector(vector) if vector.len() == dim => {
                for element in vector {
                    payload.extend(element.to_le_bytes());
                }
            }
            _ => return Err(invalid_stored_value(column_index, row, value)),
        }
    }
    Ok(())
}

fn encode_vector_encoded(
    column: &[Value],
    dim: usize,
    encoding: VectorEncoding,
    payload: &mut Vec<u8>,
    column_index: usize,
) -> DevonResult<()> {
    let slot_len = encoded_vector_slot_len(dim, encoding)
        .ok_or_else(|| invalid_argument("encoded vector slot length overflows usize"))?;
    for (row, value) in column.iter().enumerate() {
        match value {
            Value::Null => payload.resize(payload.len() + slot_len, 0),
            Value::Vector(vector) if vector.len() == dim => {
                let slot = match encoding {
                    VectorEncoding::F16 => encode_f16(vector),
                    VectorEncoding::I8 => encode_i8(vector),
                    VectorEncoding::B1 { rotation_seed, .. } => encode_b1(vector, rotation_seed),
                };
                payload.extend(slot);
            }
            _ => return Err(invalid_stored_value(column_index, row, value)),
        }
    }
    Ok(())
}

fn encode_strings(column: &[Value], payload: &mut Vec<u8>, column_index: usize) -> DevonResult<()> {
    encode_heap_values(column, payload, column_index, HeapValueKind::String)
}

#[derive(Clone, Copy)]
enum HeapValueKind {
    String,
    Bytes,
    Json,
}

impl HeapValueKind {
    fn bytes(self, value: &Value) -> Option<&[u8]> {
        match (self, value) {
            (Self::String, Value::String(value)) | (Self::Json, Value::Json(value)) => {
                Some(value.as_bytes())
            }
            (Self::Bytes, Value::Bytes(value)) => Some(value),
            _ => None,
        }
    }

    const fn name(self) -> &'static str {
        match self {
            Self::String => "String",
            Self::Bytes => "Bytes",
            Self::Json => "Json",
        }
    }
}

fn encode_heap_values(
    column: &[Value],
    payload: &mut Vec<u8>,
    column_index: usize,
    kind: HeapValueKind,
) -> DevonResult<()> {
    let offsets_start = payload.len();
    let offsets_len = column
        .len()
        .checked_add(1)
        .and_then(|count| count.checked_mul(4))
        .ok_or_else(|| {
            invalid_argument(format!("{} offsets length overflows usize", kind.name()))
        })?;
    payload.resize(offsets_start + offsets_len, 0);
    let mut heap_len = 0_u32;

    for (row, value) in column.iter().enumerate() {
        if let Some(bytes) = kind.bytes(value) {
            let value_len = u32::try_from(bytes.len()).map_err(|_| {
                invalid_argument(format!("{} value length exceeds u32", kind.name()))
            })?;
            heap_len = heap_len.checked_add(value_len).ok_or_else(|| {
                invalid_argument(format!("{} heap length exceeds u32", kind.name()))
            })?;
            payload.extend(bytes);
        } else if !matches!(value, Value::Null) {
            return Err(invalid_stored_value(column_index, row, value));
        }
        let offset = offsets_start + (row + 1) * 4;
        payload[offset..offset + 4].copy_from_slice(&heap_len.to_le_bytes());
    }
    Ok(())
}

fn compute_zone_map(
    column: &[Value],
    logical_type: &LogicalType,
    column_index: usize,
) -> DevonResult<ZoneMapStats> {
    let null_count = u32::try_from(
        column
            .iter()
            .filter(|value| matches!(value, Value::Null))
            .count(),
    )
    .map_err(|_| invalid_argument("zone-map null count exceeds u32"))?;
    let (min, max) = match logical_type {
        LogicalType::Int64 => int64_zone_map(column, column_index)?,
        LogicalType::Float64 => float64_zone_map(column, column_index)?,
        LogicalType::GeoPoint => geo_zone_map(column, column_index)?,
        LogicalType::Bool
        | LogicalType::String
        | LogicalType::Vector { .. }
        | LogicalType::VectorEncoded { .. }
        | LogicalType::Timestamp
        | LogicalType::Bytes
        | LogicalType::Decimal { .. }
        | LogicalType::Json => (None, None),
    };
    Ok(ZoneMapStats {
        null_count,
        min,
        max,
    })
}

fn int64_zone_map(
    column: &[Value],
    column_index: usize,
) -> DevonResult<(Option<ZoneMapValue>, Option<ZoneMapValue>)> {
    let mut bounds: Option<(i64, i64)> = None;
    for (row, value) in column.iter().enumerate() {
        match value {
            Value::Null => {}
            Value::Int64(value) => {
                bounds = Some(bounds.map_or((*value, *value), |(min, max)| {
                    (min.min(*value), max.max(*value))
                }));
            }
            _ => return Err(invalid_stored_value(column_index, row, value)),
        }
    }
    Ok(bounds.map_or((None, None), |(min, max)| {
        (
            Some(ZoneMapValue::Int64(min)),
            Some(ZoneMapValue::Int64(max)),
        )
    }))
}

fn float64_zone_map(
    column: &[Value],
    column_index: usize,
) -> DevonResult<(Option<ZoneMapValue>, Option<ZoneMapValue>)> {
    let mut bounds: Option<(f64, f64)> = None;
    let mut has_nan = false;
    for (row, value) in column.iter().enumerate() {
        match value {
            Value::Null => {}
            Value::Float64(value) => {
                has_nan |= value.is_nan();
                bounds = Some(bounds.map_or((*value, *value), |(min, max)| {
                    let min = if value.total_cmp(&min).is_lt() {
                        *value
                    } else {
                        min
                    };
                    let max = if value.total_cmp(&max).is_gt() {
                        *value
                    } else {
                        max
                    };
                    (min, max)
                }));
            }
            _ => return Err(invalid_stored_value(column_index, row, value)),
        }
    }
    if has_nan {
        return Ok((None, None));
    }
    Ok(bounds.map_or((None, None), |(min, max)| {
        (
            Some(ZoneMapValue::Float64(min)),
            Some(ZoneMapValue::Float64(max)),
        )
    }))
}

fn geo_zone_map(
    column: &[Value],
    column_index: usize,
) -> DevonResult<(Option<ZoneMapValue>, Option<ZoneMapValue>)> {
    let mut bounds: Option<(u64, u64)> = None;
    for (row, value) in column.iter().enumerate() {
        match value {
            Value::Null => {}
            Value::GeoPoint(point) => {
                let atom = devondb_geo::grid::atom(point.lat_deg(), point.lng_deg())
                    .map_err(|error| invalid_argument(format!(
                        "column {column_index} row {row} cannot be assigned a DevonGrid atom: {error}"
                    )))?
                    .raw();
                bounds =
                    Some(bounds.map_or((atom, atom), |(min, max)| (min.min(atom), max.max(atom))));
            }
            _ => return Err(invalid_stored_value(column_index, row, value)),
        }
    }
    Ok(bounds.map_or((None, None), |(min, max)| {
        (
            Some(ZoneMapValue::GeoPoint(min)),
            Some(ZoneMapValue::GeoPoint(max)),
        )
    }))
}

fn encode_zone_map_payload(stats: &[ZoneMapStats]) -> DevonResult<Vec<u8>> {
    let capacity = stats
        .len()
        .checked_mul(ZONE_MAP_RECORD_LEN)
        .ok_or_else(|| invalid_argument("zone-map payload length overflows usize"))?;
    let mut payload = Vec::with_capacity(capacity);
    for stat in stats {
        payload.extend(stat.null_count.to_le_bytes());
        match (stat.min, stat.max) {
            (None, None) => {
                payload.extend(0_u32.to_le_bytes());
                payload.extend([0_u8; 16]);
            }
            (Some(min), Some(max)) if same_zone_map_type(min, max) => {
                payload.extend(STATS_MIN_MAX_PRESENT.to_le_bytes());
                payload.extend(zone_map_bytes(min));
                payload.extend(zone_map_bytes(max));
            }
            _ => {
                return Err(invalid_argument(
                    "zone-map min and max must be an equally typed pair",
                ));
            }
        }
    }
    Ok(payload)
}

const fn same_zone_map_type(left: ZoneMapValue, right: ZoneMapValue) -> bool {
    matches!(
        (left, right),
        (ZoneMapValue::Int64(_), ZoneMapValue::Int64(_))
            | (ZoneMapValue::Float64(_), ZoneMapValue::Float64(_))
            | (ZoneMapValue::GeoPoint(_), ZoneMapValue::GeoPoint(_))
    )
}

fn zone_map_bytes(value: ZoneMapValue) -> [u8; 8] {
    match value {
        ZoneMapValue::Int64(value) => value.to_le_bytes(),
        ZoneMapValue::Float64(value) => value.to_le_bytes(),
        ZoneMapValue::GeoPoint(value) => value.to_le_bytes(),
    }
}

fn encode_directory(
    row_count: usize,
    column_count: usize,
    entries: &[ColumnEntry],
    stats: &[ZoneMapStats],
    encodings: Option<&[(Encoding, [u8; 3])]>,
    page_size: usize,
) -> DevonResult<Vec<u8>> {
    let row_count = u32::try_from(row_count)
        .map_err(|_| invalid_argument("node-group row count exceeds u32"))?;
    let column_count = u32::try_from(column_count)
        .map_err(|_| invalid_argument("node-group column count exceeds u32"))?;
    let mut flags = ZONE_MAP_DIRECTORY_FLAG;
    if encodings.is_some() {
        flags |= COLUMN_ENCODINGS_DIRECTORY_FLAG;
    }
    let mut page = vec![0_u8; page_size];
    page[..4].copy_from_slice(DIRECTORY_MAGIC);
    page[4..8].copy_from_slice(&row_count.to_le_bytes());
    page[8..12].copy_from_slice(&column_count.to_le_bytes());
    page[12..16].copy_from_slice(&flags.to_le_bytes());

    for (index, entry) in entries.iter().enumerate() {
        let offset = DIRECTORY_HEADER_LEN + index * DIRECTORY_ENTRY_LEN;
        page[offset..offset + 8].copy_from_slice(&entry.first_page.to_le_bytes());
        page[offset + 8..offset + 12].copy_from_slice(&entry.byte_len.to_le_bytes());
        page[offset + 12..offset + 16].copy_from_slice(&entry.checksum.to_le_bytes());
    }
    // Sections in ascending bit order (docs/FORMAT.md § Extension
    // sections): bit 0 ZONE_MAP_STATS, then bit 1 COLUMN_ENCODINGS.
    let mut cursor = DIRECTORY_HEADER_LEN + entries.len() * DIRECTORY_ENTRY_LEN;
    cursor = write_section(&mut page, cursor, &encode_zone_map_payload(stats)?)?;
    if let Some(selections) = encodings {
        let payload = encodings::encode_section_payload(selections);
        write_section(&mut page, cursor, &payload)?;
    }
    Ok(page)
}

/// Writes one framed extension section (`section_len u32 ‖ crc32c u32 ‖
/// payload`) at `cursor`, returning the end offset.
fn write_section(page: &mut [u8], cursor: usize, payload: &[u8]) -> DevonResult<usize> {
    let section_len = u32::try_from(payload.len())
        .map_err(|_| invalid_argument("node-group section length exceeds u32"))?;
    let end = cursor
        .checked_add(SECTION_HEADER_LEN)
        .and_then(|offset| offset.checked_add(payload.len()))
        .ok_or_else(|| invalid_argument("node-group directory length overflows usize"))?;
    if end > page.len() {
        return Err(invalid_argument(
            "node group directory sections exceed the page capacity",
        ));
    }
    page[cursor..cursor + 4].copy_from_slice(&section_len.to_le_bytes());
    page[cursor + 4..cursor + 8].copy_from_slice(&crc32c(payload).to_le_bytes());
    page[cursor + SECTION_HEADER_LEN..end].copy_from_slice(payload);
    Ok(end)
}

fn decode_directory(
    page: &[u8],
    types: &[LogicalType],
    pager: &Pager,
    allow_pending: bool,
) -> DevonResult<NodeGroupDirectory> {
    if page.len() < DIRECTORY_HEADER_LEN {
        return Err(corrupt("node-group directory is shorter than its header"));
    }
    if &page[..4] != DIRECTORY_MAGIC {
        return Err(corrupt("node-group directory magic is not NGRP"));
    }
    let row_count = read_u32(page, 4) as usize;
    if row_count == 0 {
        return Err(corrupt("node-group directory row_count is zero"));
    }
    let column_count = read_u32(page, 8) as usize;
    if column_count == 0 {
        return Err(corrupt("node-group directory column_count is zero"));
    }
    let directory_flags = read_u32(page, 12);
    let supported_flags = validate_directory_flags(directory_flags, pager, allow_pending)?;
    if column_count != types.len() {
        return Err(corrupt(format!(
            "node-group column_count is {column_count}, expected {} from schema",
            types.len()
        )));
    }
    let entry_count = column_count
        .checked_add(b1_rescore_count(types))
        .ok_or_else(|| corrupt("node-group directory entry count overflows usize"))?;
    let entries_end = directory_entries_end(entry_count, page.len())?;
    let (zone_maps, encodings, sections_end) = decode_sections(
        page,
        entries_end,
        directory_flags,
        supported_flags,
        row_count,
        types,
    )?;
    if page[sections_end..].iter().any(|byte| *byte != 0) {
        return Err(corrupt("node-group directory padding is not zero"));
    }

    let entries = (0..entry_count)
        .map(|index| decode_entry(page, index))
        .collect();
    Ok(NodeGroupDirectory {
        row_count,
        entries,
        zone_maps,
        encodings,
    })
}

fn register_pending_directory(pager: &Pager, page_id: u64, page: &[u8]) {
    let superblock = pager.superblock();
    let flags = read_u32(page, 12);
    if !has_ungoverned_directory_flag(flags, superblock.feature_flags) {
        return;
    }
    let entry = PendingDirectory {
        db_id: superblock.db_id,
        page_id,
        bytes: page.to_vec(),
    };
    PENDING_DIRECTORIES.with(|pending| {
        let mut pending = pending.borrow_mut();
        if !pending.contains(&entry) {
            pending.push(entry);
        }
    });
}

fn is_pending_directory(pager: &Pager, page_id: u64, page: &[u8]) -> bool {
    if !pager.has_written_zone_maps() && !pager.has_written_column_encodings() {
        return false;
    }
    let superblock = pager.superblock();
    PENDING_DIRECTORIES.with(|pending| {
        let mut pending = pending.borrow_mut();
        let matches = |entry: &PendingDirectory| {
            entry.db_id == superblock.db_id && entry.page_id == page_id && entry.bytes == page
        };
        if !has_ungoverned_directory_flag(read_u32(page, 12), superblock.feature_flags) {
            pending.retain(|entry| !matches(entry));
            return false;
        }
        pending.iter().any(matches)
    })
}

fn has_ungoverned_directory_flag(directory_flags: u32, feature_flags: u64) -> bool {
    DIRECTORY_FLAG_GOVERNORS
        .iter()
        .any(|(directory_flag, governor)| {
            directory_flags & directory_flag != 0 && feature_flags & governor == 0
        })
}

/// Validates directory-bit registration and per-bit superblock governance.
///
/// `allow_pending` is true only after the directory page exactly matched bytes
/// this pager's writer produced for its current catalog-publication window.
/// Every external page therefore passes `false`, including ordinary,
/// follower, pack, compact, and statistics reads. The pending exception
/// applies only to writer-supported bits 0 and 1; it never makes an
/// unsupported or unregistered bit legal.
fn validate_directory_flags(flags: u32, pager: &Pager, allow_pending: bool) -> DevonResult<u32> {
    let superblock_flags = pager.superblock().feature_flags;
    let mut supported_flags = 0;
    for bit in (0..u32::BITS).rev() {
        let directory_flag = 1_u32 << bit;
        if flags & directory_flag == 0 {
            continue;
        }
        let Some((_, governor)) = DIRECTORY_FLAG_GOVERNORS
            .iter()
            .find(|(registered, _)| *registered == directory_flag)
        else {
            return Err(corrupt(format!(
                "node-group directory bit {bit} is unregistered (unknown bits {directory_flag:#x})"
            )));
        };
        let governed = superblock_flags & governor != 0
            || pending_directory_flag(pager, directory_flag, allow_pending);
        if !governed {
            return Err(directory_governor_clear(bit, *governor));
        }
        if governor & crate::superblock::SUPPORTED_FLAG_MASK != 0 {
            supported_flags |= directory_flag;
        } else if governor & crate::superblock::READ_SAFE_FLAG_MASK == 0 {
            return Err(corrupt(format!(
                "node-group directory bit {bit} is governed by unsupported read-unsafe superblock feature bit {}",
                governor.trailing_zeros()
            )));
        }
    }
    Ok(supported_flags)
}

fn directory_governor_clear(bit: u32, governor: u64) -> DevonError {
    let context = match bit {
        0 => "node-group directory ZONE_MAP_STATS bit is set but ZONE_MAPS feature is clear"
            .to_owned(),
        1 => {
            "node-group directory COLUMN_ENCODINGS bit is set but COLUMN_ENCODINGS feature is clear"
                .to_owned()
        }
        _ => format!(
            "node-group directory bit {bit} is set but governing superblock feature bit {} is clear",
            governor.trailing_zeros()
        ),
    };
    corrupt(context)
}

fn pending_directory_flag(pager: &Pager, flag: u32, allow_pending: bool) -> bool {
    allow_pending
        && match flag {
            ZONE_MAP_DIRECTORY_FLAG => pager.has_written_zone_maps(),
            COLUMN_ENCODINGS_DIRECTORY_FLAG => pager.has_written_column_encodings(),
            _ => false,
        }
}

/// The validated extension sections of a directory: zone maps (bit 0),
/// column encodings (bit 1), and the offset past the last section.
type DecodedSections = (
    Option<Vec<ZoneMapStats>>,
    Option<Vec<(Encoding, [u8; 3])>>,
    usize,
);

fn decode_sections(
    page: &[u8],
    mut cursor: usize,
    flags: u32,
    supported_flags: u32,
    row_count: usize,
    types: &[LogicalType],
) -> DevonResult<DecodedSections> {
    let mut zone_maps = None;
    let mut encodings = None;
    for bit in 0..u32::BITS {
        let flag = 1_u32 << bit;
        if flags & flag == 0 {
            continue;
        }
        let supported = supported_flags & flag != 0;
        let (payload, end) = decode_section_frame(page, cursor, bit, supported)?;
        cursor = end;
        if !supported {
            continue;
        }
        if flag == ZONE_MAP_DIRECTORY_FLAG {
            zone_maps = Some(decode_zone_map_payload(payload, row_count, types)?);
        } else if flag == COLUMN_ENCODINGS_DIRECTORY_FLAG {
            encodings = Some(decode_encoding_section(payload, types)?);
        }
    }
    Ok((zone_maps, encodings, cursor))
}

/// Validates and decodes the `COLUMN_ENCODINGS` section payload: exactly
/// `4 × column_count` bytes, one `encoding_id u8 · p0 · p1 · p2` record
/// per main column, ids in the §8.1 table, admissible for the column's
/// type, parameters zero for plain/constant, and at least one non-plain
/// column (the section is absent when every payload is plain).
fn decode_encoding_section(
    payload: &[u8],
    types: &[LogicalType],
) -> DevonResult<Vec<(Encoding, [u8; 3])>> {
    let expected = types
        .len()
        .checked_mul(ENCODING_RECORD_LEN)
        .ok_or_else(|| corrupt("COLUMN_ENCODINGS payload length overflows usize"))?;
    if payload.len() != expected {
        return Err(corrupt(format!(
            "COLUMN_ENCODINGS section_len is {}, expected {expected}",
            payload.len()
        )));
    }
    let mut selections = Vec::with_capacity(types.len());
    let mut all_plain = true;
    for (index, (record, logical_type)) in payload
        .as_chunks::<ENCODING_RECORD_LEN>()
        .0
        .iter()
        .zip(types)
        .enumerate()
    {
        let encoding = Encoding::from_id(record[0]).ok_or_else(|| {
            corrupt(format!(
                "COLUMN_ENCODINGS column {index} encoding id {} is not registered",
                record[0]
            ))
        })?;
        if !encoding.is_admissible_for(logical_type) {
            return Err(corrupt(format!(
                "COLUMN_ENCODINGS column {index} encoding {} is not admissible for {logical_type}",
                encoding.name()
            )));
        }
        let params = [record[1], record[2], record[3]];
        if matches!(encoding, Encoding::Plain | Encoding::Constant) && params != [0; 3] {
            return Err(corrupt(format!(
                "COLUMN_ENCODINGS column {index} encoding {} parameters are not zero",
                encoding.name()
            )));
        }
        all_plain &= matches!(encoding, Encoding::Plain);
        selections.push((encoding, params));
    }
    if all_plain {
        return Err(corrupt(
            "COLUMN_ENCODINGS section is present but every column is plain",
        ));
    }
    Ok(selections)
}

fn decode_section_frame(
    page: &[u8],
    cursor: usize,
    bit: u32,
    validate_checksum: bool,
) -> DevonResult<(&[u8], usize)> {
    let header_end = cursor.checked_add(SECTION_HEADER_LEN).ok_or_else(|| {
        corrupt(format!(
            "node-group directory section bit {bit} header offset overflows"
        ))
    })?;
    if header_end > page.len() {
        return Err(corrupt(format!(
            "node-group directory section bit {bit} header overruns the page"
        )));
    }
    let section_len = read_u32(page, cursor) as usize;
    let section_end = header_end.checked_add(section_len).ok_or_else(|| {
        corrupt(format!(
            "node-group directory section bit {bit} length overflows"
        ))
    })?;
    if section_end > page.len() {
        return Err(corrupt(format!(
            "node-group directory section bit {bit} length overruns the page"
        )));
    }
    let payload = &page[header_end..section_end];
    if validate_checksum && crc32c(payload) != read_u32(page, cursor + 4) {
        return Err(corrupt(format!(
            "node-group directory section bit {bit} CRC-32C does not match"
        )));
    }
    Ok((payload, section_end))
}

fn decode_zone_map_payload(
    payload: &[u8],
    row_count: usize,
    types: &[LogicalType],
) -> DevonResult<Vec<ZoneMapStats>> {
    let expected = types
        .len()
        .checked_mul(ZONE_MAP_RECORD_LEN)
        .ok_or_else(|| corrupt("ZONE_MAP_STATS payload length overflows usize"))?;
    if payload.len() != expected {
        return Err(corrupt(format!(
            "ZONE_MAP_STATS section_len is {}, expected {expected}",
            payload.len()
        )));
    }
    let mut stats = Vec::with_capacity(types.len());
    for (index, (record, logical_type)) in payload
        .as_chunks::<ZONE_MAP_RECORD_LEN>()
        .0
        .iter()
        .zip(types)
        .enumerate()
    {
        stats.push(decode_zone_map_record(
            record,
            row_count,
            logical_type,
            index,
        )?);
    }
    Ok(stats)
}

fn decode_zone_map_record(
    record: &[u8],
    row_count: usize,
    logical_type: &LogicalType,
    column_index: usize,
) -> DevonResult<ZoneMapStats> {
    let null_count = read_u32(record, 0);
    if null_count as usize > row_count {
        return Err(corrupt(format!(
            "ZONE_MAP_STATS column {column_index} null_count {null_count} exceeds row_count {row_count}"
        )));
    }
    let stats_flags = read_u32(record, 4);
    if stats_flags & !STATS_MIN_MAX_PRESENT != 0 {
        return Err(corrupt(format!(
            "ZONE_MAP_STATS column {column_index} stats_flags contain unknown bits {:#x}",
            stats_flags & !STATS_MIN_MAX_PRESENT
        )));
    }
    let present = stats_flags & STATS_MIN_MAX_PRESENT != 0;
    let min_bytes = copy_array(&record[8..16]);
    let max_bytes = copy_array(&record[16..24]);
    if !present && (min_bytes != [0; 8] || max_bytes != [0; 8]) {
        return Err(corrupt(format!(
            "ZONE_MAP_STATS column {column_index} absent min/max bytes are not zero"
        )));
    }
    let has_non_null = (null_count as usize) < row_count;
    let (min, max) = decode_zone_map_bounds(
        logical_type,
        present,
        has_non_null,
        min_bytes,
        max_bytes,
        column_index,
    )?;
    Ok(ZoneMapStats {
        null_count,
        min,
        max,
    })
}

fn decode_zone_map_bounds(
    logical_type: &LogicalType,
    present: bool,
    has_non_null: bool,
    min_bytes: [u8; 8],
    max_bytes: [u8; 8],
    column_index: usize,
) -> DevonResult<(Option<ZoneMapValue>, Option<ZoneMapValue>)> {
    match logical_type {
        LogicalType::Int64 => {
            require_stats_presence(present, has_non_null, logical_type, column_index)?;
            if !present {
                return Ok((None, None));
            }
            let min = i64::from_le_bytes(min_bytes);
            let max = i64::from_le_bytes(max_bytes);
            if min > max {
                return Err(inverted_bounds(column_index));
            }
            Ok((
                Some(ZoneMapValue::Int64(min)),
                Some(ZoneMapValue::Int64(max)),
            ))
        }
        LogicalType::Float64 => {
            if present && !has_non_null {
                return Err(invalid_stats_presence(column_index, logical_type));
            }
            if !present {
                return Ok((None, None));
            }
            let min = f64::from_le_bytes(min_bytes);
            let max = f64::from_le_bytes(max_bytes);
            if min.is_nan() || max.is_nan() {
                return Err(corrupt(format!(
                    "ZONE_MAP_STATS column {column_index} present Float64 bounds contain NaN"
                )));
            }
            if min.total_cmp(&max).is_gt() {
                return Err(inverted_bounds(column_index));
            }
            Ok((
                Some(ZoneMapValue::Float64(min)),
                Some(ZoneMapValue::Float64(max)),
            ))
        }
        LogicalType::GeoPoint => {
            require_stats_presence(present, has_non_null, logical_type, column_index)?;
            if !present {
                return Ok((None, None));
            }
            let min = u64::from_le_bytes(min_bytes);
            let max = u64::from_le_bytes(max_bytes);
            validate_atom_key(min, column_index, "min")?;
            validate_atom_key(max, column_index, "max")?;
            if min > max {
                return Err(inverted_bounds(column_index));
            }
            Ok((
                Some(ZoneMapValue::GeoPoint(min)),
                Some(ZoneMapValue::GeoPoint(max)),
            ))
        }
        LogicalType::Bool
        | LogicalType::String
        | LogicalType::Vector { .. }
        | LogicalType::VectorEncoded { .. }
        | LogicalType::Timestamp
        | LogicalType::Bytes
        | LogicalType::Decimal { .. }
        | LogicalType::Json => {
            if present {
                return Err(invalid_stats_presence(column_index, logical_type));
            }
            Ok((None, None))
        }
    }
}

fn require_stats_presence(
    present: bool,
    has_non_null: bool,
    logical_type: &LogicalType,
    column_index: usize,
) -> DevonResult<()> {
    if present != has_non_null {
        return Err(invalid_stats_presence(column_index, logical_type));
    }
    Ok(())
}

fn invalid_stats_presence(column_index: usize, logical_type: &LogicalType) -> DevonError {
    corrupt(format!(
        "ZONE_MAP_STATS column {column_index} min/max presence violates {logical_type} rules"
    ))
}

fn inverted_bounds(column_index: usize) -> DevonError {
    corrupt(format!(
        "ZONE_MAP_STATS column {column_index} min is greater than max"
    ))
}

fn validate_atom_key(value: u64, column_index: usize, endpoint: &str) -> DevonResult<()> {
    let atom = devondb_geo::index::CellIndex::try_from(value).map_err(|error| {
        corrupt(format!(
            "ZONE_MAP_STATS column {column_index} {endpoint} is not a valid atom key: {error}"
        ))
    })?;
    if !atom.is_atom() {
        return Err(corrupt(format!(
            "ZONE_MAP_STATS column {column_index} {endpoint} is not resolution 15"
        )));
    }
    Ok(())
}

fn directory_entries_end(column_count: usize, page_len: usize) -> DevonResult<usize> {
    let entries_len = column_count
        .checked_mul(DIRECTORY_ENTRY_LEN)
        .ok_or_else(|| corrupt("node-group directory entry length overflows usize"))?;
    let end = DIRECTORY_HEADER_LEN
        .checked_add(entries_len)
        .ok_or_else(|| corrupt("node-group directory length overflows usize"))?;
    if end > page_len {
        return Err(corrupt(
            "node-group column_count exceeds directory page capacity",
        ));
    }
    Ok(end)
}

fn decode_entry(page: &[u8], index: usize) -> ColumnEntry {
    let offset = DIRECTORY_HEADER_LEN + index * DIRECTORY_ENTRY_LEN;
    ColumnEntry {
        first_page: read_u64(page, offset),
        byte_len: read_u32(page, offset + 8),
        checksum: read_u32(page, offset + 12),
    }
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
        return Err(invalid_argument("column payload cannot occupy zero pages"));
    }
    // One atomic run allocation: the pager guarantees contiguity whether
    // the run is reused from the free-page ledger or appended.
    let first_page = pager.allocate_run(page_count)?;
    (0..page_count)
        .map(|index| {
            first_page
                .checked_add(index as u64)
                .ok_or_else(|| corrupt("payload page run overflows u64"))
        })
        .collect()
}

fn read_payload(
    pager: &Pager,
    entry: ColumnEntry,
    page_size: usize,
    column_index: usize,
) -> DevonResult<Vec<u8>> {
    let byte_len = entry.byte_len as usize;
    let page_count = byte_len.div_ceil(page_size);
    let mut payload = Vec::with_capacity(byte_len);
    for index in 0..page_count {
        let offset = u64::try_from(index)
            .map_err(|_| corrupt(format!("column {column_index} page index exceeds u64")))?;
        let page_id = entry.first_page.checked_add(offset).ok_or_else(|| {
            corrupt(format!(
                "column {column_index} payload page run overflows u64"
            ))
        })?;
        let page = read_referenced_page(pager, page_id, column_index)?;
        let take = (byte_len - payload.len()).min(page_size);
        payload.extend_from_slice(&page[..take]);
        if take < page_size && page[take..].iter().any(|byte| *byte != 0) {
            return Err(corrupt(format!(
                "column {column_index} payload page padding is not zero"
            )));
        }
    }
    Ok(payload)
}

fn read_referenced_page(pager: &Pager, page_id: u64, column_index: usize) -> DevonResult<Vec<u8>> {
    match pager.read_page(page_id) {
        Ok(page) => Ok(page),
        Err(DevonError::InvalidArgument { context }) => Err(corrupt(format!(
            "column {column_index} references invalid payload page {page_id}: {context}"
        ))),
        Err(error) => Err(error),
    }
}

/// The declared values-section encoding of one main column, defaulting to
/// plain when the directory carries no `COLUMN_ENCODINGS` section.
fn column_encoding(
    encodings: Option<&[(Encoding, [u8; 3])]>,
    column_index: usize,
) -> (Encoding, [u8; 3]) {
    encodings
        .and_then(|selections| selections.get(column_index))
        .copied()
        .unwrap_or((Encoding::Plain, [0; 3]))
}

fn validate_payload_len(
    byte_len: u32,
    row_count: usize,
    logical_type: &LogicalType,
    column_index: usize,
    encoding: Encoding,
) -> DevonResult<()> {
    let actual = byte_len as usize;
    // Every payload, any encoding, must at least cover the validity bitmap.
    let validity_len = bitmap_len(row_count);
    if actual < validity_len {
        return Err(corrupt(format!(
            "column {column_index} byte_len is {actual}, smaller than the validity bitmap {validity_len}"
        )));
    }
    match encoding {
        Encoding::Plain => {
            validate_plain_payload_len(actual, row_count, logical_type, column_index)
        }
        Encoding::Constant => {
            match encodings::constant::value_section_len(logical_type) {
                Some(width) => {
                    let expected = validity_len.checked_add(width).ok_or_else(|| {
                        corrupt(format!(
                            "column {column_index} implied payload length overflows"
                        ))
                    })?;
                    if actual != expected {
                        return Err(corrupt(format!(
                            "column {column_index} byte_len is {actual}, expected exactly {expected} for constant-encoded {logical_type}"
                        )));
                    }
                }
                // String: the values section is u32 length + bytes; the
                // exact length is data-dependent and validated at decode.
                None => {
                    let minimum = validity_len.checked_add(4).ok_or_else(|| {
                        corrupt(format!(
                            "column {column_index} implied payload length overflows"
                        ))
                    })?;
                    if actual < minimum {
                        return Err(corrupt(format!(
                            "column {column_index} byte_len is {actual}, smaller than constant-encoded String prefix {minimum}"
                        )));
                    }
                }
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

fn validate_plain_payload_len(
    actual: usize,
    row_count: usize,
    logical_type: &LogicalType,
    column_index: usize,
) -> DevonResult<()> {
    if let Some(kind) = heap_value_kind(logical_type) {
        let minimum = string_payload_prefix_len(row_count).ok_or_else(|| {
            corrupt(format!(
                "column {column_index} {} prefix length overflows",
                kind.name()
            ))
        })?;
        if actual < minimum {
            return Err(corrupt(format!(
                "column {column_index} byte_len is {actual}, smaller than {} prefix {minimum}",
                kind.name()
            )));
        }
        return Ok(());
    }

    let expected = fixed_payload_len(row_count, logical_type).ok_or_else(|| {
        corrupt(format!(
            "column {column_index} implied payload length overflows"
        ))
    })?;
    if actual != expected {
        return Err(corrupt(format!(
            "column {column_index} byte_len is {actual}, expected exactly {expected} for {logical_type}"
        )));
    }
    Ok(())
}

fn fixed_payload_len(row_count: usize, logical_type: &LogicalType) -> Option<usize> {
    let validity_len = bitmap_len(row_count);
    let values_len = match logical_type {
        LogicalType::Bool => validity_len,
        LogicalType::Int64 | LogicalType::Float64 | LogicalType::Timestamp => {
            row_count.checked_mul(8)?
        }
        LogicalType::Vector { dim } => row_count.checked_mul(*dim as usize)?.checked_mul(4)?,
        LogicalType::VectorEncoded { dim, encoding } => {
            row_count.checked_mul(encoded_vector_slot_len(*dim as usize, *encoding)?)?
        }
        LogicalType::GeoPoint | LogicalType::Decimal { .. } => row_count.checked_mul(16)?,
        LogicalType::String | LogicalType::Bytes | LogicalType::Json => return None,
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
    encoding: Encoding,
    params: [u8; 3],
) -> DevonResult<Vec<Value>> {
    if encoding == Encoding::Plain {
        return decode_column_plain(payload, row_count, logical_type, column_index);
    }
    let column = decode_column_encoded(
        payload,
        row_count,
        logical_type,
        column_index,
        encoding,
        params,
    )?;
    Ok(materialize_values(&column))
}

/// Decodes a non-plain payload through the §8.2 seam: the validity bitmap
/// is never encoded, so it is split, padding-validated, and repacked into
/// §6.3 bitmap words exactly as on the plain path before dispatch.
fn decode_column_encoded(
    payload: &[u8],
    row_count: usize,
    logical_type: &LogicalType,
    column_index: usize,
    encoding: Encoding,
    params: [u8; 3],
) -> DevonResult<Column> {
    let validity_len = bitmap_len(row_count);
    if payload.len() < validity_len {
        return Err(corrupt(format!(
            "column {column_index} payload is {} bytes, shorter than the validity bitmap {validity_len}",
            payload.len()
        )));
    }
    let validity = &payload[..validity_len];
    validate_bitmap_padding(validity, row_count, column_index, "validity")?;
    let bitmap = bitmap_from_validity_bytes(validity, row_count);
    encodings::decode_values(
        encoding,
        params,
        &payload[validity_len..],
        &bitmap,
        row_count,
        logical_type,
    )
}

/// Materializes a decoded typed column into the boxed row form the legacy
/// read API returns; [`Column::Boxed`] is borrowed unchanged.
fn materialize_values(column: &Column) -> Vec<Value> {
    match column {
        Column::Boxed(values) => values.clone(),
        _ => (0..column.len()).map(|row| column.value_at(row)).collect(),
    }
}

/// Repacks FORMAT LSB-first validity bytes into the §6.3 `u64`-word
/// bitmap the encoding seam consumes. The caller has already validated
/// trailing-bit padding on the byte form.
fn bitmap_from_validity_bytes(bytes: &[u8], row_count: usize) -> Bitmap {
    let mut bitmap = Bitmap::all_valid(row_count);
    for row in 0..row_count {
        if !bit_is_set(bytes, row) {
            bitmap.clear(row);
        }
    }
    bitmap
}

/// Repacks a §6.3 bitmap into FORMAT LSB-first validity bytes — the plain
/// arm of the seam delegates to the legacy byte-based decoders.
fn validity_bytes_from_bitmap(bitmap: &Bitmap) -> Vec<u8> {
    let mut bytes = vec![0_u8; bitmap_len(bitmap.len())];
    for row in 0..bitmap.len() {
        if bitmap.is_valid(row) {
            set_bit(&mut bytes, row);
        }
    }
    bytes
}

fn decode_column_plain(
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
        LogicalType::VectorEncoded { dim, encoding } => decode_vector_encoded(
            validity,
            values,
            row_count,
            *dim as usize,
            *encoding,
            column_index,
        ),
        LogicalType::GeoPoint => {
            crate::geo_column::decode_geo_points(validity, values, row_count, column_index)
        }
        LogicalType::Timestamp => decode_timestamps(validity, values, row_count, column_index),
        LogicalType::Bytes => decode_heap_values(
            validity,
            values,
            row_count,
            column_index,
            HeapValueKind::Bytes,
        ),
        LogicalType::Decimal { precision, scale } => decode_decimals(
            validity,
            values,
            row_count,
            column_index,
            *precision,
            *scale,
        ),
        LogicalType::Json => decode_heap_values(
            validity,
            values,
            row_count,
            column_index,
            HeapValueKind::Json,
        ),
    }
}

/// Decodes one validated column payload straight into typed [`Column`]
/// storage (docs/SCALE.md §6.5): the bytes are already the typed layout.
/// Int64/Float64/Bool/Timestamp/Decimal map the values section to typed
/// vectors and repack the FORMAT LSB-first validity bytes into §6.3 `u64`
/// words; null slots keep the payload's guaranteed zeros (the zero check
/// stays in force). Every other type decodes through [`decode_column`] into
/// [`Column::Boxed`] until its arena variant is available.
fn decode_column_typed(
    payload: &[u8],
    row_count: usize,
    logical_type: &LogicalType,
    column_index: usize,
    encoding: Encoding,
    params: [u8; 3],
) -> DevonResult<Column> {
    if encoding != Encoding::Plain {
        return decode_column_encoded(
            payload,
            row_count,
            logical_type,
            column_index,
            encoding,
            params,
        );
    }
    let validity_len = bitmap_len(row_count);
    let validity = &payload[..validity_len];
    validate_bitmap_padding(validity, row_count, column_index, "validity")?;
    let values = &payload[validity_len..];
    match logical_type {
        LogicalType::Bool => {
            let (values, validity) = decode_typed_bools(validity, values, row_count, column_index)?;
            Ok(Column::Bool { values, validity })
        }
        LogicalType::Int64 => {
            let (values, validity) =
                decode_typed_slots(validity, values, row_count, 8, column_index, |slot| {
                    i64::from_le_bytes(copy_array(slot))
                })?;
            Ok(Column::Int64 { values, validity })
        }
        LogicalType::Float64 => {
            let (values, validity) =
                decode_typed_slots(validity, values, row_count, 8, column_index, |slot| {
                    f64::from_le_bytes(copy_array(slot))
                })?;
            Ok(Column::Float64 { values, validity })
        }
        LogicalType::Timestamp => {
            let (values, validity) =
                decode_typed_slots(validity, values, row_count, 8, column_index, |slot| {
                    i64::from_le_bytes(copy_array(slot))
                })?;
            Ok(Column::Timestamp { values, validity })
        }
        LogicalType::Decimal { precision, scale } => {
            let (values, validity) = decode_typed_decimals(
                validity,
                values,
                row_count,
                column_index,
                *precision,
                *scale,
            )?;
            Ok(Column::Decimal {
                values,
                scale: *scale,
                validity,
            })
        }
        LogicalType::String
        | LogicalType::Bytes
        | LogicalType::Json
        | LogicalType::Vector { .. }
        | LogicalType::VectorEncoded { .. }
        | LogicalType::GeoPoint => Ok(Column::Boxed(decode_column(
            payload,
            row_count,
            logical_type,
            column_index,
            Encoding::Plain,
            [0; 3],
        )?)),
    }
}

/// Reads, checksums, and typed-decodes one column payload run.
fn read_typed_payload(
    pager: &Pager,
    entry: ColumnEntry,
    page_size: usize,
    row_count: usize,
    logical_type: &LogicalType,
    column_index: usize,
    selection: (Encoding, [u8; 3]),
) -> DevonResult<Column> {
    let (encoding, params) = selection;
    validate_payload_len(
        entry.byte_len,
        row_count,
        logical_type,
        column_index,
        encoding,
    )?;
    let payload = read_payload(pager, entry, page_size, column_index)?;
    if crc32c(&payload) != entry.checksum {
        return Err(corrupt(format!(
            "column {column_index} payload CRC-32C does not match"
        )));
    }
    decode_column_typed(
        &payload,
        row_count,
        logical_type,
        column_index,
        encoding,
        params,
    )
}

/// Fixed-width typed decode: one slot per row. Null slots decode from the
/// payload's zeroed bytes (verified by [`ensure_null_slot_zero`]), so the
/// typed vector holds zero at null slots per docs/SCALE.md §6.3; the
/// validity bitmap is allocated lazily on the first NULL.
fn decode_typed_slots<T: Copy>(
    validity: &[u8],
    values: &[u8],
    row_count: usize,
    slot_len: usize,
    column_index: usize,
    decode: impl Fn(&[u8]) -> T,
) -> DevonResult<(Vec<T>, Option<Bitmap>)> {
    let mut column = Vec::with_capacity(row_count);
    let mut bitmap: Option<Bitmap> = None;
    for row in 0..row_count {
        let start = row * slot_len;
        let slot = &values[start..start + slot_len];
        if !bit_is_set(validity, row) {
            ensure_null_slot_zero(slot, column_index, row)?;
            bitmap
                .get_or_insert_with(|| Bitmap::all_valid(row_count))
                .clear(row);
        }
        column.push(decode(slot));
    }
    Ok((column, bitmap))
}

/// Bool typed decode: the values section is a second LSB-first bitmap.
fn decode_typed_bools(
    validity: &[u8],
    values: &[u8],
    row_count: usize,
    column_index: usize,
) -> DevonResult<(Vec<bool>, Option<Bitmap>)> {
    validate_bitmap_padding(values, row_count, column_index, "Bool values")?;
    let mut column = Vec::with_capacity(row_count);
    let mut bitmap: Option<Bitmap> = None;
    for row in 0..row_count {
        let value = bit_is_set(values, row);
        if !bit_is_set(validity, row) {
            if value {
                return Err(corrupt(format!(
                    "column {column_index} row {row} null Bool slot is not zero"
                )));
            }
            bitmap
                .get_or_insert_with(|| Bitmap::all_valid(row_count))
                .clear(row);
        }
        column.push(value);
    }
    Ok((column, bitmap))
}

/// Decimal typed decode: 16-byte i128 digits per row with the declared
/// precision/scale validation of the boxed decoder kept in force.
fn decode_typed_decimals(
    validity: &[u8],
    values: &[u8],
    row_count: usize,
    column_index: usize,
    precision: u8,
    scale: u8,
) -> DevonResult<(Vec<i128>, Option<Bitmap>)> {
    validate_decimal_type(precision, scale, column_index)?;
    let mut column = Vec::with_capacity(row_count);
    let mut bitmap: Option<Bitmap> = None;
    for row in 0..row_count {
        let slot = &values[row * 16..(row + 1) * 16];
        if !bit_is_set(validity, row) {
            ensure_null_slot_zero(slot, column_index, row)?;
            bitmap
                .get_or_insert_with(|| Bitmap::all_valid(row_count))
                .clear(row);
            column.push(0);
            continue;
        }
        let digits = i128::from_le_bytes(copy_array(slot));
        let value = Decimal128::new(digits, scale).map_err(|error| {
            corrupt(format!(
                "column {column_index} row {row} Decimal is invalid: {error}"
            ))
        })?;
        if !value.fits(precision, scale) {
            return Err(corrupt(format!(
                "column {column_index} row {row} Decimal digits exceed declared precision {precision}"
            )));
        }
        column.push(digits);
    }
    Ok((column, bitmap))
}

fn decode_bools(
    validity: &[u8],
    values: &[u8],
    row_count: usize,
    column_index: usize,
) -> DevonResult<Vec<Value>> {
    validate_bitmap_padding(values, row_count, column_index, "Bool values")?;
    let mut column = Vec::with_capacity(row_count);
    for row in 0..row_count {
        if bit_is_set(validity, row) {
            column.push(Value::Bool(bit_is_set(values, row)));
        } else {
            if bit_is_set(values, row) {
                return Err(corrupt(format!(
                    "column {column_index} row {row} null Bool slot is not zero"
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
    column_index: usize,
) -> DevonResult<Vec<Value>> {
    decode_fixed_slots(validity, values, row_count, 8, column_index, |slot| {
        Value::Int64(i64::from_le_bytes(copy_array(slot)))
    })
}

fn decode_float64s(
    validity: &[u8],
    values: &[u8],
    row_count: usize,
    column_index: usize,
) -> DevonResult<Vec<Value>> {
    decode_fixed_slots(validity, values, row_count, 8, column_index, |slot| {
        Value::Float64(f64::from_le_bytes(copy_array(slot)))
    })
}

fn decode_timestamps(
    validity: &[u8],
    values: &[u8],
    row_count: usize,
    column_index: usize,
) -> DevonResult<Vec<Value>> {
    decode_fixed_slots(validity, values, row_count, 8, column_index, |slot| {
        Value::Timestamp(i64::from_le_bytes(copy_array(slot)))
    })
}

fn decode_decimals(
    validity: &[u8],
    values: &[u8],
    row_count: usize,
    column_index: usize,
    precision: u8,
    scale: u8,
) -> DevonResult<Vec<Value>> {
    validate_decimal_type(precision, scale, column_index)?;
    let mut column = Vec::with_capacity(row_count);
    for row in 0..row_count {
        let slot = &values[row * 16..(row + 1) * 16];
        if !bit_is_set(validity, row) {
            ensure_null_slot_zero(slot, column_index, row)?;
            column.push(Value::Null);
            continue;
        }
        let value =
            Decimal128::new(i128::from_le_bytes(copy_array(slot)), scale).map_err(|error| {
                corrupt(format!(
                    "column {column_index} row {row} Decimal is invalid: {error}"
                ))
            })?;
        if !value.fits(precision, scale) {
            return Err(corrupt(format!(
                "column {column_index} row {row} Decimal digits exceed declared precision {precision}"
            )));
        }
        column.push(Value::Decimal(value));
    }
    Ok(column)
}

fn validate_decimal_type(precision: u8, scale: u8, column_index: usize) -> DevonResult<()> {
    if !(1..=MAX_PRECISION).contains(&precision) || scale > precision {
        return Err(corrupt(format!(
            "column {column_index} has invalid Decimal({precision}, {scale}) declaration"
        )));
    }
    Ok(())
}

fn decode_vectors(
    validity: &[u8],
    values: &[u8],
    row_count: usize,
    dim: usize,
    column_index: usize,
) -> DevonResult<Vec<Value>> {
    let slot_len = dim.checked_mul(4).ok_or_else(|| {
        corrupt(format!(
            "column {column_index} vector slot length overflows"
        ))
    })?;
    decode_fixed_slots(
        validity,
        values,
        row_count,
        slot_len,
        column_index,
        |slot| {
            let vector = slot
                .as_chunks::<4>()
                .0
                .iter()
                .map(|bytes| f32::from_le_bytes(copy_array(bytes)))
                .collect();
            Value::Vector(vector)
        },
    )
}

fn decode_vector_encoded(
    validity: &[u8],
    values: &[u8],
    row_count: usize,
    dim: usize,
    encoding: VectorEncoding,
    column_index: usize,
) -> DevonResult<Vec<Value>> {
    let slot_len = encoded_vector_slot_len(dim, encoding).ok_or_else(|| {
        corrupt(format!(
            "column {column_index} vector slot length overflows"
        ))
    })?;
    let mut column = Vec::with_capacity(row_count);
    for row in 0..row_count {
        let start = row * slot_len;
        let slot = &values[start..start + slot_len];
        if !bit_is_set(validity, row) {
            ensure_null_slot_zero(slot, column_index, row)?;
            column.push(Value::Null);
            continue;
        }
        let vector = match encoding {
            VectorEncoding::F16 => decode_f16(slot),
            VectorEncoding::I8 => decode_i8(slot),
            VectorEncoding::B1 { rotation_seed, .. } => decode_b1(slot, dim, rotation_seed),
        }
        .map_err(|error| {
            corrupt(format!(
                "column {column_index} row {row} encoded vector is invalid: {error}"
            ))
        })?;
        column.push(Value::Vector(vector));
    }
    Ok(column)
}

fn encoded_vector_slot_len(dim: usize, encoding: VectorEncoding) -> Option<usize> {
    match encoding {
        VectorEncoding::F16 => dim.checked_mul(2),
        VectorEncoding::I8 => dim.checked_add(8),
        VectorEncoding::B1 { .. } => dim.checked_add(7)?.checked_div(8),
    }
}

fn b1_rescore_type(logical_type: &LogicalType) -> Option<LogicalType> {
    let LogicalType::VectorEncoded {
        dim,
        encoding: VectorEncoding::B1 { rescore, .. },
    } = *logical_type
    else {
        return None;
    };
    match rescore {
        B1Rescore::None => None,
        B1Rescore::F16 => Some(LogicalType::VectorEncoded {
            dim,
            encoding: VectorEncoding::F16,
        }),
        B1Rescore::I8 => Some(LogicalType::VectorEncoded {
            dim,
            encoding: VectorEncoding::I8,
        }),
        B1Rescore::F32 => Some(LogicalType::Vector { dim }),
    }
}

fn b1_rescore_count(types: &[LogicalType]) -> usize {
    types
        .iter()
        .filter(|logical_type| b1_rescore_type(logical_type).is_some())
        .count()
}

/// Sniffs one directory page's flags word for the `COLUMN_ENCODINGS`
/// section bit — the catalog-save derivation of superblock feature bit 13
/// (`docs/SCALE.md` §8.1: set iff any node group carries a non-plain
/// payload). A raw header read, tolerant of unreadable or malformed pages
/// exactly like the superseded-page expansion in catalog save; full
/// directory validation lives on the read paths.
pub(crate) fn directory_declares_column_encodings(pager: &Pager, directory_page: u64) -> bool {
    let Ok(page) = pager.read_page(directory_page) else {
        return false;
    };
    page.len() >= DIRECTORY_HEADER_LEN
        && &page[..4] == DIRECTORY_MAGIC
        && read_u32(&page, 12) & COLUMN_ENCODINGS_DIRECTORY_FLAG != 0
}

/// Enumerates every page one persisted node group occupies: the directory
/// page plus each column payload run. The superseded-set expansion at
/// publication retires a whole group through this
/// (`docs/FREE_PAGES.md` § Retirement and reclamation sequence), and the
/// offline sweep will share it.
pub fn group_page_inventory(
    pager: &Pager,
    directory_page: u64,
    types: &[LogicalType],
) -> DevonResult<Vec<u64>> {
    // Catalog publication may inventory a newly written group before the
    // governing superblock flip. `read_directory` admits that state only
    // when the page exactly matches bytes this pager's writer just produced;
    // ordinary inventory of any external page stays strict.
    let directory = NodeGroup::read_directory(pager, directory_page, types)?;
    let page_size = pager.superblock().page_size as usize;
    let mut pages = vec![directory_page];
    for entry in &directory.entries {
        extend_payload_run(&mut pages, entry.first_page, entry.byte_len, page_size)?;
    }
    Ok(pages)
}

/// Appends the contiguous page run `first_page + ceil(byte_len / page_size)`
/// that one column payload occupies on disk.
fn extend_payload_run(
    pages: &mut Vec<u64>,
    first_page: u64,
    byte_len: u32,
    page_size: usize,
) -> DevonResult<()> {
    let count = (byte_len as usize).div_ceil(page_size).max(1);
    for index in 0..count as u64 {
        let page_id = first_page
            .checked_add(index)
            .ok_or_else(|| corrupt("column payload page run overflows u64"))?;
        pages.push(page_id);
    }
    Ok(())
}

fn validate_rescore_rows(
    main: &[Value],
    rescore: &[Value],
    column_index: usize,
) -> DevonResult<()> {
    if main.len() != rescore.len() {
        return Err(corrupt(format!(
            "column {column_index} b1 main and rescore row counts disagree"
        )));
    }
    for (row, (main_value, rescore_value)) in main.iter().zip(rescore).enumerate() {
        if matches!(main_value, Value::Null) != matches!(rescore_value, Value::Null) {
            return Err(corrupt(format!(
                "column {column_index} row {row} b1 main and rescore null positions disagree"
            )));
        }
    }
    Ok(())
}

fn validate_stats_against_columns(
    stats: &[ZoneMapStats],
    columns: &[Vec<Value>],
    types: &[LogicalType],
    row_count: usize,
) -> DevonResult<()> {
    if stats.len() != columns.len() || columns.len() != types.len() {
        return Err(corrupt(
            "ZONE_MAP_STATS column count disagrees with decoded payload columns",
        ));
    }
    for (index, ((stat, column), logical_type)) in stats.iter().zip(columns).zip(types).enumerate()
    {
        let actual_nulls = column
            .iter()
            .filter(|value| matches!(value, Value::Null))
            .count();
        if actual_nulls != stat.null_count as usize {
            return Err(corrupt(format!(
                "ZONE_MAP_STATS column {index} null_count {} disagrees with validity bitmap count {actual_nulls}",
                stat.null_count
            )));
        }
        validate_column_bounds(stat, column, logical_type, row_count, index)?;
    }
    Ok(())
}

fn validate_column_bounds(
    stat: &ZoneMapStats,
    column: &[Value],
    logical_type: &LogicalType,
    row_count: usize,
    column_index: usize,
) -> DevonResult<()> {
    match (logical_type, stat.min, stat.max) {
        (LogicalType::Int64, Some(ZoneMapValue::Int64(min)), Some(ZoneMapValue::Int64(max))) => {
            for value in column {
                if let Value::Int64(value) = value
                    && (*value < min || *value > max)
                {
                    return Err(payload_outside_bounds(column_index));
                }
            }
        }
        (
            LogicalType::Float64,
            Some(ZoneMapValue::Float64(min)),
            Some(ZoneMapValue::Float64(max)),
        ) => {
            for value in column {
                if let Value::Float64(value) = value
                    && (value.is_nan()
                        || value.total_cmp(&min).is_lt()
                        || value.total_cmp(&max).is_gt())
                {
                    return Err(payload_outside_bounds(column_index));
                }
            }
        }
        (
            LogicalType::GeoPoint,
            Some(ZoneMapValue::GeoPoint(min)),
            Some(ZoneMapValue::GeoPoint(max)),
        ) => {
            for value in column {
                if let Value::GeoPoint(point) = value {
                    let atom = devondb_geo::grid::atom(point.lat_deg(), point.lng_deg())
                        .map_err(|error| {
                            corrupt(format!(
                                "column {column_index} GeoPoint cannot be assigned an atom: {error}"
                            ))
                        })?
                        .raw();
                    if atom < min || atom > max {
                        return Err(payload_outside_bounds(column_index));
                    }
                }
            }
        }
        (LogicalType::Float64, None, None) => {
            let has_non_null = (stat.null_count as usize) < row_count;
            let has_nan = column
                .iter()
                .any(|value| matches!(value, Value::Float64(value) if value.is_nan()));
            if has_non_null && !has_nan {
                return Err(corrupt(format!(
                    "ZONE_MAP_STATS column {column_index} omits Float64 bounds without a NaN"
                )));
            }
        }
        (_, None, None) => {}
        _ => {
            return Err(corrupt(format!(
                "ZONE_MAP_STATS column {column_index} bounds type disagrees with schema"
            )));
        }
    }
    Ok(())
}

fn payload_outside_bounds(column_index: usize) -> DevonError {
    corrupt(format!(
        "column {column_index} payload value is outside its ZONE_MAP_STATS bounds"
    ))
}

fn decode_fixed_slots<F>(
    validity: &[u8],
    values: &[u8],
    row_count: usize,
    slot_len: usize,
    column_index: usize,
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
            ensure_null_slot_zero(slot, column_index, row)?;
            column.push(Value::Null);
        }
    }
    Ok(column)
}

fn decode_strings(
    validity: &[u8],
    values: &[u8],
    row_count: usize,
    column_index: usize,
) -> DevonResult<Vec<Value>> {
    decode_heap_values(
        validity,
        values,
        row_count,
        column_index,
        HeapValueKind::String,
    )
}

fn decode_heap_values(
    validity: &[u8],
    values: &[u8],
    row_count: usize,
    column_index: usize,
    kind: HeapValueKind,
) -> DevonResult<Vec<Value>> {
    let offsets_len = (row_count + 1) * 4;
    let offsets_bytes = &values[..offsets_len];
    let heap = &values[offsets_len..];
    let offsets: Vec<usize> = offsets_bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|bytes| u32::from_le_bytes(copy_array(bytes)) as usize)
        .collect();
    validate_string_offsets(&offsets, heap.len(), column_index, kind)?;

    let mut column = Vec::with_capacity(row_count);
    for row in 0..row_count {
        let start = offsets[row];
        let end = offsets[row + 1];
        if bit_is_set(validity, row) {
            column.push(decode_heap_value(
                &heap[start..end],
                column_index,
                row,
                kind,
            )?);
        } else {
            if start != end {
                return Err(corrupt(format!(
                    "column {column_index} row {row} null {} slot is not zero length",
                    kind.name()
                )));
            }
            column.push(Value::Null);
        }
    }
    Ok(column)
}

fn decode_heap_value(
    bytes: &[u8],
    column_index: usize,
    row: usize,
    kind: HeapValueKind,
) -> DevonResult<Value> {
    if matches!(kind, HeapValueKind::Bytes) {
        return Ok(Value::Bytes(bytes.to_vec()));
    }
    let value = str::from_utf8(bytes).map_err(|error| {
        corrupt(format!(
            "column {column_index} row {row} {} is not valid UTF-8: {error}",
            kind.name()
        ))
    })?;
    Ok(match kind {
        HeapValueKind::String => Value::String(value.to_owned()),
        HeapValueKind::Json => Value::Json(value.to_owned()),
        HeapValueKind::Bytes => Value::Bytes(bytes.to_vec()),
    })
}

fn validate_string_offsets(
    offsets: &[usize],
    heap_len: usize,
    column_index: usize,
    kind: HeapValueKind,
) -> DevonResult<()> {
    if offsets.first() != Some(&0) {
        return Err(corrupt(format!(
            "column {column_index} {} offsets do not start at zero",
            kind.name()
        )));
    }
    if offsets.windows(2).any(|pair| pair[0] > pair[1]) {
        return Err(corrupt(format!(
            "column {column_index} {} offsets are not monotonic",
            kind.name()
        )));
    }
    if offsets.last() != Some(&heap_len) {
        return Err(corrupt(format!(
            "column {column_index} final {} offset does not equal heap length {heap_len}",
            kind.name()
        )));
    }
    Ok(())
}

fn heap_value_kind(logical_type: &LogicalType) -> Option<HeapValueKind> {
    match logical_type {
        LogicalType::String => Some(HeapValueKind::String),
        LogicalType::Bytes => Some(HeapValueKind::Bytes),
        LogicalType::Json => Some(HeapValueKind::Json),
        _ => None,
    }
}

fn validate_bitmap_padding(
    bitmap: &[u8],
    row_count: usize,
    column_index: usize,
    name: &str,
) -> DevonResult<()> {
    let used_bits = row_count % 8;
    if used_bits == 0 {
        return Ok(());
    }
    let used_mask = (1_u8 << used_bits) - 1;
    if bitmap.last().is_some_and(|byte| byte & !used_mask != 0) {
        return Err(corrupt(format!(
            "column {column_index} {name} bitmap trailing bits are not zero"
        )));
    }
    Ok(())
}

fn ensure_null_slot_zero(slot: &[u8], column_index: usize, row: usize) -> DevonResult<()> {
    if slot.iter().any(|byte| *byte != 0) {
        return Err(corrupt(format!(
            "column {column_index} row {row} null value slot is not zero"
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
        "column {column} row {row} has invalid stored value {value}"
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
    use std::fs::OpenOptions;
    use std::io::{Read, Seek, SeekFrom, Write};
    use std::path::Path;

    use devondb_types::{Decimal128, DevonError, logical_type::LogicalType, value::Value};
    use tempfile::{TempDir, tempdir};

    use super::{
        ColumnEntry, DIRECTORY_ENTRY_LEN, DIRECTORY_HEADER_LEN, Encoding, NODE_GROUP_CAPACITY,
        NodeGroup, Pager, decode_entry,
    };

    const PAGE_SIZE: u32 = 4096;
    const DB_ID: [u8; 16] = *b"node-group-tests";

    fn all_types() -> Vec<LogicalType> {
        vec![
            LogicalType::Bool,
            LogicalType::Int64,
            LogicalType::Float64,
            LogicalType::String,
            LogicalType::Vector { dim: 3 },
        ]
    }

    fn all_types_group() -> NodeGroup {
        let mut group = NodeGroup::new(all_types()).unwrap();
        group
            .push_row(vec![
                Value::Bool(true),
                Value::Int64(42),
                Value::Float64(1.5),
                Value::String("devon".into()),
                Value::Vector(vec![0.25, -2.0, 3.5]),
            ])
            .unwrap();
        group
            .push_row(vec![
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
            ])
            .unwrap();
        group
            .push_row(vec![
                Value::Bool(false),
                Value::Int64(i64::MIN),
                Value::Float64(-0.0),
                Value::String(String::new()),
                Value::Vector(vec![0.0, 1.0, -1.0]),
            ])
            .unwrap();
        group
    }

    fn decimal(digits: i128, scale: u8) -> Value {
        Value::Decimal(Decimal128::new(digits, scale).unwrap())
    }

    #[test]
    fn read_column_matches_full_read_and_refuses_out_of_bounds() {
        // Exercise the pruned-decode primitive: equivalence against the
        // full read, bounds refusal, and directory validation intact.
        let directory = tempdir().unwrap();
        let path = directory.path().join("read-column.devondb");
        let pager = Pager::create(&path, PAGE_SIZE, DB_ID).unwrap();
        let group = all_types_group();
        let page = group.write(&pager).unwrap();

        let full = NodeGroup::read(&pager, page, &all_types()).unwrap();
        for column in 0..4 {
            let (values, rows) =
                NodeGroup::read_column(&pager, page, &all_types(), column).unwrap();
            assert_eq!(rows, full.row_count());
            let expected = (0..full.row_count())
                .map(|row| full.value(row, column).unwrap().clone())
                .collect::<Vec<_>>();
            assert_eq!(values, expected, "column {column} diverged from full read");
        }

        let out_of_bounds = NodeGroup::read_column(&pager, page, &all_types(), 5).unwrap_err();
        assert!(
            out_of_bounds.to_string().contains("out of bounds"),
            "{out_of_bounds}"
        );
    }

    #[test]
    fn scalar_v2_layouts_and_nulls_round_trip_across_reopen() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("scalar-v2.devondb");
        let pager = Pager::create(&path, PAGE_SIZE, DB_ID).unwrap();
        let types = vec![
            LogicalType::Timestamp,
            LogicalType::Bytes,
            LogicalType::Decimal {
                precision: 38,
                scale: 0,
            },
            LogicalType::Json,
        ];
        let mut group = NodeGroup::new(types.clone()).unwrap();
        group
            .push_row(vec![
                Value::Timestamp(0),
                Value::Bytes(Vec::new()),
                decimal(99_999_999_999_999_999_999_999_999_999_999_999_999, 0),
                Value::Json(r#"{"nested":{"items":[1,true,null]}}"#.to_owned()),
            ])
            .unwrap();
        group
            .push_row(vec![
                Value::Timestamp(i64::MIN),
                Value::Bytes(vec![0, 0xff, 0x80]),
                decimal(-99_999_999_999_999_999_999_999_999_999_999_999_999, 0),
                Value::Json("[]".to_owned()),
            ])
            .unwrap();
        group
            .push_row(vec![Value::Null, Value::Null, Value::Null, Value::Null])
            .unwrap();

        let directory_page = group.write(&pager).unwrap();
        publish_zone_maps(&pager);
        drop(pager);

        let reopened = Pager::open(path).unwrap();
        assert_eq!(
            NodeGroup::read(&reopened, directory_page, &types).unwrap(),
            group
        );
        assert_eq!(super::fixed_payload_len(3, &types[0]), Some(25));
        assert_eq!(super::fixed_payload_len(3, &types[2]), Some(49));
        assert_eq!(super::fixed_payload_len(3, &types[1]), None);
        assert_eq!(super::fixed_payload_len(3, &types[3]), None);
    }

    #[test]
    fn decimal_payload_exceeding_declared_precision_is_corruption() {
        let mut payload = vec![1_u8];
        payload.extend(1000_i128.to_le_bytes());
        let decimal = LogicalType::Decimal {
            precision: 3,
            scale: 0,
        };
        let error = super::decode_column(&payload, 1, &decimal, 7, Encoding::Plain, [0; 3])
            .expect_err("four digits must not decode through Decimal(3, 0)");
        let DevonError::Corrupt { context } = error else {
            panic!("expected Corrupt, got {error}");
        };
        assert!(context.contains("column 7"), "{context}");
        assert!(context.contains("precision 3"), "{context}");

        // The typed decode path runs the same precision fence.
        assert_corrupt(super::decode_column_typed(
            &payload,
            1,
            &decimal,
            7,
            Encoding::Plain,
            [0; 3],
        ));
    }

    #[test]
    fn all_types_and_nulls_round_trip_across_reopen() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("all-types.devondb");
        let pager = Pager::create(&path, PAGE_SIZE, DB_ID).unwrap();
        let group = all_types_group();
        let directory_page = group.write(&pager).unwrap();
        publish_zone_maps(&pager);
        drop(pager);

        let reopened = Pager::open(path).unwrap();
        let decoded = NodeGroup::read(&reopened, directory_page, &all_types()).unwrap();

        assert_eq!(decoded, group);
        assert_eq!(decoded.row_count(), 3);
        assert_eq!(decoded.column_count(), 5);
        assert_eq!(decoded.types(), all_types());
        assert_eq!(decoded.value(1, 3), Some(&Value::Null));
        assert_eq!(decoded.value(2, 3), Some(&Value::String(String::new())));
        assert_eq!(decoded.value(3, 0), None);
        assert_eq!(decoded.value(0, 5), None);
    }

    #[test]
    fn multi_page_int64_payload_round_trips() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("multi-page.devondb");
        let pager = Pager::create(&path, PAGE_SIZE, DB_ID).unwrap();
        let types = vec![LogicalType::Int64];
        let mut group = NodeGroup::new(types.clone()).unwrap();
        // Pseudo-random wide-range values: incompressible, so the §8.4
        // selection keeps plain and this test still exercises a multi-page
        // plain payload run.
        let mut state = 0x9E37_79B9_7F4A_7C15_u64;
        for _ in 0..NODE_GROUP_CAPACITY {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            group.push_row(vec![Value::Int64(state as i64)]).unwrap();
        }

        let directory_page = group.write(&pager).unwrap();
        let directory = pager.read_page(directory_page).unwrap();
        let entry = decode_entry(&directory, 0);
        assert_eq!(entry.byte_len, 16_640);
        assert_eq!((entry.byte_len as usize).div_ceil(PAGE_SIZE as usize), 5);
        publish_zone_maps(&pager);
        drop(pager);

        let reopened = Pager::open(path).unwrap();
        assert_eq!(
            NodeGroup::read(&reopened, directory_page, &types).unwrap(),
            group
        );
    }

    #[test]
    fn payload_byte_corruption_is_rejected() {
        let (_directory, path, directory_page, entry) = write_one_int_group("payload-corrupt");
        flip_file_byte(&path, entry.first_page * u64::from(PAGE_SIZE) + 1);

        let pager = Pager::open(path).unwrap();
        assert_corrupt(NodeGroup::read(
            &pager,
            directory_page,
            &[LogicalType::Int64],
        ));
        // The typed decode path runs the same CRC fence.
        assert_corrupt(NodeGroup::read_column_typed(
            &pager,
            directory_page,
            &[LogicalType::Int64],
            0,
        ));
    }

    #[test]
    fn bad_directory_magic_is_rejected() {
        let (_directory, path, directory_page, _) = write_one_int_group("magic-corrupt");
        write_file_bytes(&path, directory_page * u64::from(PAGE_SIZE), b"NOPE");

        let pager = Pager::open(path).unwrap();
        assert_corrupt(NodeGroup::read(
            &pager,
            directory_page,
            &[LogicalType::Int64],
        ));
        assert_corrupt(NodeGroup::read_column_typed(
            &pager,
            directory_page,
            &[LogicalType::Int64],
            0,
        ));
    }

    #[test]
    fn doctored_directory_byte_len_is_rejected() {
        let (_directory, path, directory_page, entry) = write_one_int_group("length-corrupt");
        let byte_len_offset =
            directory_page * u64::from(PAGE_SIZE) + (DIRECTORY_HEADER_LEN + 8) as u64;
        write_file_bytes(&path, byte_len_offset, &(entry.byte_len + 1).to_le_bytes());

        let pager = Pager::open(path).unwrap();
        assert_corrupt(NodeGroup::read(
            &pager,
            directory_page,
            &[LogicalType::Int64],
        ));
        // The typed decode path runs the same byte-length fence.
        assert_corrupt(NodeGroup::read_column_typed(
            &pager,
            directory_page,
            &[LogicalType::Int64],
            0,
        ));
    }

    #[test]
    fn wrong_schema_type_is_corruption_not_a_misread() {
        let (_directory, path, directory_page, _) = write_one_int_group("wrong-type");
        let pager = Pager::open(path).unwrap();

        assert_corrupt(NodeGroup::read(
            &pager,
            directory_page,
            &[LogicalType::Bool],
        ));
        assert_corrupt(NodeGroup::read_column_typed(
            &pager,
            directory_page,
            &[LogicalType::Bool],
            0,
        ));
    }

    #[test]
    fn zone_map_section_length_overrun_is_corruption_and_restores_green() {
        let (_directory, path, directory_page, _) = write_one_int_group("stats-overrun");
        assert_directory_corruption_restores(
            &path,
            directory_page,
            &[LogicalType::Int64],
            |page| {
                page[stats_section_start(1)..stats_section_start(1) + 4]
                    .copy_from_slice(&u32::MAX.to_le_bytes());
            },
            "length overruns the page",
        );
    }

    #[test]
    fn zone_map_section_crc_mismatch_is_corruption_and_restores_green() {
        let (_directory, path, directory_page, _) = write_one_int_group("stats-crc");
        assert_directory_corruption_restores(
            &path,
            directory_page,
            &[LogicalType::Int64],
            |page| page[stats_section_start(1) + 4] ^= 0xff,
            "section bit 0 CRC-32C does not match",
        );
    }

    #[test]
    fn zone_map_section_exact_length_is_corruption_and_restores_green() {
        let (_directory, path, directory_page, _) = write_one_int_group("stats-length");
        assert_directory_corruption_restores(
            &path,
            directory_page,
            &[LogicalType::Int64],
            |page| {
                let section = stats_section_start(1);
                page[section..section + 4].copy_from_slice(&23_u32.to_le_bytes());
                refresh_stats_crc(page, section);
            },
            "section_len is 23, expected 24",
        );
    }

    #[test]
    fn zone_map_null_count_above_rows_is_corruption_and_restores_green() {
        let (_directory, path, directory_page, _) = write_one_int_group("stats-null-overrun");
        assert_stats_record_corruption(
            &path,
            directory_page,
            &[LogicalType::Int64],
            |record| record[..4].copy_from_slice(&2_u32.to_le_bytes()),
            "null_count 2 exceeds row_count 1",
        );
    }

    #[test]
    fn zone_map_unknown_stats_flags_are_corruption_and_restore_green() {
        let (_directory, path, directory_page, _) = write_one_int_group("stats-flags");
        assert_stats_record_corruption(
            &path,
            directory_page,
            &[LogicalType::Int64],
            |record| record[4..8].copy_from_slice(&2_u32.to_le_bytes()),
            "stats_flags contain unknown bits 0x2",
        );
    }

    #[test]
    fn zone_map_missing_required_presence_is_corruption_and_restores_green() {
        let (_directory, path, directory_page, _) = write_one_int_group("stats-presence");
        assert_stats_record_corruption(
            &path,
            directory_page,
            &[LogicalType::Int64],
            |record| record[4..].fill(0),
            "min/max presence violates Int64 rules",
        );
    }

    #[test]
    fn zone_map_presence_on_never_stats_type_is_corruption_and_restores_green() {
        let (_directory, path, directory_page) =
            write_one_value_group("stats-bool-presence", LogicalType::Bool, Value::Bool(true));
        assert_stats_record_corruption(
            &path,
            directory_page,
            &[LogicalType::Bool],
            |record| record[4..8].copy_from_slice(&1_u32.to_le_bytes()),
            "min/max presence violates Bool rules",
        );
    }

    #[test]
    fn zone_map_inverted_bounds_are_corruption_and_restore_green() {
        let (_directory, path, directory_page, _) = write_one_int_group("stats-inverted");
        assert_stats_record_corruption(
            &path,
            directory_page,
            &[LogicalType::Int64],
            |record| {
                record[8..16].copy_from_slice(&43_i64.to_le_bytes());
                record[16..24].copy_from_slice(&42_i64.to_le_bytes());
            },
            "min is greater than max",
        );
    }

    #[test]
    fn zone_map_null_count_disagreement_is_corruption_and_restores_green() {
        let (_directory, path, directory_page, _) = write_one_int_group("stats-null-mismatch");
        assert_stats_record_corruption(
            &path,
            directory_page,
            &[LogicalType::Int64],
            |record| {
                record[..4].copy_from_slice(&1_u32.to_le_bytes());
                record[4..].fill(0);
            },
            "null_count 1 disagrees with validity bitmap count 0",
        );
    }

    #[test]
    fn zone_map_payload_outside_bounds_is_corruption_and_restores_green() {
        let (_directory, path, directory_page, _) = write_one_int_group("stats-outside");
        assert_stats_record_corruption(
            &path,
            directory_page,
            &[LogicalType::Int64],
            |record| {
                record[8..16].copy_from_slice(&43_i64.to_le_bytes());
                record[16..24].copy_from_slice(&43_i64.to_le_bytes());
            },
            "payload value is outside its ZONE_MAP_STATS bounds",
        );
    }

    #[test]
    fn float_zone_map_absence_requires_nan_and_restores_green() {
        let (_directory, path, directory_page) = write_one_value_group(
            "stats-float-absence",
            LogicalType::Float64,
            Value::Float64(1.5),
        );
        assert_stats_record_corruption(
            &path,
            directory_page,
            &[LogicalType::Float64],
            |record| record[4..].fill(0),
            "omits Float64 bounds without a NaN",
        );
    }

    #[test]
    fn float_zone_map_total_order_pins_signed_zero_bounds() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("stats-signed-zero.devondb");
        let pager = Pager::create(path, PAGE_SIZE, DB_ID).unwrap();
        let mut group = NodeGroup::new(vec![LogicalType::Float64]).unwrap();
        group.push_row(vec![Value::Float64(0.0)]).unwrap();
        group.push_row(vec![Value::Float64(-0.0)]).unwrap();
        let directory_page = group.write(&pager).unwrap();
        let directory =
            NodeGroup::read_directory(&pager, directory_page, &[LogicalType::Float64]).unwrap();
        let stats = directory.zone_maps().unwrap()[0];
        let Some(super::ZoneMapValue::Float64(min)) = stats.min else {
            panic!("missing Float64 min");
        };
        let Some(super::ZoneMapValue::Float64(max)) = stats.max else {
            panic!("missing Float64 max");
        };
        assert_eq!(min.to_bits(), (-0.0_f64).to_bits());
        assert_eq!(max.to_bits(), 0.0_f64.to_bits());
    }

    #[test]
    fn capacity_is_enforced_at_exactly_the_writer_limit() {
        let mut group = NodeGroup::new(vec![LogicalType::Int64]).unwrap();
        for value in 0..NODE_GROUP_CAPACITY {
            group.push_row(vec![Value::Int64(value as i64)]).unwrap();
        }
        assert_eq!(group.row_count(), NODE_GROUP_CAPACITY);
        assert!(matches!(
            group.push_row(vec![Value::Int64(2049)]),
            Err(DevonError::InvalidArgument { .. })
        ));
        assert_eq!(group.row_count(), NODE_GROUP_CAPACITY);
    }

    #[test]
    fn row_and_type_validation_reject_bad_inputs() {
        assert!(matches!(
            NodeGroup::new(Vec::new()),
            Err(DevonError::InvalidArgument { .. })
        ));
        let mut group =
            NodeGroup::new(vec![LogicalType::Int64, LogicalType::Vector { dim: 2 }]).unwrap();

        assert_invalid_argument_mentions(group.push_row(vec![Value::Int64(1)]), "arity");
        assert_invalid_argument_mentions(
            group.push_row(vec![Value::Bool(true), Value::Vector(vec![1.0, 2.0])]),
            "column 0",
        );
        assert_invalid_argument_mentions(
            group.push_row(vec![Value::Int64(1), Value::Vector(vec![1.0])]),
            "column 1",
        );
        assert_eq!(group.row_count(), 0);
    }

    #[test]
    fn empty_group_cannot_be_written() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("empty.devondb");
        let pager = Pager::create(path, PAGE_SIZE, DB_ID).unwrap();
        let group = NodeGroup::new(vec![LogicalType::Bool]).unwrap();

        assert!(matches!(
            group.write(&pager),
            Err(DevonError::InvalidArgument { .. })
        ));
    }

    #[test]
    fn identical_groups_have_byte_identical_payload_pages() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("deterministic.devondb");
        let pager = Pager::create(path, PAGE_SIZE, DB_ID).unwrap();
        let group = all_types_group();
        let first_directory = group.write(&pager).unwrap();
        let second_directory = group.write(&pager).unwrap();

        let first_entries = read_entries(&pager, first_directory, group.column_count());
        let second_entries = read_entries(&pager, second_directory, group.column_count());
        for (first, second) in first_entries.iter().zip(&second_entries) {
            assert_eq!(first.byte_len, second.byte_len);
            let page_count = (first.byte_len as usize).div_ceil(PAGE_SIZE as usize);
            for offset in 0..page_count {
                let offset = offset as u64;
                assert_eq!(
                    pager.read_page(first.first_page + offset).unwrap(),
                    pager.read_page(second.first_page + offset).unwrap()
                );
            }
        }
    }

    fn write_one_int_group(name: &str) -> (TempDir, std::path::PathBuf, u64, ColumnEntry) {
        let directory = tempdir().unwrap();
        let path = directory.path().join(format!("{name}.devondb"));
        let pager = Pager::create(&path, PAGE_SIZE, DB_ID).unwrap();
        let mut group = NodeGroup::new(vec![LogicalType::Int64]).unwrap();
        group.push_row(vec![Value::Int64(42)]).unwrap();
        let directory_page = group.write(&pager).unwrap();
        let entry = decode_entry(&pager.read_page(directory_page).unwrap(), 0);
        publish_zone_maps(&pager);
        drop(pager);
        (directory, path, directory_page, entry)
    }

    fn write_one_value_group(
        name: &str,
        logical_type: LogicalType,
        value: Value,
    ) -> (TempDir, std::path::PathBuf, u64) {
        let directory = tempdir().unwrap();
        let path = directory.path().join(format!("{name}.devondb"));
        let pager = Pager::create(&path, PAGE_SIZE, DB_ID).unwrap();
        let mut group = NodeGroup::new(vec![logical_type]).unwrap();
        group.push_row(vec![value]).unwrap();
        let directory_page = group.write(&pager).unwrap();
        publish_zone_maps(&pager);
        drop(pager);
        (directory, path, directory_page)
    }

    fn publish_zone_maps(pager: &Pager) {
        let mut superblock = pager.superblock();
        superblock.checkpoint_lsn += 1;
        superblock.feature_flags |= crate::superblock::ZONE_MAPS_FLAG;
        pager.commit_superblock(superblock).unwrap();
    }

    fn stats_section_start(entry_count: usize) -> usize {
        DIRECTORY_HEADER_LEN + entry_count * DIRECTORY_ENTRY_LEN
    }

    fn refresh_stats_crc(page: &mut [u8], section_start: usize) {
        let section_len =
            u32::from_le_bytes(page[section_start..section_start + 4].try_into().unwrap()) as usize;
        let payload_start = section_start + super::SECTION_HEADER_LEN;
        let checksum = crc32c::crc32c(&page[payload_start..payload_start + section_len]);
        page[section_start + 4..section_start + 8].copy_from_slice(&checksum.to_le_bytes());
    }

    fn assert_stats_record_corruption(
        path: &Path,
        directory_page: u64,
        types: &[LogicalType],
        mutate: impl FnOnce(&mut [u8]),
        expected: &str,
    ) {
        assert_directory_corruption_restores(
            path,
            directory_page,
            types,
            |page| {
                let section = stats_section_start(types.len());
                let payload_start = section + super::SECTION_HEADER_LEN;
                mutate(&mut page[payload_start..payload_start + super::ZONE_MAP_RECORD_LEN]);
                refresh_stats_crc(page, section);
            },
            expected,
        );
    }

    fn assert_directory_corruption_restores(
        path: &Path,
        directory_page: u64,
        types: &[LogicalType],
        mutate: impl FnOnce(&mut [u8]),
        expected: &str,
    ) {
        let offset = directory_page * u64::from(PAGE_SIZE);
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .unwrap();
        file.seek(SeekFrom::Start(offset)).unwrap();
        let mut original = vec![0_u8; PAGE_SIZE as usize];
        file.read_exact(&mut original).unwrap();
        let mut doctored = original.clone();
        mutate(&mut doctored);
        file.seek(SeekFrom::Start(offset)).unwrap();
        file.write_all(&doctored).unwrap();
        file.sync_all().unwrap();
        drop(file);

        let pager = Pager::open(path).unwrap();
        let error = NodeGroup::read(&pager, directory_page, types)
            .expect_err("doctored zone-map directory unexpectedly decoded");
        let DevonError::Corrupt { context } = error else {
            panic!("expected Corrupt, got {error}");
        };
        assert!(
            context.contains(expected),
            "expected `{expected}` in `{context}`"
        );
        drop(pager);

        write_file_bytes(path, offset, &original);
        let pager = Pager::open(path).unwrap();
        NodeGroup::read(&pager, directory_page, types).unwrap();
    }

    fn read_entries(pager: &Pager, page_id: u64, count: usize) -> Vec<ColumnEntry> {
        let directory = pager.read_page(page_id).unwrap();
        (0..count)
            .map(|index| decode_entry(&directory, index))
            .collect()
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

    fn assert_corrupt<T>(result: Result<T, DevonError>) {
        assert!(matches!(result, Err(DevonError::Corrupt { .. })));
    }

    fn assert_invalid_argument_mentions<T>(result: Result<T, DevonError>, expected: &str) {
        let DevonError::InvalidArgument { context } = result.err().unwrap() else {
            panic!("expected InvalidArgument");
        };
        assert!(context.contains(expected));
    }

    #[test]
    fn directory_entries_use_the_specified_width() {
        assert_eq!(DIRECTORY_ENTRY_LEN, 16);
    }
}
