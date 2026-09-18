//! Logical column types (`docs/PLAN_IR.md` § Type system) and the physical
//! quantized-column vocabulary (`docs/FORMAT.md` § Quantized vector element
//! encodings).

use serde::de::{Deserializer, Error as DeError};
use serde::ser::{SerializeMap, Serializer};
use serde::{Deserialize, Serialize};

/// The catalog spelling of the i8 per-vector metadata locations. These are
/// fixed layout names, not tunable values (`docs/FORMAT.md` § Quantized
/// vector element encodings).
const PER_VECTOR_F32_LE: &str = "per_vector_f32_le";

/// The rescore representation of a b1-encoded vector column.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum B1Rescore {
    /// No rescore run; candidates rank by the asymmetric estimate alone.
    None,
    /// IEEE-754 binary16 rescore run.
    F16,
    /// Per-vector symmetric i8 rescore run.
    I8,
    /// Full f32 rescore run.
    F32,
}

impl B1Rescore {
    fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::F16 => "f16",
            Self::I8 => "i8",
            Self::F32 => "f32",
        }
    }
}

/// Physical element encoding of a `VectorEncoded` column.
///
/// The canonical catalog JSON spellings are format law (`docs/FORMAT.md`
/// § Quantized vector element encodings); serialization is hand-written so
/// the emitted field order matches the spec table exactly and readers
/// reject unknown or malformed fields instead of ignoring them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VectorEncoding {
    /// IEEE-754 binary16 elements: `{"kind":"f16"}`.
    F16,
    /// Per-vector symmetric i8 with f32 scale/offset metadata slots:
    /// `{"kind":"i8","scale":"per_vector_f32_le","offset":"per_vector_f32_le"}`.
    I8,
    /// 1-bit sign quantization after a seeded deterministic rotation:
    /// `{"kind":"b1","rotation_seed":S,"rescore":"none"|"f16"|"i8"|"f32"}`.
    B1 {
        /// Rotation seed, fixed for the lifetime of the column.
        rotation_seed: u64,
        /// Rescore representation stored in the column's auxiliary run.
        rescore: B1Rescore,
    },
}

impl Serialize for VectorEncoding {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::F16 => {
                let mut map = serializer.serialize_map(Some(1))?;
                map.serialize_entry("kind", "f16")?;
                map.end()
            }
            Self::I8 => {
                let mut map = serializer.serialize_map(Some(3))?;
                map.serialize_entry("kind", "i8")?;
                map.serialize_entry("scale", PER_VECTOR_F32_LE)?;
                map.serialize_entry("offset", PER_VECTOR_F32_LE)?;
                map.end()
            }
            Self::B1 {
                rotation_seed,
                rescore,
            } => {
                let mut map = serializer.serialize_map(Some(3))?;
                map.serialize_entry("kind", "b1")?;
                map.serialize_entry("rotation_seed", rotation_seed)?;
                map.serialize_entry("rescore", rescore.as_str())?;
                map.end()
            }
        }
    }
}

/// Strict intermediate for [`VectorEncoding`] deserialization. A plain
/// struct supports `deny_unknown_fields`, which serde's internally tagged
/// enums do not; per-kind field validation then happens in one place.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct VectorEncodingRepr {
    kind: String,
    #[serde(default)]
    scale: Option<String>,
    #[serde(default)]
    offset: Option<String>,
    #[serde(default)]
    rotation_seed: Option<u64>,
    #[serde(default)]
    rescore: Option<B1Rescore>,
}

impl<'de> Deserialize<'de> for VectorEncoding {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let repr = VectorEncodingRepr::deserialize(deserializer)?;
        let forbid = |field: &str, absent: bool| {
            if absent {
                Ok(())
            } else {
                Err(D::Error::custom(format!(
                    "vector encoding kind `{}` forbids field `{field}`",
                    repr.kind
                )))
            }
        };
        match repr.kind.as_str() {
            "f16" => {
                forbid("scale", repr.scale.is_none())?;
                forbid("offset", repr.offset.is_none())?;
                forbid("rotation_seed", repr.rotation_seed.is_none())?;
                forbid("rescore", repr.rescore.is_none())?;
                Ok(Self::F16)
            }
            "i8" => {
                forbid("rotation_seed", repr.rotation_seed.is_none())?;
                forbid("rescore", repr.rescore.is_none())?;
                for (name, value) in [("scale", &repr.scale), ("offset", &repr.offset)] {
                    match value.as_deref() {
                        Some(PER_VECTOR_F32_LE) => {}
                        Some(other) => {
                            return Err(D::Error::custom(format!(
                                "i8 vector encoding field `{name}` must be \
                                 `{PER_VECTOR_F32_LE}`, got `{other}`"
                            )));
                        }
                        None => {
                            return Err(D::Error::custom(format!(
                                "i8 vector encoding requires field `{name}`"
                            )));
                        }
                    }
                }
                Ok(Self::I8)
            }
            "b1" => {
                forbid("scale", repr.scale.is_none())?;
                forbid("offset", repr.offset.is_none())?;
                let rotation_seed = repr.rotation_seed.ok_or_else(|| {
                    D::Error::custom("b1 vector encoding requires field `rotation_seed`")
                })?;
                let rescore = repr.rescore.ok_or_else(|| {
                    D::Error::custom("b1 vector encoding requires field `rescore`")
                })?;
                Ok(Self::B1 {
                    rotation_seed,
                    rescore,
                })
            }
            other => Err(D::Error::custom(format!(
                "unknown vector encoding kind `{other}`"
            ))),
        }
    }
}

