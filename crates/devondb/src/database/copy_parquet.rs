//! Parquet source for `COPY` (`docs/SCALE.md` §5), behind the
//! `parquet` cargo feature.
//!
//! Reads one row group at a time through the parquet record API (no arrow
//! crates) and feeds schema-ordered `Vec<Value>` rows into the same sink
//! the CSV path uses. The type mapping is law — refuse, never round:
//! timestamps rescale only when exact (millis ×1000; nanos only when
//! divisible by 1000), decimals keep their declared scale, list lengths
//! must match the declared vector dimension, and JSON is re-serialized to
//! the canonical form the text parser enforces (`docs/PLAN_IR.md` § Type
//! system).

use std::{collections::VecDeque, fs::File};

use devondb_storage::budget::MemoryBudget;
use devondb_types::{
    Decimal128, DevonError, DevonResult, GeoPoint,
    logical_type::LogicalType,
    schema::{Column, NodeTableSchema, suggestion_suffix},
    value::Value,
};
use parquet::{
    basic::{ConvertedType, LogicalType as ParquetLogical, TimeUnit, Type as PhysicalType},
    file::reader::{FileReader, SerializedFileReader},
    record::{Field, Row},
    schema::types::TypePtr,
};

use super::super::{corrupt, invalid_argument};

/// A streaming Parquet row source for `COPY`: yields schema-ordered rows.
pub(super) struct ParquetRows<'budget> {
    reader: SerializedFileReader<File>,
    columns: Vec<Column>,
    mappings: Vec<ColumnMapping>,
    /// Validated parquet field position → schema column index.
    field_to_column: Vec<usize>,
    buffered: VecDeque<Row>,
    next_group: usize,
    group_charge: usize,
    budget: &'budget MemoryBudget,
    /// 1-based data row across the whole file, for error messages.
    row: usize,
    done: bool,
}

impl<'budget> ParquetRows<'budget> {
    /// Opens `path`, validating every schema column against the Parquet
    /// schema by exact name (docs/SCALE.md §5.2 header law) and every
    /// column's type mapping before any row is read.
    pub(super) fn open(
        path: &str,
        schema: &NodeTableSchema,
        budget: &'budget MemoryBudget,
    ) -> DevonResult<Self> {
        let reader = SerializedFileReader::new(File::open(path)?).map_err(parquet_error)?;
        let fields = reader
            .metadata()
            .file_metadata()
            .schema()
            .get_fields()
            .to_vec();
        let mapped = map_columns(schema.columns(), &fields)?;
        Ok(Self {
            reader,
            columns: schema.columns().to_vec(),
            mappings: mapped.mappings,
            field_to_column: mapped.field_to_column,
            buffered: VecDeque::new(),
            next_group: 0,
            group_charge: 0,
            budget,
            row: 0,
            done: false,
        })
    }

    /// Reads the next row group into the row buffer, charging its
    /// uncompressed byte size to the shared budget for as long as the
    /// buffer is held. Returns `false` when no row group remains.
    fn load_next_group(&mut self) -> DevonResult<bool> {
        self.release_group_charge();
        let metadata = self.reader.metadata();
        if self.next_group >= metadata.num_row_groups() {
            return Ok(false);
        }
        let bytes = usize::try_from(metadata.row_group(self.next_group).total_byte_size())
            .unwrap_or(usize::MAX);
        // Endpoint lookup can leave the shared budget full of clean frames.
        // Reclaim them before refusing a row group whose live buffer fits.
        if !self.budget.charge_or_reclaim(bytes) {
            return Err(DevonError::BudgetExceeded {
                context: format!(
                    "COPY Parquet row-group buffer requested {bytes} additional bytes"
                ),
            });
        }
        self.group_charge = bytes;
        let group = self
            .reader
            .get_row_group(self.next_group)
            .map_err(parquet_error)?;
        let rows = group.get_row_iter(None).map_err(parquet_error)?;
        for row in rows {
            self.buffered.push_back(row.map_err(parquet_error)?);
        }
        self.next_group += 1;
        Ok(true)
    }

