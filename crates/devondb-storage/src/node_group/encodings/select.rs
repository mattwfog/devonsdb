//! Adaptive per-payload encoding selection at group write (`docs/SCALE.md`
//! §8.4). This is writer policy, not format law. The byte layouts the
//! choices produce are format law; the choice itself is not. Readers decode
//! whatever a payload declares, and goldens pin bytes per encoding, not this
//! heuristic.
//!
//! For each main column at group write, sample the payload —
//! the first 256 rows plus every 8th row — estimate the values-section
//! bytes each admissible encoding would produce for the full column from
//! that sample, and rank the encodings whose estimate beats the plain
//! estimate by at least 25 % (integer rule: `estimate × 4 ≤ plain × 3`),
//! ascending by `(estimate, encoding id)`. The writer then encodes with
//! the first ranked candidate that actually accepts the full column,
//! falling back to plain — so a sampled choice disproven by unsampled rows
//! (a constant column with one deviating tail row) can never
//! produce invalid bytes, and identical input always yields an identical
//! choice.

use devondb_types::column::Column;
use devondb_types::logical_type::LogicalType;
use devondb_types::value::Value;

use super::{Encoding, encode_values};

/// Rows `0 .. HEAD_ROWS` are always sampled (the contiguous head, so run
/// lengths measured there are true runs).
const HEAD_ROWS: usize = 256;

/// Every `STRIDE`-th row is sampled on top of the head.
const STRIDE: usize = 8;

/// A row index is in the sample when it is in the head block or lands on
/// the stride. Deterministic by construction.
const fn is_sampled(row: usize) -> bool {
    row < HEAD_ROWS || row.is_multiple_of(STRIDE)
}

/// One column's sampled rows, plus how many there are.
struct Sample<'a> {
    values: Vec<&'a Value>,
    row_count: u64,
}

impl<'a> Sample<'a> {
    fn take(column: &'a [Value]) -> Self {
        let values = column
            .iter()
            .enumerate()
            .filter(|(row, _)| is_sampled(*row))
            .map(|(_, value)| value)
            .collect();
        Self {
            values,
            row_count: column.len() as u64,
        }
    }

    /// The number of sampled rows (never zero for a non-empty column:
    /// row 0 is always in the head).
    fn len(&self) -> u64 {
        self.values.len() as u64
    }

    /// Scales a sampled quantity to the full column by the row ratio
    /// `row_count / sample_rows` (floor).
    fn scale(&self, sampled: u64) -> u64 {
        if self.values.is_empty() {
            return sampled;
        }
        sampled
            .saturating_mul(self.row_count)
            .saturating_div(self.len())
    }
}

/// The encoding candidates for one column's values section, ranked for
/// the writer to try in order: only admissible encodings whose sampled
/// cost estimate beats the plain estimate by at least 25 %, ascending by
/// `(estimate, encoding id)`. Empty means plain wins; the writer always
/// appends a plain fallback after trying the ranked candidates.
pub(crate) fn ranked_candidates(column: &[Value], ty: &LogicalType) -> Vec<Encoding> {
    if column.is_empty() {
        return Vec::new();
    }
    let sample = Sample::take(column);
    let Some(plain) = plain_estimate(&sample, ty) else {
        return Vec::new();
    };
    let mut estimates: Vec<(Encoding, u64)> = candidate_encodings(ty)
        .into_iter()
        .filter_map(|encoding| estimate(encoding, &sample, ty).map(|e| (encoding, e)))
        .filter(|(_, estimate)| estimate.saturating_mul(4) <= plain.saturating_mul(3))
        .collect();
    estimates.sort_by_key(|(encoding, estimate)| (*estimate, encoding.id()));
    estimates
        .into_iter()
        .map(|(encoding, _)| encoding)
        .collect()
}

/// The non-plain encodings admissible for `ty` (`docs/SCALE.md` §8.1),
/// in registry-id order.
fn candidate_encodings(ty: &LogicalType) -> Vec<Encoding> {
    [
        Encoding::Constant,
        Encoding::Rle,
        Encoding::BitpackFor,
        Encoding::Dictionary,
        Encoding::Fsst,
        Encoding::Alp,
    ]
    .into_iter()
    .filter(|encoding| encoding.is_admissible_for(ty))
    .collect()
}

/// The estimated full-column values-section bytes of `encoding`, from the
/// sample; `None` when the sample rules the encoding out (e.g. the sampled
/// rows are not all equal for constant, or a trial encoding failed).
fn estimate(encoding: Encoding, sample: &Sample<'_>, ty: &LogicalType) -> Option<u64> {
    match encoding {
        Encoding::Constant => constant_estimate(sample, ty),
        Encoding::Rle => rle_estimate(sample, ty),
        Encoding::BitpackFor => bitpack_for_estimate(sample),
        Encoding::Dictionary => dictionary_estimate(sample),
        Encoding::Fsst => fsst_estimate(sample, ty),
        Encoding::Alp => alp_estimate(sample, ty),
        Encoding::Plain => None, // plain is the baseline, never a candidate
    }
}

