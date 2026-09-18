use core::f64::consts::PI;

use devondb_geo::{
    GeoError,
    covering::{
        DiscCovering, FULL_CONTAINMENT_EPSILON_M, MAX_CENTER_TO_BOUNDARY_M, cover_disc,
        great_circle_meters,
    },
    grid,
    index::CellIndex,
    math::{Vec3, asin_det, cos_det, sin_det, sqrt_det},
    projection,
};
use h3o::{CellIndex as OracleCell, LatLng, Resolution};

const BOUND_SEED: u64 = 0x107c_0fee_5eed_b01d;
const SOUNDNESS_SEED: u64 = 0xd15c_c0a5_1070_0001;
const FULL_ORACLE_SEED: u64 = 0x286f_c011_7a1e_d00d;
const FINE_BOUND_SAMPLES: usize = 65_536;
const DISC_COUNT: usize = 200;
const PROBES_PER_DISC: usize = 200;
const FINE_FULL_CELL_SAMPLES: usize = 256;
const DESCENDANT_CENTER_SAMPLES: usize = 8;
const DEGREES_PER_RADIAN: f64 = 180.0 / PI;
const ORACLE_EARTH_RADIUS_M: f64 = 6_371_007.180_918_475;

#[test]
fn frozen_bounds_cover_oracle_measurements_with_safety_factor() {
    let measured = rederive_oracle_bounds();
    for (resolution, (maximum, frozen)) in measured
        .into_iter()
        .zip(MAX_CENTER_TO_BOUNDARY_M)
        .enumerate()
    {
        assert!(
            maximum * 1.3 <= frozen,
            "resolution {resolution}: measured {maximum} m, frozen {frozen} m"
        );
    }
}

#[test]
fn great_circle_matches_h3_authalic_oracle() {
    let cases = [
        ((0.0, 0.0), (0.0, 0.0)),
        ((48.864_716, 2.349_014), (31.224_361, 121.469_17)),
        ((0.0, 179.99), (0.0, -179.99)),
        ((89.9, -90.0), (89.9, 90.0)),
        ((-90.0, 0.0), (90.0, 0.0)),
    ];
    for (first, second) in cases {
        let oracle_first = LatLng::new(first.0, first.1).unwrap();
        let oracle_second = LatLng::new(second.0, second.1).unwrap();
        let expected = oracle_first.distance_m(oracle_second);
        let actual = great_circle_meters(first.0, first.1, second.0, second.1);
        assert!(
            (actual - expected).abs() <= 0.000_001,
            "{actual} != {expected}"
        );
    }
}

#[test]
fn covering_validates_all_inputs() {
    for (lat, lng, radius, res) in [
        (f64::NAN, 0.0, 1.0, 0),
        (91.0, 0.0, 1.0, 0),
        (0.0, f64::INFINITY, 1.0, 0),
        (0.0, 181.0, 1.0, 0),
        (0.0, 0.0, 0.0, 0),
        (0.0, 0.0, -1.0, 0),
        (0.0, 0.0, f64::INFINITY, 0),
    ] {
        assert!(cover_disc(lat, lng, radius, res).is_err());
    }
    assert_eq!(
        cover_disc(0.0, 0.0, 1.0, 16),
        Err(GeoError::InvalidResolution { resolution: 16 })
    );
    assert_eq!(
        cover_disc(95.0, 0.0, 1.0, 0),
        Err(GeoError::InvalidArgument {
            value_bits: 95.0_f64.to_bits(),
            reason: "latitude must be finite and in [-90, 90]".to_owned(),
        })
    );
    assert!(cover_disc(90.0, 180.0, 1.0, 0).is_ok());
}

#[test]
fn seeded_disc_coverings_are_sound() {
    let mut random = SplitMix64::new(SOUNDNESS_SEED);
    for disc_index in 0..DISC_COUNT {
        let (lat, lng) = uniform_sphere_point(&mut random);
        let radius = log_uniform_radius(&mut random);
        let resolution = bounded_resolution(radius);
        let covering = cover_disc(lat, lng, radius, resolution).unwrap();
        assert!(covering.full.len() + covering.boundary.len() < 10_000);
        check_disc_probes(
            disc_index,
            (lat, lng),
            radius,
            resolution,
            &covering,
            &mut random,
        );
    }
}

