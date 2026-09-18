//! ALP encoding (id 6): adaptive lossless floating-point compression
//! (Afroozeh & Boncz 2023) for `Float64` columns.
//!
//! Values are decimal-scaled to integers (`i = round(v × 10^e / 10^f)`)
//! and stored under FastLanes 1024-value transposed bit-packing with a
//! frame of reference; rows that do not round-trip bit-exactly are kept
//! raw in an exception table, so decoding reproduces every input
//! bit-exactly (NaN, ±inf, −0.0, subnormals are exceptions). The byte
//! layout is part of the on-disk format contract.
//!
//! FastLanes transposed block packing is implemented privately by
//! `pack_blocks` and `unpack_blocks`.

use devondb_types::DevonResult;
use devondb_types::column::{Bitmap, Column};
use devondb_types::logical_type::LogicalType;
use devondb_types::value::Value;

/// `u8 e · u8 f · 2 reserved · u32 exception_count`.
const HEADER_LEN: usize = 8;
/// `i64 reference · u8 bit_width · 3 reserved`.
const FOR_HEADER_LEN: usize = 12;
/// Slots per FastLanes block.
const BLOCK_VALUES: usize = 1024;
/// Slots per transposed lane word.
const LANE: usize = 64;
/// `{ u32 row, f64 bits }`.
const EXCEPTION_RECORD_LEN: usize = 12;
/// Writer search bound for `e`/`f`: 10^k is exact in f64 through k = 22,
/// so every emitted power is exact (`alp.md` § Writer law).
const MAX_EXPONENT: u8 = 18;

/// The f64 value of 10^k (`alp.md` § Encoding rule).
fn power(k: u8) -> f64 {
    10_f64.powi(i32::from(k))
}

/// The rounded scaled value fits `i64` exactly (covers NaN and ±inf).
fn fits_i64(rounded: f64) -> bool {
    (-9223372036854775808.0..9223372036854775808.0).contains(&rounded)
}

/// `round(v × P(e) / P(f))` as an integer, or `None` when the row is an
/// exception (no i64 fit, or the decode formula misses the input bits).
fn encode_row(value: f64, e: u8, f: u8) -> Option<i64> {
    let rounded = ((value * power(e)) / power(f)).round();
    if !fits_i64(rounded) {
        return None;
    }
    let integer = rounded as i64;
    let back = (integer as f64 / power(e)) * power(f);
    (back.to_bits() == value.to_bits()).then_some(integer)
}

/// The encoded integers, frame of reference, and exception table of one
/// column under `(e, f)`.
struct Encoded {
    /// Per-row delta `(i_r − reference) mod 2^64`; 0 at NULL and
    /// exception slots.
    deltas: Vec<u64>,
    reference: i64,
    bit_width: u8,
    /// `(row, exact f64 bits)` in increasing row order.
    exceptions: Vec<(u32, u64)>,
}

/// Encodes one column's values section as ALP, returning the section
/// bytes and the directory parameters `[e, f, 0]`. FIXED signature per
/// `docs/SCALE.md` §8.2.
pub(crate) fn encode(column: &Column, ty: &LogicalType) -> DevonResult<(Vec<u8>, [u8; 3])> {
    if !matches!(ty, LogicalType::Float64) {
        return Err(super::super::invalid_argument(format!(
            "encoding alp is not admissible for {ty}"
        )));
    }
    let values = collect_values(column)?;
    let (e, f) = select_parameters(&values);
    let encoded = encode_values_with(&values, e, f);
    let bytes = write_section(&encoded, e, f);
    Ok((bytes, [e, f, 0]))
}

/// The column as per-row `Option<f64>` (`None` at NULL slots). A value
/// the type does not admit is a writer-policy error, never corruption.
fn collect_values(column: &Column) -> DevonResult<Vec<Option<f64>>> {
    let mut values = Vec::with_capacity(column.len());
    for row in 0..column.len() {
        match column.value_at(row) {
            Value::Null => values.push(None),
            Value::Float64(value) => values.push(Some(value)),
            other => {
                return Err(super::super::invalid_argument(format!(
                    "encoding alp value {other} does not match Float64"
                )));
            }
        }
    }
    Ok(values)
}

