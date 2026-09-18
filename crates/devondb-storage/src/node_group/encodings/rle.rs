//! Run-length encoding (id 2): runs of equal values for `Int64`,
//! `Timestamp`, `Decimal`, and `Bool` columns.
//!
//! Its byte layout is part of the on-disk format contract.

use devondb_types::column::{Bitmap, Column};
use devondb_types::logical_type::LogicalType;
use devondb_types::value::Value;
use devondb_types::{Decimal128, DevonResult};

/// Encodes the values section as `u32 run_count` followed by
/// `run_len u32 · value` per run. Runs cover ALL rows including NULL slots
/// (a NULL slot's value is zero by law, so runs merge across NULLs whose
/// neighbours are zero). Parameters are unused and returned zero.
pub(crate) fn encode(column: &Column, ty: &LogicalType) -> DevonResult<(Vec<u8>, [u8; 3])> {
    let bytes = match ty {
        LogicalType::Int64 | LogicalType::Timestamp => encode_runs(
            column.len(),
            |row| i64_slot(column, row, ty),
            |body, value| body.extend_from_slice(&value.to_le_bytes()),
        )?,
        LogicalType::Bool => encode_runs(
            column.len(),
            |row| bool_slot(column, row, ty),
            |body, value| body.push(u8::from(value)),
        )?,
        LogicalType::Decimal { scale, .. } => encode_runs(
            column.len(),
            |row| decimal_slot(column, row, *scale, ty),
            |body, digits| body.extend_from_slice(&digits.to_le_bytes()),
        )?,
        other => {
            return Err(super::super::invalid_argument(format!(
                "encoding rle is not admissible for {other}"
            )));
        }
    };
    Ok((bytes, [0; 3]))
}

/// Runs the slot extractor over every row and serializes the run section.
/// A mismatch between a slot value and the column type is a writer-policy
/// error (`invalid_argument`), never corruption.
fn encode_runs<T: Copy + PartialEq>(
    row_count: usize,
    mut slot: impl FnMut(usize) -> DevonResult<T>,
    mut push_value: impl FnMut(&mut Vec<u8>, T),
) -> DevonResult<Vec<u8>> {
    let mut body = Vec::new();
    let mut run_count = 0_u32;
    let mut current: Option<(T, usize)> = None;
    for row in 0..row_count {
        let value = slot(row)?;
        if let Some((held, len)) = &mut current
            && *held == value
        {
            *len += 1;
            continue;
        }
        if let Some((held, len)) = current.take() {
            flush_run(&mut body, &mut run_count, &mut push_value, held, len)?;
        }
        current = Some((value, 1));
    }
    if let Some((held, len)) = current {
        flush_run(&mut body, &mut run_count, &mut push_value, held, len)?;
    }
    let mut bytes = Vec::with_capacity(4 + body.len());
    bytes.extend_from_slice(&run_count.to_le_bytes());
    bytes.extend_from_slice(&body);
    Ok(bytes)
}

fn flush_run<T: Copy>(
    body: &mut Vec<u8>,
    run_count: &mut u32,
    push_value: &mut impl FnMut(&mut Vec<u8>, T),
    value: T,
    len: usize,
) -> DevonResult<()> {
    let run_len = u32::try_from(len)
        .map_err(|_| super::super::invalid_argument("rle run length exceeds u32"))?;
    body.extend_from_slice(&run_len.to_le_bytes());
    push_value(body, value);
    *run_count = run_count
        .checked_add(1)
        .ok_or_else(|| super::super::invalid_argument("rle run count exceeds u32"))?;
    Ok(())
}

/// The raw i64 slot at `row` (zero at NULL), for `Int64` and `Timestamp`.
fn i64_slot(column: &Column, row: usize, ty: &LogicalType) -> DevonResult<i64> {
    match column.value_at(row) {
        Value::Null => Ok(0),
        Value::Int64(value) | Value::Timestamp(value) => Ok(value),
        other => Err(super::super::invalid_argument(format!(
            "encoding rle value {other} does not match {ty}"
        ))),
    }
}

