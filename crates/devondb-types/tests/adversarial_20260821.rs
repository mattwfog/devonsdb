//! Adversarial `devondb-types` tests against the binding specifications.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::str::FromStr;

use devondb_types::Decimal128;
use devondb_types::GeoPoint;
use devondb_types::decimal::MAX_PRECISION;
use devondb_types::logical_type::{B1Rescore, LogicalType, VectorEncoding};
use devondb_types::schema::{Column, NodeTableSchema, RelTableSchema};
use devondb_types::value::Value;

const ALL_DIGITS_NINES_39: &str = "999999999999999999999999999999999999999";
const ALL_DIGITS_NINES_38: &str = "99999999999999999999999999999999999999";

fn decimal(text: &str) -> Decimal128 {
    Decimal128::from_str(text).unwrap_or_else(|error| panic!("parse `{text}`: {error}"))
}

fn decimal_value(text: &str) -> Value {
    Value::Decimal(decimal(text))
}

fn json_value(text: &str) -> Value {
    Value::Json(text.to_owned())
}

fn bytes_value(length: usize) -> Value {
    Value::Bytes(vec![0x41; length])
}

/// PLAN_IR § Type system: every admitted value survives canonical JSON.
#[test]
fn every_value_variant_round_trips_json() {
    for value in all_value_samples() {
        let encoded = serde_json::to_string(&value)
            .unwrap_or_else(|error| panic!("serialize {value:?}: {error}"));
        let decoded: Value = serde_json::from_str(&encoded)
            .unwrap_or_else(|error| panic!("decode `{encoded}`: {error}"));
        assert_eq!(decoded, value, "round-trip failed through `{encoded}`");
    }
}

/// PLAN_IR § Type system: inferred logical types admit their own values.
#[test]
fn inferred_logical_type_admits_own_value() {
    for value in all_value_samples() {
        if value == Value::Null {
            continue;
        }
        let ty = value.logical_type().expect("non-null inference");
        assert!(
            value.matches_type(&ty),
            "type `{ty}` rejected its own value {value:?}"
        );
    }
}

/// PLAN_IR § Type system: Null is a value admitted by every column type.
#[test]
fn null_is_admitted_by_every_logical_type() {
    for ty in logical_type_samples() {
        assert!(Value::Null.matches_type(&ty), "Null refused by `{ty}`");
    }
}

/// PLAN_IR § Type system: vector dimension is part of the type.
#[test]
fn vector_dimension_mismatch_is_refused() {
    for dim in [0u32, 1, 2, 3, 7, 16, 63, 64] {
        assert!(Value::Vector(vec![1.; dim as usize]).matches_type(&LogicalType::Vector { dim }));
        assert!(
            !Value::Vector(vec![1.; dim as usize + 1]).matches_type(&LogicalType::Vector { dim })
        );
    }
}

/// PLAN_IR § Type system: JSON text is opaque; String never matches Json.
#[test]
fn string_never_matches_json_or_bytes() {
    for text in ["", "null", "{}", "quoted \"text\"", "unicode 🦀"] {
        assert!(json_value(text).matches_type(&LogicalType::Json));
        assert!(!Value::String(text.to_owned()).matches_type(&LogicalType::Json));
        assert!(!json_value(text).matches_type(&LogicalType::Bytes));
    }
}

/// PLAN_IR § Type system: Decimal comparisons are legal only at equal scales.
#[test]
fn same_numeric_different_scales_do_not_match() {
    for digits in [-1i128, 0, 1, 1250, i128::MAX] {
        for scale in 0..=38u8 {
            let left = Decimal128::new(digits, scale).unwrap();
            let right_scale = (scale % 11) + 1;
            let right = Decimal128::new(digits, right_scale).unwrap();
            assert_ne!(left.scale(), right.scale());
            assert!(!right.fits(left.precision(), left.scale()));
            assert!(!left.fits(right.precision(), right.scale()));
        }
    }
}

/// PLAN_IR § Type system: Decimal precision bounds declared digit capacity.
#[test]
fn decimal_precision_law_holds() {
    for digits in [-1i128, 0, 1] {
        let value = Decimal128::new(digits, 18).unwrap();
        assert_eq!(value.precision(), 1);
        assert!(value.fits(18, 18));
        assert!(Value::Decimal(value).matches_type(&LogicalType::Decimal {
            precision: 18,
            scale: 18
        }));
        assert!(value.fits(38, 18));
    }
}

