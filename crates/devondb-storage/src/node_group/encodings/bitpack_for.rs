//! bitpack_for encoding (id 3): FastLanes 1024-value transposed bit-packing
//! with a frame of reference, for `Int64` and `Timestamp` values sections.
//!
//! The byte layout is binding: `i64 reference` (the minimum),
//! `u8 bit_width`, 3 reserved zero
//! bytes, then `ceil(row_count / 1024)` blocks of 1024 deltas packed in the
//! FastLanes transposed order (Afroozeh & Boncz, "The FastLanes Compression
//! Layout", PVLDB 16(9), 2023). The scalar decoder below is the reference
//! implementation; a SIMD kernel at any lane width reads the same bytes.

use devondb_types::DevonResult;
use devondb_types::column::{Bitmap, Column};
use devondb_types::logical_type::LogicalType;

/// Values per FastLanes block (the paper's vector size).
const BLOCK_ROWS: usize = 1024;
/// u64 words per bit-plane: 1024 bits.
const WORDS_PER_PLANE: usize = 16;
/// Lanes per u64 word.
const LANES_PER_WORD: usize = 64;
/// Bytes per packed block at width `w`: `w` planes × 16 words × 8 bytes.
const BYTES_PER_PLANE: usize = WORDS_PER_PLANE * 8;
/// Section header: `i64 reference` + `u8 bit_width` + 3 reserved zero bytes.
const HEADER_LEN: usize = 12;
/// The widest legal bit width (a full `u64` delta).
const MAX_BIT_WIDTH: u8 = 64;

/// Encodes the values section: one slot per row (NULL slots encode as delta
/// 0), deltas `(value − reference)` packed in the FastLanes transposed order.
/// `reference` is the minimum over the non-NULL values (0 when every row is
/// NULL) and `bit_width` the narrowest width holding every delta. Parameters
/// return `[bit_width, 0, 0]` (`docs/SCALE.md` §8.1: p0 duplicates the width
/// for directory-level sanity).
pub(crate) fn encode(column: &Column, ty: &LogicalType) -> DevonResult<(Vec<u8>, [u8; 3])> {
    let (values, validity) = typed_slots(column, ty)?;
    let reference = reference_of(values, validity);
    let bit_width = bit_width_of(values, validity, reference);
    let block_count = values.len().div_ceil(BLOCK_ROWS);
    let mut bytes =
        Vec::with_capacity(HEADER_LEN + block_count * bit_width as usize * BYTES_PER_PLANE);
    bytes.extend_from_slice(&reference.to_le_bytes());
    bytes.push(bit_width);
    bytes.extend_from_slice(&[0; 3]);
    let mut deltas = [0_u64; BLOCK_ROWS];
    for block in 0..block_count {
        fill_deltas(&mut deltas, values, validity, reference, block);
        pack_block(&deltas, bit_width, &mut bytes);
    }
    Ok((bytes, [bit_width, 0, 0]))
}

/// Decodes a bitpack_for values section into the typed [`Column`], NULL
/// slots materialized as zero (`docs/SCALE.md` §6.3). Every field is
/// validated — parameters, header, reserved bytes, exact section length,
/// NULL/padding slot zeros, and `reference + delta` overflow — and any
/// violation is `Corrupt`, never a panic.
pub(crate) fn decode(
    params: [u8; 3],
    bytes: &[u8],
    validity: &Bitmap,
    row_count: usize,
    ty: &LogicalType,
) -> DevonResult<Column> {
    if !matches!(ty, LogicalType::Int64 | LogicalType::Timestamp) {
        return Err(super::super::corrupt(format!(
            "encoding bitpack_for is not admissible for {ty}"
        )));
    }
    let (reference, bit_width) = header(params, bytes)?;
    let expected_len = section_len(row_count, bit_width)?;
    if bytes.len() != expected_len {
        return Err(super::super::corrupt(format!(
            "bitpack_for section is {} bytes, expected exactly {expected_len} for {row_count} rows at bit width {bit_width}",
            bytes.len()
        )));
    }
    if (0..row_count).all(|row| !validity.is_valid(row)) && reference != 0 {
        return Err(super::super::corrupt(format!(
            "bitpack_for reference is {reference} but every row is NULL"
        )));
    }
    let values = decode_blocks(bytes, reference, bit_width, validity, row_count)?;
    let validity = column_validity(validity, row_count);
    Ok(match ty {
        LogicalType::Int64 => Column::Int64 { values, validity },
        _ => Column::Timestamp { values, validity },
    })
}