/// The lexicographically smallest `(e, f)` with the fewest exceptions
/// (`alp.md` § Writer law): scan `e` then `f` ascending; a strictly lower
/// count replaces the incumbent; the first zero-exception pair wins.
fn select_parameters(values: &[Option<f64>]) -> (u8, u8) {
    let mut best = (0, 0);
    let mut best_exceptions = usize::MAX;
    for e in 0..=MAX_EXPONENT {
        for f in 0..=MAX_EXPONENT {
            let exceptions = count_exceptions(values, e, f);
            if exceptions < best_exceptions {
                best = (e, f);
                best_exceptions = exceptions;
            }
            if exceptions == 0 {
                return best;
            }
        }
    }
    best
}

fn count_exceptions(values: &[Option<f64>], e: u8, f: u8) -> usize {
    values
        .iter()
        .flatten()
        .filter(|value| encode_row(**value, e, f).is_none())
        .count()
}

/// Encodes every valid row under `(e, f)`, deriving the frame of
/// reference (minimum integer, or 0 when there are none) and the bit
/// width of the maximum delta.
fn encode_values_with(values: &[Option<f64>], e: u8, f: u8) -> Encoded {
    let mut integers = vec![None; values.len()];
    let mut exceptions = Vec::new();
    for (row, value) in values.iter().enumerate() {
        let Some(value) = value else { continue };
        match encode_row(*value, e, f) {
            Some(integer) => integers[row] = Some(integer),
            None => exceptions.push((row as u32, value.to_bits())),
        }
    }
    let reference = integers.iter().flatten().min().copied().unwrap_or(0);
    let deltas = integers
        .iter()
        .map(|integer| integer.map_or(0, |i| (i as u64).wrapping_sub(reference as u64)))
        .collect::<Vec<u64>>();
    let bit_width = deltas
        .iter()
        .max()
        .map_or(0, |max| (u64::BITS - max.leading_zeros()) as u8);
    Encoded {
        deltas,
        reference,
        bit_width,
        exceptions,
    }
}

/// Serializes the values section in the exact `alp.md` byte order.
fn write_section(encoded: &Encoded, e: u8, f: u8) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(section_len(
        encoded.deltas.len(),
        encoded.bit_width,
        encoded.exceptions.len(),
    ));
    bytes.push(e);
    bytes.push(f);
    bytes.extend_from_slice(&[0, 0]);
    bytes.extend_from_slice(&(encoded.exceptions.len() as u32).to_le_bytes());
    bytes.extend_from_slice(&encoded.reference.to_le_bytes());
    bytes.push(encoded.bit_width);
    bytes.extend_from_slice(&[0, 0, 0]);
    pack_blocks(&encoded.deltas, encoded.bit_width, &mut bytes);
    for (row, bits) in &encoded.exceptions {
        bytes.extend_from_slice(&row.to_le_bytes());
        bytes.extend_from_slice(&bits.to_le_bytes());
    }
    bytes
}

/// The exact section length law (`alp.md` § Values section).
fn section_len(row_count: usize, bit_width: u8, exception_count: usize) -> usize {
    HEADER_LEN
        + FOR_HEADER_LEN
        + usize::from(bit_width) * 8 * total_lanes(row_count)
        + exception_count * EXCEPTION_RECORD_LEN
}

/// Lane words over all blocks: 16 per full 1024-slot block,
/// `ceil(tail / 64)` for the partial tail block.
fn total_lanes(row_count: usize) -> usize {
    (row_count / BLOCK_VALUES) * (BLOCK_VALUES / LANE) + (row_count % BLOCK_VALUES).div_ceil(LANE)
}

/// FastLanes transposed packing: bit-plane-major, word `(b × lanes + k)`
/// holds bit `b` of slots `64k + j` at its bit `j`; absent tail slots
/// leave zero padding bits.
fn pack_blocks(deltas: &[u64], bit_width: u8, out: &mut Vec<u8>) {
    for block in deltas.chunks(BLOCK_VALUES) {
        let lanes = block.len().div_ceil(LANE);
        for plane in 0..bit_width {
            for lane in 0..lanes {
                let word = pack_word(block, plane, lane);
                out.extend_from_slice(&word.to_le_bytes());
            }
        }
    }
}

fn pack_word(block: &[u64], plane: u8, lane: usize) -> u64 {
    let mut word = 0_u64;
    for (j, delta) in block[lane * LANE..(lane * LANE + LANE).min(block.len())]
        .iter()
        .enumerate()
    {
        word |= ((delta >> plane) & 1) << j;
    }
    word
}

/// The parsed and validated section header.
struct Header {
    e: u8,
    f: u8,
    exception_count: usize,
    reference: i64,
    bit_width: u8,
}

