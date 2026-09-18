use devondb_storage::vector_encoding::{
    I8Metadata, VectorEncodingError, decode_b1, decode_b1_signs, decode_f16, decode_i8,
    decode_i8_metadata, encode_b1, encode_f16, encode_i8, estimate_b1_cosine_distance, rotate_b1,
};

const B1_SEED: u64 = 0x0123_4567_89ab_cdef;

#[test]
fn f16_exact_hand_computed_bit_patterns() {
    let minimum_subnormal = 2.0_f32.powi(-24);
    let largest_subnormal = 1023.0 * minimum_subnormal;
    let values = [
        (0.0, 0x0000),
        (-0.0, 0x8000),
        (1.0, 0x3c00),
        (-2.5, 0xc100),
        (65504.0, 0x7bff),
        (minimum_subnormal, 0x0001),
        (largest_subnormal, 0x03ff),
        (2.0_f32.powi(-14), 0x0400),
    ];
    let input: Vec<f32> = values.iter().map(|(value, _)| *value).collect();
    let encoded = encode_f16(&input);

    let actual: Vec<u16> = encoded
        .as_chunks::<2>()
        .0
        .iter()
        .map(|bytes| u16::from_le_bytes([bytes[0], bytes[1]]))
        .collect();
    let expected: Vec<u16> = values.iter().map(|(_, bits)| *bits).collect();
    assert_eq!(actual, expected);
}

#[test]
fn f16_rounds_halfway_cases_to_even() {
    let minimum_subnormal = 2.0_f32.powi(-24);
    let values = [
        1.0 + 2.0_f32.powi(-11),
        1.0 + 3.0 * 2.0_f32.powi(-11),
        minimum_subnormal * 0.5,
        minimum_subnormal * 1.5,
    ];
    let encoded = encode_f16(&values);
    let actual: Vec<u16> = encoded
        .as_chunks::<2>()
        .0
        .iter()
        .map(|bytes| u16::from_le_bytes([bytes[0], bytes[1]]))
        .collect();

    assert_eq!(actual, [0x3c00, 0x3c02, 0x0000, 0x0002]);
}

#[test]
fn f16_round_trip_stays_within_binary16_precision() {
    let values = [
        -65504.0,
        -123.8125,
        -std::f32::consts::PI,
        -0.000_071,
        -2.0_f32.powi(-24),
        0.0,
        2.0_f32.powi(-24),
        0.33333334,
        7.125,
        4095.25,
    ];
    let decoded = decode_f16(&encode_f16(&values)).unwrap();

    for (expected, actual) in values.iter().zip(decoded) {
        let half_ulp_bound = (expected.abs() * 2.0_f32.powi(-11)).max(2.0_f32.powi(-25))
            + expected.abs() * f32::EPSILON;
        assert!(
            (actual - expected).abs() <= half_ulp_bound,
            "{expected} became {actual}, bound {half_ulp_bound}"
        );
    }
}

#[test]
fn f16_decode_rejects_an_incomplete_element() {
    assert!(matches!(
        decode_f16(&[0x00]),
        Err(VectorEncodingError::InvalidLength {
            encoding: "f16",
            ..
        })
    ));
}

#[test]
fn every_finite_f16_pattern_round_trips_bit_exactly() {
    let patterns: Vec<u16> = (0_u16..=u16::MAX)
        .filter(|bits| bits & 0x7c00 != 0x7c00)
        .collect();
    let mut bytes = Vec::with_capacity(patterns.len() * 2);
    for bits in &patterns {
        bytes.extend(bits.to_le_bytes());
    }

    let decoded = decode_f16(&bytes).unwrap();
    assert_eq!(encode_f16(&decoded), bytes);
}

#[test]
fn i8_round_trip_error_is_bounded_by_half_a_step() {
    let values = [-11.0, -7.25, -1.0, -0.125, 0.0, 1.25, 6.75, 13.0];
    let encoded = encode_i8(&values);
    let metadata = decode_i8_metadata(&encoded).unwrap();
    let decoded = decode_i8(&encoded).unwrap();

    assert_eq!(encoded.len(), 8 + values.len());
    for (expected, actual) in values.iter().zip(decoded) {
        let rounding_slop = expected.abs().max(1.0) * f32::EPSILON * 2.0;
        assert!(
            (actual - expected).abs() <= metadata.scale * 0.5 + rounding_slop,
            "{expected} became {actual} at scale {}",
            metadata.scale
        );
    }
}