    fn release_group_charge(&mut self) {
        // Drop rows and the queue allocation before releasing their reservation.
        self.buffered = VecDeque::new();
        if self.group_charge > 0 {
            self.budget.release(self.group_charge);
            self.group_charge = 0;
        }
    }

    fn convert_row(&self, row: Row) -> DevonResult<Vec<Value>> {
        let fields = row.into_columns();
        if fields.len() != self.columns.len() {
            return Err(corrupt(format!(
                "Parquet row {} has {} fields; expected {}",
                self.row,
                fields.len(),
                self.columns.len()
            )));
        }
        let mut values = vec![Value::Null; self.columns.len()];
        for (field_index, (_name, field)) in fields.into_iter().enumerate() {
            let column_index = self
                .field_to_column
                .get(field_index)
                .copied()
                .ok_or_else(|| corrupt("validated Parquet row gained a field"))?;
            values[column_index] = convert_field(
                self.row,
                &self.columns[column_index],
                &self.mappings[column_index],
                field,
            )?;
        }
        Ok(values)
    }
}

impl Iterator for ParquetRows<'_> {
    type Item = DevonResult<Vec<Value>>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        if self.buffered.is_empty() {
            match self.load_next_group() {
                Ok(true) => {}
                Ok(false) => {
                    self.done = true;
                    return None;
                }
                Err(error) => {
                    self.done = true;
                    return Some(Err(error));
                }
            }
        }
        self.row += 1;
        let Some(row) = self.buffered.pop_front() else {
            self.done = true;
            return Some(Err(corrupt("COPY Parquet row group yielded no rows")));
        };
        let result = self.convert_row(row);
        if result.is_err() {
            self.done = true;
        }
        Some(result)
    }
}

impl Drop for ParquetRows<'_> {
    fn drop(&mut self) {
        self.release_group_charge();
    }
}

/// The validated mapping from one Parquet column to its devondb column.
enum ColumnMapping {
    /// INT64 / INT32 (exact) → Int64.
    Int64,
    /// DOUBLE / FLOAT (f32→f64 widening) → Float64.
    Float64,
    Bool,
    /// BYTE_ARRAY + UTF8/STRING.
    String,
    /// BYTE_ARRAY / FIXED_LEN_BYTE_ARRAY with no logical type.
    Bytes,
    Timestamp(TimestampUnit),
    /// DECIMAL over INT32/INT64/FIXED_LEN_BYTE_ARRAY whose scale already
    /// equals the column's declared scale.
    Decimal {
        scale: u8,
    },
    /// LIST of FLOAT or DOUBLE; every list must have exactly `dim`
    /// elements.
    Vector {
        dim: u32,
    },
    /// BYTE_ARRAY + JSON logical; re-serialized to canonical form.
    Json,
    /// group {lat_deg: DOUBLE, lng_deg: DOUBLE}.
    GeoPoint,
}

/// Timestamp units admitted by the mapping law.
#[derive(Clone, Copy)]
enum TimestampUnit {
    Millis,
    Micros,
    Nanos,
}

struct MappedColumns {
    mappings: Vec<ColumnMapping>,
    field_to_column: Vec<usize>,
}

