//! Arrow C Data Interface export for query results
//! (<https://arrow.apache.org/docs/format/CDataInterface.html>): a column/row
//! block of [`Value`]s becomes a struct array (`+s`) whose children are the
//! result columns. No arrow crates are involved; the two `#[repr(C)]` structs
//! below are copied spec-verbatim (field order is ABI). This module is the
//! single canonical builder — the binding crates (`devondb-c`,
//! `devondb-python`) are thin FFI/capsule callers over it.
//!
//! Type mapping (devondb value type from the first non-null value in a
//! column; mixed types in one column are an error naming the column):
//!
//! | devondb value | Arrow format | Buffers (validity first, when present) |
//! |---|---|---|
//! | Int64 | `l` | i64 values |
//! | Float64 | `g` | f64 values |
//! | Bool | `b` | bit-packed values |
//! | String | `u` | i32 offsets (n+1), data bytes (refused past i32::MAX) |
//! | Timestamp (epoch µs UTC) | `tsu:UTC` | i64 values |
//! | Decimal(p, s) | `d:p,s` | 16-byte little-endian two's-complement digits |
//! | Bytes | `z` | i32 offsets (n+1), data bytes |
//! | Json | `u` + extension metadata `arrow.json` | i32 offsets, data bytes |
//! | Vector(n) | `+w:n` over child `f` | child f32 values (n × rows) |
//! | GeoPoint | `+s` over `lat_deg: g`, `lng_deg: g` | child f64 values |
//! | every value NULL, type unknown | `n` | none |
//!
//! NULL slots clear the validity bit and hold zero in typed buffers (the
//! same law as `docs/SCALE.md` §6.3). All pointed-to data is owned by a
//! `private_data` payload box freed by the release callback; a release
//! callback releases children recursively, frees its own payload (including
//! the child struct allocations), and sets `release = NULL`.

#![allow(
    unsafe_code,
    reason = "the Arrow C Data Interface requires raw-pointer marshalling and extern release callbacks"
)]

use std::ffi::{CString, c_char, c_void};
use std::fmt;
use std::ptr;

use crate::value::Value;

/// `ARROW_FLAG_NULLABLE` from the C Data Interface specification.
const ARROW_FLAG_NULLABLE: i64 = 2;

/// The Arrow C Data Interface schema struct, copied spec-verbatim.
#[repr(C)]
pub struct ArrowSchema {
    /// Null-terminated format string describing the data type.
    pub format: *const c_char,
    /// Optional null-terminated field name (NULL when omitted).
    pub name: *const c_char,
    /// Optional binary key/value metadata (NULL when omitted); NOT
    /// null-terminated, int32-length-prefixed pairs in native endianness.
    pub metadata: *const c_char,
    /// OR'd flag bits (`ARROW_FLAG_NULLABLE` is 2).
    pub flags: i64,
    /// Number of child schemas.
    pub n_children: i64,
    /// Array of `n_children` child schema pointers (NULL iff none).
    pub children: *mut *mut ArrowSchema,
    /// Dictionary type; always NULL here (no dictionary encoding).
    pub dictionary: *mut ArrowSchema,
    /// Producer release callback; NULL marks a released struct.
    pub release: Option<unsafe extern "C" fn(*mut ArrowSchema)>,
    /// Opaque producer bookkeeping; freed by the release callback.
    pub private_data: *mut c_void,
}

/// The Arrow C Data Interface array struct, copied spec-verbatim.
#[repr(C)]
pub struct ArrowArray {
    /// Logical number of items.
    pub length: i64,
    /// Number of null items.
    pub null_count: i64,
    /// Logical offset into the buffers; always 0 here.
    pub offset: i64,
    /// Number of physical buffers backing this array.
    pub n_buffers: i64,
    /// Number of child arrays.
    pub n_children: i64,
    /// Array of `n_buffers` buffer pointers (NULL iff none).
    pub buffers: *mut *const c_void,
    /// Array of `n_children` child array pointers (NULL iff none).
    pub children: *mut *mut ArrowArray,
    /// Dictionary values; always NULL here (no dictionary encoding).
    pub dictionary: *mut ArrowArray,
    /// Producer release callback; NULL marks a released struct.
    pub release: Option<unsafe extern "C" fn(*mut ArrowArray)>,
    /// Opaque producer bookkeeping; freed by the release callback.
    pub private_data: *mut c_void,
}