/// Decodes a values section written by this encoding, materializing the
/// typed `Column::Float64` (NULL slots decode to 0.0 per
/// `docs/SCALE.md` §6.3). Every field is validated before use; failures
/// are `Corrupt` naming the region, never a panic. FIXED signature per
/// `docs/SCALE.md` §8.2.
pub(crate) fn decode(
    params: [u8; 3],
    bytes: &[u8],
    validity: &Bitmap,
    row_count: usize,
    ty: &LogicalType,
) -> DevonResult<Column> {
    if !matches!(ty, LogicalType::Float64) {
        return Err(super::super::corrupt(format!(
            "encoding alp is not admissible for {ty}"
        )));
    }
    let header = parse_header(params, bytes, row_count)?;
    let expected = checked_blocks_len(row_count, header.bit_width)?
        .checked_add(HEADER_LEN + FOR_HEADER_LEN)
        .and_then(|n| {
            header
                .exception_count
                .checked_mul(EXCEPTION_RECORD_LEN)
                .and_then(|table| n.checked_add(table))
        })
        .ok_or_else(|| super::super::corrupt("alp implied values-section length overflows"))?;
    if bytes.len() != expected {
        return Err(super::super::corrupt(format!(
            "alp values section is {} bytes, expected exactly {expected} for row_count {row_count}, bit_width {}, exception_count {}",
            bytes.len(),
            header.bit_width,
            header.exception_count
        )));
    }
    let blocks_len =
        expected - HEADER_LEN - FOR_HEADER_LEN - header.exception_count * EXCEPTION_RECORD_LEN;
    let blocks = &bytes[HEADER_LEN + FOR_HEADER_LEN..HEADER_LEN + FOR_HEADER_LEN + blocks_len];
    let deltas = unpack_blocks(blocks, row_count, header.bit_width)?;
    let exceptions = parse_exceptions(
        &bytes[HEADER_LEN + FOR_HEADER_LEN + blocks_len..],
        header.exception_count,
        validity,
        row_count,
    )?;
    Ok(materialize(
        &header,
        &deltas,
        &exceptions,
        validity,
        row_count,
    ))
}

/// Reads `N` little-endian bytes at `offset`; every call site has already
/// validated the region length, so the copy cannot fail.
fn take<const N: usize>(bytes: &[u8], offset: usize) -> [u8; N] {
    let mut slot = [0_u8; N];
    slot.copy_from_slice(&bytes[offset..offset + N]);
    slot
}

/// Validates the header and the directory parameters against it.
fn parse_header(params: [u8; 3], bytes: &[u8], row_count: usize) -> DevonResult<Header> {
    if bytes.len() < HEADER_LEN {
        return Err(super::super::corrupt(format!(
            "alp values section is {} bytes, shorter than the 8-byte header",
            bytes.len()
        )));
    }
    let (e, f) = (bytes[0], bytes[1]);
    if bytes[2] != 0 || bytes[3] != 0 {
        return Err(super::super::corrupt("alp reserved bytes are not zero"));
    }
    let exception_count = u32::from_le_bytes(take(bytes, 4)) as usize;
    if exception_count > row_count {
        return Err(super::super::corrupt(format!(
            "alp exception_count {exception_count} exceeds row_count {row_count}"
        )));
    }
    validate_params(params, e, f)?;
    parse_for_header(bytes, e, f, exception_count)
}

fn validate_params(params: [u8; 3], e: u8, f: u8) -> DevonResult<()> {
    if params[2] != 0 {
        return Err(super::super::corrupt(format!(
            "alp directory parameter p2 is {}, expected zero",
            params[2]
        )));
    }
    if params[0] != e {
        return Err(super::super::corrupt(format!(
            "alp directory exponent {} does not match the values-section header {e}",
            params[0]
        )));
    }
    if params[1] != f {
        return Err(super::super::corrupt(format!(
            "alp directory factor {} does not match the values-section header {f}",
            params[1]
        )));
    }
    Ok(())
}

