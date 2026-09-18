//! Dictionary encoding (id 4): each distinct non-null string once in a
//! sorted dictionary plus one u32 code per row.
//!
//! Its byte layout is part of the on-disk format contract. Decoding uses
//! [`Column::Boxed`] until the arena variant is available (`docs/SCALE.md`
//! §6.2).

use devondb_types::DevonResult;
use devondb_types::column::{Bitmap, Column};
use devondb_types::logical_type::LogicalType;
use devondb_types::value::Value;

/// The v1 code width in bytes (`p0`); narrower widths are reserved.
const CODE_WIDTH_V1: u8 = 4;
/// v1 codes and heap offsets are u32.
const U32: usize = 4;
/// The v1 parameter spelling: `p0` = 4, `p1`/`p2` zero.
const PARAMS_V1: [u8; 3] = [CODE_WIDTH_V1, 0, 0];

/// Encodes one String column's values section as a bytewise-sorted unique
/// dictionary plus per-row u32 codes. NULL rows carry code 0 — the payload's
/// validity bitmap governs them (`docs/SCALE.md` §8.1). FIXED signature per
/// §8.2.
pub(crate) fn encode(column: &Column, ty: &LogicalType) -> DevonResult<(Vec<u8>, [u8; 3])> {
    if !matches!(ty, LogicalType::String) {
        return Err(super::super::invalid_argument(format!(
            "encoding dictionary is not admissible for {ty}"
        )));
    }
    let dictionary = collect_dictionary(column, ty)?;
    let bytes = write_section(column, &dictionary)?;
    Ok((bytes, PARAMS_V1))
}

/// Decodes a values section written by this encoding, validating every field
/// (parameters, lengths, offsets, entry UTF-8 and ordering, code ranges, NULL
/// codes) and returning `Corrupt` — never panicking. Valid rows materialize
/// as `Value::String`, NULL slots as `Value::Null` (`docs/SCALE.md` §6.3).
/// FIXED signature per §8.2.
pub(crate) fn decode(
    params: [u8; 3],
    bytes: &[u8],
    validity: &Bitmap,
    row_count: usize,
    ty: &LogicalType,
) -> DevonResult<Column> {
    if params != PARAMS_V1 {
        return Err(super::super::corrupt(format!(
            "encoding dictionary parameters are not the v1 spelling {PARAMS_V1:?}"
        )));
    }
    if !matches!(ty, LogicalType::String) {
        return Err(super::super::corrupt(format!(
            "encoding dictionary is not admissible for {ty}"
        )));
    }
    let (offsets, heap, codes) = split_section(bytes, row_count)?;
    let entries = validate_entries(&offsets, heap)?;
    materialize(entries, codes, validity, row_count)
}

/// The dictionary: every distinct non-null string, sorted bytewise ascending
/// (`str` order IS byte order) and deduplicated.
fn collect_dictionary(column: &Column, ty: &LogicalType) -> DevonResult<Vec<String>> {
    let mut dictionary: Vec<String> = Vec::new();
    for row in 0..column.len() {
        match column.value_at(row) {
            Value::Null => {}
            Value::String(text) => dictionary.push(text),
            other => {
                return Err(super::super::invalid_argument(format!(
                    "encoding dictionary value {other} does not match {ty}"
                )));
            }
        }
    }
    dictionary.sort_unstable();
    dictionary.dedup();
    Ok(dictionary)
}

/// The values-section bytes: `u32 dict_count ‖ (dict_count + 1) u32 offsets
/// ‖ heap ‖ row_count u32 codes`.
fn write_section(column: &Column, dictionary: &[String]) -> DevonResult<Vec<u8>> {
    let dict_count = u32::try_from(dictionary.len()).map_err(|_| {
        super::super::invalid_argument("encoding dictionary entry count exceeds u32")
    })?;
    let heap_len = dictionary
        .iter()
        .fold(0_usize, |sum, entry| sum + entry.len());
    let heap_len_u32 = u32::try_from(heap_len).map_err(|_| {
        super::super::invalid_argument("encoding dictionary heap length exceeds u32")
    })?;
    let section_len = U32
        .saturating_add(U32.saturating_mul(dictionary.len() + 1))
        .saturating_add(heap_len)
        .saturating_add(U32.saturating_mul(column.len()));
    let mut bytes = Vec::with_capacity(section_len);
    bytes.extend_from_slice(&dict_count.to_le_bytes());
    let mut offset = 0_u32;
    bytes.extend_from_slice(&offset.to_le_bytes());
    for entry in dictionary {
        offset += u32::try_from(entry.len())
            .map_err(|_| super::super::invalid_argument("dictionary entry length exceeds u32"))?;
        bytes.extend_from_slice(&offset.to_le_bytes());
    }
    debug_assert_eq!(offset, heap_len_u32);
    for entry in dictionary {
        bytes.extend_from_slice(entry.as_bytes());
    }
    for row in 0..column.len() {
        let code = match column.value_at(row) {
            Value::Null => 0,
            Value::String(text) => dictionary
                .binary_search(&text)
                .map(|index| index as u32)
                .map_err(|_| {
                    super::super::invalid_argument(
                        "encoding dictionary lost an entry between passes",
                    )
                })?,
            other => {
                return Err(super::super::invalid_argument(format!(
                    "encoding dictionary value {other} does not match String"
                )));
            }
        };
        bytes.extend_from_slice(&code.to_le_bytes());
    }
    Ok(bytes)
}