impl ArrowSchema {
    /// An already-released empty schema: the state out-parameters are left in
    /// when an export fails, so calling `release` on them is a no-op.
    #[must_use]
    pub fn released() -> Self {
        Self {
            format: ptr::null(),
            name: ptr::null(),
            metadata: ptr::null(),
            flags: 0,
            n_children: 0,
            children: ptr::null_mut(),
            dictionary: ptr::null_mut(),
            release: None,
            private_data: ptr::null_mut(),
        }
    }
}

impl ArrowArray {
    /// An already-released empty array: the state out-parameters are left in
    /// when an export fails, so calling `release` on them is a no-op.
    #[must_use]
    pub fn released() -> Self {
        Self {
            length: 0,
            null_count: 0,
            offset: 0,
            n_buffers: 0,
            n_children: 0,
            buffers: ptr::null_mut(),
            children: ptr::null_mut(),
            dictionary: ptr::null_mut(),
            release: None,
            private_data: ptr::null_mut(),
        }
    }
}

/// One result-set cell: a [`Value`] with the Decimal payload unpacked into
/// the digits/precision/scale the Arrow layout needs.
enum Cell {
    Null,
    Bool(bool),
    Int64(i64),
    Float64(f64),
    String(String),
    Vector(Vec<f32>),
    GeoPoint(f64, f64),
    Timestamp(i64),
    Bytes(Vec<u8>),
    Decimal {
        digits: i128,
        precision: u8,
        scale: u8,
    },
    Json(String),
}

fn cell_of(value: &Value) -> Cell {
    match value {
        Value::Null => Cell::Null,
        Value::Bool(value) => Cell::Bool(*value),
        Value::Int64(value) => Cell::Int64(*value),
        Value::Float64(value) => Cell::Float64(*value),
        Value::String(value) => Cell::String(value.clone()),
        Value::Vector(value) => Cell::Vector(value.clone()),
        Value::GeoPoint(point) => Cell::GeoPoint(point.lat_deg(), point.lng_deg()),
        Value::Timestamp(value) => Cell::Timestamp(*value),
        Value::Bytes(value) => Cell::Bytes(value.clone()),
        Value::Decimal(value) => Cell::Decimal {
            digits: value.digits(),
            precision: value.precision(),
            scale: value.scale(),
        },
        Value::Json(value) => Cell::Json(value.clone()),
    }
}

/// Exports a column/row block as an Arrow C Data Interface struct array: the
/// returned schema has format `+s` and one child per column, the returned
/// array has `length == rows.len()` and matching child arrays.
///
/// The caller owns both structs; all buffers and strings they point to are
/// producer-owned and freed by calling the structs' release callbacks
/// (top-level only — each release releases its children recursively).
///
/// # Errors
///
/// Returns an error message when a column mixes value types, a row is
/// missing a column, a column name is not representable as a C string, or a
/// variable-length column exceeds the 32-bit offset limit.
pub fn export(
    columns: &[String],
    rows: &[Vec<Value>],
) -> Result<(Box<ArrowSchema>, Box<ArrowArray>), String> {
    let mut children = Vec::with_capacity(columns.len());
    for (index, name) in columns.iter().enumerate() {
        let cells = column_cells(name, index, rows)?;
        children.push(build_column(name, &cells)?);
    }
    let spec = ArraySpec {
        format: "+s".to_owned(),
        name: None,
        metadata: None,
        nullable: false,
        length: rows.len() as i64,
        null_count: 0,
        validity_slot: true,
        validity: None,
        buffers: Vec::new(),
        children,
    };
    let schema = materialize_schema(&spec)?;
    let array = materialize_array(spec);
    Ok((schema, array))
}

fn column_cells(name: &str, index: usize, rows: &[Vec<Value>]) -> Result<Vec<Cell>, String> {
    let mut cells = Vec::with_capacity(rows.len());
    for row in rows {
        let value = row
            .get(index)
            .ok_or_else(|| format!("result row is missing column `{name}`"))?;
        cells.push(cell_of(value));
    }
    Ok(cells)
}

