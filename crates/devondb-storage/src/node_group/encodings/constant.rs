//! Constant encoding (id 1): every non-null row holds one shared value.
//!
//! Its byte layout is part of the on-disk format contract.

use devondb_types::column::{Bitmap, Column};
use devondb_types::logical_type::LogicalType;
use devondb_types::value::Value;
use devondb_types::{Decimal128, DevonResult};

/// The values-section byte length for constant's fixed-width types, or
/// `None` for `String` (variable: u32 length prefix + bytes) and for types
/// the encoding does not admit.
pub(crate) const fn value_section_len(ty: &LogicalType) -> Option<usize> {
    match ty {
        LogicalType::Int64 | LogicalType::Float64 | LogicalType::Timestamp => Some(8),
        LogicalType::Bool => Some(1),
        LogicalType::Decimal { .. } => Some(16),
        LogicalType::String => None,
        _ => None,
    }
}

/// Encodes the values section as one shared value in the plain fixed-width
/// spelling (`String`: u32 length + bytes). Every non-null row must hold
/// the same value — the writer picks the encoding, so a disagreement is a
/// writer-policy error, not corruption. An all-NULL column stores the zero
/// spelling. Parameters are unused and returned zero.
pub(crate) fn encode(column: &Column, ty: &LogicalType) -> DevonResult<(Vec<u8>, [u8; 3])> {
    let mut constant: Option<Value> = None;
    for row in 0..column.len() {
        let value = column.value_at(row);
        if matches!(value, Value::Null) {
            continue;
        }
        match &constant {
            None => constant = Some(value),
            Some(shared) if same_value(shared, &value) => {}
            Some(_) => {
                return Err(super::super::invalid_argument(format!(
                    "encoding constant requires one shared non-null value but row {row} disagrees"
                )));
            }
        }
    }
    let bytes = match &constant {
        Some(value) => encode_value(value, ty)?,
        None => zero_spelling(ty),
    };
    Ok((bytes, [0; 3]))
}

/// Decodes the shared value and materializes the typed [`Column`]: every
/// valid row holds it, NULL slots hold zero (typed) or `Value::Null`
/// (boxed) per `docs/SCALE.md` §6.3.
pub(crate) fn decode(
    params: [u8; 3],
    bytes: &[u8],
    validity: &Bitmap,
    row_count: usize,
    ty: &LogicalType,
) -> DevonResult<Column> {
    if params != [0; 3] {
        return Err(super::super::corrupt(
            "encoding constant parameters are not zero",
        ));
    }
    let all_null = (0..row_count).all(|row| !validity.is_valid(row));
    if all_null && *bytes != zero_spelling(ty) {
        return Err(super::super::corrupt(
            "constant value slot is not the zero spelling but every row is NULL",
        ));
    }
    match ty {
        LogicalType::Int64 => {
            let value = i64::from_le_bytes(fixed_slot(bytes, ty)?);
            Ok(Column::Int64 {
                values: slots(value, validity, row_count),
                validity: column_validity(validity, row_count),
            })
        }
        LogicalType::Timestamp => {
            let value = i64::from_le_bytes(fixed_slot(bytes, ty)?);
            Ok(Column::Timestamp {
                values: slots(value, validity, row_count),
                validity: column_validity(validity, row_count),
            })
        }
        LogicalType::Float64 => {
            let value = f64::from_le_bytes(fixed_slot(bytes, ty)?);
            Ok(Column::Float64 {
                values: slots(value, validity, row_count),
                validity: column_validity(validity, row_count),
            })
        }
        LogicalType::Bool => {
            let slot = fixed_slot::<1>(bytes, ty)?[0];
            if slot > 1 {
                return Err(super::super::corrupt(format!(
                    "constant Bool value is {slot}, expected 0 or 1"
                )));
            }
            Ok(Column::Bool {
                values: slots(slot == 1, validity, row_count),
                validity: column_validity(validity, row_count),
            })
        }
        LogicalType::Decimal { precision, scale } => {
            decode_decimal(bytes, validity, row_count, *precision, *scale)
        }
        LogicalType::String => decode_string(bytes, validity, row_count),
        other => Err(super::super::corrupt(format!(
            "encoding constant is not admissible for {other}"
        ))),
    }
}

/// The canonical value bytes for an all-NULL column: zero-filled fixed
/// width, or an empty `String` spelling (u32 length 0).
fn zero_spelling(ty: &LogicalType) -> Vec<u8> {
    match value_section_len(ty) {
        Some(width) => vec![0_u8; width],
        None => vec![0_u8; 4],
    }
}