fn parse_for_header(bytes: &[u8], e: u8, f: u8, exception_count: usize) -> DevonResult<Header> {
    if bytes.len() < HEADER_LEN + FOR_HEADER_LEN {
        return Err(super::super::corrupt(format!(
            "alp values section is {} bytes, shorter than the 20-byte frame-of-reference header",
            bytes.len()
        )));
    }
    let reference = i64::from_le_bytes(take(bytes, 8));
    let bit_width = bytes[16];
    if bit_width > 64 {
        return Err(super::super::corrupt(format!(
            "alp bit_width is {bit_width}, expected at most 64"
        )));
    }
    if bytes[17..20] != [0, 0, 0] {
        return Err(super::super::corrupt(
            "alp frame-of-reference reserved bytes are not zero",
        ));
    }
    Ok(Header {
        e,
        f,
        exception_count,
        reference,
        bit_width,
    })
}

/// The checked byte length of the delta-block region (`alp.md`'s exact
/// length law, computed before any allocation).
fn checked_blocks_len(row_count: usize, bit_width: u8) -> DevonResult<usize> {
    usize::from(bit_width)
        .checked_mul(8)
        .and_then(|width| width.checked_mul(total_lanes(row_count)))
        .ok_or_else(|| super::super::corrupt("alp implied values-section length overflows"))
}

/// The inverse of [`pack_blocks`]; the region's length is already exact,
/// so only the tail padding bits remain to validate.
fn unpack_blocks(bytes: &[u8], row_count: usize, bit_width: u8) -> DevonResult<Vec<u64>> {
    let mut deltas = vec![0_u64; row_count];
    let mut cursor = 0;
    for block_start in (0..row_count).step_by(BLOCK_VALUES) {
        let n = (row_count - block_start).min(BLOCK_VALUES);
        let lanes = n.div_ceil(LANE);
        for plane in 0..bit_width {
            for lane in 0..lanes {
                let word = u64::from_le_bytes(take(bytes, cursor));
                cursor += 8;
                validate_tail_padding(word, n, lane, lanes)?;
                apply_word(&mut deltas, block_start, n, plane, lane, word);
            }
        }
    }
    Ok(deltas)
}

/// Bits past the tail block's last slot MUST be zero (`alp.md`).
fn validate_tail_padding(word: u64, n: usize, lane: usize, lanes: usize) -> DevonResult<()> {
    if lane == lanes - 1 && !n.is_multiple_of(LANE) && (word >> (n % LANE)) != 0 {
        return Err(super::super::corrupt(
            "alp tail block padding bits are not zero",
        ));
    }
    Ok(())
}

fn apply_word(deltas: &mut [u64], block_start: usize, n: usize, plane: u8, lane: usize, word: u64) {
    for j in 0..LANE.min(n - lane * LANE) {
        deltas[block_start + lane * LANE + j] |= ((word >> j) & 1) << plane;
    }
}

/// The exception table, validated against the row space and the validity
/// bitmap (`alp.md`: strictly increasing, in bounds, valid rows only).
fn parse_exceptions(
    bytes: &[u8],
    count: usize,
    validity: &Bitmap,
    row_count: usize,
) -> DevonResult<Vec<(usize, u64)>> {
    let mut exceptions = Vec::with_capacity(count);
    let mut previous: Option<usize> = None;
    for index in 0..count {
        let record = index * EXCEPTION_RECORD_LEN;
        let row = u32::from_le_bytes(take(bytes, record)) as usize;
        if row >= row_count {
            return Err(super::super::corrupt(format!(
                "alp exception row {row} is out of bounds for row_count {row_count}"
            )));
        }
        if previous.is_some_and(|prev| row <= prev) {
            return Err(super::super::corrupt(
                "alp exception rows are not strictly increasing",
            ));
        }
        if !validity.is_valid(row) {
            return Err(super::super::corrupt(format!(
                "alp exception row {row} is a NULL row"
            )));
        }
        previous = Some(row);
        exceptions.push((row, u64::from_le_bytes(take(bytes, record + 4))));
    }
    Ok(exceptions)
}