/// A column's Arrow type, resolved from its first non-null cell.
enum ColumnType {
    Null,
    Bool,
    Int64,
    Float64,
    Utf8 { json: bool },
    Timestamp,
    Binary,
    Decimal { precision: u8, scale: u8 },
    FixedList { dim: usize },
    GeoPoint,
}

impl ColumnType {
    fn of(cell: &Cell) -> Self {
        match cell {
            Cell::Null => Self::Null,
            Cell::Bool(_) => Self::Bool,
            Cell::Int64(_) => Self::Int64,
            Cell::Float64(_) => Self::Float64,
            Cell::String(_) => Self::Utf8 { json: false },
            Cell::Vector(vector) => Self::FixedList { dim: vector.len() },
            Cell::GeoPoint(..) => Self::GeoPoint,
            Cell::Timestamp(_) => Self::Timestamp,
            Cell::Bytes(_) => Self::Binary,
            Cell::Decimal {
                precision, scale, ..
            } => Self::Decimal {
                precision: *precision,
                scale: *scale,
            },
            Cell::Json(_) => Self::Utf8 { json: true },
        }
    }

    fn tag(&self) -> &'static str {
        match self {
            Self::Null => "Null",
            Self::Bool => "Bool",
            Self::Int64 => "Int64",
            Self::Float64 => "Float64",
            Self::Utf8 { json: false } => "String",
            Self::Utf8 { json: true } => "Json",
            Self::Timestamp => "Timestamp",
            Self::Binary => "Bytes",
            Self::Decimal { .. } => "Decimal",
            Self::FixedList { .. } => "Vector",
            Self::GeoPoint => "GeoPoint",
        }
    }
}

fn resolve_column_type(name: &str, cells: &[Cell]) -> Result<ColumnType, String> {
    let mut resolved: Option<ColumnType> = None;
    for cell in cells {
        if matches!(cell, Cell::Null) {
            continue;
        }
        resolved = Some(match (resolved, ColumnType::of(cell)) {
            (None, current) => current,
            (Some(previous), current) => merge_column_types(name, previous, current)?,
        });
    }
    Ok(resolved.unwrap_or(ColumnType::Null))
}

fn merge_column_types(
    name: &str,
    previous: ColumnType,
    current: ColumnType,
) -> Result<ColumnType, String> {
    let previous_tag = previous.tag();
    let current_tag = current.tag();
    let mixed = || format!("mixed types in column `{name}`: `{previous_tag}` and `{current_tag}`");
    match (previous, current) {
        (ColumnType::Null, _) | (_, ColumnType::Null) => Err(mixed()),
        // One Decimal column holds values of different digit counts: the
        // scale must match and the exported precision covers every value.
        (
            ColumnType::Decimal {
                precision: a,
                scale,
            },
            ColumnType::Decimal { precision: b, .. },
        ) => Ok(ColumnType::Decimal {
            precision: a.max(b),
            scale,
        }),
        (ColumnType::FixedList { dim: a }, ColumnType::FixedList { dim: b }) if a == b => {
            Ok(ColumnType::FixedList { dim: a })
        }
        (previous, current) if previous.tag() == current.tag() => Ok(previous),
        _ => Err(mixed()),
    }
}

/// A fully-resolved array: format string plus every buffer it owns.
struct ArraySpec {
    format: String,
    name: Option<String>,
    metadata: Option<Vec<u8>>,
    nullable: bool,
    length: i64,
    null_count: i64,
    /// Whether buffer 0 is the validity slot (every type except `n`).
    validity_slot: bool,
    validity: Option<Vec<u64>>,
    buffers: Vec<Vec<u64>>,
    children: Vec<ArraySpec>,
}

impl ArraySpec {
    fn leaf(
        format: String,
        name: Option<String>,
        nullable: bool,
        length: i64,
        buffers: Vec<Vec<u64>>,
    ) -> Self {
        Self {
            format,
            name,
            metadata: None,
            nullable,
            length,
            null_count: 0,
            validity_slot: true,
            validity: None,
            buffers,
            children: Vec::new(),
        }
    }
}

