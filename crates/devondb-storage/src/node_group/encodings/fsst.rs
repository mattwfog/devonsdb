//! FSST string compression (id 5): a static symbol table of up to 255
//! symbols of length 1–8 bytes; code byte 255 is the escape (next byte
//! literal). Table construction is a compact form of the paper's
//! iterative algorithm (Boncz/Neumann/Leis, VLDB 2020).
//!
//! Its byte layout is part of the on-disk format contract.

use std::collections::HashMap;

use devondb_types::DevonResult;
use devondb_types::column::{Bitmap, Column};
use devondb_types::logical_type::LogicalType;
use devondb_types::value::Value;

/// The escape code: the next heap byte is a literal output byte.
const ESCAPE: u8 = 255;
/// The table holds at most 255 symbols; code 255 is the escape.
const MAX_SYMBOLS: usize = 255;
/// Symbols are 1..=8 bytes.
const MAX_SYMBOL_LEN: usize = 8;
/// Table-construction iterations (documented in the layout doc).
const ITERATIONS: usize = 5;
/// Sample cap in total string bytes; larger columns are strided
/// (documented in the layout doc).
const SAMPLE_CAP_BYTES: usize = 1 << 16;

/// A built symbol table: `symbols[code]` is the byte string code `code`
/// expands to.
struct SymbolTable {
    symbols: Vec<Vec<u8>>,
}

/// Encodes one `String` column's values section as FSST (`docs/format/
/// encodings/fsst.md`). NULL rows are validity-only: they carry a
/// zero-length code stream. Parameters are unused and returned zero.
pub(crate) fn encode(column: &Column, ty: &LogicalType) -> DevonResult<(Vec<u8>, [u8; 3])> {
    let mut strings: Vec<Option<Vec<u8>>> = Vec::with_capacity(column.len());
    for row in 0..column.len() {
        match column.value_at(row) {
            Value::Null => strings.push(None),
            Value::String(text) => strings.push(Some(text.into_bytes())),
            other => {
                return Err(super::super::invalid_argument(format!(
                    "encoding fsst value {other} does not match {ty}"
                )));
            }
        }
    }
    let valid: Vec<&[u8]> = strings.iter().filter_map(|slot| slot.as_deref()).collect();
    let table = SymbolTable::build(&valid);
    let index = table.first_byte_index();

    let mut section = Vec::new();
    section.push(table.symbols.len() as u8);
    for symbol in &table.symbols {
        section.push(symbol.len() as u8);
        section.extend_from_slice(symbol);
    }
    let offsets_at = section.len();
    section.resize(offsets_at + 4 * (strings.len() + 1), 0);
    let mut offsets = Vec::with_capacity(strings.len() + 1);
    offsets.push(0_u32);
    for slot in &strings {
        if let Some(bytes) = slot {
            encode_bytes(&table, &index, bytes, &mut section);
        }
        let end =
            u32::try_from(section.len() - offsets_at - 4 * (strings.len() + 1)).map_err(|_| {
                super::super::invalid_argument("encoding fsst compressed heap length exceeds u32")
            })?;
        offsets.push(end);
    }
    for (index, offset) in offsets.iter().enumerate() {
        section[offsets_at + 4 * index..offsets_at + 4 * index + 4]
            .copy_from_slice(&offset.to_le_bytes());
    }
    Ok((section, [0; 3]))
}

/// Decodes an FSST values section, returning `Corrupt` on any field
/// violation and never panicking. Output is `Column::Boxed` with
/// `Value::String` at valid rows and `Value::Null` at NULL slots
/// (`docs/SCALE.md` §6.3).
pub(crate) fn decode(
    params: [u8; 3],
    bytes: &[u8],
    validity: &Bitmap,
    row_count: usize,
    ty: &LogicalType,
) -> DevonResult<Column> {
    if params != [0; 3] {
        return Err(super::super::corrupt(
            "encoding fsst parameters are not zero",
        ));
    }
    if *ty != LogicalType::String {
        return Err(super::super::corrupt(format!(
            "encoding fsst is not admissible for {ty}"
        )));
    }
    let (symbols, rest) = parse_symbol_table(bytes)?;
    let (offsets, heap) = parse_offsets(rest, row_count)?;
    let mut values = Vec::with_capacity(row_count);
    for row in 0..row_count {
        let start = offsets[row] as usize;
        let end = offsets[row + 1] as usize;
        if !validity.is_valid(row) {
            if start != end {
                return Err(super::super::corrupt(format!(
                    "fsst NULL row {row} has a nonempty code stream"
                )));
            }
            values.push(Value::Null);
            continue;
        }
        values.push(Value::String(decode_row(
            &symbols,
            &heap[start..end],
            start,
            row,
        )?));
    }
    Ok(Column::Boxed(values))
}

