//! Typed chunk columns (`docs/SCALE.md` §6).
//!
//! `Column` is the decode target shared by the executor and storage decoders.
//! It lives in `devondb-types` because `devondb-storage` cannot depend on
//! `devondb-exec` (§6.2). `Column::value_at` materializes the same `Value`
//! the boxed path carried, preserving behavior while storage becomes typed.

use crate::decimal::{Decimal128, MAX_SCALE};
use crate::logical_type::LogicalType;
use crate::value::Value;

/// Row-validity bitmap for typed columns (docs/SCALE.md §6.3): bit `i`
/// (word `i / 64`, bit `i % 64`) SET means row `i` is non-NULL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bitmap {
    words: Vec<u64>,
    len: usize,
}

impl Bitmap {
    /// Creates a bitmap covering `len` rows with every bit set (all rows
    /// valid). Unused tail bits in the final word stay zero so equality
    /// and goldens stay deterministic.
    #[must_use]
    pub fn all_valid(len: usize) -> Self {
        let mut words = vec![u64::MAX; len.div_ceil(64)];
        let tail = len % 64;
        if tail != 0
            && let Some(last) = words.last_mut()
        {
            *last = (1_u64 << tail) - 1;
        }
        Self { words, len }
    }

    /// Returns the number of rows the bitmap covers.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Returns whether the bitmap covers no rows.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Returns whether row `i` is valid (non-NULL). Rows past [`Bitmap::len`]
    /// report invalid.
    #[must_use]
    pub fn is_valid(&self, i: usize) -> bool {
        self.words
            .get(i / 64)
            .is_some_and(|word| word & (1_u64 << (i % 64)) != 0)
    }

    /// Marks row `i` valid (non-NULL).
    ///
    /// Panics when `i` is past the bitmap's row count.
    pub fn set(&mut self, i: usize) {
        self.words[i / 64] |= 1_u64 << (i % 64);
    }

    /// Marks row `i` NULL.
    ///
    /// Panics when `i` is past the bitmap's row count.
    pub fn clear(&mut self, i: usize) {
        self.words[i / 64] &= !(1_u64 << (i % 64));
    }

    /// Returns the raw 64-bit words (little-endian row order).
    #[must_use]
    pub fn as_words(&self) -> &[u64] {
        &self.words
    }

    /// Budget charge: eight bytes per word (docs/SCALE.md §6.6).
    #[must_use]
    pub fn approx_bytes(&self) -> usize {
        self.words.len() * size_of::<u64>()
    }
}

/// A typed chunk column (docs/SCALE.md §6.2).
///
/// Fixed-width variants store values contiguously with an optional validity
/// bitmap (`None` = all rows valid); NULL slots hold zero in the typed
/// vector (§6.3). [`Column::Boxed`] is the fallback carrier for `String`,
/// `Bytes`, `Json`, `Vector`, and `GeoPoint`: it keeps `Value::Null` inline
/// and carries no bitmap.
///
/// The `Decimal` variant carries the column's `scale` because `value_at`
/// must reconstruct `Decimal128` without the column's
/// `LogicalType`; precision stays in the `LogicalType`, exactly as
/// `Decimal128` pairs digits with scale.
#[derive(Debug, Clone)]
pub enum Column {
    /// `Int64` values.
    Int64 {
        /// Contiguous values; zero at NULL slots.
        values: Vec<i64>,
        /// Row validity; `None` means all rows valid.
        validity: Option<Bitmap>,
    },
    /// `Float64` values.
    Float64 {
        /// Contiguous values; zero at NULL slots.
        values: Vec<f64>,
        /// Row validity; `None` means all rows valid.
        validity: Option<Bitmap>,
    },
    /// `Bool` values.
    Bool {
        /// Contiguous values; false at NULL slots.
        values: Vec<bool>,
        /// Row validity; `None` means all rows valid.
        validity: Option<Bitmap>,
    },
    /// `Timestamp` values (epoch microseconds).
    Timestamp {
        /// Contiguous values; zero at NULL slots.
        values: Vec<i64>,
        /// Row validity; `None` means all rows valid.
        validity: Option<Bitmap>,
    },
    /// `Decimal` values as unscaled i128 digits at the column's `scale`.
    Decimal {
        /// Contiguous unscaled digits; zero at NULL slots.
        values: Vec<i128>,
        /// Digits right of the decimal point, shared by every row.
        scale: u8,
        /// Row validity; `None` means all rows valid.
        validity: Option<Bitmap>,
    },
    /// The fallback carrier for types without a typed variant.
    Boxed(Vec<Value>),
}