fn build_column(name: &str, cells: &[Cell]) -> Result<ArraySpec, String> {
    let column_type = resolve_column_type(name, cells)?;
    if matches!(column_type, ColumnType::Null) {
        return Ok(null_spec(name, cells.len()));
    }
    let (validity, null_count) = validity_words(cells);
    let mut spec = match &column_type {
        ColumnType::Null => unreachable!("null columns returned above"),
        ColumnType::Bool => primitive_spec("b", name, cells, vec![bool_words(cells)]),
        ColumnType::Int64 => primitive_spec("l", name, cells, vec![i64_words(cells)]),
        ColumnType::Float64 => primitive_spec("g", name, cells, vec![f64_words(cells)]),
        ColumnType::Timestamp => {
            primitive_spec("tsu:UTC", name, cells, vec![timestamp_words(cells)])
        }
        ColumnType::Decimal { precision, scale } => primitive_spec(
            format!("d:{precision},{scale}"),
            name,
            cells,
            vec![decimal_words(cells)],
        ),
        ColumnType::Utf8 { json } => {
            let mut spec = primitive_spec("u", name, cells, varbinary_buffers(name, cells)?);
            if *json {
                spec.metadata = Some(json_extension_metadata());
            }
            spec
        }
        ColumnType::Binary => primitive_spec("z", name, cells, varbinary_buffers(name, cells)?),
        ColumnType::FixedList { dim } => fixed_list_spec(name, cells, *dim),
        ColumnType::GeoPoint => geo_point_spec(name, cells),
    };
    spec.validity = validity;
    spec.null_count = null_count;
    Ok(spec)
}

fn null_spec(name: &str, length: usize) -> ArraySpec {
    ArraySpec {
        format: "n".to_owned(),
        name: Some(name.to_owned()),
        metadata: None,
        nullable: true,
        length: length as i64,
        null_count: length as i64,
        validity_slot: false,
        validity: None,
        buffers: Vec::new(),
        children: Vec::new(),
    }
}

fn primitive_spec(
    format: impl Into<String>,
    name: &str,
    cells: &[Cell],
    buffers: Vec<Vec<u64>>,
) -> ArraySpec {
    ArraySpec::leaf(
        format.into(),
        Some(name.to_owned()),
        true,
        cells.len() as i64,
        buffers,
    )
}

fn fixed_list_spec(name: &str, cells: &[Cell], dim: usize) -> ArraySpec {
    let child = ArraySpec::leaf(
        "f".to_owned(),
        Some("item".to_owned()),
        false,
        (dim * cells.len()) as i64,
        vec![f32_words(cells, dim)],
    );
    ArraySpec {
        format: format!("+w:{dim}"),
        name: Some(name.to_owned()),
        metadata: None,
        nullable: true,
        length: cells.len() as i64,
        null_count: 0,
        validity_slot: true,
        validity: None,
        buffers: Vec::new(),
        children: vec![child],
    }
}

fn geo_point_spec(name: &str, cells: &[Cell]) -> ArraySpec {
    let component = |component_name: &str, pick: fn(f64, f64) -> f64| {
        ArraySpec::leaf(
            "g".to_owned(),
            Some(component_name.to_owned()),
            false,
            cells.len() as i64,
            vec![geo_component_words(cells, pick)],
        )
    };
    ArraySpec {
        format: "+s".to_owned(),
        name: Some(name.to_owned()),
        metadata: None,
        nullable: true,
        length: cells.len() as i64,
        null_count: 0,
        validity_slot: true,
        validity: None,
        buffers: Vec::new(),
        children: vec![
            component("lat_deg", |lat_deg, _| lat_deg),
            component("lng_deg", |_, lng_deg| lng_deg),
        ],
    }
}

/// Bit i of the validity bitmap is set iff row i is valid; NULL rows leave
/// zero in every typed buffer (docs/SCALE.md §6.3).
fn validity_words(cells: &[Cell]) -> (Option<Vec<u64>>, i64) {
    let null_count = cells
        .iter()
        .filter(|cell| matches!(cell, Cell::Null))
        .count();
    if null_count == 0 {
        return (None, 0);
    }
    let mut words = vec![0u64; cells.len().div_ceil(64)];
    for (index, cell) in cells.iter().enumerate() {
        if !matches!(cell, Cell::Null) {
            words[index / 64] |= 1u64 << (index % 64);
        }
    }
    (Some(words), null_count as i64)
}