/// Splits and length-validates the section into offsets, heap, and codes
/// regions. No allocation beyond the section length happens before every
/// length is proven against `row_count`.
fn split_section(bytes: &[u8], row_count: usize) -> DevonResult<(Vec<u32>, &[u8], &[u8])> {
    if bytes.len() < U32 {
        return Err(super::super::corrupt(format!(
            "dictionary section is {} bytes, smaller than the u32 dict_count 4",
            bytes.len()
        )));
    }
    let dict_count = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
    let offsets_len = U32
        .checked_mul(
            dict_count.checked_add(1).ok_or_else(|| {
                super::super::corrupt("dictionary offsets length overflows usize")
            })?,
        )
        .ok_or_else(|| super::super::corrupt("dictionary offsets length overflows usize"))?;
    let codes_len = U32.checked_mul(row_count).ok_or_else(|| {
        super::super::corrupt(format!(
            "dictionary codes length overflows usize for {row_count} rows"
        ))
    })?;
    let minimum = U32
        .checked_add(offsets_len)
        .and_then(|sum| sum.checked_add(codes_len))
        .ok_or_else(|| super::super::corrupt("dictionary section length overflows usize"))?;
    if bytes.len() < minimum {
        return Err(super::super::corrupt(format!(
            "dictionary section is {} bytes, too short for dict_count {dict_count} and {row_count} codes ({minimum} minimum)",
            bytes.len()
        )));
    }
    let heap_len = bytes.len() - minimum;
    let mut offsets = Vec::with_capacity(dict_count + 1);
    for chunk in bytes[U32..U32 + offsets_len].as_chunks::<U32>().0 {
        offsets.push(u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
    }
    let heap = &bytes[U32 + offsets_len..U32 + offsets_len + heap_len];
    let codes = &bytes[U32 + offsets_len + heap_len..];
    Ok((offsets, heap, codes))
}

/// Validates the offset law (`offsets[0]` = 0, non-decreasing, ending at the
/// heap length) and each entry (valid UTF-8, unique, strictly ascending).
fn validate_entries<'heap>(offsets: &[u32], heap: &'heap [u8]) -> DevonResult<Vec<&'heap str>> {
    if offsets.first().copied() != Some(0) {
        return Err(super::super::corrupt(format!(
            "dictionary offsets start at {}, expected 0",
            offsets.first().copied().unwrap_or(0)
        )));
    }
    for pair in offsets.windows(2) {
        if pair[1] < pair[0] {
            return Err(super::super::corrupt(
                "dictionary offsets are not non-decreasing",
            ));
        }
    }
    let heap_len = u32::try_from(heap.len())
        .map_err(|_| super::super::corrupt("dictionary heap length exceeds u32"))?;
    if offsets.last().copied() != Some(heap_len) {
        return Err(super::super::corrupt(format!(
            "dictionary offsets end at {} but the heap is {heap_len} bytes",
            offsets.last().copied().unwrap_or(0)
        )));
    }
    let mut entries: Vec<&str> = Vec::new();
    for (index, pair) in offsets.windows(2).enumerate() {
        let entry =
            std::str::from_utf8(&heap[pair[0] as usize..pair[1] as usize]).map_err(|error| {
                super::super::corrupt(format!(
                    "dictionary entry {index} is not valid UTF-8: {error}"
                ))
            })?;
        if let Some(previous) = entries.last()
            && entry <= *previous
        {
            return Err(super::super::corrupt(format!(
                "dictionary entries are not unique and sorted bytewise ascending at entry {index}"
            )));
        }
        entries.push(entry);
    }
    Ok(entries)
}

/// The materialized column: entry strings at valid rows, `Value::Null` at
/// NULL slots. Codes are range-checked against the dictionary; NULL rows
/// must carry code 0 (`docs/FORMAT.md` determinism law).
fn materialize(
    entries: Vec<&str>,
    codes: &[u8],
    validity: &Bitmap,
    row_count: usize,
) -> DevonResult<Column> {
    let mut values = Vec::with_capacity(row_count);
    for (row, chunk) in codes.as_chunks::<U32>().0.iter().enumerate() {
        let code = u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        if !validity.is_valid(row) {
            if code != 0 {
                return Err(super::super::corrupt(format!(
                    "dictionary code at NULL row {row} is {code}, expected 0"
                )));
            }
            values.push(Value::Null);
            continue;
        }
        let entry = entries.get(code as usize).ok_or_else(|| {
            super::super::corrupt(format!(
                "dictionary code {code} at row {row} is out of range for dict_count {}",
                entries.len()
            ))
        })?;
        values.push(Value::String((*entry).to_owned()));
    }
    Ok(Column::Boxed(values))
}