/// Maps schema columns to Parquet top-level fields by exact name: every
/// schema column exactly once, unknown field names refused with
/// did-you-mean, order arbitrary (docs/SCALE.md §5.2 header law).
fn map_columns(columns: &[Column], fields: &[TypePtr]) -> DevonResult<MappedColumns> {
    let mut field_to_column = Vec::with_capacity(fields.len());
    let mut matched = vec![false; columns.len()];
    for field in fields {
        let Some(column_index) = columns.iter().position(|c| c.name == field.name()) else {
            return Err(invalid_argument(format!(
                "unknown Parquet column `{}`{}",
                field.name(),
                suggestion_suffix(field.name(), columns.iter().map(|c| c.name.as_str()))
            )));
        };
        if matched[column_index] {
            return Err(invalid_argument(format!(
                "duplicate Parquet column `{}`",
                field.name()
            )));
        }
        matched[column_index] = true;
        field_to_column.push(column_index);
    }
    if let Some(index) = matched.iter().position(|present| !present) {
        return Err(invalid_argument(format!(
            "Parquet file is missing column `{}`",
            columns[index].name
        )));
    }
    let mut mappings: Vec<Option<ColumnMapping>> = (0..columns.len()).map(|_| None).collect();
    for (field_index, field) in fields.iter().enumerate() {
        let column_index = field_to_column[field_index];
        mappings[column_index] = Some(column_mapping(&columns[column_index], field)?);
    }
    let mut result = Vec::with_capacity(columns.len());
    for mapping in mappings {
        let Some(mapping) = mapping else {
            return Err(corrupt("validated Parquet column mapping is incomplete"));
        };
        result.push(mapping);
    }
    Ok(MappedColumns {
        mappings: result,
        field_to_column,
    })
}

/// Validates the type-mapping law for one column pair.
fn column_mapping(column: &Column, field: &TypePtr) -> DevonResult<ColumnMapping> {
    let mapping = match &column.ty {
        LogicalType::Int64 => primitive_info(field)
            .filter(is_plain_integer)
            .map(|_| ColumnMapping::Int64),
        LogicalType::Float64 => primitive_info(field)
            .filter(|info| {
                matches!(info.physical, PhysicalType::FLOAT | PhysicalType::DOUBLE)
                    && info.logical.is_none()
                    && info.converted == ConvertedType::NONE
            })
            .map(|_| ColumnMapping::Float64),
        LogicalType::Bool => primitive_info(field)
            .filter(|info| {
                info.physical == PhysicalType::BOOLEAN
                    && info.logical.is_none()
                    && info.converted == ConvertedType::NONE
            })
            .map(|_| ColumnMapping::Bool),
        LogicalType::String => primitive_info(field)
            .filter(|info| {
                info.physical == PhysicalType::BYTE_ARRAY
                    && (info.logical == Some(ParquetLogical::String)
                        || info.converted == ConvertedType::UTF8)
            })
            .map(|_| ColumnMapping::String),
        LogicalType::Bytes => primitive_info(field)
            .filter(|info| {
                matches!(
                    info.physical,
                    PhysicalType::BYTE_ARRAY | PhysicalType::FIXED_LEN_BYTE_ARRAY
                ) && info.logical.is_none()
                    && info.converted == ConvertedType::NONE
            })
            .map(|_| ColumnMapping::Bytes),
        LogicalType::Timestamp => primitive_info(field)
            .and_then(timestamp_unit)
            .map(ColumnMapping::Timestamp),
        LogicalType::Decimal { scale, .. } => {
            return decimal_column_mapping(column, field, *scale);
        }
        LogicalType::Vector { dim } | LogicalType::VectorEncoded { dim, .. } => {
            vector_mapping(field).map(|()| ColumnMapping::Vector { dim: *dim })
        }
        LogicalType::Json => primitive_info(field)
            .filter(|info| {
                info.physical == PhysicalType::BYTE_ARRAY
                    && (info.logical == Some(ParquetLogical::Json)
                        || info.converted == ConvertedType::JSON)
            })
            .map(|_| ColumnMapping::Json),
        LogicalType::GeoPoint => geo_mapping(field).map(|()| ColumnMapping::GeoPoint),
    };
    mapping.ok_or_else(|| refuse_mapping(column, field))
}

fn refuse_mapping(column: &Column, field: &TypePtr) -> DevonError {
    invalid_argument(format!(
        "Parquet column `{}` has type {}; COPY cannot map it to column type {}",
        column.name,
        describe_field(field),
        column.ty
    ))
}