/// The typed `i64` slots and validity of the input column; anything but the
/// matching typed variant is a writer error, not corruption.
fn typed_slots<'a>(
    column: &'a Column,
    ty: &LogicalType,
) -> DevonResult<(&'a [i64], Option<&'a Bitmap>)> {
    match (ty, column) {
        (LogicalType::Int64, Column::Int64 { values, validity })
        | (LogicalType::Timestamp, Column::Timestamp { values, validity }) => {
            Ok((values, validity.as_ref()))
        }
        _ => Err(super::super::invalid_argument(format!(
            "encoding bitpack_for column storage does not match {ty}"
        ))),
    }
}

/// The frame of reference: the minimum non-NULL value, or 0 when no row is
/// valid (the all-NULL zero spelling, mirroring `constant.md`).
fn reference_of(values: &[i64], validity: Option<&Bitmap>) -> i64 {
    let mut reference: Option<i64> = None;
    for (row, &value) in values.iter().enumerate() {
        if !is_valid(validity, row) {
            continue;
        }
        reference = Some(reference.map_or(value, |minimum| minimum.min(value)));
    }
    reference.unwrap_or(0)
}

/// The narrowest width holding every `(value − reference)` delta. Deltas are
/// computed in `i128` so `i64::MIN ..= i64::MAX` spans fit a `u64`.
fn bit_width_of(values: &[i64], validity: Option<&Bitmap>, reference: i64) -> u8 {
    let mut widest = 0_u64;
    for (row, &value) in values.iter().enumerate() {
        if is_valid(validity, row) {
            widest = widest.max(delta_of(value, reference));
        }
    }
    MAX_BIT_WIDTH - widest.leading_zeros() as u8
}

/// The delta of one value against the reference, as an unsigned 64-bit
/// distance (always nonnegative: reference is the minimum).
fn delta_of(value: i64, reference: i64) -> u64 {
    (value as i128 - reference as i128) as u64
}

fn is_valid(validity: Option<&Bitmap>, row: usize) -> bool {
    validity.is_none_or(|bitmap| bitmap.is_valid(row))
}

/// Fills one block's delta slots: valid rows carry their delta, NULL rows
/// and padding past `values.len()` carry 0 (format law, decoder-enforced).
fn fill_deltas(
    deltas: &mut [u64; BLOCK_ROWS],
    values: &[i64],
    validity: Option<&Bitmap>,
    reference: i64,
    block: usize,
) {
    for (slot, delta) in deltas.iter_mut().enumerate() {
        let row = block * BLOCK_ROWS + slot;
        *delta = if row < values.len() && is_valid(validity, row) {
            delta_of(values[row], reference)
        } else {
            0
        };
    }
}

/// Packs one block in the FastLanes transposed order: bit `plane` of the
/// value at in-block slot `16 × lane + word` lives at bit `lane` of word
/// `word` of plane `plane` — word `i` of every plane holds the 64 values
/// striding 16 (`i, 16 + i, …, 1008 + i`), the paper's 64×16 transpose.
fn pack_block(deltas: &[u64; BLOCK_ROWS], bit_width: u8, out: &mut Vec<u8>) {
    for plane in 0..bit_width as u64 {
        for word in 0..WORDS_PER_PLANE {
            let mut packed = 0_u64;
            for lane in 0..LANES_PER_WORD as u64 {
                packed |= ((deltas[16 * lane as usize + word] >> plane) & 1) << lane;
            }
            out.extend_from_slice(&packed.to_le_bytes());
        }
    }
}