#[cfg(test)]
mod tests {
    use super::{PARAMS_V1, decode, encode};
    use devondb_types::column::{Bitmap, Column};
    use devondb_types::logical_type::LogicalType;
    use devondb_types::value::Value;
    use devondb_types::{DevonError, DevonResult};

    fn round_trip(rows: Vec<Value>) -> DevonResult<Column> {
        let column = Column::from_values(&LogicalType::String, rows);
        let (bytes, params) = encode(&column, &LogicalType::String)?;
        let mut validity = Bitmap::all_valid(column.len());
        for row in 0..column.len() {
            if matches!(column.value_at(row), Value::Null) {
                validity.clear(row);
            }
        }
        decode(
            params,
            &bytes,
            &validity,
            column.len(),
            &LogicalType::String,
        )
    }

    #[test]
    fn round_trip_covers_null_patterns_and_edge_strings() {
        let cases: Vec<Vec<Value>> = vec![
            vec![Value::String("b".to_owned()), Value::String("a".to_owned())],
            vec![Value::Null; 3],
            vec![Value::Null],
            vec![Value::String("only".to_owned())],
            vec![
                Value::String(String::new()),
                Value::Null,
                Value::String(String::new()),
            ],
            vec![
                Value::String("édith 🦀".to_owned()),
                Value::String("edith".to_owned()),
                Value::Null,
                Value::String("édith 🦀".to_owned()),
            ],
            vec![Value::String("same".to_owned()); 4],
        ];
        for rows in cases {
            let decoded = round_trip(rows.clone()).unwrap();
            assert_eq!(decoded, rows, "round trip diverged for {rows:?}");
        }
    }

    #[test]
    fn encode_sorts_entries_bytewise_and_codes_nulls_zero() {
        let rows = vec![
            Value::String("pear".to_owned()),
            Value::String("apple".to_owned()),
            Value::Null,
            Value::String("apple".to_owned()),
        ];
        let column = Column::from_values(&LogicalType::String, rows);
        let (bytes, params) = encode(&column, &LogicalType::String).unwrap();
        assert_eq!(params, PARAMS_V1);
        let expected: &[u8] = &[
            2, 0, 0, 0, // dict_count 2
            0, 0, 0, 0, 5, 0, 0, 0, 9, 0, 0, 0, // offsets 0, 5, 9
            b'a', b'p', b'p', b'l', b'e', b'p', b'e', b'a', b'r', // heap
            1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, // codes 1 0 0 0
        ];
        assert_eq!(bytes, expected);
    }

    #[test]
    fn inadmissible_types_are_refused() {
        let column = Column::from_values(&LogicalType::Int64, vec![Value::Int64(1)]);
        let error = encode(&column, &LogicalType::Int64).unwrap_err();
        assert!(
            matches!(error, DevonError::InvalidArgument { .. }),
            "expected InvalidArgument, got {error}"
        );
        let validity = Bitmap::all_valid(1);
        let error = decode(
            PARAMS_V1,
            &[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
            &validity,
            1,
            &LogicalType::Int64,
        )
        .unwrap_err();
        assert!(
            matches!(error, DevonError::Corrupt { .. }),
            "expected Corrupt, got {error}"
        );
    }

    #[test]
    fn corrupt_sections_are_refused_without_panicking() {
        let mut validity = Bitmap::all_valid(2);
        validity.clear(1);
        // dict_count 1, offsets 0/1, heap "a", codes 0 / 0.
        let good: &[u8] = &[
            1, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, b'a', 0, 0, 0, 0, 0, 0, 0, 0,
        ];
        let decoded = decode(PARAMS_V1, good, &validity, 2, &LogicalType::String).unwrap();
        assert_eq!(decoded, vec![Value::String("a".to_owned()), Value::Null]);
        let cases: Vec<([u8; 3], &[u8], usize)> = vec![
            ([4, 1, 0], good, 2),                    // nonzero p1
            ([1, 0, 0], good, 2),                    // code width 1
            ([4, 0, 0], &[1, 2, 3], 2),              // shorter than dict_count
            ([4, 0, 0], &good[..good.len() - 1], 2), // truncated code
            ([4, 0, 0], &good[..13], 2),             // offsets/heap truncated
        ];
        for (params, bytes, rows) in cases {
            let result = decode(params, bytes, &validity, rows, &LogicalType::String);
            assert!(
                matches!(result, Err(DevonError::Corrupt { .. })),
                "expected Corrupt for {bytes:?}"
            );
        }
    }
}