/// The plain values-section estimate: exact for fixed-width types, the
/// sampled string heap scaled by the row ratio for `String`. `None` for
/// types no non-plain encoding admits (they never reach selection).
fn plain_estimate(sample: &Sample<'_>, ty: &LogicalType) -> Option<u64> {
    let row_count = sample.row_count;
    match ty {
        LogicalType::Bool => Some(row_count.div_ceil(8)),
        LogicalType::Int64 | LogicalType::Float64 | LogicalType::Timestamp => {
            Some(row_count.saturating_mul(8))
        }
        LogicalType::Decimal { .. } => Some(row_count.saturating_mul(16)),
        LogicalType::String => {
            let heap: u64 = sample
                .values
                .iter()
                .map(|value| match value {
                    Value::String(text) => text.len() as u64,
                    _ => 0,
                })
                .sum();
            Some(
                row_count
                    .saturating_add(1)
                    .saturating_mul(4)
                    .saturating_add(sample.scale(heap)),
            )
        }
        LogicalType::Vector { .. }
        | LogicalType::VectorEncoded { .. }
        | LogicalType::GeoPoint
        | LogicalType::Bytes
        | LogicalType::Json => None,
    }
}

/// Constant: candidate only when every sampled non-null value is equal
/// (float equality is bit-exact, matching `constant::encode`); the
/// estimate is one value's spelling — the type's fixed width, or
/// `4 + len` for the shared string, or the zero spelling when every
/// sampled row is NULL.
fn constant_estimate(sample: &Sample<'_>, ty: &LogicalType) -> Option<u64> {
    let mut shared: Option<&Value> = None;
    for value in sample
        .values
        .iter()
        .filter(|value| !matches!(value, Value::Null))
    {
        match shared {
            None => shared = Some(value),
            Some(held) if same_bits(held, value) => {}
            Some(_) => return None,
        }
    }
    let width = match (ty, shared) {
        (LogicalType::String, Some(Value::String(text))) => 4_u64.saturating_add(text.len() as u64),
        (LogicalType::String, _) => 4,
        (LogicalType::Bool, _) => 1,
        (LogicalType::Int64 | LogicalType::Float64 | LogicalType::Timestamp, _) => 8,
        (LogicalType::Decimal { .. }, _) => 16,
        _ => return None,
    };
    Some(width)
}

/// The bit-exact value equality `constant::encode` applies (so a sampled
/// choice can never disagree with the encoder on `-0.0`/`+0.0` or NaN
/// spellings).
fn same_bits(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Float64(left), Value::Float64(right)) => left.to_bits() == right.to_bits(),
        _ => left == right,
    }
}

/// RLE: count runs over the sampled row sequence — NULL slots count as
/// the type's zero, exactly as the encoder merges them (`rle.md`) — and
/// scale the run count by the row ratio. The strided tail can split true
/// runs; that conservatism is documented writer policy.
fn rle_estimate(sample: &Sample<'_>, ty: &LogicalType) -> Option<u64> {
    let width: u64 = match ty {
        LogicalType::Bool => 1,
        LogicalType::Int64 | LogicalType::Timestamp => 8,
        LogicalType::Decimal { .. } => 16,
        _ => return None,
    };
    let mut runs = 0_u64;
    let mut previous: Option<i128> = None;
    for value in &sample.values {
        let key = run_key(ty, value)?;
        if previous != Some(key) {
            runs = runs.saturating_add(1);
            previous = Some(key);
        }
    }
    let runs = sample.scale(runs).max(1);
    Some(4_u64.saturating_add(runs.saturating_mul(4 + width)))
}

/// A row's run-merge key: the value widened to `i128`, or the type's zero
/// at a NULL slot (`rle.md`: runs may merge across NULLs whose neighbours
/// are zero). `None` for a value outside the type's variant.
fn run_key(ty: &LogicalType, value: &Value) -> Option<i128> {
    match (ty, value) {
        (_, Value::Null) => Some(0),
        (LogicalType::Bool, Value::Bool(flag)) => Some(i128::from(*flag)),
        (LogicalType::Int64, Value::Int64(int))
        | (LogicalType::Timestamp, Value::Timestamp(int)) => Some(i128::from(*int)),
        (LogicalType::Decimal { .. }, Value::Decimal(decimal)) => Some(decimal.digits()),
        _ => None,
    }
}