/// The decimal law: a Parquet decimal whose scale disagrees with the
/// column's declared scale names the mismatch — never rescale.
fn decimal_column_mapping(
    column: &Column,
    field: &TypePtr,
    column_scale: u8,
) -> DevonResult<ColumnMapping> {
    if let Some(mapping) = decimal_mapping(field, column_scale) {
        return Ok(mapping);
    }
    if let Some(info) = primitive_info(field) {
        let decimal = matches!(info.converted, ConvertedType::DECIMAL)
            || matches!(info.logical, Some(ParquetLogical::Decimal(_)));
        let physical = matches!(
            info.physical,
            PhysicalType::INT32 | PhysicalType::INT64 | PhysicalType::FIXED_LEN_BYTE_ARRAY
        );
        if decimal && physical {
            return Err(invalid_argument(format!(
                "Parquet column `{}`: DECIMAL scale {} does not match column scale {column_scale}; COPY never rescales",
                column.name, info.scale
            )));
        }
    }
    Err(refuse_mapping(column, field))
}

struct PrimitiveInfo {
    physical: PhysicalType,
    logical: Option<ParquetLogical>,
    converted: ConvertedType,
    scale: i32,
}

fn primitive_info(field: &TypePtr) -> Option<PrimitiveInfo> {
    field.is_primitive().then(|| PrimitiveInfo {
        physical: field.get_physical_type(),
        logical: field.get_basic_info().logical_type_ref().cloned(),
        converted: field.get_basic_info().converted_type(),
        scale: field.get_scale(),
    })
}

/// Plain integers only: INT32/INT64 with no logical type or an `Integer`
/// logical type that always fits i64 (unsigned 64-bit does not).
fn is_plain_integer(info: &PrimitiveInfo) -> bool {
    if !matches!(info.physical, PhysicalType::INT32 | PhysicalType::INT64) {
        return false;
    }
    match &info.logical {
        Some(ParquetLogical::Integer(integer)) => integer.is_signed || integer.bit_width <= 32,
        Some(_) => false,
        None => matches!(
            info.converted,
            ConvertedType::NONE
                | ConvertedType::INT_8
                | ConvertedType::INT_16
                | ConvertedType::INT_32
                | ConvertedType::INT_64
                | ConvertedType::UINT_8
                | ConvertedType::UINT_16
                | ConvertedType::UINT_32
        ),
    }
}

fn timestamp_unit(info: PrimitiveInfo) -> Option<TimestampUnit> {
    if let Some(ParquetLogical::Timestamp(timestamp)) = &info.logical {
        return Some(match timestamp.unit {
            TimeUnit::MILLIS => TimestampUnit::Millis,
            TimeUnit::MICROS => TimestampUnit::Micros,
            TimeUnit::NANOS => TimestampUnit::Nanos,
        });
    }
    match info.converted {
        ConvertedType::TIMESTAMP_MILLIS => Some(TimestampUnit::Millis),
        ConvertedType::TIMESTAMP_MICROS => Some(TimestampUnit::Micros),
        _ => None,
    }
}

/// The decimal law: the Parquet scale must EQUAL the column's declared
/// scale (never rescale); digit fit against the declared precision is a
/// per-value check, decidable only when a value arrives.
fn decimal_mapping(field: &TypePtr, column_scale: u8) -> Option<ColumnMapping> {
    let info = primitive_info(field)?;
    let decimal = matches!(info.converted, ConvertedType::DECIMAL)
        || matches!(info.logical, Some(ParquetLogical::Decimal(_)));
    let physical = matches!(
        info.physical,
        PhysicalType::INT32 | PhysicalType::INT64 | PhysicalType::FIXED_LEN_BYTE_ARRAY
    );
    if !decimal || !physical || info.scale != i32::from(column_scale) {
        return None;
    }
    Some(ColumnMapping::Decimal {
        scale: column_scale,
    })
}