/// The raw bool slot at `row` (false at NULL).
fn bool_slot(column: &Column, row: usize, ty: &LogicalType) -> DevonResult<bool> {
    match column.value_at(row) {
        Value::Null => Ok(false),
        Value::Bool(value) => Ok(value),
        other => Err(super::super::invalid_argument(format!(
            "encoding rle value {other} does not match {ty}"
        ))),
    }
}

/// The raw unscaled digits at `row` (zero at NULL), admitted only at the
/// column's own scale — rescaling here would be the silent-rescale the
/// decimal law forbids.
fn decimal_slot(column: &Column, row: usize, scale: u8, ty: &LogicalType) -> DevonResult<i128> {
    match column.value_at(row) {
        Value::Null => Ok(0),
        Value::Decimal(value) if value.scale() == scale => Ok(value.digits()),
        other => Err(super::super::invalid_argument(format!(
            "encoding rle value {other} does not match {ty}"
        ))),
    }
}

/// Decodes the run section and materializes the typed [`Column`]: every
/// valid row holds its run's value, NULL slots hold zero per
/// `docs/SCALE.md` §6.3. Every field is validated; failures are `Corrupt`
/// naming the region, never a panic.
pub(crate) fn decode(
    params: [u8; 3],
    bytes: &[u8],
    validity: &Bitmap,
    row_count: usize,
    ty: &LogicalType,
) -> DevonResult<Column> {
    if params != [0; 3] {
        return Err(super::super::corrupt(
            "encoding rle parameters are not zero",
        ));
    }
    match ty {
        LogicalType::Int64 => Ok(Column::Int64 {
            values: decode_runs(bytes, row_count, ty, |slot| Ok(i64::from_le_bytes(slot)))?,
            validity: column_validity(validity, row_count),
        }),
        LogicalType::Timestamp => Ok(Column::Timestamp {
            values: decode_runs(bytes, row_count, ty, |slot| Ok(i64::from_le_bytes(slot)))?,
            validity: column_validity(validity, row_count),
        }),
        LogicalType::Bool => Ok(Column::Bool {
            values: decode_runs(bytes, row_count, ty, |slot: [u8; 1]| {
                let byte = slot[0];
                if byte > 1 {
                    return Err(super::super::corrupt(format!(
                        "rle Bool value is {byte}, expected 0 or 1"
                    )));
                }
                Ok(byte == 1)
            })?,
            validity: column_validity(validity, row_count),
        }),
        LogicalType::Decimal { precision, scale } => {
            decode_decimal(bytes, validity, row_count, *precision, *scale)
        }
        other => Err(super::super::corrupt(format!(
            "encoding rle is not admissible for {other}"
        ))),
    }
}

/// Parses and validates the whole run section, THEN materializes the typed
/// values vector: the `row_count`-sized output is allocated only after the
/// run lengths are proven to cover exactly `row_count` rows.
fn decode_runs<T: Copy, const N: usize>(
    bytes: &[u8],
    row_count: usize,
    ty: &LogicalType,
    read_value: impl Fn([u8; N]) -> DevonResult<T>,
) -> DevonResult<Vec<T>> {
    let runs = parse_runs(bytes, row_count, ty, &read_value)?;
    let mut values = Vec::with_capacity(row_count);
    for (run_len, value) in runs {
        values.extend(std::iter::repeat_n(value, run_len as usize));
    }
    Ok(values)
}