#[test]
fn full_ranges_are_sorted_merged_and_cell_derived() {
    let cases = [
        (0.0, 0.0, 100_000.0, 6),
        (37.7749, -122.4194, 25_000.0, 7),
        (-33.8688, 151.2093, 10_000.0, 8),
        (64.7, 10.5362, 100_000.0, 6),
        (0.0, 179.99, 50_000.0, 7),
        (89.9, 45.0, 30_000.0, 7),
    ];
    for (lat, lng, radius, resolution) in cases {
        let covering = cover_disc(lat, lng, radius, resolution).unwrap();
        assert_range_hygiene(&covering, resolution);
        let input_ranges = independently_classified_full_ranges(lat, lng, radius, resolution);
        let expected = reference_merge(input_ranges.clone());
        assert_eq!(covering.full, expected);
        assert!(covering.full.len() <= input_ranges.len());
    }
}

#[test]
fn full_containment_margin_is_sound_against_unrounded_oracle() {
    let center = (17.25, -42.5);
    let radius = 10_000_000.0;
    let mut random = SplitMix64::new(FULL_ORACLE_SEED);

    for resolution in 0..=3 {
        let covering = cover_disc(center.0, center.1, radius, resolution).unwrap();
        let oracle_resolution = Resolution::try_from(resolution).unwrap();
        let mut full_count = 0;
        for base in OracleCell::base_cells() {
            for cell in base.children(oracle_resolution) {
                if classify_cell(cell, &covering) == CellClass::Full {
                    verify_full_cell(cell, center, radius, &mut random);
                    full_count += 1;
                }
            }
        }
        assert!(full_count > 0, "no full cells verified at r{resolution}");
    }

    for resolution in 4..=15 {
        verify_seeded_fine_full_cells(resolution, center, &mut random);
    }
}

#[test]
fn full_containment_epsilon_demotes_to_boundary_recheck() {
    let cell = CellIndex::from_h3(u64::from(OracleCell::base_cells().next().unwrap())).unwrap();
    let center = projection::cell_center(cell);
    let radius = MAX_CENTER_TO_BOUNDARY_M[0] + FULL_CONTAINMENT_EPSILON_M * 0.5;
    let covering = cover_disc(center.0, center.1, radius, 0).unwrap();

    assert_eq!(
        classify_cell(OracleCell::try_from(cell.to_h3()).unwrap(), &covering),
        CellClass::Boundary
    );
}

#[test]
fn determinism_goldens_cover_scales_and_seams() {
    for golden in GOLDENS {
        let first = cover_disc(golden.lat, golden.lng, golden.radius, golden.resolution).unwrap();
        let second = cover_disc(golden.lat, golden.lng, golden.radius, golden.resolution).unwrap();
        assert_eq!(first, second, "{} was not deterministic", golden.name);
        assert_eq!(
            covering_hash(golden, &first),
            golden.hash,
            "{}",
            golden.name
        );
    }
}

#[test]
fn small_covering_goldens_have_exact_lists() {
    for exact in EXACT_GOLDENS {
        let covering = cover_disc(exact.lat, exact.lng, exact.radius, exact.resolution).unwrap();
        assert_eq!(covering.full, exact.full, "{} full", exact.name);
        assert_eq!(
            covering
                .boundary
                .iter()
                .map(|cell| cell.raw())
                .collect::<Vec<_>>(),
            exact.boundary,
            "{} boundary",
            exact.name
        );
    }
}

#[test]
fn antimeridian_and_near_pole_probes_are_covered() {
    assert_inside_probes_covered(
        (0.0, 179.99),
        5_000.0,
        9,
        &[(0.0, 179.98), (0.0, -179.99), (0.02, -179.995)],
    );
    assert_inside_probes_covered(
        (89.9, 45.0),
        30_000.0,
        8,
        &[(89.95, 45.0), (89.95, -135.0), (89.8, 135.0)],
    );
}

fn rederive_oracle_bounds() -> [f64; 16] {
    let mut maxima = [0.0_f64; 16];
    for resolution in 0_u8..=5 {
        let oracle_resolution = Resolution::try_from(resolution).unwrap();
        for base in OracleCell::base_cells() {
            for cell in base.children(oracle_resolution) {
                update_maximum(&mut maxima[usize::from(resolution)], cell);
            }
        }
    }
    sample_fine_bounds(&mut maxima);
    maxima
}

