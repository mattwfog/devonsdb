//! Internal-format API for GeoPoint column values (`docs/GEO.md` §5;
//! `docs/FORMAT.md` § node groups).
//!
//! Activation is gated by the schema floor and a feature bit.

use devondb_types::{DevonError, DevonResult, GeoPoint, value::Value};

const GEO_POINT_SLOT_LEN: usize = 16;

/// Appends the fixed-width values section for a GeoPoint column.
///
/// Null slots are zeroed; callers encode the validity bitmap separately.
///
/// # Errors
///
/// Returns [`DevonError::InvalidArgument`] if a non-null value is not a
/// GeoPoint.
pub fn encode_geo_points(
    column: &[Value],
    payload: &mut Vec<u8>,
    column_index: usize,
) -> DevonResult<()> {
    for (row, value) in column.iter().enumerate() {
        match value {
            Value::Null => payload.extend([0_u8; GEO_POINT_SLOT_LEN]),
            Value::GeoPoint(point) => {
                payload.extend(point.lat_deg().to_le_bytes());
                payload.extend(point.lng_deg().to_le_bytes());
            }
            _ => return Err(invalid_stored_value(column_index, row, value)),
        }
    }
    Ok(())
}

/// Decodes and validates a fixed-width GeoPoint values section.
///
/// Every non-null pair must already be canonical. Persisted values are never
/// normalized, so accepted component bit patterns re-encode exactly.
///
/// # Errors
///
/// Returns [`DevonError::Corrupt`] for malformed lengths, validity padding,
/// nonzero null slots, or non-canonical coordinate pairs.
pub fn decode_geo_points(
    validity: &[u8],
    values: &[u8],
    row_count: usize,
    column_index: usize,
) -> DevonResult<Vec<Value>> {
    validate_lengths(validity, values, row_count, column_index)?;
    validate_bitmap_padding(validity, row_count, column_index)?;

    let mut column = Vec::with_capacity(row_count);
    for (row, slot) in values
        .as_chunks::<GEO_POINT_SLOT_LEN>()
        .0
        .iter()
        .enumerate()
    {
        if !bit_is_set(validity, row) {
            ensure_null_slot_zero(slot, column_index, row)?;
            column.push(Value::Null);
            continue;
        }

        let lat_deg = read_f64(&slot[..8]);
        let lng_deg = read_f64(&slot[8..]);
        let point = GeoPoint::from_canonical(lat_deg, lng_deg).map_err(|error| {
            corrupt(format!(
                "column {column_index} row {row} GeoPoint is non-canonical: {error}"
            ))
        })?;
        column.push(Value::GeoPoint(point));
    }
    Ok(column)
}

/// Returns the full fixed payload length, including the validity bitmap.
#[must_use]
pub fn fixed_geo_payload_len(row_count: usize) -> Option<usize> {
    bitmap_len(row_count).checked_add(row_count.checked_mul(GEO_POINT_SLOT_LEN)?)
}

fn validate_lengths(
    validity: &[u8],
    values: &[u8],
    row_count: usize,
    column_index: usize,
) -> DevonResult<()> {
    let expected_validity_len = bitmap_len(row_count);
    if validity.len() != expected_validity_len {
        return Err(corrupt(format!(
            "column {column_index} GeoPoint validity length is {}, expected exactly {expected_validity_len}",
            validity.len()
        )));
    }

    let expected_values_len = row_count.checked_mul(GEO_POINT_SLOT_LEN).ok_or_else(|| {
        corrupt(format!(
            "column {column_index} GeoPoint values length overflows"
        ))
    })?;
    if values.len() != expected_values_len {
        return Err(corrupt(format!(
            "column {column_index} GeoPoint values length is {}, expected exactly {expected_values_len}",
            values.len()
        )));
    }
    Ok(())
}

fn validate_bitmap_padding(
    validity: &[u8],
    row_count: usize,
    column_index: usize,
) -> DevonResult<()> {
    let used_bits = row_count % 8;
    if used_bits == 0 {
        return Ok(());
    }
    let used_mask = (1_u8 << used_bits) - 1;
    if validity.last().is_some_and(|byte| byte & !used_mask != 0) {
        return Err(corrupt(format!(
            "column {column_index} GeoPoint validity bitmap trailing bits are not zero"
        )));
    }
    Ok(())
}

fn ensure_null_slot_zero(slot: &[u8], column_index: usize, row: usize) -> DevonResult<()> {
    if slot.iter().any(|byte| *byte != 0) {
        return Err(corrupt(format!(
            "column {column_index} row {row} null GeoPoint slot is not zero"
        )));
    }
    Ok(())
}

fn bitmap_len(row_count: usize) -> usize {
    row_count.div_ceil(8)
}

fn bit_is_set(bitmap: &[u8], row: usize) -> bool {
    bitmap[row / 8] & (1 << (row % 8)) != 0
}

fn read_f64(bytes: &[u8]) -> f64 {
    let mut array = [0_u8; 8];
    array.copy_from_slice(bytes);
    f64::from_le_bytes(array)
}

fn invalid_stored_value(column: usize, row: usize, value: &Value) -> DevonError {
    DevonError::InvalidArgument {
        context: format!("column {column} row {row} has invalid stored value {value}"),
    }
}

fn corrupt(context: impl Into<String>) -> DevonError {
    DevonError::Corrupt {
        context: context.into(),
    }
}