/// Validates the section framing — the `run_count` prefix, the exact run
/// section length (a trailing byte is corruption), no zero-length runs,
/// coverage of exactly `row_count` rows — and returns the decoded runs.
/// All arithmetic is checked; the runs vector stays bounded by the input
/// byte count (`run_count ≤ (len - 4) / (4 + N)`).
fn parse_runs<T: Copy, const N: usize>(
    bytes: &[u8],
    row_count: usize,
    ty: &LogicalType,
    read_value: &impl Fn([u8; N]) -> DevonResult<T>,
) -> DevonResult<Vec<(u32, T)>> {
    if bytes.len() < 4 {
        return Err(super::super::corrupt(format!(
            "rle values section is {} bytes, smaller than the 4-byte run_count prefix",
            bytes.len()
        )));
    }
    let run_count = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
    let entry_len = 4 + N;
    let needed = run_count.checked_mul(entry_len).ok_or_else(|| {
        super::super::corrupt(format!(
            "rle run_count {run_count} overflows the run section length for {ty}"
        ))
    })?;
    let actual = bytes.len() - 4;
    if actual < needed {
        return Err(super::super::corrupt(format!(
            "rle run section is {actual} bytes, expected {needed} for {run_count} runs"
        )));
    }
    if actual > needed {
        return Err(super::super::corrupt(format!(
            "rle values section has {} trailing bytes after {run_count} runs",
            actual - needed
        )));
    }
    let mut runs = Vec::with_capacity(run_count);
    let mut covered = 0_usize;
    for index in 0..run_count {
        let start = 4 + index * entry_len;
        let run_len = u32::from_le_bytes([
            bytes[start],
            bytes[start + 1],
            bytes[start + 2],
            bytes[start + 3],
        ]);
        if run_len == 0 {
            return Err(super::super::corrupt(format!(
                "rle run {index} has length 0"
            )));
        }
        covered = covered
            .checked_add(run_len as usize)
            .ok_or_else(|| super::super::corrupt("rle run lengths overflow row coverage"))?;
        let mut slot = [0_u8; N];
        slot.copy_from_slice(&bytes[start + 4..start + entry_len]);
        runs.push((run_len, read_value(slot)?));
    }
    if covered != row_count {
        return Err(super::super::corrupt(format!(
            "rle runs cover {covered} rows, expected {row_count}"
        )));
    }
    Ok(runs)
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
            "rle column has invalid Decimal({precision}, {scale}) declaration"
        )));
    }
    let ty = LogicalType::Decimal { precision, scale };
    let values = decode_runs(bytes, row_count, &ty, |slot| {
        let digits = i128::from_le_bytes(slot);
        let value = Decimal128::new(digits, scale)
            .map_err(|error| super::super::corrupt(format!("rle Decimal is invalid: {error}")))?;
        if !value.fits(precision, scale) {
            return Err(super::super::corrupt(format!(
                "rle Decimal digits exceed declared precision {precision}"
            )));
        }
        Ok(digits)
    })?;
    Ok(Column::Decimal {
        values,
        scale,
        validity: column_validity(validity, row_count),
    })
}