fn vector_mapping(field: &TypePtr) -> Option<()> {
    let is_list = field.get_basic_info().logical_type_ref() == Some(&ParquetLogical::List)
        || field.get_basic_info().converted_type() == ConvertedType::LIST;
    if !field.is_group() || !is_list {
        return None;
    }
    match list_leaf_physical(field)? {
        PhysicalType::FLOAT | PhysicalType::DOUBLE => Some(()),
        _ => None,
    }
}

/// Walks single-child group nesting to the one leaf primitive of a list.
fn list_leaf_physical(field: &TypePtr) -> Option<PhysicalType> {
    let mut current = field.clone();
    loop {
        if current.is_primitive() {
            return Some(current.get_physical_type());
        }
        let fields = current.get_fields();
        if fields.len() != 1 {
            return None;
        }
        current = fields[0].clone();
    }
}

fn geo_mapping(field: &TypePtr) -> Option<()> {
    if !field.is_group()
        || field.get_basic_info().logical_type_ref().is_some()
        || field.get_basic_info().converted_type() != ConvertedType::NONE
    {
        return None;
    }
    let fields = field.get_fields();
    let canonical = fields.len() == 2
        && fields.iter().all(|child| {
            matches!(child.name(), "lat_deg" | "lng_deg")
                && child.is_primitive()
                && child.get_physical_type() == PhysicalType::DOUBLE
                && child.get_basic_info().logical_type_ref().is_none()
                && child.get_basic_info().converted_type() == ConvertedType::NONE
        });
    canonical.then_some(())
}

fn describe_field(field: &TypePtr) -> String {
    let Some(info) = primitive_info(field) else {
        return match field.get_basic_info().logical_type_ref() {
            Some(logical) => format!("group ({logical:?})"),
            None => "group".to_owned(),
        };
    };
    let mut text = format!("{:?}", info.physical);
    if let Some(logical) = &info.logical {
        text += &format!(" ({logical:?})");
    } else if info.converted != ConvertedType::NONE {
        text += &format!(" ({:?})", info.converted);
    }
    text
}

/// Converts one record-API field through its validated mapping. Nulls map
/// to `Value::Null` for every mapping (the NULL-in-primary-key refusal is
/// the shared sink's, exactly as on the CSV path).
fn convert_field(
    row: usize,
    column: &Column,
    mapping: &ColumnMapping,
    field: Field,
) -> DevonResult<Value> {
    match (mapping, field) {
        (_, Field::Null) => Ok(Value::Null),
        (ColumnMapping::Int64, field) => int64_field(row, column, field),
        (ColumnMapping::Float64, Field::Float(value)) => Ok(Value::Float64(f64::from(value))),
        (ColumnMapping::Float64, Field::Double(value)) => Ok(Value::Float64(value)),
        (ColumnMapping::Bool, Field::Bool(value)) => Ok(Value::Bool(value)),
        (ColumnMapping::String, field) => string_field(row, column, field),
        (ColumnMapping::Bytes, Field::Bytes(value)) => Ok(Value::Bytes(value.data().to_vec())),
        (ColumnMapping::Timestamp(unit), field) => timestamp_field(row, column, *unit, field),
        (ColumnMapping::Decimal { scale }, Field::Decimal(value)) => {
            decimal_field(row, column, *scale, value.data())
        }
        (ColumnMapping::Vector { dim }, Field::ListInternal(list)) => {
            vector_field(row, column, *dim, list.elements())
        }
        (ColumnMapping::Json, field) => json_field(row, column, field),
        (ColumnMapping::GeoPoint, Field::Group(group)) => geo_field(row, column, group),
        (_, field) => Err(corrupt(format!(
            "Parquet row {row}, column `{}`: value {field:?} disagrees with the validated mapping",
            column.name
        ))),
    }
}