/// Parses the `u8 symbol_count` prefix and the symbol table records,
/// returning borrowed symbol slices and the remaining section.
fn parse_symbol_table(bytes: &[u8]) -> DevonResult<(Vec<&[u8]>, &[u8])> {
    let Some((&symbol_count, mut rest)) = bytes.split_first() else {
        return Err(super::super::corrupt(
            "fsst values section is empty, expected at least the symbol count",
        ));
    };
    let mut symbols = Vec::with_capacity(symbol_count as usize);
    for index in 0..symbol_count as usize {
        let Some((&len, tail)) = rest.split_first() else {
            return Err(super::super::corrupt(format!(
                "fsst symbol table is truncated at symbol {index}"
            )));
        };
        if !(1..=MAX_SYMBOL_LEN).contains(&(len as usize)) {
            return Err(super::super::corrupt(format!(
                "fsst symbol {index} length is {len}, expected 1..={MAX_SYMBOL_LEN}"
            )));
        }
        if tail.len() < len as usize {
            return Err(super::super::corrupt(format!(
                "fsst symbol {index} is truncated: {len} bytes declared, {} remain",
                tail.len()
            )));
        }
        symbols.push(&tail[..len as usize]);
        rest = &tail[len as usize..];
    }
    Ok((symbols, rest))
}