#[test]
fn i8_metadata_has_a_stable_little_endian_round_trip() {
    let encoded = encode_i8(&[-9.0, -1.0, 3.0, 7.0]);
    let metadata = decode_i8_metadata(&encoded).unwrap();
    let raw: [u8; 8] = encoded[..8].try_into().unwrap();

    assert_eq!(I8Metadata::from_le_bytes(raw), metadata);
    assert_eq!(metadata.to_le_bytes(), raw);
    assert_eq!(&raw[..4], &metadata.scale.to_le_bytes());
    assert_eq!(&raw[4..], &metadata.offset.to_le_bytes());
}

#[test]
fn i8_constant_vectors_use_zero_scale_and_zero_codes() {
    for constant in [3.25_f32, -0.0] {
        let values = [constant; 11];
        let encoded = encode_i8(&values);
        let metadata = decode_i8_metadata(&encoded).unwrap();
        let decoded = decode_i8(&encoded).unwrap();

        assert_eq!(metadata.scale.to_bits(), 0.0_f32.to_bits());
        assert_eq!(metadata.offset.to_bits(), constant.to_bits());
        assert!(encoded[8..].iter().all(|code| *code == 0));
        assert!(
            decoded
                .iter()
                .all(|value| value.to_bits() == constant.to_bits())
        );
    }
}

#[test]
fn i8_decode_rejects_reserved_and_inconsistent_codes() {
    let mut reserved = encode_i8(&[-1.0, 1.0]);
    reserved[8] = 0x80;
    assert_eq!(decode_i8(&reserved), Err(VectorEncodingError::InvalidI8));

    let mut inconsistent = encode_i8(&[4.0, 4.0]);
    inconsistent[9] = 1;
    assert_eq!(
        decode_i8(&inconsistent),
        Err(VectorEncodingError::InvalidI8)
    );
}

#[test]
fn b1_encoding_is_deterministic_for_a_seed() {
    let values = seeded_values(37, 0xfeed_face_cafe_beef);
    let first = encode_b1(&values, B1_SEED);
    let second = encode_b1(&values, B1_SEED);

    assert_eq!(first, [0xb4, 0xd1, 0x71, 0x8d, 0x16]);
    assert_eq!(first, second);
    assert_ne!(first, encode_b1(&values, B1_SEED + 1));
}

#[test]
fn b1_packing_uses_exactly_the_dimension_ceiling() {
    for dimension in [0_usize, 1, 7, 8, 9, 15, 16, 17, 31, 32, 33] {
        let encoded = encode_b1(&vec![0.25; dimension], B1_SEED);
        assert_eq!(
            encoded.len(),
            dimension.div_ceil(8),
            "dimension {dimension}"
        );
        if dimension % 8 != 0 {
            let padding_mask = !((1_u8 << (dimension % 8)) - 1);
            assert_eq!(encoded.last().unwrap() & padding_mask, 0);
        }
    }
}

#[test]
fn b1_recovers_every_rotated_sign() {
    let values = seeded_values(29, 91);
    let rotated = rotate_b1(&values, B1_SEED);
    let encoded = encode_b1(&values, B1_SEED);
    let signs = decode_b1_signs(&encoded, values.len()).unwrap();

    for (value, sign) in rotated.iter().zip(signs) {
        assert_eq!(sign, if *value >= 0.0 { 1.0 } else { -1.0 });
    }
}

#[test]
fn b1_dequantization_inverts_the_rotation() {
    let values = seeded_values(41, 17);
    let encoded = encode_b1(&values, B1_SEED);
    let decoded = decode_b1(&encoded, values.len(), B1_SEED).unwrap();
    let rerotated = rotate_b1(&decoded, B1_SEED);
    let signs = decode_b1_signs(&encoded, values.len()).unwrap();

    for (actual, expected) in rerotated.iter().zip(signs) {
        assert!((actual - expected).abs() < 2.0e-6);
    }
}

