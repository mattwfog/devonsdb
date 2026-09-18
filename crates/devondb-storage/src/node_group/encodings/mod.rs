//! Per-payload values-section encodings (`docs/SCALE.md` §8.2).
//!
//! A column payload stays `validity bitmap ‖ values`; encodings apply to
//! the values section only. The node-group directory's `COLUMN_ENCODINGS`
//! section (directory-flags bit 1, governed by superblock feature bit 13)
//! declares one `encoding_id u8 · p0 · p1 · p2` record per main column.
//! Encoding ids, admissibility, and byte layouts are part of the format
//! contract (`docs/SCALE.md` §8.1).
//!
//! The `select` module implements the sampled adaptive writer policy used by
//! the default write path (`docs/SCALE.md` §8.4). The `Plain` arms below
//! delegate to the legacy payload code in the parent module.

use devondb_types::DevonResult;
use devondb_types::column::{Bitmap, Column};
use devondb_types::logical_type::LogicalType;
use devondb_types::value::Value;

pub(crate) mod alp;
pub(crate) mod bitpack_for;
pub(crate) mod constant;
pub(crate) mod dictionary;
pub(crate) mod fsst;
pub(crate) mod rle;
pub(crate) mod select;

/// The values-section encoding of one column payload (`docs/SCALE.md`
/// §8.1 registry — ids are format law).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Encoding {
    /// Id 0: today's plain layout (`docs/FORMAT.md` § Column payload
    /// encoding).
    Plain,
    /// Id 1: one shared non-null value.
    Constant,
    /// Id 2: run-length encoding.
    Rle,
    /// Id 3: FastLanes 1024-value transposed bit-packing + frame of
    /// reference.
    BitpackFor,
    /// Id 4: dictionary encoding.
    Dictionary,
    /// Id 5: FSST string compression.
    Fsst,
    /// Id 6: ALP float compression.
    Alp,
}

impl Encoding {
    /// The format-law encoding id (`docs/SCALE.md` §8.1).
    pub(crate) const fn id(self) -> u8 {
        match self {
            Self::Plain => 0,
            Self::Constant => 1,
            Self::Rle => 2,
            Self::BitpackFor => 3,
            Self::Dictionary => 4,
            Self::Fsst => 5,
            Self::Alp => 6,
        }
    }

    /// Resolves a persisted encoding id; `None` for an id outside the
    /// §8.1 table (corruption on the read path).
    pub(crate) const fn from_id(id: u8) -> Option<Self> {
        match id {
            0 => Some(Self::Plain),
            1 => Some(Self::Constant),
            2 => Some(Self::Rle),
            3 => Some(Self::BitpackFor),
            4 => Some(Self::Dictionary),
            5 => Some(Self::Fsst),
            6 => Some(Self::Alp),
            _ => None,
        }
    }

    /// The registry name, used in error messages and stub refusals.
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Plain => "plain",
            Self::Constant => "constant",
            Self::Rle => "rle",
            Self::BitpackFor => "bitpack_for",
            Self::Dictionary => "dictionary",
            Self::Fsst => "fsst",
            Self::Alp => "alp",
        }
    }

    /// The §8.1 admissibility table: which logical types an encoding may
    /// carry. An id applied to an inadmissible type is corruption.
    pub(crate) fn is_admissible_for(self, ty: &LogicalType) -> bool {
        match self {
            Self::Plain => true,
            Self::Constant => matches!(
                ty,
                LogicalType::Int64
                    | LogicalType::Float64
                    | LogicalType::Bool
                    | LogicalType::Timestamp
                    | LogicalType::Decimal { .. }
                    | LogicalType::String
            ),
            Self::Rle => matches!(
                ty,
                LogicalType::Int64
                    | LogicalType::Timestamp
                    | LogicalType::Decimal { .. }
                    | LogicalType::Bool
            ),
            Self::BitpackFor => {
                matches!(ty, LogicalType::Int64 | LogicalType::Timestamp)
            }
            Self::Dictionary | Self::Fsst => matches!(ty, LogicalType::String),
            Self::Alp => matches!(ty, LogicalType::Float64),
        }
    }
}

/// Encodes one column's values section under `encoding`, returning the
/// section bytes and the encoding's parameter bytes (zero when unused).
///
/// FIXED signature per `docs/SCALE.md` §8.2: every encoding module exposes
/// `encode` with this shape. The input is the typed [`Column`] — never a
/// boxed `Vec<Value>` rebuild — and the validity bitmap is never encoded
/// (§8.1): NULL handling is the payload's validity section, not the
/// encoding's business.
pub(crate) fn encode_values(
    encoding: Encoding,
    column: &Column,
    ty: &LogicalType,
) -> DevonResult<(Vec<u8>, [u8; 3])> {
    if !encoding.is_admissible_for(ty) {
        return Err(super::invalid_argument(format!(
            "encoding {} is not admissible for {ty}",
            encoding.name()
        )));
    }
    match encoding {
        Encoding::Plain => {
            // The plain arm exists for dispatch completeness; the
            // production writer short-circuits plain payloads through the
            // legacy byte path so writer error messages keep their column
            // context. Materialization here bridges to the typed interface.
            let values: Vec<Value> = (0..column.len()).map(|row| column.value_at(row)).collect();
            let bytes = super::encode_values_section(&values, ty, 0)?;
            Ok((bytes, [0; 3]))
        }
        Encoding::Constant => constant::encode(column, ty),
        Encoding::Rle => rle::encode(column, ty),
        Encoding::BitpackFor => bitpack_for::encode(column, ty),
        Encoding::Dictionary => dictionary::encode(column, ty),
        Encoding::Fsst => fsst::encode(column, ty),
        Encoding::Alp => alp::encode(column, ty),
    }
}