impl std::fmt::Display for VectorEncoding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::F16 => write!(f, "f16"),
            Self::I8 => write!(f, "i8"),
            Self::B1 {
                rotation_seed,
                rescore,
            } => write!(f, "b1(seed={rotation_seed}, rescore={})", rescore.as_str()),
        }
    }
}

/// A logical column type in the v0 type system.
///
/// `Null` is a value, not a type: every column type admits null values in v0,
/// so nullability is not modeled here.
///
/// `VectorEncoded` is physical catalog vocabulary, not a sixth logical value
/// type: its values are `Vector(dim)` values (`docs/FORMAT.md` § Quantized
/// vector element encodings), and plan-facing code should reason through
/// [`LogicalType::value_type`]. It lives in this enum because the catalog
/// column record's `ty` field is this enum's serialization.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LogicalType {
    /// Boolean.
    Bool,
    /// 64-bit signed integer.
    Int64,
    /// 64-bit IEEE-754 float.
    Float64,
    /// UTF-8 string.
    String,
    /// A WGS84 coordinate pair in degrees (`docs/GEO.md` §5).
    ///
    /// The value type exists engine-wide, but DDL rejects `GeoPoint` columns
    /// until the geo storage codec provides the persisted layout and feature
    /// bit.
    GeoPoint,
    /// Fixed-dimension f32 vector, e.g. `Vector(768)`.
    Vector {
        /// Number of dimensions; part of the type.
        dim: u32,
    },
    /// Fixed-dimension vector persisted with a quantized element encoding.
    VectorEncoded {
        /// Number of dimensions; part of the type.
        dim: u32,
        /// Physical element encoding; part of the persisted column record.
        encoding: VectorEncoding,
    },
    /// UTC instant, microseconds since the Unix epoch.
    ///
    /// The value type exists engine-wide, but DDL rejects `Timestamp`
    /// columns until the scalar-v2 storage codec provides the persisted
    /// layout and the `SCALAR_TYPES_V2`
    /// feature bit (`docs/FORMAT.md` § Feature flag registry).
    Timestamp,
    /// Arbitrary byte string. STAGED like `Timestamp`.
    Bytes,
    /// Exact fixed-point decimal, `digits × 10^-scale` on an i128
    /// without floating-point representation. Both bounds are part of the
    /// type. Storage support follows the same staging as `Timestamp`.
    Decimal {
        /// Maximum significant digits, 1..=38; part of the type.
        precision: u8,
        /// Digits right of the point, ≤ precision; part of the type.
        scale: u8,
    },
    /// A JSON document held in canonical serialized text form. Storage
    /// support follows the same staging as `Timestamp`.
    Json,
}

impl LogicalType {
    /// The logical value type of a column of this type. `VectorEncoded`
    /// columns hold `Vector(dim)` values; every other type is its own value
    /// type.
    pub fn value_type(self) -> LogicalType {
        match self {
            Self::VectorEncoded { dim, .. } => Self::Vector { dim },
            other => other,
        }
    }

    /// The vector dimension of this type's values, if it is a vector type.
    pub fn vector_dim(self) -> Option<u32> {
        match self {
            Self::Vector { dim } | Self::VectorEncoded { dim, .. } => Some(dim),
            _ => None,
        }
    }
}