fn sample_fine_bounds(maxima: &mut [f64; 16]) {
    let bases = OracleCell::base_cells().collect::<Vec<_>>();
    let mut random = SplitMix64::new(BOUND_SEED);
    for resolution in 6_u8..=15 {
        let oracle_resolution = Resolution::try_from(resolution).unwrap();
        for _ in 0..FINE_BOUND_SAMPLES {
            let base = bases[(random.next() % bases.len() as u64) as usize];
            let position = random.next() % base.children_count(oracle_resolution);
            let cell = base.child_at(position, oracle_resolution).unwrap();
            update_maximum(&mut maxima[usize::from(resolution)], cell);
        }
    }
}

fn update_maximum(maximum: &mut f64, cell: OracleCell) {
    let center = LatLng::from(cell);
    for vertex in cell.boundary().iter().copied() {
        *maximum = maximum.max(center.distance_m(vertex));
    }
}

fn uniform_sphere_point(random: &mut SplitMix64) -> (f64, f64) {
    let z = 2.0 * random.unit() - 1.0;
    let lat = asin_det(z) * DEGREES_PER_RADIAN;
    let lng = 360.0 * random.unit() - 180.0;
    (lat, lng)
}

fn log_uniform_radius(random: &mut SplitMix64) -> f64 {
    const MANTISSAS: [f64; 16] = [
        1.0,
        1.154_781_984_689_458_3,
        1.333_521_432_163_324,
        1.539_926_526_059_492,
        1.778_279_410_038_923,
        2.053_525_026_457_146,
        2.371_373_705_661_655,
        2.738_419_634_264_361,
        3.162_277_660_168_379_5,
        3.651_741_272_548_377,
        4.216_965_034_285_822,
        4.869_675_251_658_631,
        5.623_413_251_903_491,
        6.493_816_315_762_114,
        7.498_942_093_324_558,
        8.659_643_233_600_654,
    ];
    const DECADES: [f64; 4] = [1.0, 10.0, 100.0, 1_000.0];
    let bin = (random.next() % 64) as usize;
    50.0 * DECADES[bin / 16] * MANTISSAS[bin % 16]
}

fn bounded_resolution(radius: f64) -> u8 {
    let minimum_bound = radius / 50.0;
    MAX_CENTER_TO_BOUNDARY_M
        .iter()
        .rposition(|bound| *bound >= minimum_bound)
        .unwrap_or(0) as u8
}

fn check_disc_probes(
    disc_index: usize,
    center: (f64, f64),
    radius: f64,
    resolution: u8,
    covering: &DiscCovering,
    random: &mut SplitMix64,
) {
    for probe_index in 0..PROBES_PER_DISC {
        let inside_sample = probe_index < PROBES_PER_DISC / 2;
        let unit = random.unit();
        let distance = if inside_sample {
            radius * sqrt_det(unit) * 0.999_999
        } else {
            radius * (1.000_001 + unit)
        };
        let bearing = 2.0 * PI * random.unit();
        let probe = destination(center, distance, bearing);
        assert_probe_law(
            disc_index,
            probe_index,
            center,
            radius,
            resolution,
            covering,
            probe,
        );
    }
}

fn assert_probe_law(
    disc_index: usize,
    probe_index: usize,
    center: (f64, f64),
    radius: f64,
    resolution: u8,
    covering: &DiscCovering,
    probe: (f64, f64),
) {
    let actual_distance = great_circle_meters(center.0, center.1, probe.0, probe.1);
    let atom = grid::atom(probe.0, probe.1).unwrap();
    let cell = atom.truncate_to(resolution).unwrap();
    let full = covering
        .full
        .iter()
        .any(|range| atom.raw() >= range.0 && atom.raw() <= range.1);
    let boundary = covering.boundary.binary_search(&cell).is_ok();
    let context = || {
        format!(
            "disc {disc_index}, probe {probe_index}, {center:?}, radius {radius}, res {resolution}, probe {probe:?}, distance {actual_distance}, cell {:#x}",
            cell.raw()
        )
    };
    if actual_distance <= radius {
        assert!(full || boundary, "inside probe uncovered: {}", context());
    }
    assert!(
        !full || actual_distance <= radius,
        "false FULL: {}",
        context()
    );
}