fn bool_words(cells: &[Cell]) -> Vec<u64> {
    let mut words = vec![0u64; cells.len().div_ceil(64)];
    for (index, cell) in cells.iter().enumerate() {
        if matches!(cell, Cell::Bool(true)) {
            words[index / 64] |= 1u64 << (index % 64);
        }
    }
    words
}

fn i64_words(cells: &[Cell]) -> Vec<u64> {
    cells
        .iter()
        .map(|cell| match cell {
            Cell::Int64(value) => *value as u64,
            _ => 0,
        })
        .collect()
}

fn timestamp_words(cells: &[Cell]) -> Vec<u64> {
    cells
        .iter()
        .map(|cell| match cell {
            Cell::Timestamp(value) => *value as u64,
            _ => 0,
        })
        .collect()
}

fn f64_words(cells: &[Cell]) -> Vec<u64> {
    cells
        .iter()
        .map(|cell| match cell {
            Cell::Float64(value) => value.to_bits(),
            _ => 0,
        })
        .collect()
}

fn decimal_words(cells: &[Cell]) -> Vec<u64> {
    let mut words = Vec::with_capacity(cells.len() * 2);
    for cell in cells {
        let digits = match cell {
            Cell::Decimal { digits, .. } => *digits,
            _ => 0,
        };
        let bits = digits as u128;
        words.push(bits as u64);
        words.push((bits >> 64) as u64);
    }
    words
}

fn f32_words(cells: &[Cell], dim: usize) -> Vec<u64> {
    let mut words = Vec::with_capacity((dim * cells.len()).div_ceil(2));
    let mut pending: Option<u32> = None;
    for cell in cells {
        for index in 0..dim {
            let element = match cell {
                Cell::Vector(vector) => vector.get(index).copied().unwrap_or(0.0),
                _ => 0.0,
            };
            match pending {
                None => pending = Some(element.to_bits()),
                Some(lo) => {
                    words.push(u64::from(lo) | (u64::from(element.to_bits()) << 32));
                    pending = None;
                }
            }
        }
    }
    if let Some(lo) = pending {
        words.push(u64::from(lo));
    }
    words
}

fn geo_component_words(cells: &[Cell], pick: fn(f64, f64) -> f64) -> Vec<u64> {
    cells
        .iter()
        .map(|cell| match cell {
            Cell::GeoPoint(lat_deg, lng_deg) => pick(*lat_deg, *lng_deg).to_bits(),
            _ => 0,
        })
        .collect()
}

/// Utf8/binary layout: i32 offsets (n+1 entries) then the data bytes.
fn varbinary_buffers(name: &str, cells: &[Cell]) -> Result<Vec<Vec<u64>>, String> {
    let mut offsets = Vec::with_capacity((cells.len() + 1) * 4);
    let mut data = Vec::new();
    offsets.extend_from_slice(&0i32.to_ne_bytes());
    for cell in cells {
        let bytes = match cell {
            Cell::String(text) | Cell::Json(text) => text.as_bytes(),
            Cell::Bytes(bytes) => bytes,
            _ => b"",
        };
        data.extend_from_slice(bytes);
        if data.len() > i32::MAX as usize {
            return Err(format!(
                "column `{name}`: variable-length data exceeds the {}-byte limit of 32-bit offsets",
                i32::MAX
            ));
        }
        offsets.extend_from_slice(&(data.len() as i32).to_ne_bytes());
    }
    Ok(vec![bytes_to_words(&offsets), bytes_to_words(&data)])
}

fn bytes_to_words(bytes: &[u8]) -> Vec<u64> {
    let mut words = vec![0u64; bytes.len().div_ceil(8)];
    for (index, byte) in bytes.iter().enumerate() {
        words[index / 8] |= u64::from(*byte) << (8 * (index % 8));
    }
    words
}