impl Column {
    /// Builds a column of logical type `ty` from row values for
    /// `ChunkBuilder` (`docs/SCALE.md` §6.4).
    ///
    /// The builder pre-validates every value against `ty`, so the typed
    /// variants are what runs. Defensively, a value the type does not admit
    /// degrades the WHOLE column to [`Column::Boxed`], preserving the input
    /// values exactly; `value_at` then round-trips every input row.
    #[must_use]
    pub fn from_values(ty: &LogicalType, values: Vec<Value>) -> Self {
        match ty {
            LogicalType::Int64 => match typed_parts(&values, as_int64) {
                Some((values, validity)) => Self::Int64 { values, validity },
                None => Self::Boxed(values),
            },
            LogicalType::Float64 => match typed_parts(&values, as_float64) {
                Some((values, validity)) => Self::Float64 { values, validity },
                None => Self::Boxed(values),
            },
            LogicalType::Bool => match typed_parts(&values, as_bool) {
                Some((values, validity)) => Self::Bool { values, validity },
                None => Self::Boxed(values),
            },
            LogicalType::Timestamp => match typed_parts(&values, as_timestamp) {
                Some((values, validity)) => Self::Timestamp { values, validity },
                None => Self::Boxed(values),
            },
            LogicalType::Decimal { scale, .. } => {
                if *scale > MAX_SCALE {
                    return Self::Boxed(values);
                }
                match typed_parts(&values, |value| as_decimal_digits(value, *scale)) {
                    Some((values, validity)) => Self::Decimal {
                        values,
                        scale: *scale,
                        validity,
                    },
                    None => Self::Boxed(values),
                }
            }
            LogicalType::String
            | LogicalType::Bytes
            | LogicalType::Json
            | LogicalType::Vector { .. }
            | LogicalType::VectorEncoded { .. }
            | LogicalType::GeoPoint => Self::Boxed(values),
        }
    }

    /// Returns the number of rows in the column.
    #[must_use]
    pub fn len(&self) -> usize {
        match self {
            Self::Int64 { values, .. } => values.len(),
            Self::Float64 { values, .. } => values.len(),
            Self::Bool { values, .. } => values.len(),
            Self::Timestamp { values, .. } => values.len(),
            Self::Decimal { values, .. } => values.len(),
            Self::Boxed(values) => values.len(),
        }
    }

    /// Returns whether the column holds no rows.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Materializes the value at `row`: `Value::Null` where the validity
    /// bitmap clears the slot, else the typed value reconstructed through
    /// the same `Value` constructors the boxed path used (§6.4).
    ///
    /// Panics when `row` is out of bounds; callers bounds-check first
    /// (`Chunk::value` returns `None` instead).
    #[must_use]
    pub fn value_at(&self, row: usize) -> Value {
        match self {
            Self::Int64 { values, validity } => typed_value(validity, row, values, Value::Int64),
            Self::Float64 { values, validity } => {
                typed_value(validity, row, values, Value::Float64)
            }
            Self::Bool { values, validity } => typed_value(validity, row, values, Value::Bool),
            Self::Timestamp { values, validity } => {
                typed_value(validity, row, values, Value::Timestamp)
            }
            Self::Decimal {
                values,
                scale,
                validity,
            } => typed_value(validity, row, values, |digits| {
                decimal_value(digits, *scale)
            }),
            Self::Boxed(values) => values[row].clone(),
        }
    }

    /// Borrows the value at `row` of a [`Column::Boxed`] column — the only
    /// variant with a stored `Value` to lend (§6.4; typed variants
    /// materialize through [`Column::value_at`] instead).
    #[must_use]
    pub fn value_ref_boxed(&self, row: usize) -> Option<&Value> {
        match self {
            Self::Boxed(values) => values.get(row),
            _ => None,
        }
    }

    /// Budget charge (docs/SCALE.md §6.6): fixed-width storage charges
    /// `len × width` plus bitmap words; `Boxed` charges per-value
    /// [`Value::approx_bytes`], unchanged from boxed-column accounting.
    #[must_use]
    pub fn approx_bytes(&self) -> usize {
        match self {
            Self::Int64 { values, validity } => {
                fixed_width_bytes(values.len(), size_of::<i64>(), validity)
            }
            Self::Float64 { values, validity } => {
                fixed_width_bytes(values.len(), size_of::<f64>(), validity)
            }
            Self::Bool { values, validity } => {
                fixed_width_bytes(values.len(), size_of::<bool>(), validity)
            }
            Self::Timestamp { values, validity } => {
                fixed_width_bytes(values.len(), size_of::<i64>(), validity)
            }
            Self::Decimal {
                values, validity, ..
            } => fixed_width_bytes(values.len(), size_of::<i128>(), validity),
            Self::Boxed(values) => values.iter().map(Value::approx_bytes).sum(),
        }
    }
}

/// Two columns are equal iff `value_at` agrees row-wise — this keeps
/// `Chunk: PartialEq` semantics identical to boxed columns.
impl PartialEq for Column {
    fn eq(&self, other: &Self) -> bool {
        self.len() == other.len()
            && (0..self.len()).all(|row| self.value_at(row) == other.value_at(row))
    }
}

/// Supports assertions that compare a column against a value slice;
/// equality is row-wise `value_at` agreement, as in [`PartialEq for
/// Column`](Column#impl-PartialEq-for-Column).
impl PartialEq<[Value]> for Column {
    fn eq(&self, other: &[Value]) -> bool {
        self.len() == other.len() && (0..self.len()).all(|row| self.value_at(row) == other[row])
    }
}