fn destination(center: (f64, f64), distance_m: f64, bearing: f64) -> (f64, f64) {
    const EARTH_RADIUS_M: f64 = 6_371_007.180_918_475;
    let lat = center.0 / DEGREES_PER_RADIAN;
    let lng = center.1 / DEGREES_PER_RADIAN;
    let point = Vec3::from_lat_lng_rad(lat, lng);
    let east = Vec3 {
        x: -sin_det(lng),
        y: cos_det(lng),
        z: 0.0,
    };
    let north = Vec3 {
        x: -sin_det(lat) * cos_det(lng),
        y: -sin_det(lat) * sin_det(lng),
        z: cos_det(lat),
    };
    let tangent = scaled_sum(north, cos_det(bearing), east, sin_det(bearing));
    let angle = distance_m / EARTH_RADIUS_M;
    let destination = scaled_sum(point, cos_det(angle), tangent, sin_det(angle));
    let (lat, lng) = destination.to_lat_lng_rad();
    (lat * DEGREES_PER_RADIAN, lng * DEGREES_PER_RADIAN)
}

fn scaled_sum(first: Vec3, first_scale: f64, second: Vec3, second_scale: f64) -> Vec3 {
    Vec3 {
        x: first.x * first_scale + second.x * second_scale,
        y: first.y * first_scale + second.y * second_scale,
        z: first.z * first_scale + second.z * second_scale,
    }
}

fn assert_range_hygiene(covering: &DiscCovering, resolution: u8) {
    for pair in covering.full.windows(2) {
        assert!(pair[0].0 <= pair[0].1);
        assert!(pair[0].1.saturating_add(1) < pair[1].0);
    }
    if let Some(last) = covering.full.last() {
        assert!(last.0 <= last.1);
    }
    for &(lower, upper) in &covering.full {
        assert!(range_endpoint_is_derived(lower, resolution, true));
        assert!(range_endpoint_is_derived(upper, resolution, false));
    }
    assert!(covering.boundary.windows(2).all(|pair| pair[0] < pair[1]));
    assert!(
        covering
            .boundary
            .iter()
            .all(|cell| cell.resolution() == resolution)
    );
}

fn independently_classified_full_ranges(
    lat: f64,
    lng: f64,
    radius: f64,
    resolution: u8,
) -> Vec<(u64, u64)> {
    let mut ranges = Vec::new();
    for base in OracleCell::base_cells() {
        collect_full_ranges(base, lat, lng, radius, resolution, &mut ranges);
    }
    ranges
}

