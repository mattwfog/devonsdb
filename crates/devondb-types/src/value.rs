//! Runtime values (`docs/PLAN_IR.md` § Type system).

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::decimal::Decimal128;
use crate::geo_point::GeoPoint;
use crate::logical_type::LogicalType;

/// A runtime value in the v0 type system.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Value {
    /// A null value, admitted by every logical type in v0.
    Null,
    /// A boolean value.
    Bool(bool),
    /// A 64-bit signed integer value.
    Int64(i64),
    /// A 64-bit IEEE-754 floating-point value.
    Float64(f64),
    /// A UTF-8 string value.
    String(String),
    /// An f32 vector value whose length determines its logical dimension.
    Vector(Vec<f32>),
    /// A canonical WGS84 coordinate pair (`docs/GEO.md` §5). The inner
    /// type's serde enforces canonical form on every decode path.
    GeoPoint(GeoPoint),
    /// A UTC instant: microseconds since the Unix epoch.
    Timestamp(i64),
    /// An arbitrary byte string; the canonical literal form is lowercase
    /// hex.
    Bytes(Vec<u8>),
    /// An exact fixed-point decimal; the inner type's serde is the
    /// decimal string form, never a float.
    Decimal(Decimal128),
    /// A JSON document in canonical serialized text form. The
    /// canonical-form law (`docs/PLAN_IR.md` § Type system) is enforced
    /// at the parse boundaries that construct these values.
    Json(String),
}

impl Value {
    /// Returns whether this value is admitted by `ty`.
    #[must_use]
    pub fn matches_type(&self, ty: &LogicalType) -> bool {
        match (self, ty) {
            (Self::Null, _) => true,
            (Self::Bool(_), LogicalType::Bool)
            | (Self::Int64(_), LogicalType::Int64)
            | (Self::Float64(_), LogicalType::Float64)
            | (Self::String(_), LogicalType::String) => true,
            (
                Self::Vector(value),
                LogicalType::Vector { dim } | LogicalType::VectorEncoded { dim, .. },
            ) => value.len() == *dim as usize,
            (Self::GeoPoint(_), LogicalType::GeoPoint) => true,
            (Self::Timestamp(_), LogicalType::Timestamp) => true,
            (Self::Bytes(_), LogicalType::Bytes) => true,
            (Self::Decimal(value), LogicalType::Decimal { precision, scale }) => {
                value.fits(*precision, *scale)
            }
            (Self::Json(_), LogicalType::Json) => true,
            _ => false,
        }
    }

    /// Estimates this value's in-memory footprint for budget accounting.
    ///
    /// The constants are writer policy (docs/MVCC.md §7.2), not format:
    /// scalar variants cost 16 bytes; heap variants add a 32-byte
    /// container estimate plus their payload.
    #[must_use]
    pub fn approx_bytes(&self) -> usize {
        match self {
            Self::Null | Self::Bool(_) | Self::Int64(_) | Self::Float64(_) => 16,
            Self::String(value) => 32 + value.len(),
            Self::Vector(value) => 32 + 4 * value.len(),
            Self::GeoPoint(_) => 24,
            Self::Timestamp(_) => 16,
            Self::Bytes(value) => 32 + value.len(),
            Self::Decimal(_) => 24,
            Self::Json(value) => 32 + value.len(),
        }
    }

    /// Returns this value's logical type, or `None` for [`Value::Null`].
    #[must_use]
    pub fn logical_type(&self) -> Option<LogicalType> {
        match self {
            Self::Null => None,
            Self::Bool(_) => Some(LogicalType::Bool),
            Self::Int64(_) => Some(LogicalType::Int64),
            Self::Float64(_) => Some(LogicalType::Float64),
            Self::String(_) => Some(LogicalType::String),
            Self::Vector(value) => Some(LogicalType::Vector {
                dim: value.len() as u32,
            }),
            Self::GeoPoint(_) => Some(LogicalType::GeoPoint),
            Self::Timestamp(_) => Some(LogicalType::Timestamp),
            Self::Bytes(_) => Some(LogicalType::Bytes),
            Self::Decimal(value) => Some(LogicalType::Decimal {
                precision: value.precision(),
                scale: value.scale(),
            }),
            Self::Json(_) => Some(LogicalType::Json),
        }
    }
}

/// Formats epoch-microseconds as canonical ISO-8601 UTC (`docs/PLAN_IR.md`
/// § Literals). Years 0001–9999 use four digits; other years use a
/// mandatory sign and six digits. `.ffffff` is present iff the microsecond
/// part is nonzero. Proleptic Gregorian via the standard civil-from-days
/// algorithm; no dependencies.
fn format_timestamp_micros(f: &mut fmt::Formatter<'_>, micros: i64) -> fmt::Result {
    let days = micros.div_euclid(86_400_000_000);
    let of_day = micros.rem_euclid(86_400_000_000);
    let (year, month, day) = civil_from_days(days);
    let seconds = of_day / 1_000_000;
    let sub = of_day % 1_000_000;
    let (hour, minute, second) = (seconds / 3600, (seconds / 60) % 60, seconds % 60);
    format_timestamp_year(f, year)?;
    write!(f, "-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}")?;
    if sub != 0 {
        write!(f, ".{sub:06}")?;
    }
    write!(f, "Z")
}