/// Reconstructs the typed values: `v = (i / P(e)) × P(f)` for valid
/// non-exception rows, the raw bits at exception rows, 0.0 at NULL slots.
fn materialize(
    header: &Header,
    deltas: &[u64],
    exceptions: &[(usize, u64)],
    validity: &Bitmap,
    row_count: usize,
) -> Column {
    let (pe, pf) = (power(header.e), power(header.f));
    let mut values = vec![0.0; row_count];
    for row in 0..row_count {
        if validity.is_valid(row) {
            let integer = (header.reference as u64).wrapping_add(deltas[row]) as i64;
            values[row] = (integer as f64 / pe) * pf;
        }
    }
    for (row, bits) in exceptions {
        values[*row] = f64::from_bits(*bits);
    }
    let has_null = (0..row_count).any(|row| !validity.is_valid(row));
    Column::Float64 {
        values,
        validity: has_null.then(|| validity.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::{decode, encode};
    use devondb_types::column::{Bitmap, Column};
    use devondb_types::logical_type::LogicalType;
    use devondb_types::value::Value;
    use devondb_types::{DevonError, DevonResult};

    fn round_trip(rows: Vec<Value>) -> DevonResult<Column> {
        let column = Column::from_values(&LogicalType::Float64, rows);
        let (bytes, params) = encode(&column, &LogicalType::Float64)?;
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
            &LogicalType::Float64,
        )
    }

    fn assert_bit_exact(rows: &[Value], decoded: &Column) {
        let Column::Float64 { values, .. } = decoded else {
            panic!("expected typed Float64 storage");
        };
        assert_eq!(values.len(), rows.len());
        for (row, expected) in rows.iter().enumerate() {
            match expected {
                Value::Null => assert_eq!(values[row], 0.0, "NULL slot {row} must be zero"),
                Value::Float64(value) => assert_eq!(
                    values[row].to_bits(),
                    value.to_bits(),
                    "row {row} diverged from the input bits"
                ),
                other => panic!("unexpected value {other}"),
            }
        }
    }

    #[test]
    fn round_trip_is_bit_exact_over_hard_floats() {
        let rows = vec![
            Value::Float64(1.5),
            Value::Float64(2.25),
            Value::Null,
            Value::Float64(-7.125),
            Value::Float64(-0.0),
            Value::Float64(f64::NAN),
            Value::Float64(f64::INFINITY),
            Value::Float64(f64::NEG_INFINITY),
            Value::Float64(f64::MIN),
            Value::Float64(f64::MAX),
            Value::Float64(f64::from_bits(1)),
            Value::Float64(1e300),
            Value::Float64(0.1),
        ];
        let decoded = round_trip(rows.clone()).unwrap();
        assert_bit_exact(&rows, &decoded);
    }

    #[test]
    fn all_null_and_single_row_columns_round_trip() {
        for rows in [
            vec![Value::Null; 5],
            vec![Value::Float64(42.5)],
            vec![Value::Null],
        ] {
            let decoded = round_trip(rows.clone()).unwrap();
            assert_bit_exact(&rows, &decoded);
        }
    }

    #[test]
    fn exceptions_keep_the_exact_bits() {
        let rows = vec![
            Value::Float64(0.5),
            Value::Float64(f64::NAN),
            Value::Float64(-0.0),
            Value::Float64(1.25),
        ];
        let column = Column::from_values(&LogicalType::Float64, rows.clone());
        let (bytes, params) = encode(&column, &LogicalType::Float64).unwrap();
        assert_eq!(params[2], 0);
        // NaN and −0.0 are exceptions: rows 1 and 2, increasing.
        let count = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
        assert_eq!(count, 2);
        let table = &bytes[bytes.len() - 24..];
        assert_eq!(u32::from_le_bytes(table[0..4].try_into().unwrap()), 1);
        assert_eq!(&table[4..12], f64::NAN.to_bits().to_le_bytes());
        assert_eq!(u32::from_le_bytes(table[12..16].try_into().unwrap()), 2);
        assert_eq!(&table[16..24], (-0.0_f64).to_bits().to_le_bytes());
        let validity = Bitmap::all_valid(4);
        let decoded = decode(params, &bytes, &validity, 4, &LogicalType::Float64).unwrap();
        assert_bit_exact(&rows, &decoded);
    }

    #[test]
    fn inadmissible_types_are_refused() {
        let column = Column::from_values(&LogicalType::Int64, vec![Value::Int64(1)]);
        let error = encode(&column, &LogicalType::Int64).unwrap_err();
        let DevonError::InvalidArgument { context } = error else {
            panic!("expected InvalidArgument, got {error}");
        };
        assert_eq!(context, "encoding alp is not admissible for Int64");

        let validity = Bitmap::all_valid(1);
        let error = decode([0; 3], &[], &validity, 1, &LogicalType::Int64).unwrap_err();
        let DevonError::Corrupt { context } = error else {
            panic!("expected Corrupt, got {error}");
        };
        assert_eq!(context, "encoding alp is not admissible for Int64");
    }

    #[test]
    fn corrupt_sections_are_refused_with_exact_messages() {
        let validity = Bitmap::all_valid(4);
        let column = Column::from_values(
            &LogicalType::Float64,
            vec![
                Value::Float64(1.01),
                Value::Float64(1.02),
                Value::Float64(1.03),
                Value::Float64(1.04),
            ],
        );
        let (bytes, params) = encode(&column, &LogicalType::Float64).unwrap();
        let ty = LogicalType::Float64;
        let corrupt = |params: [u8; 3], bytes: &[u8]| -> String {
            match decode(params, bytes, &validity, 4, &ty) {
                Err(DevonError::Corrupt { context }) => context,
                other => panic!("expected Corrupt, got {other:?}"),
            }
        };

        assert_eq!(
            corrupt(params, &bytes[..7]),
            "alp values section is 7 bytes, shorter than the 8-byte header"
        );
        let mut reserved = bytes.clone();
        reserved[2] = 1;
        assert_eq!(
            corrupt(params, &reserved),
            "alp reserved bytes are not zero"
        );
        let mut count = bytes.clone();
        count[4] = 5;
        assert_eq!(
            corrupt(params, &count),
            "alp exception_count 5 exceeds row_count 4"
        );
        assert_eq!(
            corrupt([params[0], params[1], 1], &bytes),
            "alp directory parameter p2 is 1, expected zero"
        );
        assert_eq!(
            corrupt([params[0] + 1, params[1], 0], &bytes),
            format!(
                "alp directory exponent {} does not match the values-section header {}",
                params[0] + 1,
                params[0]
            )
        );
        assert_eq!(
            corrupt([params[0], params[1] + 1, 0], &bytes),
            format!(
                "alp directory factor {} does not match the values-section header {}",
                params[1] + 1,
                params[1]
            )
        );
        assert_eq!(
            corrupt(params, &bytes[..19]),
            "alp values section is 19 bytes, shorter than the 20-byte frame-of-reference header"
        );
        let mut wide = bytes.clone();
        wide[16] = 65;
        assert_eq!(
            corrupt(params, &wide),
            "alp bit_width is 65, expected at most 64"
        );
        let mut for_reserved = bytes.clone();
        for_reserved[17] = 1;
        assert_eq!(
            corrupt(params, &for_reserved),
            "alp frame-of-reference reserved bytes are not zero"
        );
        assert_eq!(
            corrupt(params, &bytes[..bytes.len() - 1]),
            format!(
                "alp values section is {} bytes, expected exactly {} for row_count 4, bit_width {}, exception_count 0",
                bytes.len() - 1,
                bytes.len(),
                bytes[16]
            )
        );
        let mut padding = bytes;
        *padding.last_mut().unwrap() |= 0b1111_0000;
        assert_eq!(
            corrupt(params, &padding),
            "alp tail block padding bits are not zero"
        );
    }

    #[test]
    fn exception_table_validation_names_the_region() {
        let validity = Bitmap::all_valid(4);
        let ty = LogicalType::Float64;
        // Header for e=1 f=0, two exceptions at rows {1, 1} (not
        // increasing): w=0 so no block bytes.
        let base = {
            let column = Column::from_values(
                &ty,
                vec![
                    Value::Float64(0.5),
                    Value::Float64(f64::NAN),
                    Value::Float64(f64::NAN),
                    Value::Float64(0.5),
                ],
            );
            encode(&column, &ty).unwrap().0
        };
        let corrupt = |bytes: &[u8], validity: &Bitmap| -> String {
            match decode([1, 0, 0], bytes, validity, 4, &ty) {
                Err(DevonError::Corrupt { context }) => context,
                other => panic!("expected Corrupt, got {other:?}"),
            }
        };

        // Out-of-bounds exception row.
        let mut oob = base.clone();
        let at = oob.len() - 12;
        oob[at..at + 4].copy_from_slice(&4_u32.to_le_bytes());
        assert_eq!(
            corrupt(&oob, &validity),
            "alp exception row 4 is out of bounds for row_count 4"
        );

        // Not strictly increasing: repeat the first row.
        let mut dup = base.clone();
        let at = dup.len() - 12;
        let first_row = dup[at - 12..at - 8].to_vec();
        dup[at..at + 4].copy_from_slice(&first_row);
        assert_eq!(
            corrupt(&dup, &validity),
            "alp exception rows are not strictly increasing"
        );

        // Exception on a NULL row.
        let mut null_validity = Bitmap::all_valid(4);
        null_validity.clear(1);
        assert_eq!(
            corrupt(&base, &null_validity),
            "alp exception row 1 is a NULL row"
        );
    }
}