fn collect_full_ranges(
    oracle: OracleCell,
    lat: f64,
    lng: f64,
    radius: f64,
    target: u8,
    ranges: &mut Vec<(u64, u64)>,
) {
    let cell = CellIndex::try_from(u64::from(oracle)).unwrap();
    let (center_lat, center_lng) = projection::cell_center(cell);
    let distance = great_circle_meters(lat, lng, center_lat, center_lng);
    let bound = MAX_CENTER_TO_BOUNDARY_M[usize::from(cell.resolution())];
    if distance + bound + FULL_CONTAINMENT_EPSILON_M <= radius {
        ranges.push(grid::descendant_atom_range(cell));
    } else if distance - bound <= radius && cell.resolution() < target {
        let next = Resolution::try_from(cell.resolution() + 1).unwrap();
        for child in oracle.children(next) {
            collect_full_ranges(child, lat, lng, radius, target, ranges);
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CellClass {
    Full,
    Boundary,
    Outside,
}

fn classify_cell(cell: OracleCell, covering: &DiscCovering) -> CellClass {
    let cell = CellIndex::from_h3(u64::from(cell)).unwrap();
    let cell_range = cell.atom_range();
    if covering
        .full
        .iter()
        .any(|range| range.0 <= cell_range.0 && cell_range.1 <= range.1)
    {
        CellClass::Full
    } else if covering.boundary.binary_search(&cell).is_ok() {
        CellClass::Boundary
    } else {
        CellClass::Outside
    }
}

fn verify_seeded_fine_full_cells(resolution: u8, center: (f64, f64), random: &mut SplitMix64) {
    let radius = 32.0 * MAX_CENTER_TO_BOUNDARY_M[usize::from(resolution)];
    let covering = cover_disc(center.0, center.1, radius, resolution).unwrap();
    let mut cells = Vec::with_capacity(FINE_FULL_CELL_SAMPLES);
    for _ in 0..FINE_FULL_CELL_SAMPLES {
        let distance = radius * 0.25 * sqrt_det(random.unit());
        let bearing = 2.0 * PI * random.unit();
        let point = destination(center, distance, bearing);
        cells.push(grid::cell(point.0, point.1, resolution).unwrap());
    }
    cells.sort_unstable();
    cells.dedup();

    let mut full_count = 0;
    for cell in cells {
        let oracle = OracleCell::try_from(cell.to_h3()).unwrap();
        if classify_cell(oracle, &covering) == CellClass::Full {
            verify_full_cell(oracle, center, radius, random);
            full_count += 1;
        }
    }
    assert!(
        full_count > 0,
        "no sampled full cells verified at r{resolution}"
    );
}

fn verify_full_cell(cell: OracleCell, center: (f64, f64), radius: f64, random: &mut SplitMix64) {
    for vertex in cell.boundary().iter().copied() {
        assert_oracle_inside(cell, center, radius, vertex, "boundary vertex");
    }

    let atom_resolution = Resolution::Fifteen;
    let descendant_count = cell.children_count(atom_resolution);
    for _ in 0..DESCENDANT_CENTER_SAMPLES {
        let position = random.next() % descendant_count;
        let descendant = cell.child_at(position, atom_resolution).unwrap();
        assert_oracle_inside(
            cell,
            center,
            radius,
            LatLng::from(descendant),
            "descendant atom center",
        );
    }
}

fn assert_oracle_inside(
    cell: OracleCell,
    center: (f64, f64),
    radius: f64,
    point: LatLng,
    probe: &str,
) {
    let distance = unrounded_haversine_meters(center, point);
    assert!(
        distance <= radius,
        "full cell {cell:#x} has {probe} at {distance} m outside radius {radius} m"
    );
}

fn unrounded_haversine_meters(first: (f64, f64), second: LatLng) -> f64 {
    let lat_a = first.0.to_radians();
    let lat_b = second.lat_radians();
    let delta_lat = (lat_b - lat_a) * 0.5;
    let delta_lng = (second.lng_radians() - first.1.to_radians()) * 0.5;
    let haversine = delta_lat.sin().powi(2) + lat_a.cos() * lat_b.cos() * delta_lng.sin().powi(2);
    let clamped = haversine.clamp(0.0, 1.0);
    2.0 * clamped.sqrt().atan2((1.0 - clamped).sqrt()) * ORACLE_EARTH_RADIUS_M
}

fn reference_merge(mut ranges: Vec<(u64, u64)>) -> Vec<(u64, u64)> {
    ranges.sort_unstable();
    let mut merged: Vec<(u64, u64)> = Vec::new();
    for range in ranges {
        if let Some(last) = merged.last_mut()
            && range.0 <= last.1.saturating_add(1)
        {
            last.1 = last.1.max(range.1);
            continue;
        }
        merged.push(range);
    }
    merged
}

fn range_endpoint_is_derived(raw: u64, target: u8, lower: bool) -> bool {
    let Ok(atom) = CellIndex::try_from(raw) else {
        return false;
    };
    if !atom.is_atom() {
        return false;
    }
    (0..=target).any(|resolution| {
        let cell = atom.truncate_to(resolution).unwrap();
        let range = grid::descendant_atom_range(cell);
        if lower {
            range.0 == raw
        } else {
            range.1 == raw
        }
    })
}

fn assert_inside_probes_covered(
    center: (f64, f64),
    radius: f64,
    resolution: u8,
    probes: &[(f64, f64)],
) {
    let covering = cover_disc(center.0, center.1, radius, resolution).unwrap();
    for &probe in probes {
        let distance = great_circle_meters(center.0, center.1, probe.0, probe.1);
        assert!(
            distance <= radius,
            "probe precondition: {probe:?} is {distance} m away"
        );
        let atom = grid::atom(probe.0, probe.1).unwrap();
        let cell = atom.truncate_to(resolution).unwrap();
        let full = covering
            .full
            .iter()
            .any(|range| (range.0..=range.1).contains(&atom.raw()));
        assert!(
            full || covering.boundary.binary_search(&cell).is_ok(),
            "uncovered {probe:?}"
        );
    }
}

#[derive(Clone, Copy)]
struct Golden {
    name: &'static str,
    lat: f64,
    lng: f64,
    radius: f64,
    resolution: u8,
    hash: u64,
}

const GOLDENS: [Golden; 8] = [
    Golden {
        name: "equator-100m",
        lat: 0.0,
        lng: 0.0,
        radius: 100.0,
        resolution: 10,
        hash: 3_755_704_988_213_342_835,
    },
    Golden {
        name: "san-francisco-500m",
        lat: 37.7749,
        lng: -122.4194,
        radius: 500.0,
        resolution: 9,
        hash: 15_686_936_400_469_109_677,
    },
    Golden {
        name: "london-1km",
        lat: 51.5074,
        lng: -0.1278,
        radius: 1_000.0,
        resolution: 9,
        hash: 3_953_908_836_181_527_644,
    },
    Golden {
        name: "sydney-10km",
        lat: -33.8688,
        lng: 151.2093,
        radius: 10_000.0,
        resolution: 8,
        hash: 6_427_596_039_316_733_959,
    },
    Golden {
        name: "pentagon-10km",
        lat: 64.700_000_127_934_89,
        lng: 10.536_199_075_467_67,
        radius: 10_000.0,
        resolution: 8,
        hash: 5_000_256_685_622_189_342,
    },
    Golden {
        name: "antimeridian-20km",
        lat: 0.0,
        lng: 179.99,
        radius: 20_000.0,
        resolution: 7,
        hash: 15_649_435_430_935_640_866,
    },
    Golden {
        name: "north-pole-50km",
        lat: 89.9,
        lng: 45.0,
        radius: 50_000.0,
        resolution: 7,
        hash: 16_644_545_424_099_890_431,
    },
    Golden {
        name: "tokyo-100km",
        lat: 35.6762,
        lng: 139.6503,
        radius: 100_000.0,
        resolution: 6,
        hash: 4_931_273_440_010_357_693,
    },
];

#[derive(Clone, Copy)]
struct ExactGolden {
    name: &'static str,
    lat: f64,
    lng: f64,
    radius: f64,
    resolution: u8,
    full: &'static [(u64, u64)],
    boundary: &'static [u64],
}

const EXACT_GOLDENS: [ExactGolden; 2] = [
    ExactGolden {
        name: "equator-100m",
        lat: 0.0,
        lng: 0.0,
        radius: 100.0,
        resolution: 10,
        full: &[],
        boundary: &[
            0x08a7_54e6_4990_7fff,
            0x08a7_54e6_4990_ffff,
            0x08a7_54e6_4991_ffff,
            0x08a7_54e6_4992_7fff,
            0x08a7_54e6_4992_ffff,
            0x08a7_54e6_4995_7fff,
            0x08a7_54e6_4996_7fff,
            0x08a7_54e6_4997_7fff,
            0x08a7_54e6_4d2c_7fff,
            0x08a7_54e6_4d2c_ffff,
            0x08a7_54e6_4d2d_7fff,
            0x08a7_54e6_4d2d_ffff,
        ],
    },
    ExactGolden {
        name: "san-francisco-500m",
        lat: 37.7749,
        lng: -122.4194,
        radius: 500.0,
        resolution: 9,
        full: &[(0x08f2_8308_2800_0000, 0x08f2_8308_2803_6db6)],
        boundary: &[
            0x0892_8308_2807_ffff,
            0x0892_8308_280b_ffff,
            0x0892_8308_280f_ffff,
            0x0892_8308_2813_ffff,
            0x0892_8308_2817_ffff,
            0x0892_8308_281b_ffff,
            0x0892_8308_2823_ffff,
            0x0892_8308_282b_ffff,
            0x0892_8308_2833_ffff,
            0x0892_8308_283b_ffff,
            0x0892_8308_2847_ffff,
            0x0892_8308_2857_ffff,
            0x0892_8308_2873_ffff,
            0x0892_8308_2877_ffff,
            0x0892_8308_288f_ffff,
            0x0892_8308_28ab_ffff,
            0x0892_8308_28bb_ffff,
            0x0892_8308_28c7_ffff,
        ],
    },
];

fn covering_hash(golden: Golden, covering: &DiscCovering) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325;
    for value in [
        golden.lat.to_bits(),
        golden.lng.to_bits(),
        golden.radius.to_bits(),
        u64::from(golden.resolution),
        covering.full.len() as u64,
        covering.boundary.len() as u64,
    ] {
        hash = hash_word(hash, value);
    }
    for &(lower, upper) in &covering.full {
        hash = hash_word(hash_word(hash, lower), upper);
    }
    for cell in &covering.boundary {
        hash = hash_word(hash, cell.raw());
    }
    hash
}

fn hash_word(mut hash: u64, value: u64) -> u64 {
    for byte in value.to_le_bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

struct SplitMix64(u64);

impl SplitMix64 {
    const fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut value = self.0;
        value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        value ^ (value >> 31)
    }

    fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 * (1.0 / ((1_u64 << 53) as f64))
    }
}