/// Validates the section header and parameters, returning
/// `(reference, bit_width)`.
fn header(params: [u8; 3], bytes: &[u8]) -> DevonResult<(i64, u8)> {
    if params[1] != 0 || params[2] != 0 {
        return Err(super::super::corrupt(
            "encoding bitpack_for parameters p1/p2 are not zero",
        ));
    }
    if bytes.len() < HEADER_LEN {
        return Err(super::super::corrupt(format!(
            "bitpack_for header is {} bytes, expected at least {HEADER_LEN}",
            bytes.len()
        )));
    }
    let mut raw = [0_u8; 8];
    raw.copy_from_slice(&bytes[..8]);
    let reference = i64::from_le_bytes(raw);
    if bytes[9..HEADER_LEN] != [0, 0, 0] {
        return Err(super::super::corrupt(
            "bitpack_for reserved header bytes are not zero",
        ));
    }
    let bit_width = bytes[8];
    if bit_width > MAX_BIT_WIDTH {
        return Err(super::super::corrupt(format!(
            "bitpack_for bit width {bit_width} exceeds {MAX_BIT_WIDTH}"
        )));
    }
    if params[0] != bit_width {
        return Err(super::super::corrupt(format!(
            "bitpack_for directory parameter bit width {} disagrees with the section bit width {bit_width}",
            params[0]
        )));
    }
    Ok((reference, bit_width))
}

/// The exact section length for `row_count` rows at `bit_width`:
/// header plus `ceil(row_count / 1024)` full blocks (the last block is
/// zero-padded to 1024 slots, so every block is full size).
fn section_len(row_count: usize, bit_width: u8) -> DevonResult<usize> {
    row_count
        .div_ceil(BLOCK_ROWS)
        .checked_mul(bit_width as usize)
        .and_then(|planes| planes.checked_mul(BYTES_PER_PLANE))
        .and_then(|blocks| blocks.checked_add(HEADER_LEN))
        .ok_or_else(|| super::super::corrupt("bitpack_for section length overflows"))
}

/// Unpacks every block and applies the deltas, validating NULL/padding
/// zeros and `reference + delta` overflow. Runs only after the exact-length
/// check, so the allocation is bounded by the validated `row_count`.
fn decode_blocks(
    bytes: &[u8],
    reference: i64,
    bit_width: u8,
    validity: &Bitmap,
    row_count: usize,
) -> DevonResult<Vec<i64>> {
    let mut values = vec![0_i64; row_count];
    let mut deltas = [0_u64; BLOCK_ROWS];
    let block_bytes = bit_width as usize * BYTES_PER_PLANE;
    for block in 0..row_count.div_ceil(BLOCK_ROWS) {
        let start = HEADER_LEN + block * block_bytes;
        unpack_block(&bytes[start..start + block_bytes], bit_width, &mut deltas);
        apply_deltas(&mut values, &deltas, reference, validity, row_count, block)?;
    }
    Ok(values)
}

/// The scalar reference unpack of one block — the exact inverse of
/// [`pack_block`].
fn unpack_block(bytes: &[u8], bit_width: u8, deltas: &mut [u64; BLOCK_ROWS]) {
    deltas.fill(0);
    for plane in 0..bit_width as u64 {
        for word in 0..WORDS_PER_PLANE {
            let offset = (plane as usize * WORDS_PER_PLANE + word) * 8;
            let mut raw = [0_u8; 8];
            raw.copy_from_slice(&bytes[offset..offset + 8]);
            let packed = u64::from_le_bytes(raw);
            for lane in 0..LANES_PER_WORD as u64 {
                deltas[16 * lane as usize + word] |= ((packed >> lane) & 1) << plane;
            }
        }
    }
}

/// Materializes one block's values, enforcing the zero laws: padding slots
/// past `row_count` and NULL slots must carry delta 0, and
/// `reference + delta` must fit `i64`.
fn apply_deltas(
    values: &mut [i64],
    deltas: &[u64; BLOCK_ROWS],
    reference: i64,
    validity: &Bitmap,
    row_count: usize,
    block: usize,
) -> DevonResult<()> {
    for (slot, &delta) in deltas.iter().enumerate() {
        let row = block * BLOCK_ROWS + slot;
        if row >= row_count && delta != 0 {
            return Err(super::super::corrupt(format!(
                "bitpack_for padding slot {slot} of block {block} is not zero"
            )));
        }
        if row < row_count && !validity.is_valid(row) && delta != 0 {
            return Err(super::super::corrupt(format!(
                "bitpack_for NULL slot {row} holds nonzero delta {delta}"
            )));
        }
        if row < row_count && validity.is_valid(row) {
            let value = i64::try_from(reference as i128 + delta as i128).map_err(|_| {
                super::super::corrupt(format!(
                    "bitpack_for reference {reference} + delta {delta} overflows i64 at row {row}"
                ))
            })?;
            values[row] = value;
        }
    }
    Ok(())
}