fn format_timestamp_year(f: &mut fmt::Formatter<'_>, year: i64) -> fmt::Result {
    if (1..=9999).contains(&year) {
        write!(f, "{year:04}")
    } else if year < 0 {
        write!(f, "-{:06}", -year)
    } else {
        write!(f, "+{year:06}")
    }
}

/// Days-since-epoch → (year, month, day), proleptic Gregorian
/// (Howard Hinnant's `civil_from_days`).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Null => write!(f, "null"),
            Self::Bool(value) => write!(f, "{value}"),
            Self::Int64(value) => write!(f, "{value}"),
            Self::Float64(value) => write!(f, "{value}"),
            Self::String(value) => write!(f, "{value:?}"),
            Self::Vector(value) => {
                write!(f, "[")?;
                for (index, element) in value.iter().enumerate() {
                    if index > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{element}")?;
                }
                write!(f, "]")
            }
            Self::GeoPoint(value) => write!(f, "{value}"),
            Self::Timestamp(micros) => {
                write!(f, "timestamp(\"")?;
                format_timestamp_micros(f, *micros)?;
                write!(f, "\")")
            }
            Self::Bytes(value) => {
                write!(f, "bytes(\"")?;
                for byte in value {
                    write!(f, "{byte:02x}")?;
                }
                write!(f, "\")")
            }
            Self::Decimal(value) => write!(f, "decimal(\"{value}\")"),
            Self::Json(value) => write!(f, "json({value:?})"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Value;
    use crate::logical_type::LogicalType;

    fn geo(lat_deg: f64, lng_deg: f64) -> Value {
        Value::GeoPoint(crate::GeoPoint::from_canonical(lat_deg, lng_deg).unwrap())
    }

    /// The exact serde spelling from `docs/GEO.md` §5 — format law, like the
    /// VectorEncoded spellings.
    #[test]
    fn geo_point_canonical_spelling() {
        let value = geo(45.5, -122.625);
        let json = serde_json::to_string(&value).unwrap();
        assert_eq!(json, r#"{"GeoPoint":{"lat_deg":45.5,"lng_deg":-122.625}}"#);
        let back: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(back, value);
        assert_eq!(value.to_string(), "geo(45.5, -122.625)");
        assert!(
            serde_json::from_str::<Value>(r#"{"GeoPoint":{"lat_deg":10.0,"lng_deg":180.0}}"#)
                .is_err()
        );
    }

    #[test]
    fn geo_point_type_plumbing() {
        let value = geo(10.0, 20.0);
        assert!(value.matches_type(&LogicalType::GeoPoint));
        assert!(!value.matches_type(&LogicalType::Float64));
        assert!(!Value::Float64(1.0).matches_type(&LogicalType::GeoPoint));
        assert!(Value::Null.matches_type(&LogicalType::GeoPoint));
        assert_eq!(value.logical_type(), Some(LogicalType::GeoPoint));
        assert_eq!(value.approx_bytes(), 24);
    }

    #[test]
    fn matches_type_for_every_variant() {
        assert!(Value::Bool(true).matches_type(&LogicalType::Bool));
        assert!(Value::Int64(-42).matches_type(&LogicalType::Int64));
        assert!(Value::Float64(1.5).matches_type(&LogicalType::Float64));
        assert!(Value::String("devon".into()).matches_type(&LogicalType::String));
        assert!(Value::Vector(vec![0.1, 0.2]).matches_type(&LogicalType::Vector { dim: 2 }));
        let encoded = LogicalType::VectorEncoded {
            dim: 2,
            encoding: crate::logical_type::VectorEncoding::F16,
        };
        assert!(Value::Vector(vec![0.1, 0.2]).matches_type(&encoded));
        assert!(!Value::Vector(vec![0.1]).matches_type(&encoded));

        assert!(!Value::Bool(true).matches_type(&LogicalType::Int64));
        assert!(!Value::Int64(42).matches_type(&LogicalType::Float64));
        assert!(!Value::Float64(1.5).matches_type(&LogicalType::String));
        assert!(!Value::String("devon".into()).matches_type(&LogicalType::Bool));
        assert!(!Value::Vector(vec![0.1, 0.2]).matches_type(&LogicalType::Vector { dim: 3 }));
        assert!(!Value::Vector(vec![0.1, 0.2]).matches_type(&LogicalType::String));
    }

    #[test]
    fn null_matches_every_type() {
        for ty in [
            LogicalType::Bool,
            LogicalType::Int64,
            LogicalType::Float64,
            LogicalType::String,
            LogicalType::Vector { dim: 3 },
        ] {
            assert!(Value::Null.matches_type(&ty));
        }
        assert_eq!(Value::Null.logical_type(), None);
    }

    #[test]
    fn inferred_types_match_their_values() {
        let values = [
            Value::Bool(false),
            Value::Int64(i64::MIN),
            Value::Float64(2.5),
            Value::String("devon".into()),
            Value::Vector(vec![0.1, 0.2, 0.3]),
        ];

        for value in values {
            let ty = value.logical_type().unwrap();
            assert!(value.matches_type(&ty));
        }
    }

    #[test]
    fn serde_json_round_trips_every_variant() {
        let values = [
            Value::Null,
            Value::Bool(true),
            Value::Int64(-42),
            Value::Float64(1.5),
            Value::String("devon".into()),
            Value::Vector(vec![0.1, 0.2]),
        ];

        for value in values {
            let json = serde_json::to_string(&value).unwrap();
            let decoded: Value = serde_json::from_str(&json).unwrap();
            assert_eq!(decoded, value);
        }
    }

    #[test]
    fn display_uses_plan_text_spelling() {
        assert_eq!(Value::Null.to_string(), "null");
        assert_eq!(Value::Bool(true).to_string(), "true");
        assert_eq!(Value::Int64(-42).to_string(), "-42");
        assert_eq!(Value::Float64(1.5).to_string(), "1.5");
        assert_eq!(Value::String("devon".into()).to_string(), "\"devon\"");
        assert_eq!(Value::Vector(vec![0.1, 0.2]).to_string(), "[0.1, 0.2]");
    }

    /// Canonical text spellings for the staged scalar types
    /// (`docs/PLAN_IR.md` § Literals). The text-form parser must accept
    /// these strings and the printer must emit them.
    #[test]
    fn scalar_v2_display_spellings() {
        assert_eq!(
            Value::Timestamp(0).to_string(),
            "timestamp(\"1970-01-01T00:00:00Z\")"
        );
        assert_eq!(
            Value::Timestamp(1_723_161_600_123_456).to_string(),
            "timestamp(\"2024-08-09T00:00:00.123456Z\")"
        );
        assert_eq!(
            Value::Timestamp(-1_000_000).to_string(),
            "timestamp(\"1969-12-31T23:59:59Z\")"
        );
        assert_eq!(
            Value::Bytes(vec![0x00, 0xff, 0x1a]).to_string(),
            "bytes(\"00ff1a\")"
        );
        let money: crate::Decimal128 = "19.99".parse().unwrap();
        assert_eq!(Value::Decimal(money).to_string(), "decimal(\"19.99\")");
        assert_eq!(
            Value::Json("{\"k\":1}".into()).to_string(),
            "json(\"{\\\"k\\\":1}\")"
        );
    }

    #[test]
    fn scalar_v2_type_plumbing() {
        let money: crate::Decimal128 = "19.99".parse().unwrap();
        assert!(Value::Timestamp(1).matches_type(&LogicalType::Timestamp));
        assert!(Value::Bytes(vec![1]).matches_type(&LogicalType::Bytes));
        assert!(Value::Json("{}".into()).matches_type(&LogicalType::Json));
        assert!(Value::Decimal(money).matches_type(&LogicalType::Decimal {
            precision: 10,
            scale: 2
        }));
        assert!(
            !Value::Decimal(money).matches_type(&LogicalType::Decimal {
                precision: 3,
                scale: 2
            }),
            "4 digits must not fit precision 3"
        );
        assert!(
            !Value::Decimal(money).matches_type(&LogicalType::Decimal {
                precision: 10,
                scale: 4
            }),
            "scale mismatch is a type error, never a silent rescale"
        );
        assert!(!Value::Timestamp(1).matches_type(&LogicalType::Int64));
        assert!(!Value::Int64(1).matches_type(&LogicalType::Timestamp));
        assert!(!Value::Bytes(vec![1]).matches_type(&LogicalType::String));
        assert!(!Value::String("{}".into()).matches_type(&LogicalType::Json));
        for value in [
            Value::Timestamp(7),
            Value::Bytes(vec![9]),
            Value::Decimal(money),
            Value::Json("{}".into()),
        ] {
            let ty = value.logical_type().unwrap();
            assert!(value.matches_type(&ty));
            let json = serde_json::to_string(&value).unwrap();
            let back: Value = serde_json::from_str(&json).unwrap();
            assert_eq!(back, value);
        }
    }

    /// Format law: the WAL tagged form of a Decimal value carries the
    /// decimal STRING, never a JSON float.
    #[test]
    fn decimal_tagged_serde_is_exact() {
        let money: crate::Decimal128 = "-0.05".parse().unwrap();
        let json = serde_json::to_string(&Value::Decimal(money)).unwrap();
        assert_eq!(json, r#"{"Decimal":"-0.05"}"#);
    }
}