/// Decodes one values section of `bytes` declared as `encoding` with
/// `params`, materializing the typed [`Column`] (`docs/SCALE.md` §6.5 —
/// never `Vec<Value>`). `validity` is the payload's validity bitmap in
/// §6.3 word form; NULL slots decode to zero (typed) or `Value::Null`
/// (boxed), never from the section bytes.
///
/// FIXED signature per §8.2. Decoders validate everything (lengths,
/// parameters, UTF-8) and return `Corrupt`, never panic — the
/// `vector_encoding.rs` / `fuzz_decode.rs` precedent.
pub(crate) fn decode_values(
    encoding: Encoding,
    params: [u8; 3],
    bytes: &[u8],
    validity: &Bitmap,
    row_count: usize,
    ty: &LogicalType,
) -> DevonResult<Column> {
    match encoding {
        Encoding::Plain => {
            if params != [0; 3] {
                return Err(super::corrupt("encoding plain parameters are not zero"));
            }
            // Rebuild the legacy payload byte form (LSB-first validity
            // bytes ‖ values) so the plain arm runs the exact legacy
            // decoder with every validation in force. Column context in
            // its error messages is placeholder 0; the persisted-file
            // path routes plain payloads through the legacy entry points
            // directly and never through this arm.
            let mut payload = super::validity_bytes_from_bitmap(validity);
            payload.extend_from_slice(bytes);
            super::decode_column_typed(&payload, row_count, ty, 0, Encoding::Plain, [0; 3])
        }
        Encoding::Constant => constant::decode(params, bytes, validity, row_count, ty),
        Encoding::Rle => rle::decode(params, bytes, validity, row_count, ty),
        Encoding::BitpackFor => bitpack_for::decode(params, bytes, validity, row_count, ty),
        Encoding::Dictionary => dictionary::decode(params, bytes, validity, row_count, ty),
        Encoding::Fsst => fsst::decode(params, bytes, validity, row_count, ty),
        Encoding::Alp => alp::decode(params, bytes, validity, row_count, ty),
    }
}

/// Encodes the directory's `COLUMN_ENCODINGS` section payload: one
/// `encoding_id u8 · p0 · p1 · p2` record per main column, catalog order
/// (`docs/SCALE.md` §8.1).
pub(crate) fn encode_section_payload(selections: &[(Encoding, [u8; 3])]) -> Vec<u8> {
    let mut payload = Vec::with_capacity(selections.len() * 4);
    for (encoding, params) in selections {
        payload.push(encoding.id());
        payload.extend_from_slice(params);
    }
    payload
}

#[cfg(test)]
mod tests {
    use super::{Encoding, encode_section_payload};
    use devondb_types::logical_type::LogicalType;

    const ALL: [Encoding; 7] = [
        Encoding::Plain,
        Encoding::Constant,
        Encoding::Rle,
        Encoding::BitpackFor,
        Encoding::Dictionary,
        Encoding::Fsst,
        Encoding::Alp,
    ];

    #[test]
    fn ids_round_trip_through_from_id() {
        for encoding in ALL {
            assert_eq!(Encoding::from_id(encoding.id()), Some(encoding));
        }
        assert_eq!(Encoding::from_id(7), None);
        assert_eq!(Encoding::from_id(u8::MAX), None);
    }

    #[test]
    fn admissibility_matches_the_scale_8_1_table() {
        let cases: &[(LogicalType, &[Encoding])] = &[
            (
                LogicalType::Int64,
                &[
                    Encoding::Plain,
                    Encoding::Constant,
                    Encoding::Rle,
                    Encoding::BitpackFor,
                ],
            ),
            (
                LogicalType::Float64,
                &[Encoding::Plain, Encoding::Constant, Encoding::Alp],
            ),
            (
                LogicalType::Bool,
                &[Encoding::Plain, Encoding::Constant, Encoding::Rle],
            ),
            (
                LogicalType::Timestamp,
                &[
                    Encoding::Plain,
                    Encoding::Constant,
                    Encoding::Rle,
                    Encoding::BitpackFor,
                ],
            ),
            (
                LogicalType::Decimal {
                    precision: 10,
                    scale: 2,
                },
                &[Encoding::Plain, Encoding::Constant, Encoding::Rle],
            ),
            (
                LogicalType::String,
                &[
                    Encoding::Plain,
                    Encoding::Constant,
                    Encoding::Dictionary,
                    Encoding::Fsst,
                ],
            ),
            (LogicalType::Bytes, &[Encoding::Plain]),
            (LogicalType::Json, &[Encoding::Plain]),
            (LogicalType::Vector { dim: 4 }, &[Encoding::Plain]),
            (LogicalType::GeoPoint, &[Encoding::Plain]),
        ];
        for (ty, admissible) in cases {
            for encoding in ALL {
                assert_eq!(
                    encoding.is_admissible_for(ty),
                    admissible.contains(&encoding),
                    "encoding {:?} admissibility for {ty}",
                    encoding
                );
            }
        }
    }

    #[test]
    fn section_payload_is_four_bytes_per_column() {
        let selections = [
            (Encoding::Plain, [0; 3]),
            (Encoding::Constant, [0; 3]),
            (Encoding::Alp, [9, 8, 7]),
        ];
        assert_eq!(
            encode_section_payload(&selections),
            vec![0, 0, 0, 0, 1, 0, 0, 0, 6, 9, 8, 7]
        );
    }
}