#[test]
fn b1_decode_rejects_nonzero_padding() {
    let mut encoded = encode_b1(&seeded_values(9, 44), B1_SEED);
    encoded[1] |= 0x80;

    assert_eq!(
        decode_b1_signs(&encoded, 9),
        Err(VectorEncodingError::NonZeroB1Padding { dimension: 9 })
    );
}

#[test]
fn b1_asymmetric_estimator_tracks_true_distance_on_seeded_corpus() {
    const DIMENSION: usize = 256;
    const CORPUS_SIZE: usize = 200;
    let mut generator = Lcg::new(0xd1ce_ba5e_1234_5678);
    let mut query = generator.vector(DIMENSION);
    normalize(&mut query);
    let mut true_distances = Vec::with_capacity(CORPUS_SIZE);
    let mut estimated_distances = Vec::with_capacity(CORPUS_SIZE);

    for index in 0..CORPUS_SIZE {
        let similarity = -0.95 + 1.9 * index as f32 / (CORPUS_SIZE - 1) as f32;
        let mut noise = generator.vector(DIMENSION);
        make_unit_orthogonal(&mut noise, &query);
        let noise_weight = (1.0 - similarity * similarity).sqrt();
        let candidate: Vec<f32> = query
            .iter()
            .zip(noise)
            .map(|(query_value, noise_value)| similarity * query_value + noise_weight * noise_value)
            .collect();
        let encoded = encode_b1(&candidate, B1_SEED);
        true_distances.push(cosine_distance(&query, &candidate));
        estimated_distances.push(estimate_b1_cosine_distance(&query, &encoded, B1_SEED).unwrap());
    }

    let correlation = rank_correlation(&true_distances, &estimated_distances);
    assert!(correlation >= 0.8, "rank correlation was {correlation}");
}

fn seeded_values(length: usize, seed: u64) -> Vec<f32> {
    Lcg::new(seed).vector(length)
}

fn normalize(values: &mut [f32]) {
    let norm = values.iter().map(|value| value * value).sum::<f32>().sqrt();
    for value in values {
        *value /= norm;
    }
}

fn make_unit_orthogonal(values: &mut [f32], reference: &[f32]) {
    let projection: f32 = values.iter().zip(reference).map(|(a, b)| a * b).sum();
    for (value, reference_value) in values.iter_mut().zip(reference) {
        *value -= projection * reference_value;
    }
    normalize(values);
}

fn cosine_distance(left: &[f32], right: &[f32]) -> f32 {
    let dot: f32 = left.iter().zip(right).map(|(a, b)| a * b).sum();
    let left_norm = left.iter().map(|value| value * value).sum::<f32>().sqrt();
    let right_norm = right.iter().map(|value| value * value).sum::<f32>().sqrt();
    1.0 - dot / (left_norm * right_norm)
}

fn rank_correlation(left: &[f32], right: &[f32]) -> f64 {
    let left_ranks = ranks(left);
    let right_ranks = ranks(right);
    let mean = (left.len() - 1) as f64 / 2.0;
    let numerator: f64 = left_ranks
        .iter()
        .zip(&right_ranks)
        .map(|(a, b)| (*a - mean) * (*b - mean))
        .sum();
    let left_sum: f64 = left_ranks.iter().map(|rank| (*rank - mean).powi(2)).sum();
    let right_sum: f64 = right_ranks.iter().map(|rank| (*rank - mean).powi(2)).sum();
    numerator / (left_sum * right_sum).sqrt()
}

fn ranks(values: &[f32]) -> Vec<f64> {
    let mut order: Vec<usize> = (0..values.len()).collect();
    order.sort_by(|left, right| values[*left].total_cmp(&values[*right]));
    let mut ranks = vec![0.0; values.len()];
    for (rank, index) in order.into_iter().enumerate() {
        ranks[index] = rank as f64;
    }
    ranks
}

struct Lcg {
    state: u64,
}

impl Lcg {
    const fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    fn next_f32(&mut self) -> f32 {
        self.state = self
            .state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        let unit = (self.state >> 40) as f32 / (1_u32 << 24) as f32;
        unit * 2.0 - 1.0
    }

    fn vector(&mut self, length: usize) -> Vec<f32> {
        (0..length).map(|_| self.next_f32()).collect()
    }
}