fn int64_field(row: usize, column: &Column, field: Field) -> DevonResult<Value> {
    let value = match field {
        Field::Int(value) => i64::from(value),
        Field::Long(value) => value,
        Field::Byte(value) => i64::from(value),
        Field::Short(value) => i64::from(value),
        Field::UByte(value) => i64::from(value),
        Field::UShort(value) => i64::from(value),
        Field::UInt(value) => i64::from(value),
        other => {
            return Err(corrupt(format!(
                "Parquet row {row}, column `{}`: value {other:?} disagrees with the validated mapping",
                column.name
            )));
        }
    };
    Ok(Value::Int64(value))
}

fn string_field(row: usize, column: &Column, field: Field) -> DevonResult<Value> {
    match field {
        Field::Str(value) => Ok(Value::String(value)),
        // A writer that sets the STRING logical type without the legacy
        // UTF8 converted type surfaces as raw bytes.
        Field::Bytes(value) => utf8_text(row, column, value.data()).map(Value::String),
        other => Err(corrupt(format!(
            "Parquet row {row}, column `{}`: value {other:?} disagrees with the validated mapping",
            column.name
        ))),
    }
}

fn utf8_text(row: usize, column: &Column, bytes: &[u8]) -> DevonResult<String> {
    String::from_utf8(bytes.to_vec()).map_err(|error| {
        invalid_argument(format!(
            "Parquet row {row}, column `{}`: field is not UTF-8: {error}",
            column.name
        ))
    })
}

/// Timestamp law: millis ×1000 exactly, micros pass through, nanos must
/// be divisible by 1000 — never truncate.
fn timestamp_field(
    row: usize,
    column: &Column,
    unit: TimestampUnit,
    field: Field,
) -> DevonResult<Value> {
    let value = match field {
        Field::TimestampMillis(value) | Field::TimestampMicros(value) | Field::Long(value) => value,
        other => {
            return Err(corrupt(format!(
                "Parquet row {row}, column `{}`: value {other:?} disagrees with the validated mapping",
                column.name
            )));
        }
    };
    let micros = match unit {
        TimestampUnit::Millis => value.checked_mul(1000).ok_or_else(|| {
            invalid_argument(format!(
                "Parquet row {row}, column `{}`: timestamp millis value {value} overflows epoch micros",
                column.name
            ))
        })?,
        TimestampUnit::Micros => value,
        TimestampUnit::Nanos => {
            if value % 1000 != 0 {
                return Err(invalid_argument(format!(
                    "Parquet row {row}, column `{}`: timestamp nanos value {value} is not divisible by 1000; COPY never truncates",
                    column.name
                )));
            }
            value / 1000
        }
    };
    Ok(Value::Timestamp(micros))
}

/// Decimal law: exact digits, declared scale, digits must fit the
/// column's declared precision.
fn decimal_field(row: usize, column: &Column, scale: u8, unscaled_be: &[u8]) -> DevonResult<Value> {
    let digits = i128_from_big_endian(unscaled_be).ok_or_else(|| {
        invalid_argument(format!(
            "Parquet row {row}, column `{}`: decimal unscaled value exceeds 128 bits",
            column.name
        ))
    })?;
    let value = Decimal128::new(digits, scale)?;
    let LogicalType::Decimal { precision, .. } = column.ty else {
        return Err(corrupt(format!(
            "Parquet row {row}, column `{}`: decimal mapping on a non-decimal column",
            column.name
        )));
    };
    if !value.fits(precision, scale) {
        return Err(invalid_argument(format!(
            "Parquet row {row}, column `{}`: decimal value {value} does not fit Decimal({precision}, {scale}); COPY never rescales",
            column.name
        )));
    }
    Ok(Value::Decimal(value))
}