/// The `arrow.json` extension marker: one metadata key/value pair, int32
/// lengths in native endianness per the C Data Interface spec.
fn json_extension_metadata() -> Vec<u8> {
    const KEY: &str = "ARROW:extension:name";
    const VALUE: &str = "arrow.json";
    let mut metadata = Vec::new();
    metadata.extend_from_slice(&1i32.to_ne_bytes());
    metadata.extend_from_slice(&(KEY.len() as i32).to_ne_bytes());
    metadata.extend_from_slice(KEY.as_bytes());
    metadata.extend_from_slice(&(VALUE.len() as i32).to_ne_bytes());
    metadata.extend_from_slice(VALUE.as_bytes());
    metadata
}

/// Everything an exported `ArrowSchema` points into; the release callback
/// owns and frees exactly one of these per schema node. `_children` is never
/// read through Rust — it exists so the child struct allocations are freed
/// when the payload drops during release.
struct SchemaPayload {
    format: CString,
    name: Option<CString>,
    metadata: Option<Vec<u8>>,
    child_ptrs: Vec<*mut ArrowSchema>,
    // Boxing is load-bearing: the parent's `children` pointer array holds
    // addresses of these structs, so they must never move.
    #[allow(clippy::vec_box)]
    _children: Vec<Box<ArrowSchema>>,
}

/// Everything an exported `ArrowArray` points into; buffer storage is
/// `u64`-backed so every buffer is 8-byte aligned. `_owned` and `_children`
/// are never read through Rust — they exist so the buffer and child struct
/// allocations are freed when the payload drops during release.
struct ArrayPayload {
    _owned: Vec<Vec<u64>>,
    buffer_ptrs: Vec<*const c_void>,
    child_ptrs: Vec<*mut ArrowArray>,
    // Boxing is load-bearing: the parent's `children` pointer array holds
    // addresses of these structs, so they must never move.
    #[allow(clippy::vec_box)]
    _children: Vec<Box<ArrowArray>>,
}

fn c_string(text: &str, what: &str) -> Result<CString, String> {
    CString::new(text).map_err(|_| format!("{what} `{text}` contains a NUL byte"))
}

fn materialize_schema(spec: &ArraySpec) -> Result<Box<ArrowSchema>, String> {
    let mut children = Vec::with_capacity(spec.children.len());
    for child in &spec.children {
        children.push(materialize_schema(child)?);
    }
    let child_ptrs = children
        .iter_mut()
        .map(|child| child.as_mut() as *mut ArrowSchema)
        .collect();
    let mut payload = Box::new(SchemaPayload {
        format: c_string(&spec.format, "format string")?,
        name: spec
            .name
            .as_deref()
            .map(|name| c_string(name, "column name"))
            .transpose()?,
        metadata: spec.metadata.clone(),
        child_ptrs,
        _children: children,
    });
    let children_ptr = if payload.child_ptrs.is_empty() {
        ptr::null_mut()
    } else {
        payload.child_ptrs.as_mut_ptr()
    };
    let mut schema = Box::new(ArrowSchema {
        format: payload.format.as_ptr(),
        name: payload
            .name
            .as_ref()
            .map_or(ptr::null(), |name| name.as_ptr()),
        metadata: payload
            .metadata
            .as_ref()
            .map_or(ptr::null(), |metadata| metadata.as_ptr().cast()),
        flags: if spec.nullable {
            ARROW_FLAG_NULLABLE
        } else {
            0
        },
        n_children: payload.child_ptrs.len() as i64,
        children: children_ptr,
        dictionary: ptr::null_mut(),
        release: Some(schema_release),
        private_data: ptr::null_mut(),
    });
    schema.private_data = Box::into_raw(payload).cast();
    Ok(schema)
}