/// Exact rounding rounds midpoint ties away from zero.
#[test]
fn midpoint_rounding_rounds_away_from_zero() {
    for (magnitude, places, expected_display) in [
        (5_i128, 0, "1.0"),
        (15, 0, "2.0"),
        (25, 0, "3.0"),
        (95, 0, "10.0"),
        (105, 0, "11.0"),
        (995, 0, "100.0"),
        (1005, 0, "101.0"),
        (5, 1, "0.5"),
        (15, 1, "1.5"),
        (25, 1, "2.5"),
        (95, 1, "9.5"),
        (105, 1, "10.5"),
        (995, 1, "99.5"),
        (1005, 1, "100.5"),
    ] {
        let positive = Decimal128::new(magnitude, 1).unwrap();
        let rounded = positive.round_to_places(places, MAX_PRECISION).unwrap();
        assert_eq!(
            rounded.to_string(),
            expected_display,
            "positive magnitude {magnitude}"
        );
        let negative = Decimal128::new(-magnitude, 1).unwrap();
        let rounded_negative = negative.round_to_places(places, MAX_PRECISION).unwrap();
        let expected_negative = format!("-{expected_display}");
        assert_eq!(
            rounded_negative.to_string(),
            expected_negative,
            "negative magnitude {magnitude}"
        );
    }
}

/// Decimal `round` and `round_div` retain the declared scale.
#[test]
fn round_preserves_declared_scale() {
    for digits in [-9999_i64, -105, -104, -1, 0, 1, 104, 105, 9999] {
        for places in 0..=4u8 {
            let original = Decimal128::new(i128::from(digits), 4).unwrap();
            let rounded = original.round_to_places(places, MAX_PRECISION).unwrap();
            assert_eq!(rounded.scale(), original.scale());
            let text = rounded.to_string();
            assert_eq!(text.split('.').nth(1).map(str::len), Some(4));
        }
    }
}

/// Decimal `round_div` uses one exact rational rounding step without floats.
#[test]
fn round_div_matches_exact_half_up_rational() {
    for numerator in [-999_999_i64, -501, -500, -1, 0, 1, 500, 501, 999_999] {
        for denominator in [1_i64, 2, 3, 7, 999] {
            let numerator_decimal = Decimal128::new(i128::from(numerator), 2).unwrap();
            let denominator_decimal = Decimal128::new(i128::from(denominator), 0).unwrap();
            let result = numerator_decimal
                .round_div_to_places(denominator_decimal, 2, MAX_PRECISION)
                .unwrap();
            let doubled = i128::from(numerator).abs() * 200;
            let mut expected_magnitude = doubled / (i128::from(denominator) * 200);
            if doubled % (i128::from(denominator) * 200) * 2 >= i128::from(denominator) * 200 {
                expected_magnitude += 1;
            }
            let signed_expected = if numerator < 0 {
                -expected_magnitude
            } else {
                expected_magnitude
            };
            assert_eq!(result.digits(), signed_expected);
            assert_eq!(result.scale(), 2);
        }
    }
}