/// bitpack_for: the sampled non-null range fixes the bit width
/// (`12 + ceil(n / 1024) blocks × width × 128 bytes`, `bitpack_for.md`).
fn bitpack_for_estimate(sample: &Sample<'_>) -> Option<u64> {
    let mut bounds: Option<(i64, i64)> = None;
    for value in &sample.values {
        let int = match value {
            Value::Null => continue,
            Value::Int64(int) | Value::Timestamp(int) => *int,
            _ => return None,
        };
        bounds = Some(bounds.map_or((int, int), |(min, max)| (min.min(int), max.max(int))));
    }
    let width = bounds.map_or(0, |(min, max)| {
        let span = (max as i128 - min as i128) as u128;
        u64::from(128 - span.leading_zeros())
    });
    let blocks = sample.row_count.div_ceil(1024);
    Some(12_u64.saturating_add(blocks.saturating_mul(width).saturating_mul(128)))
}

/// Dictionary: the sampled distinct count and heap bytes scaled by the
/// row ratio stand in for the full dictionary — an all-distinct sample
/// therefore prices an all-distinct dictionary and stays plain, while a
/// low-cardinality column prices slightly conservatively; codes are
/// always `4 × row_count` (`dictionary.md`: v1 code width is u32).
fn dictionary_estimate(sample: &Sample<'_>) -> Option<u64> {
    let mut entries = std::collections::BTreeSet::new();
    for value in &sample.values {
        match value {
            Value::Null => {}
            Value::String(text) => {
                entries.insert(text.as_str());
            }
            _ => return None,
        }
    }
    let dict_count = sample.scale(entries.len() as u64).min(sample.row_count);
    let heap: u64 = entries.iter().map(|entry| entry.len() as u64).sum();
    let heap = sample.scale(heap);
    Some(
        4_u64
            .saturating_add(dict_count.saturating_add(1).saturating_mul(4))
            .saturating_add(heap)
            .saturating_add(sample.row_count.saturating_mul(4)),
    )
}

/// FSST: trial-compress the sampled strings with the real encoder, split
/// the trial section into its fixed part (symbol table + row offsets) and
/// its compressed heap, and scale the heap by the row ratio (`fsst.md`).
fn fsst_estimate(sample: &Sample<'_>, ty: &LogicalType) -> Option<u64> {
    let trial = trial_encode(Encoding::Fsst, sample, ty)?;
    let symbol_count = *trial.first()?;
    let mut cursor = 1_usize;
    for _ in 0..symbol_count {
        let len = *trial.get(cursor)? as usize;
        if !(1..=8).contains(&len) {
            return None;
        }
        cursor = cursor.checked_add(1 + len)?;
    }
    let offsets = (sample.len() as usize).checked_add(1)?.checked_mul(4)?;
    let heap = trial.len().checked_sub(cursor.checked_add(offsets)?)? as u64;
    Some(
        (cursor as u64)
            .saturating_add(sample.row_count.saturating_add(1).saturating_mul(4))
            .saturating_add(sample.scale(heap)),
    )
}

/// ALP: trial-encode the sampled floats with the real encoder (which runs
/// the `(e, f)` scan and so embodies the sample's exception rate), then
/// price the full column from the trial's header: bit width `w` and the
/// exception count scaled by the row ratio (`alp.md`).
fn alp_estimate(sample: &Sample<'_>, ty: &LogicalType) -> Option<u64> {
    let trial = trial_encode(Encoding::Alp, sample, ty)?;
    if trial.len() < 20 {
        return None;
    }
    let exception_count = u64::from(u32::from_le_bytes(trial[4..8].try_into().ok()?));
    let width = u64::from(*trial.get(16)?);
    if width > 64 {
        return None;
    }
    let row_count = sample.row_count;
    let full_blocks = row_count / 1024;
    let tail_lanes = (row_count % 1024).div_ceil(64);
    let blocks = full_blocks
        .saturating_mul(width)
        .saturating_mul(128)
        .saturating_add(tail_lanes.saturating_mul(width).saturating_mul(8));
    Some(
        20_u64
            .saturating_add(blocks)
            .saturating_add(sample.scale(exception_count).saturating_mul(12)),
    )
}