/// The column's validity: `None` when every row is valid (`docs/SCALE.md`
/// §6.3).
fn column_validity(validity: &Bitmap, row_count: usize) -> Option<Bitmap> {
    let has_null = (0..row_count).any(|row| !validity.is_valid(row));
    has_null.then(|| validity.clone())
}

#[cfg(test)]
mod tests {
    use super::{BLOCK_ROWS, HEADER_LEN, decode, encode};
    use devondb_types::column::{Bitmap, Column};
    use devondb_types::logical_type::LogicalType;
    use devondb_types::value::Value;
    use devondb_types::{DevonError, DevonResult};

    fn round_trip(ty: &LogicalType, rows: Vec<Value>) -> DevonResult<Column> {
        let column = Column::from_values(ty, rows);
        let (bytes, params) = encode(&column, ty)?;
        let mut validity = Bitmap::all_valid(column.len());
        for row in 0..column.len() {
            if matches!(column.value_at(row), Value::Null) {
                validity.clear(row);
            }
        }
        decode(params, &bytes, &validity, column.len(), ty)
    }

    #[test]
    fn round_trip_covers_null_patterns_and_boundaries() {
        let block = BLOCK_ROWS as i64;
        let cases: Vec<Vec<Value>> = vec![
            vec![Value::Int64(7)],
            vec![Value::Null],
            vec![Value::Null; 3],
            vec![Value::Int64(0), Value::Null, Value::Int64(-1)],
            vec![Value::Int64(i64::MIN), Value::Int64(i64::MAX)],
            vec![Value::Int64(-5), Value::Int64(-5), Value::Null],
            (0..block).map(Value::Int64).collect(),
            (0..2 * block)
                .map(|row| {
                    if row % 3 == 0 {
                        Value::Null
                    } else {
                        Value::Int64(1000 + row)
                    }
                })
                .collect(),
            (0..block + 1).map(|row| Value::Int64(-row)).collect(),
        ];
        for rows in cases {
            let decoded = round_trip(&LogicalType::Int64, rows.clone()).unwrap();
            assert_eq!(decoded, rows, "round trip diverged");
            let decoded = round_trip(
                &LogicalType::Timestamp,
                rows.iter()
                    .map(|value| match value {
                        Value::Int64(value) => Value::Timestamp(*value),
                        other => other.clone(),
                    })
                    .collect(),
            )
            .unwrap();
            assert_eq!(decoded.len(), rows.len(), "timestamp round trip length");
        }
    }

    #[test]
    fn zero_width_means_every_value_is_the_reference() {
        let decoded = round_trip(
            &LogicalType::Int64,
            vec![Value::Int64(-9), Value::Null, Value::Int64(-9)],
        )
        .unwrap();
        let (bytes, params) = encode(
            &Column::from_values(
                &LogicalType::Int64,
                vec![Value::Int64(-9), Value::Null, Value::Int64(-9)],
            ),
            &LogicalType::Int64,
        )
        .unwrap();
        assert_eq!(decoded.len(), 3);
        assert_eq!(bytes.len(), HEADER_LEN, "bit width 0 carries no blocks");
        assert_eq!(params, [0, 0, 0]);
        assert_eq!(&bytes[..8], &(-9_i64).to_le_bytes());
    }