/// Parquet stores decimal unscaled values as big-endian two's complement.
fn i128_from_big_endian(bytes: &[u8]) -> Option<i128> {
    if bytes.len() > 16 {
        return None;
    }
    let fill = if bytes.first().is_some_and(|b| b & 0x80 != 0) {
        0xFF
    } else {
        0x00
    };
    let mut padded = [fill; 16];
    padded[16 - bytes.len()..].copy_from_slice(bytes);
    Some(i128::from_be_bytes(padded))
}

/// Vector law: every list must have exactly `dim` elements. DOUBLE
/// elements narrow f64→f32 because the mapping law admits LIST<DOUBLE>
/// into the f32 `Vector` type.
fn vector_field(row: usize, column: &Column, dim: u32, elements: &[Field]) -> DevonResult<Value> {
    if elements.len() != dim as usize {
        return Err(invalid_argument(format!(
            "Parquet row {row}, column `{}`: list has {} elements; expected {dim}",
            column.name,
            elements.len()
        )));
    }
    let mut vector = Vec::with_capacity(elements.len());
    for element in elements {
        match element {
            Field::Float(value) => vector.push(*value),
            Field::Double(value) => vector.push(*value as f32),
            Field::Null => {
                return Err(invalid_argument(format!(
                    "Parquet row {row}, column `{}`: list element is null",
                    column.name
                )));
            }
            other => {
                return Err(corrupt(format!(
                    "Parquet row {row}, column `{}`: list element {other:?} disagrees with the validated mapping",
                    column.name
                )));
            }
        }
    }
    Ok(Value::Vector(vector))
}

/// JSON law: the canonical form the text parser enforces
/// (`docs/PLAN_IR.md` § Type system canonical-form law).
fn json_field(row: usize, column: &Column, field: Field) -> DevonResult<Value> {
    let text = match field {
        Field::Str(value) => value,
        Field::Bytes(value) => utf8_text(row, column, value.data())?,
        other => {
            return Err(corrupt(format!(
                "Parquet row {row}, column `{}`: value {other:?} disagrees with the validated mapping",
                column.name
            )));
        }
    };
    let document: serde_json::Value = serde_json::from_str(&text).map_err(|error| {
        invalid_argument(format!(
            "Parquet row {row}, column `{}`: invalid JSON: {error}",
            column.name
        ))
    })?;
    let canonical = serde_json::to_string(&document).map_err(|error| {
        corrupt(format!(
            "Parquet row {row}, column `{}`: JSON could not be canonically serialized: {error}",
            column.name
        ))
    })?;
    Ok(Value::Json(canonical))
}

/// GeoPoint law: `GeoPoint::from_canonical`; non-canonical is refused.
fn geo_field(row: usize, column: &Column, group: Row) -> DevonResult<Value> {
    let mut lat_deg = None;
    let mut lng_deg = None;
    for (name, field) in group.into_columns() {
        let target = match name.as_str() {
            "lat_deg" => &mut lat_deg,
            "lng_deg" => &mut lng_deg,
            other => {
                return Err(invalid_argument(format!(
                    "Parquet row {row}, column `{}`: geo group has unexpected field `{other}`",
                    column.name
                )));
            }
        };
        let Field::Double(value) = field else {
            return Err(invalid_argument(format!(
                "Parquet row {row}, column `{}`: geo component `{name}` must be DOUBLE",
                column.name
            )));
        };
        *target = Some(value);
    }
    let (Some(lat_deg), Some(lng_deg)) = (lat_deg, lng_deg) else {
        return Err(invalid_argument(format!(
            "Parquet row {row}, column `{}`: geo group requires lat_deg and lng_deg",
            column.name
        )));
    };
    let point = GeoPoint::from_canonical(lat_deg, lng_deg).map_err(|error| {
        invalid_argument(format!(
            "Parquet row {row}, column `{}`: {error}",
            column.name
        ))
    })?;
    Ok(Value::GeoPoint(point))
}

fn parquet_error(error: parquet::errors::ParquetError) -> DevonError {
    invalid_argument(format!("invalid Parquet file: {error}"))
}