/// Runs the real encoder over the sampled rows as a typed [`Column`] —
/// the FSST/ALP trials. Returns the sample's values-section bytes;
/// `None` when the trial fails (the encoding is then not a candidate).
fn trial_encode(encoding: Encoding, sample: &Sample<'_>, ty: &LogicalType) -> Option<Vec<u8>> {
    let typed = Column::from_values(ty, sample.values.iter().map(|v| (*v).clone()).collect());
    let (bytes, _) = encode_values(encoding, &typed, ty).ok()?;
    Some(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ints(values: &[i64]) -> Vec<Value> {
        values.iter().map(|value| Value::Int64(*value)).collect()
    }

    #[test]
    fn the_sample_is_the_head_plus_the_stride() {
        let rows: Vec<usize> = (0..2048).filter(|row| is_sampled(*row)).collect();
        assert_eq!(&rows[..256], &(0..256).collect::<Vec<_>>()[..]);
        assert!(rows.contains(&512));
        assert!(rows.contains(&2040));
        assert!(!rows.contains(&511));
        assert_eq!(rows.len(), 256 + 2048 / 8 - 256 / 8);
    }

    #[test]
    fn a_constant_column_ranks_constant_first() {
        let column = ints(&[7; 512]);
        assert_eq!(
            ranked_candidates(&column, &LogicalType::Int64).first(),
            Some(&Encoding::Constant)
        );
    }

    #[test]
    fn an_all_null_column_ranks_constant_first() {
        let column = vec![Value::Null; 64];
        assert_eq!(
            ranked_candidates(&column, &LogicalType::Int64).first(),
            Some(&Encoding::Constant)
        );
    }

    #[test]
    fn a_run_heavy_column_ranks_rle() {
        // 32 runs of 64 rows each over a wide value range: bitpack_for
        // prices a 25-bit width, rle prices ~130 runs.
        let mut values = Vec::new();
        for run in 0..32_i64 {
            values.extend(std::iter::repeat_n(run * 1_000_000, 64));
        }
        let column = ints(&values);
        assert_eq!(column.len(), 2048);
        let ranked = ranked_candidates(&column, &LogicalType::Int64);
        assert_eq!(ranked.first(), Some(&Encoding::Rle));
    }

    #[test]
    fn a_narrow_range_column_ranks_bitpack_for() {
        let column: Vec<Value> = (0..2048)
            .map(|row| Value::Int64(1_000_000 + row % 61))
            .collect();
        let ranked = ranked_candidates(&column, &LogicalType::Int64);
        assert_eq!(ranked.first(), Some(&Encoding::BitpackFor));
    }

    #[test]
    fn a_low_cardinality_string_column_ranks_dictionary() {
        let names = ["alpha", "beta", "gamma", "delta"];
        let column: Vec<Value> = (0..2048)
            .map(|row| Value::String(names[row % names.len()].to_owned()))
            .collect();
        let ranked = ranked_candidates(&column, &LogicalType::String);
        assert_eq!(ranked.first(), Some(&Encoding::Dictionary));
    }

    #[test]
    fn a_repetitive_string_column_ranks_fsst() {
        let column: Vec<Value> = (0..2048)
            .map(|row| {
                Value::String(format!(
                    "https://edge.example.com/sensors/reading/{row:06}/celsius"
                ))
            })
            .collect();
        let ranked = ranked_candidates(&column, &LogicalType::String);
        assert!(ranked.contains(&Encoding::Fsst), "ranked: {ranked:?}");
    }

    #[test]
    fn a_decimal_like_float_column_ranks_alp() {
        let column: Vec<Value> = (0..2048)
            .map(|row| Value::Float64(row as f64 / 100.0))
            .collect();
        let ranked = ranked_candidates(&column, &LogicalType::Float64);
        assert_eq!(ranked.first(), Some(&Encoding::Alp));
    }

    #[test]
    fn all_distinct_random_strings_stay_plain() {
        let mut state = 0x243F_6A88_85A3_08D3_u64;
        let column: Vec<Value> = (0..2048)
            .map(|_| {
                // 32 characters drawn from the 94-printable-ASCII alphabet:
                // high enough entropy per byte that FSST's symbol table
                // finds no repeated n-gram worth a code.
                let mut text = String::with_capacity(32);
                for _ in 0..32 {
                    state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
                    text.push(char::from((state >> 33) as u8 % 94 + 33));
                }
                Value::String(text)
            })
            .collect();
        assert!(
            ranked_candidates(&column, &LogicalType::String).is_empty(),
            "incompressible strings must not qualify any candidate"
        );
    }

    #[test]
    fn selection_is_deterministic() {
        let column: Vec<Value> = (0..2048).map(|row| Value::Int64(row % 9)).collect();
        assert_eq!(
            ranked_candidates(&column, &LogicalType::Int64),
            ranked_candidates(&column, &LogicalType::Int64)
        );
    }

    #[test]
    fn plain_only_types_never_rank() {
        let column = vec![Value::Null; 8];
        for ty in [
            LogicalType::Bytes,
            LogicalType::Json,
            LogicalType::GeoPoint,
            LogicalType::Vector { dim: 4 },
        ] {
            assert!(ranked_candidates(&column, &ty).is_empty(), "{ty}");
        }
    }
}