/// Two values are the same constant: `Float64` compares by bit pattern so
/// an all-NaN column stays encodable and the frozen spelling round-trips.
fn same_value(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Float64(left), Value::Float64(right)) => left.to_bits() == right.to_bits(),
        _ => left == right,
    }
}

fn encode_value(value: &Value, ty: &LogicalType) -> DevonResult<Vec<u8>> {
    match (ty, value) {
        (LogicalType::Int64, Value::Int64(value))
        | (LogicalType::Timestamp, Value::Timestamp(value)) => Ok(value.to_le_bytes().to_vec()),
        (LogicalType::Float64, Value::Float64(value)) => Ok(value.to_le_bytes().to_vec()),
        (LogicalType::Bool, Value::Bool(value)) => Ok(vec![u8::from(*value)]),
        (LogicalType::Decimal { .. }, Value::Decimal(value)) => {
            Ok(value.digits().to_le_bytes().to_vec())
        }
        (LogicalType::String, Value::String(value)) => {
            let len = u32::try_from(value.len()).map_err(|_| {
                super::super::invalid_argument("encoding constant String value length exceeds u32")
            })?;
            let mut bytes = Vec::with_capacity(4 + value.len());
            bytes.extend_from_slice(&len.to_le_bytes());
            bytes.extend_from_slice(value.as_bytes());
            Ok(bytes)
        }
        _ => Err(super::super::invalid_argument(format!(
            "encoding constant value {value} does not match {ty}"
        ))),
    }
}

/// The fixed-width value slot, with the exact-length law enforced.
fn fixed_slot<const N: usize>(bytes: &[u8], ty: &LogicalType) -> DevonResult<[u8; N]> {
    if bytes.len() != N {
        return Err(super::super::corrupt(format!(
            "constant value section is {} bytes, expected exactly {N} for {ty}",
            bytes.len()
        )));
    }
    let mut slot = [0_u8; N];
    slot.copy_from_slice(bytes);
    Ok(slot)
}

/// The materialized values vector: the shared value at valid rows, the
/// type's zero at NULL slots (`docs/SCALE.md` §6.3).
fn slots<T: Copy + Default>(value: T, validity: &Bitmap, row_count: usize) -> Vec<T> {
    (0..row_count)
        .map(|row| {
            if validity.is_valid(row) {
                value
            } else {
                T::default()
            }
        })
        .collect()
}

/// The column's validity: `None` when every row is valid (§6.3).
fn column_validity(validity: &Bitmap, row_count: usize) -> Option<Bitmap> {
    let has_null = (0..row_count).any(|row| !validity.is_valid(row));
    has_null.then(|| validity.clone())
}

fn decode_decimal(
    bytes: &[u8],
    validity: &Bitmap,
    row_count: usize,
    precision: u8,
    scale: u8,
) -> DevonResult<Column> {
    if !(1..=devondb_types::decimal::MAX_PRECISION).contains(&precision) || scale > precision {
        return Err(super::super::corrupt(format!(
            "constant column has invalid Decimal({precision}, {scale}) declaration"
        )));
    }
    let digits = i128::from_le_bytes(fixed_slot(
        bytes,
        &LogicalType::Decimal { precision, scale },
    )?);
    let value = Decimal128::new(digits, scale)
        .map_err(|error| super::super::corrupt(format!("constant Decimal is invalid: {error}")))?;
    if !value.fits(precision, scale) {
        return Err(super::super::corrupt(format!(
            "constant Decimal digits exceed declared precision {precision}"
        )));
    }
    Ok(Column::Decimal {
        values: slots(digits, validity, row_count),
        scale,
        validity: column_validity(validity, row_count),
    })
}

fn decode_string(bytes: &[u8], validity: &Bitmap, row_count: usize) -> DevonResult<Column> {
    if bytes.len() < 4 {
        return Err(super::super::corrupt(format!(
            "constant String value section is {} bytes, expected at least 4",
            bytes.len()
        )));
    }
    let len = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
    if bytes.len() - 4 != len {
        return Err(super::super::corrupt(format!(
            "constant String value section is {} bytes, expected {} for a {len}-byte value",
            bytes.len(),
            4 + len
        )));
    }
    let text = std::str::from_utf8(&bytes[4..]).map_err(|error| {
        super::super::corrupt(format!("constant String is not valid UTF-8: {error}"))
    })?;
    Ok(Column::Boxed(
        (0..row_count)
            .map(|row| {
                if validity.is_valid(row) {
                    Value::String(text.to_owned())
                } else {
                    Value::Null
                }
            })
            .collect(),
    ))
}