/// The column's validity: `None` when every row is valid (§6.3).
fn column_validity(validity: &Bitmap, row_count: usize) -> Option<Bitmap> {
    let has_null = (0..row_count).any(|row| !validity.is_valid(row));
    has_null.then(|| validity.clone())
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
                vec![
                    Value::Int64(i64::MIN),
                    Value::Int64(i64::MIN),
                    Value::Int64(i64::MAX),
                    Value::Null,
                    Value::Int64(i64::MAX),
                ],
            ),
            // NULL slots are zero by law: [0, NULL, 0] is ONE run of three.
            (
                LogicalType::Int64,
                vec![Value::Int64(0), Value::Null, Value::Int64(0)],
            ),
            (LogicalType::Int64, vec![Value::Null; 3]),
            (LogicalType::Int64, vec![Value::Int64(1)]),
            (LogicalType::Int64, vec![Value::Null]),
            (
                LogicalType::Timestamp,
                vec![
                    Value::Timestamp(-1),
                    Value::Null,
                    Value::Timestamp(-1),
                    Value::Timestamp(i64::MAX),
                ],
            ),
            (
                LogicalType::Bool,
                vec![
                    Value::Bool(true),
                    Value::Bool(true),
                    Value::Bool(false),
                    Value::Null,
                ],
            ),
            (LogicalType::Bool, vec![Value::Bool(false); 2]),
            (
                decimal,
                vec![
                    Value::Decimal(Decimal128::new(-4225, 2).unwrap()),
                    Value::Null,
                    Value::Decimal(Decimal128::new(-4225, 2).unwrap()),
                    Value::Decimal(Decimal128::new(0, 2).unwrap()),
                ],
            ),
        ];
        for (ty, rows) in cases {
            let column = round_trip(&ty, rows.clone());
            assert_eq!(column, rows, "round trip diverged for {ty}");
        }
    }

    #[test]
    fn null_merging_run_encodes_as_one_run() {
        let column = Column::from_values(
            &LogicalType::Int64,
            vec![Value::Int64(0), Value::Null, Value::Int64(0)],
        );
        let (bytes, params) = encode(&column, &LogicalType::Int64).unwrap();
        assert_eq!(params, [0; 3]);
        let mut expected = vec![1, 0, 0, 0, 3, 0, 0, 0];
        expected.extend_from_slice(&[0; 8]);
        assert_eq!(bytes, expected, "one merged run of three zeros");
    }

    #[test]
    fn mistyped_values_are_a_writer_error() {
        let column = Column::from_values(
            &LogicalType::Int64,
            vec![Value::Int64(1), Value::String("nope".to_owned())],
        );
        // The mismatched value degrades the column to Boxed; encoding it as
        // rle Int64 is a writer-policy error.
        let error = encode(&column, &LogicalType::Int64).unwrap_err();
        let DevonError::InvalidArgument { context } = error else {
            panic!("expected InvalidArgument, got {error}");
        };
        assert!(context.contains("encoding rle value"), "{context}");

        let error = encode(&column, &LogicalType::String).unwrap_err();
        assert!(
            matches!(error, DevonError::InvalidArgument { .. }),
            "String is not rle-admissible"
        );
    }

    #[test]
    fn corrupt_sections_are_refused_without_panicking() {
        let validity = Bitmap::all_valid(3);
        let cases: Vec<(&[u8], LogicalType)> = vec![
            // Shorter than the run_count prefix.
            (&[1, 2, 3], LogicalType::Int64),
            // run_count 2 but only one run present.
            (
                &[2, 0, 0, 0, 1, 0, 0, 0, 7, 0, 0, 0, 0, 0, 0, 0],
                LogicalType::Int64,
            ),
            // One run declared, a trailing byte behind it.
            (
                &[1, 0, 0, 0, 3, 0, 0, 0, 7, 0, 0, 0, 0, 0, 0, 0, 0xff],
                LogicalType::Int64,
            ),
            // A zero-length run.
            (
                &[1, 0, 0, 0, 0, 0, 0, 0, 7, 0, 0, 0, 0, 0, 0, 0],
                LogicalType::Int64,
            ),
            // Runs cover 2 rows of a 3-row column.
            (
                &[1, 0, 0, 0, 2, 0, 0, 0, 7, 0, 0, 0, 0, 0, 0, 0],
                LogicalType::Int64,
            ),
            // A Bool byte above 1.
            (&[1, 0, 0, 0, 3, 0, 0, 0, 2], LogicalType::Bool),
        ];
        for (bytes, ty) in cases {
            let result = decode([0; 3], bytes, &validity, 3, &ty);
            assert!(
                matches!(result, Err(DevonError::Corrupt { .. })),
                "expected Corrupt for {ty} section {bytes:?}"
            );
        }
        let good = &[1, 0, 0, 0, 3, 0, 0, 0, 7, 0, 0, 0, 0, 0, 0, 0];
        let params = decode([1, 0, 0], good, &validity, 3, &LogicalType::Int64);
        assert!(matches!(params, Err(DevonError::Corrupt { .. })));
        let inadmissible = decode([0; 3], good, &validity, 3, &LogicalType::Float64);
        assert!(matches!(inadmissible, Err(DevonError::Corrupt { .. })));
    }
}
