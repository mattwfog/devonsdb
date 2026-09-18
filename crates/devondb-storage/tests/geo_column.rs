use devondb_storage::geo_column::{decode_geo_points, encode_geo_points, fixed_geo_payload_len};
use devondb_types::{DevonError, GeoPoint, value::Value};
use proptest::{
    collection,
    prelude::*,
    test_runner::{Config as ProptestConfig, RngSeed},
};

const COLUMN_INDEX: usize = 37;
const PROPTEST_SEED: u64 = 0x4745_4f43_4f4c_3130;
const ROUND_TRIP_SEED: u64 = 0x1060_8a4d_c29f_7be1;

#[test]
fn seeded_points_and_nulls_round_trip_exact_values_and_bytes() {
    let column = seeded_column();
    let validity = encode_validity(&column);
    let mut encoded = Vec::new();
    encode_geo_points(&column, &mut encoded, COLUMN_INDEX).unwrap();

    let decoded = decode_geo_points(&validity, &encoded, column.len(), COLUMN_INDEX).unwrap();
    assert_eq!(decoded, column);

    let mut reencoded = Vec::new();
    encode_geo_points(&decoded, &mut reencoded, COLUMN_INDEX).unwrap();
    assert_eq!(reencoded, encoded);
}

#[test]
fn fixed_payload_length_includes_validity_and_rejects_overflow() {
    assert_eq!(fixed_geo_payload_len(0), Some(0));
    assert_eq!(fixed_geo_payload_len(1), Some(17));
    assert_eq!(fixed_geo_payload_len(9), Some(146));
    assert_eq!(fixed_geo_payload_len(usize::MAX), None);
}

#[test]
fn truncated_payload_is_corrupt() {
    assert_corrupt(&[1], &[0; 15], 1);
}

#[test]
fn oversized_payload_is_corrupt() {
    assert_corrupt(&[1], &[0; 17], 1);
}

#[test]
fn non_finite_bits_in_either_component_are_corrupt() {
    for (lat_deg, lng_deg) in [
        (f64::NAN, 0.0),
        (f64::INFINITY, 0.0),
        (0.0, f64::NEG_INFINITY),
        (0.0, f64::from_bits(0x7ff8_0000_0000_0001)),
    ] {
        assert_corrupt(&[1], &slot(lat_deg, lng_deg), 1);
    }
}

#[test]
fn out_of_range_latitude_is_corrupt() {
    for lat_deg in [-90.000_000_000_000_01, 90.000_000_000_000_01] {
        assert_corrupt(&[1], &slot(lat_deg, 0.0), 1);
    }
}

#[test]
fn out_of_range_longitude_including_positive_180_is_corrupt() {
    for lng_deg in [-180.000_000_000_000_03, 180.0, 180.000_000_000_000_03] {
        assert_corrupt(&[1], &slot(0.0, lng_deg), 1);
    }
}

#[test]
fn nonzero_pole_longitude_is_corrupt() {
    for (lat_deg, lng_deg) in [(90.0, f64::MIN_POSITIVE), (-90.0, -0.5)] {
        assert_corrupt(&[1], &slot(lat_deg, lng_deg), 1);
    }
}

#[test]
fn nonzero_bytes_in_a_null_slot_are_corrupt() {
    let mut values = [0_u8; 16];
    values[15] = 1;
    assert_corrupt(&[0], &values, 1);
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 512,
        failure_persistence: None,
        rng_seed: RngSeed::Fixed(PROPTEST_SEED),
        ..ProptestConfig::default()
    })]

    #[test]
    fn arbitrary_bytes_at_valid_lengths_never_panic(
        (row_count, validity, values) in arbitrary_geo_payload()
    ) {
        match decode_geo_points(&validity, &values, row_count, COLUMN_INDEX) {
            Ok(column) => prop_assert_eq!(column.len(), row_count),
            Err(DevonError::Corrupt { context }) => {
                let column_label = format!("column {COLUMN_INDEX}");
                prop_assert!(context.contains(&column_label));
            }
            Err(error) => prop_assert!(false, "unexpected error class: {}", error),
        }
    }
}

fn seeded_column() -> Vec<Value> {
    let smallest_subnormal = f64::from_bits(1);
    let fixtures = [
        geo(90.0, 0.0),
        geo(-90.0, -0.0),
        geo(0.0, -180.0),
        geo(0.0, 0.0),
        geo(-0.0, -0.0),
        geo(smallest_subnormal, -smallest_subnormal),
        geo(f64::MIN_POSITIVE, -f64::MIN_POSITIVE),
    ];
    let mut column = Vec::with_capacity(400);
    for (index, point) in fixtures.into_iter().enumerate() {
        column.push(point);
        if index % 2 == 0 {
            column.push(Value::Null);
        }
    }

    let mut random = SplitMix64::new(ROUND_TRIP_SEED);
    for row in 0..320 {
        if row % 5 == 0 || row % 17 == 0 {
            column.push(Value::Null);
        } else {
            let lat_deg = -89.0 + unit_interval(random.next()) * 178.0;
            let lng_deg = -179.0 + unit_interval(random.next()) * 358.0;
            column.push(geo(lat_deg, lng_deg));
        }
    }
    column
}

fn arbitrary_geo_payload() -> impl Strategy<Value = (usize, Vec<u8>, Vec<u8>)> {
    (0_usize..=64).prop_flat_map(|row_count| {
        (
            Just(row_count),
            collection::vec(any::<u8>(), row_count.div_ceil(8)),
            collection::vec(any::<u8>(), row_count * 16),
        )
    })
}

fn encode_validity(column: &[Value]) -> Vec<u8> {
    let mut validity = vec![0_u8; column.len().div_ceil(8)];
    for (row, value) in column.iter().enumerate() {
        if !matches!(value, Value::Null) {
            validity[row / 8] |= 1 << (row % 8);
        }
    }
    validity
}

fn geo(lat_deg: f64, lng_deg: f64) -> Value {
    Value::GeoPoint(GeoPoint::from_canonical(lat_deg, lng_deg).unwrap())
}

fn slot(lat_deg: f64, lng_deg: f64) -> [u8; 16] {
    let mut bytes = [0_u8; 16];
    bytes[..8].copy_from_slice(&lat_deg.to_le_bytes());
    bytes[8..].copy_from_slice(&lng_deg.to_le_bytes());
    bytes
}

fn assert_corrupt(validity: &[u8], values: &[u8], row_count: usize) {
    let error = decode_geo_points(validity, values, row_count, COLUMN_INDEX).unwrap_err();
    let DevonError::Corrupt { context } = error else {
        panic!("expected Corrupt, got {error}");
    };
    assert!(
        context.contains(&format!("column {COLUMN_INDEX}")),
        "corruption did not identify the column: {context}"
    );
}

fn unit_interval(random: u64) -> f64 {
    (random >> 11) as f64 / (1_u64 << 53) as f64
}

struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    const fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    fn next(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut value = self.state;
        value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        value ^ (value >> 31)
    }
}