/// Parses the `(row_count + 1)` × u32 offsets, enforcing the start-at-0,
/// non-decreasing, ends-at-heap-length law.
fn parse_offsets(bytes: &[u8], row_count: usize) -> DevonResult<(Vec<u32>, &[u8])> {
    let needed = 4 * (row_count + 1);
    if bytes.len() < needed {
        return Err(super::super::corrupt(format!(
            "fsst offsets section is {} bytes, expected {needed} for {row_count} rows",
            bytes.len()
        )));
    }
    let (offset_bytes, heap) = bytes.split_at(needed);
    let mut offsets = Vec::with_capacity(row_count + 1);
    for chunk in offset_bytes.as_chunks::<4>().0 {
        offsets.push(u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
    }
    if offsets[0] != 0 {
        return Err(super::super::corrupt(format!(
            "fsst offsets start at {}, expected 0",
            offsets[0]
        )));
    }
    for row in 0..row_count {
        if offsets[row] > offsets[row + 1] {
            return Err(super::super::corrupt(format!(
                "fsst offsets decrease at row {row}"
            )));
        }
    }
    if offsets[row_count] as usize != heap.len() {
        return Err(super::super::corrupt(format!(
            "fsst offsets end at {} but the heap is {} bytes",
            offsets[row_count],
            heap.len()
        )));
    }
    Ok((offsets, heap))
}

/// Decodes one row's code stream. `heap_offset` is the stream's absolute
/// heap position, used in corruption messages.
fn decode_row(
    symbols: &[&[u8]],
    stream: &[u8],
    heap_offset: usize,
    row: usize,
) -> DevonResult<String> {
    // A code expands to at most MAX_SYMBOL_LEN bytes, so the output is
    // bounded by the (already length-validated) stream.
    let mut decoded = Vec::with_capacity(stream.len() * MAX_SYMBOL_LEN);
    let mut position = 0;
    while position < stream.len() {
        let code = stream[position];
        if code == ESCAPE {
            if position + 1 == stream.len() {
                return Err(super::super::corrupt(format!(
                    "fsst escape at heap offset {} has no literal byte",
                    heap_offset + position
                )));
            }
            decoded.push(stream[position + 1]);
            position += 2;
        } else if (code as usize) < symbols.len() {
            decoded.extend_from_slice(symbols[code as usize]);
            position += 1;
        } else {
            return Err(super::super::corrupt(format!(
                "fsst heap byte at offset {} is code {code}, but the symbol table has {} symbols",
                heap_offset + position,
                symbols.len()
            )));
        }
    }
    String::from_utf8(decoded).map_err(|error| {
        super::super::corrupt(format!(
            "fsst decoded row {row} is not valid UTF-8: {error}"
        ))
    })
}

impl SymbolTable {
    /// Builds the table deterministically from sampled input, a single-byte
    /// seed, and five gain-ranked iterations.
    fn build(valid: &[&[u8]]) -> Self {
        let sample = sample_strings(valid);
        let mut table = Self {
            symbols: seed_symbols(&sample),
        };
        for _ in 0..ITERATIONS {
            table = table.refine(&sample);
        }
        table
    }

    /// One construction iteration: walk the sample with the current
    /// table (greedy longest match; an uncovered byte counts as its own
    /// 1-byte candidate), counting symbol and adjacent-pair usage, then
    /// keep the 255 highest-gain candidates (`occurrences × byte
    /// length`, ties by symbol bytes ascending).
    fn refine(&self, sample: &[&[u8]]) -> Self {
        let index = self.first_byte_index();
        let mut counts: HashMap<Vec<u8>, u64> = HashMap::new();
        for bytes in sample {
            let mut position = 0;
            let mut previous: Option<Vec<u8>> = None;
            while position < bytes.len() {
                let symbol = match self.longest_match(&index, &bytes[position..]) {
                    Some(code) => self.symbols[code as usize].clone(),
                    None => vec![bytes[position]],
                };
                *counts.entry(symbol.clone()).or_insert(0) += 1;
                if let Some(pair) = previous
                    && pair.len() + symbol.len() <= MAX_SYMBOL_LEN
                {
                    let mut joined = pair;
                    joined.extend_from_slice(&symbol);
                    *counts.entry(joined).or_insert(0) += 1;
                }
                previous = Some(symbol.clone());
                position += symbol.len();
            }
        }
        let mut candidates: Vec<(Vec<u8>, u64)> = counts.into_iter().collect();
        candidates.sort_by(|(left_bytes, left_count), (right_bytes, right_count)| {
            (right_count * right_bytes.len() as u64)
                .cmp(&(left_count * left_bytes.len() as u64))
                .then_with(|| left_bytes.cmp(right_bytes))
        });
        candidates.truncate(MAX_SYMBOLS);
        Self {
            symbols: candidates.into_iter().map(|(bytes, _)| bytes).collect(),
        }
    }

    /// First-byte lookup: `index[byte]` lists the codes of symbols
    /// starting with `byte`, longest symbol first, so greedy encoding is
    /// a linear scan of a short list.
    fn first_byte_index(&self) -> [Vec<u8>; 256] {
        let mut index: [Vec<u8>; 256] = std::array::from_fn(|_| Vec::new());
        for (code, symbol) in self.symbols.iter().enumerate() {
            index[symbol[0] as usize].push(code as u8);
        }
        for codes in &mut index {
            codes.sort_by_key(|&code| std::cmp::Reverse(self.symbols[code as usize].len()));
        }
        index
    }

    /// The code of the longest symbol that prefixes `bytes`, if any.
    fn longest_match(&self, index: &[Vec<u8>; 256], bytes: &[u8]) -> Option<u8> {
        index[bytes[0] as usize]
            .iter()
            .find(|&&code| bytes.starts_with(&self.symbols[code as usize]))
            .copied()
    }
}

/// The deterministic sample: every valid string up to a total of
/// `SAMPLE_CAP_BYTES`, else every `k`-th with
/// `k = ceil(total / SAMPLE_CAP_BYTES)`.
fn sample_strings<'a>(valid: &[&'a [u8]]) -> Vec<&'a [u8]> {
    let total: usize = valid.iter().map(|bytes| bytes.len()).sum();
    if total <= SAMPLE_CAP_BYTES {
        return valid.to_vec();
    }
    let stride = total.div_ceil(SAMPLE_CAP_BYTES);
    valid.iter().step_by(stride).copied().collect()
}

/// The seed table: the sample's distinct single bytes, most frequent
/// first (ties by byte value ascending), at most 255.
fn seed_symbols(sample: &[&[u8]]) -> Vec<Vec<u8>> {
    let mut counts = [0_u64; 256];
    for bytes in sample {
        for &byte in *bytes {
            counts[byte as usize] += 1;
        }
    }
    let mut seeded: Vec<u8> = (0..=255)
        .filter(|&byte| counts[byte as usize] > 0)
        .collect();
    seeded.sort_by_key(|&byte| std::cmp::Reverse(counts[byte as usize]));
    seeded.truncate(MAX_SYMBOLS);
    seeded.into_iter().map(|byte| vec![byte]).collect()
}