/// Array-literal form of [`PartialEq<[Value]> for Column`].
impl<const N: usize> PartialEq<[Value; N]> for Column {
    fn eq(&self, other: &[Value; N]) -> bool {
        self == other.as_slice()
    }
}

/// `Vec` form of [`PartialEq<[Value]> for Column`].
impl PartialEq<Vec<Value>> for Column {
    fn eq(&self, other: &Vec<Value>) -> bool {
        self == other.as_slice()
    }
}

/// Builds typed storage from row values: `Value::Null` becomes a zero slot
/// plus a cleared validity bit (bitmap allocated lazily on the first NULL).
/// Returns `None` when a value does not match the column type; the caller
/// falls back to `Boxed` with the input values untouched.
fn typed_parts<T: Default + Copy>(
    values: &[Value],
    extract: impl Fn(&Value) -> Option<T>,
) -> Option<(Vec<T>, Option<Bitmap>)> {
    let mut slots = Vec::with_capacity(values.len());
    let mut validity: Option<Bitmap> = None;
    for (index, value) in values.iter().enumerate() {
        match value {
            Value::Null => {
                validity
                    .get_or_insert_with(|| Bitmap::all_valid(values.len()))
                    .clear(index);
                slots.push(T::default());
            }
            other => slots.push(extract(other)?),
        }
    }
    Some((slots, validity))
}

fn as_int64(value: &Value) -> Option<i64> {
    match value {
        Value::Int64(typed) => Some(*typed),
        _ => None,
    }
}

fn as_float64(value: &Value) -> Option<f64> {
    match value {
        Value::Float64(typed) => Some(*typed),
        _ => None,
    }
}

fn as_bool(value: &Value) -> Option<bool> {
    match value {
        Value::Bool(typed) => Some(*typed),
        _ => None,
    }
}

fn as_timestamp(value: &Value) -> Option<i64> {
    match value {
        Value::Timestamp(typed) => Some(*typed),
        _ => None,
    }
}

/// Decimal digits are admitted only at the column's own scale: rescaling
/// here would be the silent-rescale the decimal law forbids.
fn as_decimal_digits(value: &Value, scale: u8) -> Option<i128> {
    match value {
        Value::Decimal(decimal) if decimal.scale() == scale => Some(decimal.digits()),
        _ => None,
    }
}

/// Materializes one typed slot, honoring the validity bitmap.
fn typed_value<T: Copy>(
    validity: &Option<Bitmap>,
    row: usize,
    values: &[T],
    wrap: impl Fn(T) -> Value,
) -> Value {
    match validity {
        Some(bitmap) if !bitmap.is_valid(row) => Value::Null,
        _ => wrap(values[row]),
    }
}

/// Reconstructs a decimal from column digits at the column scale.
/// `from_values` enforces `scale ≤ MAX_SCALE`, so the error arm is
/// unreachable for columns built through this interface.
fn decimal_value(digits: i128, scale: u8) -> Value {
    match Decimal128::new(digits, scale) {
        Ok(decimal) => Value::Decimal(decimal),
        Err(_) => Value::Null,
    }
}

fn fixed_width_bytes(len: usize, width: usize, validity: &Option<Bitmap>) -> usize {
    len * width + validity.as_ref().map_or(0, |bitmap| bitmap.approx_bytes())
}

#[cfg(test)]
mod tests {
    use super::{Bitmap, Column};
    use crate::logical_type::LogicalType;
    use crate::value::Value;

    #[test]
    fn bitmap_tail_bits_stay_zero() {
        let bitmap = Bitmap::all_valid(65);
        assert_eq!(bitmap.as_words(), &[u64::MAX, 1]);
        assert!(bitmap.is_valid(64));
        assert!(!bitmap.is_valid(65), "rows past len report invalid");

        let empty = Bitmap::all_valid(0);
        assert!(empty.is_empty());
        assert_eq!(empty.as_words(), &[] as &[u64]);
    }

    #[test]
    fn set_and_clear_round_trip_bits() {
        let mut bitmap = Bitmap::all_valid(3);
        bitmap.clear(1);
        assert!(bitmap.is_valid(0));
        assert!(!bitmap.is_valid(1));
        bitmap.set(1);
        assert!(bitmap.is_valid(1));
    }

    #[test]
    fn mismatched_values_degrade_to_boxed_unchanged() {
        let values = vec![Value::Int64(1), Value::String("not-an-int".into())];
        let column = Column::from_values(&LogicalType::Int64, values.clone());
        let Column::Boxed(boxed) = &column else {
            panic!("mismatched values must degrade to Boxed: {column:?}");
        };
        assert_eq!(boxed, &values);
    }

    #[test]
    fn wrong_scale_decimals_degrade_to_boxed() {
        let value = Value::Decimal(crate::Decimal128::new(100, 2).unwrap());
        let column = Column::from_values(
            &LogicalType::Decimal {
                precision: 10,
                scale: 3,
            },
            vec![value.clone()],
        );
        let Column::Boxed(boxed) = &column else {
            panic!("scale mismatch must degrade to Boxed: {column:?}");
        };
        assert_eq!(boxed, &[value]);
    }
}