    #[test]
    fn transposed_layout_places_bits_in_the_fastlanes_order() {
        // Values 0..=1023 (deltas equal values, reference 0, bit width 10):
        // slot 1's bit 0 lands at word 1 lane 0 of plane 0; slot 16's bit 4
        // lands at word 0 lane 1 of plane 4 — the 64×16 transpose.
        let column = Column::from_values(
            &LogicalType::Int64,
            (0..BLOCK_ROWS as i64).map(Value::Int64).collect(),
        );
        let (bytes, params) = encode(&column, &LogicalType::Int64).unwrap();
        assert_eq!(params[0], 10);
        assert_eq!(bytes.len(), HEADER_LEN + 10 * 128);
        let word = |plane: usize, word: usize| {
            let offset = HEADER_LEN + (plane * 16 + word) * 8;
            u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap())
        };
        for slot in 0..BLOCK_ROWS {
            for plane in 0..10 {
                let bit = (word(plane, slot % 16) >> (slot / 16)) & 1;
                assert_eq!(bit, (slot as u64 >> plane) & 1, "slot {slot} plane {plane}");
            }
        }
    }

    #[test]
    fn corrupt_sections_are_refused_without_panicking() {
        let validity = Bitmap::all_valid(2);
        let ty = LogicalType::Int64;
        // Short header.
        assert!(matches!(
            decode([0; 3], &[0; 11], &validity, 2, &ty),
            Err(DevonError::Corrupt { .. })
        ));
        // Reserved header bytes nonzero.
        let mut bytes = vec![0_u8; HEADER_LEN];
        bytes[8] = 0; // bit width 0 → 12-byte section is exact for 2 rows
        bytes[11] = 1;
        assert!(matches!(
            decode([0; 3], &bytes, &validity, 2, &ty),
            Err(DevonError::Corrupt { .. })
        ));
        // Bit width above 64.
        let mut bytes = vec![0_u8; HEADER_LEN];
        bytes[8] = 65;
        assert!(matches!(
            decode([65, 0, 0], &bytes, &validity, 2, &ty),
            Err(DevonError::Corrupt { .. })
        ));
        // Directory parameter disagrees with the section width.
        assert!(matches!(
            decode([1, 0, 0], &[0_u8; HEADER_LEN], &validity, 2, &ty),
            Err(DevonError::Corrupt { .. })
        ));
        // p1/p2 nonzero.
        assert!(matches!(
            decode([0, 1, 0], &[0_u8; HEADER_LEN], &validity, 2, &ty),
            Err(DevonError::Corrupt { .. })
        ));
        // Section length mismatch (trailing byte past the exact law).
        let mut bytes = vec![0_u8; HEADER_LEN + 1];
        bytes[8] = 0;
        assert!(matches!(
            decode([0; 3], &bytes, &validity, 2, &ty),
            Err(DevonError::Corrupt { .. })
        ));
        // reference + delta overflows i64: reference i64::MAX, delta 1.
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&i64::MAX.to_le_bytes());
        bytes.push(1);
        bytes.extend_from_slice(&[0; 3]);
        bytes.extend_from_slice(&1_u64.to_le_bytes()); // plane 0 word 0 lane 0
        bytes.extend_from_slice(&[0_u8; 15 * 8]);
        assert!(matches!(
            decode([1, 0, 0], &bytes, &validity, 2, &ty),
            Err(DevonError::Corrupt { .. })
        ));
        // Nonzero padding slot (row_count 2, block padded to 1024).
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&0_i64.to_le_bytes());
        bytes.push(1);
        bytes.extend_from_slice(&[0; 3]);
        let mut block = vec![0_u8; 128];
        block[8] = 0b100; // word 1 lane 2 → slot 33, a padding slot at row_count 2
        bytes.append(&mut block);
        let mut sparse = Bitmap::all_valid(2);
        sparse.clear(1);
        assert!(matches!(
            decode([1, 0, 0], &bytes, &sparse, 2, &ty),
            Err(DevonError::Corrupt { .. })
        ));
        // reference nonzero in an all-NULL column.
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&5_i64.to_le_bytes());
        bytes.push(0);
        bytes.extend_from_slice(&[0; 3]);
        let mut all_null = Bitmap::all_valid(2);
        all_null.clear(0);
        all_null.clear(1);
        assert!(matches!(
            decode([0; 3], &bytes, &all_null, 2, &ty),
            Err(DevonError::Corrupt { .. })
        ));
        // Inadmissible type.
        assert!(matches!(
            decode(
                [0; 3],
                &[0_u8; HEADER_LEN],
                &validity,
                2,
                &LogicalType::Bool
            ),
            Err(DevonError::Corrupt { .. })
        ));
    }

    #[test]
    fn null_slot_with_nonzero_delta_is_corruption() {
        // validity clears row 1 but its packed delta is 1.
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&0_i64.to_le_bytes());
        bytes.push(1);
        bytes.extend_from_slice(&[0; 3]);
        let mut block = vec![0_u8; 128];
        block[8] = 0b10; // word 1 lane 1 → slot 17, whose row is NULL
        bytes.append(&mut block);
        let mut validity = Bitmap::all_valid(18);
        validity.clear(17);
        let error = decode([1, 0, 0], &bytes, &validity, 18, &LogicalType::Int64);
        assert!(
            matches!(error, Err(DevonError::Corrupt { .. })),
            "{error:?}"
        );
    }
}