fn materialize_array(spec: ArraySpec) -> Box<ArrowArray> {
    let mut children: Vec<Box<ArrowArray>> =
        spec.children.into_iter().map(materialize_array).collect();
    let child_ptrs = children
        .iter_mut()
        .map(|child| child.as_mut() as *mut ArrowArray)
        .collect::<Vec<_>>();
    let has_validity = spec.validity.is_some();
    let mut owned: Vec<Vec<u64>> = spec.validity.into_iter().collect();
    owned.extend(spec.buffers);
    let mut buffer_ptrs = Vec::with_capacity(owned.len() + 1);
    if spec.validity_slot {
        buffer_ptrs.push(if has_validity {
            owned[0].as_ptr().cast()
        } else {
            ptr::null()
        });
    }
    for buffer in &owned[usize::from(has_validity)..] {
        buffer_ptrs.push(if buffer.is_empty() {
            ptr::null()
        } else {
            buffer.as_ptr().cast()
        });
    }
    let mut payload = Box::new(ArrayPayload {
        _owned: owned,
        buffer_ptrs,
        child_ptrs,
        _children: children,
    });
    let buffers_ptr = if payload.buffer_ptrs.is_empty() {
        ptr::null_mut()
    } else {
        payload.buffer_ptrs.as_mut_ptr()
    };
    let children_ptr = if payload.child_ptrs.is_empty() {
        ptr::null_mut()
    } else {
        payload.child_ptrs.as_mut_ptr()
    };
    let mut array = Box::new(ArrowArray {
        length: spec.length,
        null_count: spec.null_count,
        offset: 0,
        n_buffers: payload.buffer_ptrs.len() as i64,
        n_children: payload.child_ptrs.len() as i64,
        buffers: buffers_ptr,
        children: children_ptr,
        dictionary: ptr::null_mut(),
        release: Some(array_release),
        private_data: ptr::null_mut(),
    });
    array.private_data = Box::into_raw(payload).cast();
    array
}

/// Release callback for exported schemas: releases children recursively,
/// frees this node's payload (including the child struct allocations), and
/// marks the struct released by setting `release = NULL`.
///
/// # Safety
///
/// `schema` must point at a live struct produced by [`export`] (or a moved
/// copy of one) whose release callback has not run yet.
unsafe extern "C" fn schema_release(schema: *mut ArrowSchema) {
    if schema.is_null() {
        return;
    }
    // SAFETY: the caller guarantees `schema` is a live exported struct.
    let schema = unsafe { &mut *schema };
    let payload_ptr = schema.private_data.cast::<SchemaPayload>();
    if !payload_ptr.is_null() {
        // SAFETY: private_data was installed by materialize_schema and is
        // reclaimed exactly once (private_data and release are nulled below).
        let payload = unsafe { Box::from_raw(payload_ptr) };
        for child in &payload.child_ptrs {
            // SAFETY: child structs are owned by this payload and stay alive
            // until it drops at the end of this scope; the NULL release guard
            // releases each child at most once.
            unsafe {
                if let Some(release) = (**child).release {
                    release(*child);
                }
            }
        }
        drop(payload);
    }
    schema.private_data = ptr::null_mut();
    schema.release = None;
}

/// Release callback for exported arrays: releases children recursively,
/// frees this node's buffers and payload (including the child struct
/// allocations), and marks the struct released by setting `release = NULL`.
///
/// # Safety
///
/// `array` must point at a live struct produced by [`export`] (or a moved
/// copy of one) whose release callback has not run yet.
unsafe extern "C" fn array_release(array: *mut ArrowArray) {
    if array.is_null() {
        return;
    }
    // SAFETY: the caller guarantees `array` is a live exported struct.
    let array = unsafe { &mut *array };
    let payload_ptr = array.private_data.cast::<ArrayPayload>();
    if !payload_ptr.is_null() {
        // SAFETY: private_data was installed by materialize_array and is
        // reclaimed exactly once (private_data and release are nulled below).
        let payload = unsafe { Box::from_raw(payload_ptr) };
        for child in &payload.child_ptrs {
            // SAFETY: child structs are owned by this payload and stay alive
            // until it drops at the end of this scope; the NULL release guard
            // releases each child at most once.
            unsafe {
                if let Some(release) = (**child).release {
                    release(*child);
                }
            }
        }
        drop(payload);
    }
    array.private_data = ptr::null_mut();
    array.release = None;
}

impl fmt::Debug for ArrowSchema {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ArrowSchema")
            .field("format", &self.format)
            .field("name", &self.name)
            .field("flags", &self.flags)
            .field("n_children", &self.n_children)
            .finish_non_exhaustive()
    }
}

impl fmt::Debug for ArrowArray {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ArrowArray")
            .field("length", &self.length)
            .field("null_count", &self.null_count)
            .field("n_buffers", &self.n_buffers)
            .field("n_children", &self.n_children)
            .finish_non_exhaustive()
    }
}