#[cfg(test)]
mod tests {
    use super::{decode, encode};
    use devondb_types::column::{Bitmap, Column};
    use devondb_types::logical_type::LogicalType;
    use devondb_types::value::Value;
    use devondb_types::{Decimal128, DevonError};

    fn round_trip(ty: &LogicalType, rows: Vec<Value>) -> Column {
        let column = Column::from_values(ty, rows);
        let (bytes, params) = encode(&column, ty).unwrap();
        let mut validity = Bitmap::all_valid(column.len());
        for row in 0..column.len() {
            if matches!(column.value_at(row), Value::Null) {
                validity.clear(row);
            }
        }
        decode(params, &bytes, &validity, column.len(), ty).unwrap()
    }

    #[test]
    fn round_trip_covers_every_admissible_type_and_null_pattern() {
        let decimal = LogicalType::Decimal {
            precision: 10,
            scale: 2,
        };
        let cases: Vec<(LogicalType, Vec<Value>)> = vec![
            (LogicalType::Int64, vec![Value::Int64(-7); 4]),
            (
                LogicalType::Int64,
                vec![Value::Int64(9), Value::Null, Value::Int64(9)],
            ),
            (LogicalType::Int64, vec![Value::Null; 3]),
            (LogicalType::Int64, vec![Value::Int64(1)]),
            (LogicalType::Float64, vec![Value::Float64(2.5); 2]),
            (
                LogicalType::Float64,
                vec![Value::Float64(-0.0), Value::Null],
            ),
            (LogicalType::Bool, vec![Value::Bool(true); 3]),
            (LogicalType::Bool, vec![Value::Bool(false), Value::Null]),
            (LogicalType::Timestamp, vec![Value::Timestamp(-1); 2]),
            (
                decimal,
                vec![
                    Value::Decimal(Decimal128::new(125, 2).unwrap()),
                    Value::Null,
                    Value::Decimal(Decimal128::new(125, 2).unwrap()),
                ],
            ),
            (
                LogicalType::String,
                vec![Value::String("shared".to_owned()), Value::Null],
            ),
            (LogicalType::String, vec![Value::String(String::new()); 2]),
            (LogicalType::String, vec![Value::Null]),
        ];
        for (ty, rows) in cases {
            let column = round_trip(&ty, rows.clone());
            assert_eq!(column, rows, "round trip diverged for {ty}");
        }
    }

    #[test]
    fn nan_constant_encodes_and_decodes_by_bit_pattern() {
        // Value equality is NaN-averse, so the constant check compares
        // Float64 by bits; the decoded slots keep the exact NaN spelling.
        let column = Column::from_values(
            &LogicalType::Float64,
            vec![Value::Float64(f64::NAN), Value::Null],
        );
        let (bytes, params) = encode(&column, &LogicalType::Float64).unwrap();
        assert_eq!(bytes, f64::NAN.to_le_bytes());
        let mut validity = Bitmap::all_valid(2);
        validity.clear(1);
        let decoded = decode(params, &bytes, &validity, 2, &LogicalType::Float64).unwrap();
        let Column::Float64 { values, .. } = decoded else {
            panic!("expected typed Float64 storage");
        };
        assert!(values[0].is_nan());
        assert_eq!(values[1], 0.0, "NULL slots decode to zero");
    }

    #[test]
    fn non_constant_columns_are_a_writer_error() {
        let column =
            Column::from_values(&LogicalType::Int64, vec![Value::Int64(1), Value::Int64(2)]);
        let error = encode(&column, &LogicalType::Int64).unwrap_err();
        let DevonError::InvalidArgument { context } = error else {
            panic!("expected InvalidArgument, got {error}");
        };
        assert!(
            context.contains("encoding constant requires one shared non-null value"),
            "{context}"
        );
    }

    #[test]
    fn corrupt_sections_are_refused_without_panicking() {
        let validity = Bitmap::all_valid(2);
        let cases: Vec<(&[u8], LogicalType)> = vec![
            (&[1, 2, 3], LogicalType::Int64),
            (&[2], LogicalType::Bool),
            (&[1, 2], LogicalType::String),
            (&[5, 0, 0, 0, b'a'], LogicalType::String),
            (&[3, 0, 0, 0, 0xff, 0xff, 0xff], LogicalType::String),
        ];
        for (bytes, ty) in cases {
            let result = decode([0; 3], bytes, &validity, 2, &ty);
            assert!(
                matches!(result, Err(DevonError::Corrupt { .. })),
                "expected Corrupt for {ty} section {bytes:?}"
            );
        }
        let params = decode([1, 0, 0], &[8; 8], &validity, 2, &LogicalType::Int64);
        assert!(matches!(params, Err(DevonError::Corrupt { .. })));
    }
}