/// Greedy longest-match encoding of one string, appending codes (and
/// escape literals) to `out`.
fn encode_bytes(table: &SymbolTable, index: &[Vec<u8>; 256], bytes: &[u8], out: &mut Vec<u8>) {
    let mut position = 0;
    while position < bytes.len() {
        match table.longest_match(index, &bytes[position..]) {
            Some(code) => {
                out.push(code);
                position += table.symbols[code as usize].len();
            }
            None => {
                out.push(ESCAPE);
                out.push(bytes[position]);
                position += 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{MAX_SYMBOLS, decode, encode};
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

    fn corrupt_context(error: &DevonError) -> &str {
        match error {
            DevonError::Corrupt { context } => context,
            other => panic!("expected Corrupt, got {other}"),
        }
    }

    #[test]
    fn round_trip_covers_null_and_edge_patterns() {
        let repetitive = "devondb-fsst-symbol-table".to_owned();
        let cases: Vec<Vec<Value>> = vec![
            vec![Value::String("only".to_owned())],
            vec![Value::Null],
            vec![Value::String(String::new())],
            vec![Value::Null; 4],
            vec![
                Value::String(repetitive.clone()),
                Value::Null,
                Value::String(repetitive.clone()),
                Value::String(String::new()),
                Value::String(repetitive),
            ],
            vec![Value::String("édith 🦀 − ünïcode".to_owned()); 3],
            vec![
                Value::String("a".to_owned()),
                Value::String("12345678".to_owned()),
                Value::String("123456789".to_owned()),
                Value::String("x".repeat(1000)),
            ],
        ];
        for rows in cases {
            let decoded = round_trip(rows.clone()).unwrap();
            assert_eq!(decoded, rows, "round trip diverged");
        }
    }

    #[test]
    fn encoding_is_deterministic() {
        let rows: Vec<Value> = (0..64)
            .map(|index| Value::String(format!("user-{index}@example.devondb, role engineer")))
            .collect();
        let column = Column::from_values(&LogicalType::String, rows);
        let (first, first_params) = encode(&column, &LogicalType::String).unwrap();
        let (second, second_params) = encode(&column, &LogicalType::String).unwrap();
        assert_eq!(first, second, "identical input produced different bytes");
        assert_eq!(first_params, second_params);
    }

    #[test]
    fn repetitive_strings_compress_below_plain() {
        let rows: Vec<Value> = (0..256)
            .map(|index| {
                Value::String(format!(
                    "https://www.example.devondb/users/{index}/profile?tab=settings"
                ))
            })
            .collect();
        let column = Column::from_values(&LogicalType::String, rows);
        let plain: usize = (0..column.len())
            .map(|row| match column.value_at(row) {
                Value::String(text) => text.len(),
                _ => 0,
            })
            .sum();
        let (bytes, _) = encode(&column, &LogicalType::String).unwrap();
        assert!(
            bytes.len() < plain,
            "fsst section {} bytes, plain heap {plain} bytes",
            bytes.len()
        );
    }

    #[test]
    fn table_stays_within_the_symbol_cap() {
        // More than 255 distinct bytes never occur in UTF-8, but long
        // strings with many distinct pairs exercise the truncation law.
        let rows: Vec<Value> = (0..512)
            .map(|index| Value::String(format!("row-{index:04}-{}", "abcdefgh".repeat(4))))
            .collect();
        let column = Column::from_values(&LogicalType::String, rows);
        let (bytes, params) = encode(&column, &LogicalType::String).unwrap();
        assert!((bytes[0] as usize) <= MAX_SYMBOLS);
        let validity = Bitmap::all_valid(column.len());
        let decoded = decode(
            params,
            &bytes,
            &validity,
            column.len(),
            &LogicalType::String,
        )
        .unwrap();
        assert_eq!(decoded, column);
    }

    #[test]
    fn corrupt_sections_report_the_region() {
        let rows = vec![
            Value::String("fsst-corruption-matrix".to_owned()),
            Value::Null,
            Value::String("fsst-corruption-matrix".to_owned()),
        ];
        let column = Column::from_values(&LogicalType::String, rows);
        let (bytes, params) = encode(&column, &LogicalType::String).unwrap();
        let mut validity = Bitmap::all_valid(3);
        validity.clear(1);
        let string = LogicalType::String;
        let decode_with = |section: &[u8]| decode(params, section, &validity, 3, &string);

        let error = decode_with(&[]).unwrap_err();
        assert_eq!(
            corrupt_context(&error),
            "fsst values section is empty, expected at least the symbol count"
        );

        let mut bad_len = bytes.clone();
        bad_len[1] = 0;
        let error = decode_with(&bad_len).unwrap_err();
        assert_eq!(
            corrupt_context(&error),
            "fsst symbol 0 length is 0, expected 1..=8"
        );

        let mut big_len = bytes.clone();
        big_len[1] = 9;
        let error = decode_with(&big_len).unwrap_err();
        assert_eq!(
            corrupt_context(&error),
            "fsst symbol 0 length is 9, expected 1..=8"
        );

        let error = decode_with(&bytes[..1]).unwrap_err();
        assert_eq!(
            corrupt_context(&error),
            "fsst symbol table is truncated at symbol 0"
        );

        let params_error = decode([1, 0, 0], &bytes, &validity, 3, &string).unwrap_err();
        assert_eq!(
            corrupt_context(&params_error),
            "encoding fsst parameters are not zero"
        );

        let wrong_type = decode(params, &bytes, &validity, 3, &LogicalType::Int64).unwrap_err();
        assert_eq!(
            corrupt_context(&wrong_type),
            "encoding fsst is not admissible for Int64"
        );
    }

    #[test]
    fn corrupt_offsets_and_heap_codes_are_refused() {
        let rows = vec![
            Value::String("heap-heap-heap".to_owned()),
            Value::String("heap-heap-heap".to_owned()),
        ];
        let column = Column::from_values(&LogicalType::String, rows);
        let (bytes, params) = encode(&column, &LogicalType::String).unwrap();
        let validity = Bitmap::all_valid(2);
        let string = LogicalType::String;
        let symbol_count = bytes[0] as usize;
        let mut cursor = 1;
        for _ in 0..symbol_count {
            cursor += 1 + bytes[cursor] as usize;
        }
        let offsets_at = cursor;
        let heap_end =
            u32::from_le_bytes(bytes[offsets_at + 8..offsets_at + 12].try_into().unwrap()) as usize;

        // Nonzero first offset.
        let mut bad = bytes.clone();
        bad[offsets_at] = 1;
        let error = decode(params, &bad, &validity, 2, &string).unwrap_err();
        assert_eq!(
            corrupt_context(&error),
            "fsst offsets start at 1, expected 0"
        );

        // Decreasing offsets: zero the final offset so offsets[1] >
        // offsets[2] (the start-at-0 and end-at-heap checks stay happy).
        let mut bad = bytes.clone();
        bad[offsets_at + 8..offsets_at + 12].copy_from_slice(&0_u32.to_le_bytes());
        let error = decode(params, &bad, &validity, 2, &string).unwrap_err();
        assert_eq!(corrupt_context(&error), "fsst offsets decrease at row 1");

        // Last offset disagrees with the heap length.
        let mut bad = bytes.clone();
        bad[offsets_at + 8..offsets_at + 12]
            .copy_from_slice(&((heap_end as u32) + 1).to_le_bytes());
        let error = decode(params, &bad, &validity, 2, &string).unwrap_err();
        assert_eq!(
            corrupt_context(&error),
            format!(
                "fsst offsets end at {} but the heap is {heap_end} bytes",
                heap_end + 1
            )
        );

        // A code past the symbol table (not the escape): one symbol "a",
        // offsets [0, 1], heap [code 1].
        let bad = [1, 1, b'a', 0, 0, 0, 0, 1, 0, 0, 0, 1];
        let one_row = Bitmap::all_valid(1);
        let error = decode(params, &bad, &one_row, 1, &string).unwrap_err();
        assert_eq!(
            corrupt_context(&error),
            "fsst heap byte at offset 0 is code 1, but the symbol table has 1 symbols"
        );

        // An escape as the row's last code byte: heap [escape].
        let bad = [1, 1, b'a', 0, 0, 0, 0, 1, 0, 0, 0, 255];
        let error = decode(params, &bad, &one_row, 1, &string).unwrap_err();
        assert_eq!(
            corrupt_context(&error),
            "fsst escape at heap offset 0 has no literal byte"
        );

        // A NULL row with a nonempty code stream.
        let mut null_validity = Bitmap::all_valid(2);
        null_validity.clear(1);
        let error = decode(params, &bytes, &null_validity, 2, &string).unwrap_err();
        assert_eq!(
            corrupt_context(&error),
            "fsst NULL row 1 has a nonempty code stream"
        );

        // Invalid UTF-8: one symbol "a", offsets [0, 2], heap
        // [escape, 0xff] decodes to a lone 0xff literal.
        let bad = [1, 1, b'a', 0, 0, 0, 0, 2, 0, 0, 0, 255, 0xff];
        let error = decode(params, &bad, &one_row, 1, &string).unwrap_err();
        assert!(
            corrupt_context(&error).starts_with("fsst decoded row 0 is not valid UTF-8"),
            "{}",
            corrupt_context(&error)
        );
    }
}
