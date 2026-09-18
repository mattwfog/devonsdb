use std::cmp::Ordering;

use devondb_storage::hnsw::{
    scoring::{ConstructionScorer, compare_distance_then_offset, level_for_node},
    types::{
        HnswConfig, HnswMetric, MAX_LEVEL, NavigationEncoding, NavigationScorer, supports_index,
    },
};
use devondb_types::{
    DevonError,
    logical_type::{B1Rescore, LogicalType, VectorEncoding},
};
use proptest::prelude::*;

const LEVEL_SEED: u64 = 0x484e_5357_2026_0803;
const QUERY: [f32; 4] = [1.0, 2.0, 3.0, 4.0];
const CANDIDATE: [f32; 4] = [4.0, 2.0, 0.0, -2.0];

#[test]
fn fixed_seed_and_offset_levels_are_deterministic() {
    let config = config(LEVEL_SEED, HnswMetric::L2, NavigationEncoding::F32);
    let fixtures = [(0, 0), (2, 1), (515, 2), (11_429, 3)];

    for (offset, expected) in fixtures {
        let first = level_for_node(&config, offset).unwrap();
        let second = level_for_node(&config, offset).unwrap();
        assert_eq!(first, expected);
        assert_eq!(second, expected);
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn level_assignment_is_a_pure_bounded_function(
        seed in any::<u64>(),
        offset in any::<u64>(),
        m in 4_u16..=64,
    ) {
        let mut config = config(seed, HnswMetric::Cosine, NavigationEncoding::F32);
        config.m = m;
        config.m0 = m * 2;
        config.ef_construction = u32::from(config.m0);

        let first = level_for_node(&config, offset).unwrap();
        let second = level_for_node(&config, offset).unwrap();
        prop_assert_eq!(first, second);
        prop_assert!(first <= MAX_LEVEL);
    }
}

#[test]
fn level_distribution_is_geometric_at_one_over_m() {
    const SAMPLE_COUNT: u64 = 200_000;
    let config = config(LEVEL_SEED, HnswMetric::Cosine, NavigationEncoding::F32);
    let mut promoted_once = 0_u64;
    let mut promoted_twice = 0_u64;

    for offset in 0..SAMPLE_COUNT {
        let level = level_for_node(&config, offset).unwrap();
        promoted_once += u64::from(level >= 1);
        promoted_twice += u64::from(level >= 2);
    }

    let observed_once = promoted_once as f64 / SAMPLE_COUNT as f64;
    let observed_twice = promoted_twice as f64 / SAMPLE_COUNT as f64;
    let expected_once = 1.0 / f64::from(config.m);
    let expected_twice = expected_once * expected_once;
    assert!((observed_once - expected_once).abs() < 0.003);
    assert!((observed_twice - expected_twice).abs() < 0.0008);
}

#[test]
fn f32_navigation_matches_brute_force_scalar_references_bit_for_bit() {
    let column = LogicalType::Vector { dim: 4 };
    let slot = f32_slot(&CANDIDATE);

    for metric in [HnswMetric::L2, HnswMetric::Cosine] {
        let scorer = scorer(metric, NavigationEncoding::F32, &column, &QUERY);
        let actual = scorer.navigation_distance(&slot).unwrap();

        // This independently mirrors the brute-force scalar fallbacks at
        // crates/devondb-exec/src/simd.rs:44 (L2) and :59 (cosine), including
        // f32 accumulation, the final L2 sqrt, and zero-norm cosine = 2.0.
        let expected = brute_force_scalar_reference(&QUERY, &CANDIDATE, metric);
        assert_eq!(actual.total_cmp(&expected), Ordering::Equal);
        assert_eq!(
            scorer.rescore_distance(&CANDIDATE).unwrap().to_bits(),
            expected.to_bits()
        );
    }
}

#[test]
fn f16_navigation_decodes_then_scores_both_supported_metrics() {
    let column = encoded_type(4, VectorEncoding::F16);
    let slot = f16_fixture_slot();

    assert_supported_scalar_encodings(&column, NavigationEncoding::F16, &slot);
}

#[test]
fn i8_navigation_decodes_then_scores_both_supported_metrics() {
    let column = encoded_type(4, VectorEncoding::I8);
    let slot = i8_fixture_slot();

    assert_supported_scalar_encodings(&column, NavigationEncoding::I8, &slot);
}

#[test]
fn b1_estimator_matches_an_independently_rotated_dim8_fixture() {
    let query = [1.0_f32, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0];
    let column = b1_type(8, 0, B1Rescore::F32);
    let scorer = scorer(HnswMetric::Cosine, NavigationEncoding::B1, &column, &query);

    // Hand application of FORMAT.md's SplitMix schedule for seed 0 gives
    // initial signs -,+,-,+,-,+,-,+ and round permutations:
    // [2,4,0,7,6,1,5,3], [6,1,7,0,5,2,4,3],
    // [7,5,1,6,4,3,0,2], [0,7,3,1,6,5,4,2].
    // Four binary32 butterflies produce these exact rotated-query bits:
    let rotated_bits = [
        0x3fbf_fffc,
        0xc0ef_fffe,
        0x4127_ffff,
        0x3f00_0003,
        0xbfbf_ffff,
        0x408f_ffff,
        0x3f00_0003,
        0x4060_0000,
    ];
    let rotated = rotated_bits.map(f32::from_bits);
    let candidate_slot = [0b1010_0110];
    let candidate_signs = [-1.0_f64, 1.0, 1.0, -1.0, -1.0, 1.0, -1.0, 1.0];
    let dot: f64 = rotated
        .iter()
        .zip(candidate_signs)
        .map(|(value, sign)| f64::from(*value) * sign)
        .sum();
    assert_eq!(dot, 9.999_999_523_162_842);
    let norm = query
        .iter()
        .map(|value| f64::from(*value).powi(2))
        .sum::<f64>()
        .sqrt();
    let expected = (1.0 - (dot / (norm * 8.0_f64.sqrt())).clamp(-1.0, 1.0)) as f32;
    assert_eq!(expected.to_bits(), 0x3f40_a16c);

    assert_eq!(
        scorer
            .navigation_distance(&candidate_slot)
            .unwrap()
            .to_bits(),
        expected.to_bits()
    );
}

#[test]
fn b1_all_configured_rescore_representations_score_decoded_vectors() {
    for rescore in [B1Rescore::F16, B1Rescore::I8, B1Rescore::F32] {
        let column = b1_type(4, 7, rescore);
        assert!(supports_index(&column, HnswMetric::Cosine));
        let scorer = scorer(HnswMetric::Cosine, NavigationEncoding::B1, &column, &QUERY);
        assert_eq!(scorer.rescore_distance(&CANDIDATE).unwrap(), 1.0);
    }
}

#[test]
fn unsupported_b1_index_combinations_are_rejected() {
    let rescored = b1_type(4, 7, B1Rescore::F32);
    assert!(!supports_index(&rescored, HnswMetric::L2));
    let l2_error = ConstructionScorer::new(
        &config(7, HnswMetric::L2, NavigationEncoding::B1),
        &rescored,
        &QUERY,
    )
    .unwrap_err();
    assert!(matches!(l2_error, DevonError::InvalidArgument { .. }));

    let no_rescore = b1_type(4, 7, B1Rescore::None);
    assert!(!supports_index(&no_rescore, HnswMetric::Cosine));
    let rescore_error = ConstructionScorer::new(
        &config(7, HnswMetric::Cosine, NavigationEncoding::B1),
        &no_rescore,
        &QUERY,
    )
    .unwrap_err();
    assert!(matches!(rescore_error, DevonError::InvalidArgument { .. }));
}

#[test]
fn malformed_encoded_slots_and_root_schema_mismatch_are_rejected() {
    let f16_column = encoded_type(4, VectorEncoding::F16);
    let f16_scorer = scorer(HnswMetric::L2, NavigationEncoding::F16, &f16_column, &QUERY);
    assert!(matches!(
        f16_scorer.navigation_distance(&[0; 7]),
        Err(DevonError::Corrupt { .. })
    ));

    let i8_column = encoded_type(4, VectorEncoding::I8);
    let i8_scorer = scorer(
        HnswMetric::Cosine,
        NavigationEncoding::I8,
        &i8_column,
        &QUERY,
    );
    let mut reserved_code = i8_fixture_slot();
    reserved_code[8] = 0x80;
    assert!(matches!(
        i8_scorer.navigation_distance(&reserved_code),
        Err(DevonError::Corrupt { .. })
    ));

    let mismatch = ConstructionScorer::new(
        &config(7, HnswMetric::L2, NavigationEncoding::F32),
        &f16_column,
        &QUERY,
    );
    assert!(matches!(mismatch, Err(DevonError::Corrupt { .. })));
}

#[test]
fn comparison_uses_total_distance_then_node_offset() {
    assert_eq!(
        compare_distance_then_offset(1.0, 8, 1.0, 13),
        Ordering::Less
    );
    assert_eq!(
        compare_distance_then_offset(-0.0, 99, 0.0, 1),
        Ordering::Less
    );
    assert_eq!(
        compare_distance_then_offset(f32::NAN, 1, f32::INFINITY, 0),
        f32::NAN.total_cmp(&f32::INFINITY)
    );
}

fn assert_supported_scalar_encodings(
    column: &LogicalType,
    navigation: NavigationEncoding,
    slot: &[u8],
) {
    for metric in [HnswMetric::L2, HnswMetric::Cosine] {
        assert!(supports_index(column, metric));
        let scorer = scorer(metric, navigation, column, &QUERY);
        let actual = scorer.navigation_distance(slot).unwrap();
        let expected = match metric {
            HnswMetric::L2 => 54.0_f32.sqrt(),
            HnswMetric::Cosine => 1.0,
        };
        assert_eq!(actual.to_bits(), expected.to_bits());
        assert_eq!(
            scorer.rescore_distance(&CANDIDATE).unwrap().to_bits(),
            expected.to_bits()
        );
    }
}

fn scorer(
    metric: HnswMetric,
    navigation: NavigationEncoding,
    column: &LogicalType,
    query: &[f32],
) -> ConstructionScorer {
    ConstructionScorer::new(&config(LEVEL_SEED, metric, navigation), column, query).unwrap()
}

fn config(level_seed: u64, metric: HnswMetric, navigation: NavigationEncoding) -> HnswConfig {
    HnswConfig::with_defaults(level_seed, metric, navigation)
}

fn encoded_type(dim: u32, encoding: VectorEncoding) -> LogicalType {
    LogicalType::VectorEncoded { dim, encoding }
}

fn b1_type(dim: u32, rotation_seed: u64, rescore: B1Rescore) -> LogicalType {
    encoded_type(
        dim,
        VectorEncoding::B1 {
            rotation_seed,
            rescore,
        },
    )
}

fn f32_slot(values: &[f32]) -> Vec<u8> {
    let mut slot = Vec::with_capacity(std::mem::size_of_val(values));
    for value in values {
        slot.extend(value.to_le_bytes());
    }
    slot
}

fn f16_fixture_slot() -> Vec<u8> {
    let mut slot = Vec::with_capacity(4 * size_of::<u16>());
    for bits in [0x4400_u16, 0x4000, 0x0000, 0xc000] {
        slot.extend(bits.to_le_bytes());
    }
    slot
}

fn i8_fixture_slot() -> Vec<u8> {
    let mut slot = Vec::with_capacity(12);
    slot.extend(0.5_f32.to_le_bytes());
    slot.extend(1.0_f32.to_le_bytes());
    slot.extend([6_u8, 2, (-2_i8) as u8, (-6_i8) as u8]);
    slot
}

fn brute_force_scalar_reference(left: &[f32], right: &[f32], metric: HnswMetric) -> f32 {
    match metric {
        HnswMetric::L2 => left
            .iter()
            .zip(right)
            .map(|(left, right)| (left - right) * (left - right))
            .sum::<f32>()
            .sqrt(),
        HnswMetric::Cosine => {
            let (dot, norm_left, norm_right) = left.iter().zip(right).fold(
                (0.0_f32, 0.0_f32, 0.0_f32),
                |(dot, norm_left, norm_right), (left, right)| {
                    (
                        dot + left * right,
                        norm_left + left * left,
                        norm_right + right * right,
                    )
                },
            );
            if norm_left == 0.0 || norm_right == 0.0 {
                2.0
            } else {
                1.0 - dot / (norm_left.sqrt() * norm_right.sqrt())
            }
        }
    }
}