/// PLAN_IR § Canonical JSON form: Json values preserve key order exactly.
#[test]
fn json_key_order_and_spacing_are_opaque_text() {
    for text in [
        r#"{"z":"first","a":["second",1]}"#,
        r#"{"a":1,"z":2}"#,
        r#"{"b":{},"a":[]}"#,
    ] {
        let value = json_value(text);
        let encoded = serde_json::to_string(&value).unwrap();
        assert_eq!(encoded, format!(r#"{{"Json":{text:?}}}"#));
        let decoded: Value = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, value);
    }
}

/// FORMAT § Catalog page: identifiers fold only ASCII uppercase.
#[test]
fn ascii_only_fold() {
    for name in [
        "",
        "A",
        "Z",
        "abc",
        "Table",
        "TABLE",
        "table",
        "_Id",
        "id-Δ",
        "🦀",
        "MiXeD_Ω",
        "ABCdefGHI",
    ] {
        let folded = devondb_types::schema::fold(name);
        let expected: String = name
            .chars()
            .map(|character| character.to_ascii_lowercase())
            .collect();
        assert_eq!(folded.as_ref(), &expected);
    }
}

/// FORMAT § Catalog page: fold-equal duplicate columns are DDL refusals.
#[test]
fn fold_equal_columns_are_rejected() {
    for (name, duplicate) in [("id", "ID"), ("Name", "name"), ("xY", "Xy"), ("aBc", "AbC")] {
        let columns = vec![
            Column {
                name: name.to_owned(),
                ty: LogicalType::Int64,
                primary_key: true,
            },
            Column {
                name: duplicate.to_owned(),
                ty: LogicalType::String,
                primary_key: false,
            },
        ];
        let error = NodeTableSchema::new("FoldTable".into(), columns).err();
        assert!(
            error.is_some(),
            "fold-equal duplicates accepted for `{name}`/`{duplicate}`"
        );
    }
}

/// FORMAT § Catalog page: distinct folded names remain valid and resolve foldedly.
#[test]
fn distinct_folded_names_are_accepted() {
    let schema = NodeTableSchema::new(
        "DistinctTable".into(),
        vec![
            Column {
                name: "Alpha".into(),
                ty: LogicalType::Int64,
                primary_key: true,
            },
            Column {
                name: "beta".into(),
                ty: LogicalType::String,
                primary_key: false,
            },
        ],
    )
    .unwrap();
    assert_eq!(schema.columns().len(), 2);
    assert_eq!(schema.column_index("ALPHA"), Some(0));
    assert_eq!(schema.column_index("Beta"), Some(1));
    assert_eq!(schema.column_index("gamma"), None);
}

/// FORMAT § Catalog page: relationship endpoint names must be nonempty.
#[test]
fn rel_endpoints_are_nonempty() {
    assert!(RelTableSchema::new("Rel".into(), String::new(), "Target".into(), Vec::new()).is_err());
    assert!(RelTableSchema::new("Rel".into(), "Source".into(), String::new(), Vec::new()).is_err());
    assert!(
        RelTableSchema::new("Rel".into(), "Source".into(), "Target".into(), Vec::new()).is_ok()
    );
}

/// FORMAT § Feature flag registry: staged scalar-v2 columns are node-only.
#[test]
fn staged_scalar_columns_are_node_only() {
    for ty in [
        LogicalType::Timestamp,
        LogicalType::Bytes,
        LogicalType::Decimal {
            precision: 10,
            scale: 2,
        },
        LogicalType::Json,
    ] {
        let node = NodeTableSchema::new(
            "Scalars".into(),
            vec![
                Column {
                    name: "id".into(),
                    ty: LogicalType::Int64,
                    primary_key: true,
                },
                Column {
                    name: "payload".into(),
                    ty,
                    primary_key: false,
                },
            ],
        );
        assert!(node.is_ok(), "node scalar-v2 column `{ty}` refused");
        let rel = RelTableSchema::new(
            "ScalarRel".into(),
            "A".into(),
            "B".into(),
            vec![Column {
                name: "payload".into(),
                ty,
                primary_key: false,
            }],
        );
        assert!(
            rel.is_err(),
            "relationship scalar-v2 column `{ty}` accepted"
        );
    }
}

/// FORMAT § Catalog page / PLAN_IR § Type system: Decimal type ranges are closed.
#[test]
fn invalid_decimal_column_ranges_are_refused() {
    for precision in 0..=39u8 {
        for scale in 0..=39u8 {
            let valid = (1..=MAX_PRECISION).contains(&precision) && scale <= precision;
            let result = NodeTableSchema::new(
                "Decimals".into(),
                vec![
                    Column {
                        name: "id".into(),
                        ty: LogicalType::Int64,
                        primary_key: true,
                    },
                    Column {
                        name: "amount".into(),
                        ty: LogicalType::Decimal { precision, scale },
                        primary_key: false,
                    },
                ],
            );
            assert_eq!(
                result.is_ok(),
                valid,
                "Decimal({precision},{scale}) validity mismatch"
            );
        }
    }
}

fn all_value_samples() -> Vec<Value> {
    let point = GeoPoint::from_canonical(-89.0, 179.75).unwrap();
    vec![
        Value::Null,
        Value::Bool(true),
        Value::Int64(i64::MIN),
        Value::Float64(-0.0),
        Value::String("quote\" newline\n unicode 🦀".into()),
        Value::Vector(vec![0., 1.5, f32::MIN_POSITIVE]),
        Value::GeoPoint(point),
        Value::Timestamp(-86_400_000_001),
        Value::Bytes(vec![0, 255, 128, 1]),
        decimal_value("-0.00000000000000000000000000000000000001"),
        json_value("{\"z\":null,\"a\":[]}"),
    ]
}

fn logical_type_samples() -> Vec<LogicalType> {
    vec![
        LogicalType::Bool,
        LogicalType::Int64,
        LogicalType::Float64,
        LogicalType::String,
        LogicalType::GeoPoint,
        LogicalType::Vector { dim: 0 },
        LogicalType::VectorEncoded {
            dim: 3,
            encoding: VectorEncoding::F16,
        },
        LogicalType::Timestamp,
        LogicalType::Bytes,
        LogicalType::Decimal {
            precision: 1,
            scale: 0,
        },
        LogicalType::Json,
    ]
}

/// PLAN_IR § Literals: Decimal tagged JSON is an exact string, never float.
#[test]
fn decimal_json_is_exact_string_form() {
    let cases = [
        ("12345678901234567890123456789012345678", 0),
        ("-0.00000000000000000000000000000000000001", 38),
        ("0.50000000000000000000000000000000000000", 38),
    ];
    for (text, fractional_len) in cases {
        let value = decimal(text);
        assert_eq!(value.scale(), u8::try_from(fractional_len).unwrap());
        let encoded = serde_json::to_string(&Value::Decimal(value)).unwrap();
        assert_eq!(encoded, format!(r#"{{"Decimal":{text:?}}}"#));
    }
}

/// PLAN_IR § Literals: decimal parser rejects exponent/float spellings.
#[test]
fn decimal_parser_rejects_float_spellings() {
    for text in [
        "1e5", "1E-5", "1.5e0", ".5", "5.", "", "-", "+", "nan", "inf", "0x10",
    ] {
        assert!(Decimal128::from_str(text).is_err(), "`{text}` accepted");
    }
}

/// PLAN_IR § Literals: decimal display is canonical and parse-stable.
#[test]
fn decimal_display_parse_round_trip() {
    for text in [
        "0",
        "-0.05",
        "42.0",
        "0.00000000000000000000000000000000000001",
        ALL_DIGITS_NINES_38,
    ] {
        let value = decimal(text);
        assert_eq!(value.to_string(), text);
        assert_eq!(decimal(&value.to_string()), value);
    }
}

/// PLAN_IR § Type system: maximum-scale decimals retain exactness.
#[test]
fn max_scale_decimal_stays_exact() {
    let tiny = decimal("0.00000000000000000000000000000000000001");
    assert_eq!(tiny.digits(), 1);
    assert_eq!(tiny.scale(), 38);
    let negative_tiny = decimal("-0.00000000000000000000000000000000000001");
    assert_eq!(negative_tiny.digits(), -1);
    assert_eq!(tiny.round_to_places(38, MAX_PRECISION).unwrap(), tiny);
    assert_eq!(
        negative_tiny.round_to_places(37, MAX_PRECISION).unwrap(),
        decimal("-0.00000000000000000000000000000000000000")
    );
}

/// PLAN_IR § Literals: precision beyond i128 decimal capacity is refused.
#[test]
fn decimal_overflow_is_clean_error() {
    assert!(Decimal128::from_str(ALL_DIGITS_NINES_39).is_err());
    let max_scaled = format!("1.{ALL_DIGITS_NINES_38}");
    assert!(Decimal128::from_str(&max_scaled).is_err());
    let huge = Decimal128::new(i128::MAX, 0).unwrap();
    assert_eq!(huge.precision(), 39);
    assert!(!huge.fits(MAX_PRECISION, 0));
}

/// Rounding rejects out-of-range places.
#[test]
fn rounding_place_bounds_are_enforced() {
    let value = decimal("12.34");
    for places in [3, 39, u8::MAX] {
        assert!(
            value.round_to_places(places, MAX_PRECISION).is_err(),
            "places={places}"
        );
    }
    assert!(
        decimal("1.23").round_to_places(0, 2).is_err(),
        "carry past p=2"
    );
    assert_eq!(
        decimal("1.23").round_to_places(0, 3).unwrap().to_string(),
        "1.00"
    );
}

/// Decimal `round_div` rejects a zero denominator and unrepresentable output.
#[test]
fn round_div_error_laws_hold() {
    let one = decimal("1.00");
    let zero = decimal("0.00");
    let huge = Decimal128::new(i128::MAX, 2).unwrap();
    let tiny = decimal("0.01");
    assert!(one.round_div_to_places(zero, 2, 4).is_err());
    assert!(huge.round_div_to_places(tiny, 38, 38).is_err());
    assert_eq!(
        one.round_div_to_places(decimal("4"), 2, 4).unwrap(),
        decimal("0.25")
    );
}

/// PLAN_IR § Canonical JSON form: Json carries arbitrary valid text exactly.
#[test]
fn json_value_escapes_all_control_and_quote_forms() {
    for byte in 0u8..=0x7f {
        let mut text = String::new();
        write!(&mut text, "{{\"k\":\"{}\"}}", byte as char).unwrap();
        let value = json_value(&text);
        let encoded = serde_json::to_string(&value).unwrap();
        let decoded: Value = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, value, "ASCII byte {byte:#04x}");
    }
}

/// PLAN_IR § Type system: Bytes are raw octets, independent of UTF-8 validity.
#[test]
fn bytes_survive_invalid_utf8_octets() {
    for byte in [0u8, 0x80, 0xfe, 0xff] {
        let value = Value::Bytes(vec![byte]);
        let encoded = serde_json::to_string(&value).unwrap();
        let decoded: Value = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, value, "byte {byte:#04x} through `{encoded}`");
    }
}

/// PLAN_IR § Literals / Canonical JSON form: every vector element is a JSON
/// number, so non-finite sentinels cannot survive a Value JSON round trip.
#[test]
#[ignore = "FINDING 2026-08-21: derived Value JSON emits non-finite vector elements as JSON null, violating the JSON-number vector form — PLAN_IR § Literals, crates/devondb-types/src/value.rs:12"]
fn non_finite_vector_sentinels_do_not_round_trip_json() {
    for element in [f32::NEG_INFINITY, f32::INFINITY, f32::NAN] {
        let value = Value::Vector(vec![element]);
        let encoded = serde_json::to_string(&value).unwrap();
        let decoded_result = serde_json::from_str::<Value>(&encoded);
        assert!(
            decoded_result.is_err(),
            "non-finite vector element round-tripped through `{encoded}`"
        );
    }
}

/// Decimal `round_div` rounds an exact rational quotient once.
#[test]
fn round_div_extreme_numerator_rounds_once() {
    let numerator = Decimal128::new(-999_999, 2).unwrap();
    let denominator = Decimal128::new(999, 0).unwrap();
    let result = numerator
        .round_div_to_places(denominator, 2, MAX_PRECISION)
        .unwrap();
    let doubled = (-999_999_i128 * 200).abs();
    let mut expected = doubled / (999 * 200);
    if doubled % (999 * 200) * 2 >= 999 * 200 {
        expected += 1;
    }
    assert_eq!(result.digits(), -expected);
}

/// PLAN_IR § Type system: GeoPoint canonical form refuses non-canonical decode.
#[test]
fn geo_point_decode_rejects_noncanonical_forms() {
    for json in [
        r#"{"lat_deg":0.0,"lng_deg":180.0}"#,
        r#"{"lat_deg":90.0,"lng_deg":1.0}"#,
        r#"{"lat_deg":91.0,"lng_deg":0.0}"#,
        r#"{"lat_deg":0.0,"lng_deg":-180.1}"#,
        r#"{"lat_deg":0.0,"lng_deg":0.0,"extra":1}"#,
    ] {
        let wrapped = format!(r#"{{"GeoPoint":{json}}}"#);
        assert!(
            serde_json::from_str::<Value>(&wrapped).is_err(),
            "accepted `{wrapped}`"
        );
    }
}

/// PLAN_IR § Type system: GeoPoint normalization folds only two spellings.
#[test]
fn geo_point_normalization_is_exact() {
    assert_eq!(
        GeoPoint::new(10., 180.).unwrap(),
        GeoPoint::from_canonical(10., -180.).unwrap()
    );
    assert_eq!(
        GeoPoint::new(90., 55.).unwrap(),
        GeoPoint::from_canonical(90., 0.).unwrap()
    );
    assert_eq!(
        GeoPoint::new(-90., -1.).unwrap(),
        GeoPoint::from_canonical(-90., 0.).unwrap()
    );
    assert!(GeoPoint::new(0., 180.5).is_err());
    assert!(GeoPoint::new(-90.5, 0.).is_err());
}

/// MVCC § One memory budget: scalar estimates are constant.
#[test]
fn approx_bytes_scalar_estimates_are_policy_constants() {
    assert_eq!(Value::Null.approx_bytes(), 16);
    assert_eq!(Value::Bool(false).approx_bytes(), 16);
    assert_eq!(Value::Int64(0).approx_bytes(), 16);
    assert_eq!(Value::Float64(0.).approx_bytes(), 16);
    assert_eq!(
        Value::GeoPoint(GeoPoint::from_canonical(0., 0.).unwrap()).approx_bytes(),
        24
    );
    assert_eq!(decimal_value("0").approx_bytes(), 24);
    assert_eq!(Value::Timestamp(0).approx_bytes(), 16);
}

/// MVCC § One memory budget: heap estimates grow monotonically with payload bytes.
#[test]
fn approx_bytes_heap_payloads_are_monotonic() {
    for length in [0usize, 1, 7, 63, 255, 4096, 65_536] {
        let string = Value::String("x".repeat(length));
        let bytes = bytes_value(length);
        let vector = Value::Vector(vec![1.; length / 4]);
        let json = json_value(&"x".repeat(length));
        assert_eq!(string.approx_bytes(), 32 + length);
        assert_eq!(bytes.approx_bytes(), 32 + length);
        assert_eq!(json.approx_bytes(), 32 + length);
        assert_eq!(vector.approx_bytes(), 32 + 4 * (length / 4));
    }
    let previous_string = Value::String("x".repeat(127));
    let next_string = Value::String("x".repeat(128));
    assert!(previous_string.approx_bytes() <= next_string.approx_bytes());
}

/// PLAN_IR § Type system: non-null inference is injective across variant families.
#[test]
fn logical_inference_discriminates_every_variant() {
    let samples = [
        Value::Null,
        Value::Bool(true),
        Value::Int64(1),
        Value::Float64(1.),
        Value::String("1".into()),
        Value::Vector(vec![1.]),
        Value::GeoPoint(GeoPoint::from_canonical(1., 0.).unwrap()),
        Value::Timestamp(1),
        Value::Bytes(vec![1]),
        decimal_value("1"),
        json_value("1"),
    ];
    let mut seen = BTreeMap::new();
    for (index, value) in samples.iter().enumerate() {
        if let Some(ty) = value.logical_type() {
            seen.insert(format!("{ty:?}"), index);
        }
    }
    assert_eq!(seen.len(), 10, "inferred types collapsed: {seen:?}");
}

/// FORMAT § Quantized encodings: encoded vectors expose the same value type/dim.
#[test]
fn vector_encoded_type_projection_is_exact() {
    for encoding in [
        VectorEncoding::F16,
        VectorEncoding::I8,
        VectorEncoding::B1 {
            rotation_seed: 1,
            rescore: B1Rescore::None,
        },
    ] {
        let encoded = LogicalType::VectorEncoded { dim: 7, encoding };
        assert_eq!(encoded.value_type(), LogicalType::Vector { dim: 7 });
        assert_eq!(encoded.vector_dim(), Some(7));
        assert!(Value::Vector(vec![0.; 7]).matches_type(&encoded));
        assert!(!Value::Vector(vec![0.; 8]).matches_type(&encoded));
    }
}

/// FORMAT § Quantized encodings: malformed physical encoding metadata is refused.
#[test]
fn vector_encoded_serde_rejects_malformed_metadata() {
    for json in [
        r#"{"VectorEncoded":{"dim":1,"encoding":{"kind":"unknown"}}}"#,
        r#"{"VectorEncoded":{"dim":1,"encoding":{"kind":"i8","scale":"per_vector_f32_le"}}}"#,
        r#"{"VectorEncoded":{"dim":1,"encoding":{"kind":"b1","rotation_seed":1}}}"#,
        r#"{"VectorEncoded":{"dim":1,"encoding":{"kind":"f16","extra":1}}}"#,
    ] {
        assert!(
            serde_json::from_str::<LogicalType>(json).is_err(),
            "accepted `{json}`"
        );
    }
}

/// PLAN_IR § Binding typing: unconstrained NULL uses Bool as deterministic carrier.
#[test]
fn unconstrained_null_carrier_admits_null() {
    for ty in [
        LogicalType::Bool,
        LogicalType::Int64,
        LogicalType::String,
        LogicalType::Decimal {
            precision: 1,
            scale: 0,
        },
        LogicalType::Vector { dim: 0 },
    ] {
        assert!(Value::Null.matches_type(&ty));
    }
}

/// PLAN_IR § Round-trip law: canonical JSON remains stable under re-encode.
#[test]
fn canonical_json_re_encoding_is_idempotent_for_scalar_v2() {
    let values = [
        Value::Timestamp(1_723_161_600_123_456),
        Value::Bytes(vec![0xde, 0xad]),
        decimal_value("19.99"),
        json_value("{\"b\":1,\"a\":2}"),
    ];
    for value in values {
        let first = serde_json::to_string(&value).unwrap();
        let decoded: Value = serde_json::from_str(&first).unwrap();
        let second = serde_json::to_string(&decoded).unwrap();
        assert_eq!(first, second);
    }
}