impl std::fmt::Display for LogicalType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Bool => write!(f, "Bool"),
            Self::Int64 => write!(f, "Int64"),
            Self::Float64 => write!(f, "Float64"),
            Self::String => write!(f, "String"),
            Self::GeoPoint => write!(f, "GeoPoint"),
            Self::Vector { dim } => write!(f, "Vector({dim})"),
            Self::VectorEncoded { dim, encoding } => {
                write!(f, "VectorEncoded({dim}, {encoding})")
            }
            Self::Timestamp => write!(f, "Timestamp"),
            Self::Bytes => write!(f, "Bytes"),
            Self::Decimal { precision, scale } => {
                write!(f, "Decimal({precision}, {scale})")
            }
            Self::Json => write!(f, "Json"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{B1Rescore, LogicalType, VectorEncoding};

    #[test]
    fn display_matches_plan_ir_spelling() {
        assert_eq!(LogicalType::Int64.to_string(), "Int64");
        assert_eq!(LogicalType::Vector { dim: 768 }.to_string(), "Vector(768)");
    }

    #[test]
    fn serde_round_trips() {
        let ty = LogicalType::Vector { dim: 3 };
        let json = serde_json::to_string(&ty).unwrap();
        let back: LogicalType = serde_json::from_str(&json).unwrap();
        assert_eq!(ty, back);
    }

    /// The exact catalog spellings from `docs/FORMAT.md` § Quantized vector
    /// element encodings. These strings are format law; a serialization
    /// change here is a format change.
    #[test]
    fn vector_encoded_canonical_spellings() {
        let cases = [
            (
                LogicalType::VectorEncoded {
                    dim: 64,
                    encoding: VectorEncoding::F16,
                },
                r#"{"VectorEncoded":{"dim":64,"encoding":{"kind":"f16"}}}"#,
            ),
            (
                LogicalType::VectorEncoded {
                    dim: 64,
                    encoding: VectorEncoding::I8,
                },
                r#"{"VectorEncoded":{"dim":64,"encoding":{"kind":"i8","scale":"per_vector_f32_le","offset":"per_vector_f32_le"}}}"#,
            ),
            (
                LogicalType::VectorEncoded {
                    dim: 64,
                    encoding: VectorEncoding::B1 {
                        rotation_seed: 0x4841_5348_2026_0803,
                        rescore: B1Rescore::F32,
                    },
                },
                r#"{"VectorEncoded":{"dim":64,"encoding":{"kind":"b1","rotation_seed":5206534213459118083,"rescore":"f32"}}}"#,
            ),
        ];
        for (ty, expected) in cases {
            let json = serde_json::to_string(&ty).unwrap();
            assert_eq!(json, expected);
            let back: LogicalType = serde_json::from_str(&json).unwrap();
            assert_eq!(back, ty);
        }
    }

    #[test]
    fn vector_encoded_rejects_malformed_encodings() {
        let rejected = [
            // Unknown kind.
            r#"{"VectorEncoded":{"dim":4,"encoding":{"kind":"f8"}}}"#,
            // Unknown field.
            r#"{"VectorEncoded":{"dim":4,"encoding":{"kind":"f16","extra":1}}}"#,
            // f16 with a b1 field.
            r#"{"VectorEncoded":{"dim":4,"encoding":{"kind":"f16","rotation_seed":1}}}"#,
            // i8 missing offset.
            r#"{"VectorEncoded":{"dim":4,"encoding":{"kind":"i8","scale":"per_vector_f32_le"}}}"#,
            // i8 with a wrong metadata location name.
            r#"{"VectorEncoded":{"dim":4,"encoding":{"kind":"i8","scale":"global_f32","offset":"per_vector_f32_le"}}}"#,
            // b1 missing rescore.
            r#"{"VectorEncoded":{"dim":4,"encoding":{"kind":"b1","rotation_seed":1}}}"#,
            // b1 with an unknown rescore representation.
            r#"{"VectorEncoded":{"dim":4,"encoding":{"kind":"b1","rotation_seed":1,"rescore":"f64"}}}"#,
        ];
        for json in rejected {
            assert!(
                serde_json::from_str::<LogicalType>(json).is_err(),
                "accepted malformed encoding: {json}"
            );
        }
    }

    #[test]
    fn value_type_folds_encoding_away() {
        let encoded = LogicalType::VectorEncoded {
            dim: 8,
            encoding: VectorEncoding::I8,
        };
        assert_eq!(encoded.value_type(), LogicalType::Vector { dim: 8 });
        assert_eq!(LogicalType::Bool.value_type(), LogicalType::Bool);
        assert_eq!(encoded.vector_dim(), Some(8));
        assert_eq!(LogicalType::Vector { dim: 3 }.vector_dim(), Some(3));
        assert_eq!(LogicalType::String.vector_dim(), None);
    }
}
