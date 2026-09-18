use devondb_geo::{
    grid::{atom, cell, descendant_atom_range, h3_compat_cell},
    index::{CellIndex, PENTAGON_BASE_CELLS},
};
use h3o::{CellIndex as OracleCell, LatLng, Resolution};

const CONGRUENCE_SEED: u64 = 0x105c_0a67_2d13_84eb;
const RANGE_CELL_SEED: u64 = 0x105a_70b5_47e2_c619;
const RANGE_ATOM_SEED: u64 = 0x105d_e5ce_1da0_7a9e;
const ESCAPE_HATCH_SEED: u64 = 0x105e_5ca9_e07d_f22b;
const ORACLE_SEED: u64 = 0x1050_2ac1_e5f3_778d;
const INTEROP_SEED: u64 = 0x2860_0a11_cec0_ffee;
const CONGRUENCE_SAMPLES: usize = 20_000;
const RANGE_CELL_SAMPLES: usize = 300;
const RANGE_ATOM_SAMPLES: usize = 2_048;
const ESCAPE_HATCH_SAMPLES: usize = 32_768;
const ORACLE_SAMPLES: usize = 5_000;

// Frozen DevonGrid cell-assignment rows. The two points descend from distinct
// pentagon base cells (4 and 14), and each covers every resolution.
#[rustfmt::skip]
const CELL_GOLDENS: &[(u64, u64, u8, u64)] = &[
    (0x40504ccccd562b45, 0x40239288af6a8ee3, 0, 0x08009fffffffffff),
    (0x40504ccccd562b45, 0x40239288af6a8ee3, 1, 0x081083ffffffffff),
    (0x40504ccccd562b45, 0x40239288af6a8ee3, 2, 0x0820807fffffffff),
    (0x40504ccccd562b45, 0x40239288af6a8ee3, 3, 0x0830805fffffffff),
    (0x40504ccccd562b45, 0x40239288af6a8ee3, 4, 0x08408055ffffffff),
    (0x40504ccccd562b45, 0x40239288af6a8ee3, 5, 0x085080553fffffff),
    (0x40504ccccd562b45, 0x40239288af6a8ee3, 6, 0x0860805537ffffff),
    (0x40504ccccd562b45, 0x40239288af6a8ee3, 7, 0x0870805530ffffff),
    (0x40504ccccd562b45, 0x40239288af6a8ee3, 8, 0x08808055301fffff),
    (0x40504ccccd562b45, 0x40239288af6a8ee3, 9, 0x089080553007ffff),
    (0x40504ccccd562b45, 0x40239288af6a8ee3, 10, 0x08a0805530067fff),
    (0x40504ccccd562b45, 0x40239288af6a8ee3, 11, 0x08b0805530061fff),
    (0x40504ccccd562b45, 0x40239288af6a8ee3, 12, 0x08c0805530061dff),
    (0x40504ccccd562b45, 0x40239288af6a8ee3, 13, 0x08d0805530061cbf),
    (0x40504ccccd562b45, 0x40239288af6a8ee3, 14, 0x08e0805530061caf),
    (0x40504ccccd562b45, 0x40239288af6a8ee3, 15, 0x08f0805530061cad),
    (0x4048bd35b4c79008, 0xc061d34fca4abfdc, 0, 0x0801dfffffffffff),
    (0x4048bd35b4c79008, 0xc061d34fca4abfdc, 1, 0x0811c3ffffffffff),
    (0x4048bd35b4c79008, 0xc061d34fca4abfdc, 2, 0x0821c07fffffffff),
    (0x4048bd35b4c79008, 0xc061d34fca4abfdc, 3, 0x0831c03fffffffff),
    (0x4048bd35b4c79008, 0xc061d34fca4abfdc, 4, 0x0841c03bffffffff),
    (0x4048bd35b4c79008, 0xc061d34fca4abfdc, 5, 0x0851c03a7fffffff),
    (0x4048bd35b4c79008, 0xc061d34fca4abfdc, 6, 0x0861c03a47ffffff),
    (0x4048bd35b4c79008, 0xc061d34fca4abfdc, 7, 0x0871c03a44ffffff),
    (0x4048bd35b4c79008, 0xc061d34fca4abfdc, 8, 0x0881c03a441fffff),
    (0x4048bd35b4c79008, 0xc061d34fca4abfdc, 9, 0x0891c03a4403ffff),
    (0x4048bd35b4c79008, 0xc061d34fca4abfdc, 10, 0x08a1c03a4402ffff),
    (0x4048bd35b4c79008, 0xc061d34fca4abfdc, 11, 0x08b1c03a4402bfff),
    (0x4048bd35b4c79008, 0xc061d34fca4abfdc, 12, 0x08c1c03a4402b3ff),
    (0x4048bd35b4c79008, 0xc061d34fca4abfdc, 13, 0x08d1c03a4402b2ff),
    (0x4048bd35b4c79008, 0xc061d34fca4abfdc, 14, 0x08e1c03a4402b2e7),
    (0x4048bd35b4c79008, 0xc061d34fca4abfdc, 15, 0x08f1c03a4402b2e0),
];

#[test]
fn congruent_assignment_law_is_exact() {
    let mut random = SplitMix64::new(CONGRUENCE_SEED);
    for sample in 0..CONGRUENCE_SAMPLES {
        let (lat, lng) = random.uniform_sphere();
        let finest = cell(lat, lng, 15).unwrap();
        let mut previous = cell(lat, lng, 0).unwrap();

        for resolution in 1..=15 {
            let current = cell(lat, lng, resolution).unwrap();
            assert_eq!(
                current.parent(),
                Some(previous),
                "parent law failed at sample {sample}, ({lat}, {lng}), r{resolution}"
            );
            assert_eq!(
                current,
                finest.truncate_to(resolution).unwrap(),
                "atom-truncation law failed at sample {sample}, ({lat}, {lng}), r{resolution}"
            );
            previous = current;
        }
    }
}

#[test]
fn descendant_ranges_match_truncation_membership() {
    let mut cell_random = SplitMix64::new(RANGE_CELL_SEED);
    let mut samples = Vec::with_capacity(RANGE_CELL_SAMPLES);
    for sample in 0..RANGE_CELL_SAMPLES {
        let (lat, lng) = cell_random.uniform_sphere();
        let resolution = (sample % 15) as u8;
        samples.push((cell(lat, lng, resolution).unwrap(), atom(lat, lng).unwrap()));
    }

    let mut atom_random = SplitMix64::new(RANGE_ATOM_SEED);
    let atoms = (0..RANGE_ATOM_SAMPLES)
        .map(|_| {
            let (lat, lng) = atom_random.uniform_sphere();
            atom(lat, lng).unwrap()
        })
        .collect::<Vec<_>>();

    for (sample, &(sample_cell, source_atom)) in samples.iter().enumerate() {
        let range = descendant_atom_range(sample_cell);
        assert!(range.0 <= range.1, "reversed range at sample {sample}");
        assert_atom_membership(sample_cell, source_atom, range, sample);
        for &candidate in &atoms {
            assert_atom_membership(sample_cell, candidate, range, sample);
        }
    }
}

#[test]
fn h3_escape_hatch_is_an_adjacent_crinkle_band() {
    let mut random = SplitMix64::new(ESCAPE_HATCH_SEED);
    let mut divergences = 0_usize;

    for sample in 0..ESCAPE_HATCH_SAMPLES {
        let (lat, lng) = random.uniform_sphere();
        let resolution = (random.next() % 16) as u8;
        let direct = h3_compat_cell(lat, lng, resolution).unwrap();
        let congruent = cell(lat, lng, resolution).unwrap();
        if direct != congruent {
            assert_oracle_neighbors(direct, congruent, sample, lat, lng, resolution);
            divergences += 1;
        }
    }

    // GEO.md §1 records a measured 5.91% band. Keep a deliberately broad
    // physical bound while requiring the distinction to remain observable.
    assert!(
        divergences * 100 >= ESCAPE_HATCH_SAMPLES && divergences * 100 <= ESCAPE_HATCH_SAMPLES * 8,
        "{divergences} divergences in {ESCAPE_HATCH_SAMPLES} comparisons"
    );
}

#[test]
fn h3_escape_hatch_matches_oracle_bit_for_bit() {
    let mut random = SplitMix64::new(ORACLE_SEED);
    for sample in 0..ORACLE_SAMPLES {
        let (lat, lng) = random.uniform_sphere();
        let resolution = (random.next() % 16) as u8;
        let actual = h3_compat_cell(lat, lng, resolution).unwrap();
        let expected = oracle_cell_at(lat, lng, resolution);
        assert_eq!(
            actual.raw(),
            u64::from(expected),
            "oracle mismatch at sample {sample}, ({lat}, {lng}), r{resolution}"
        );
    }
}

#[test]
fn h3_interop_round_trips_sampled_cells_at_every_resolution() {
    let mut random = SplitMix64::new(INTEROP_SEED);
    for resolution in 0..=15 {
        for sample in 0..256 {
            let (lat, lng) = random.uniform_sphere();
            let oracle = oracle_cell_at(lat, lng, resolution);
            let cell = CellIndex::from_h3(u64::from(oracle)).unwrap();

            assert_eq!(cell.to_h3(), u64::from(oracle));
            assert_eq!(CellIndex::from_h3(cell.to_h3()).unwrap(), cell);
            assert_eq!(
                OracleCell::try_from(cell.to_h3()).unwrap(),
                oracle,
                "interop mismatch at r{resolution}, sample {sample}"
            );
        }
    }

    assert!(CellIndex::from_h3(0).is_err());
}

#[test]
fn congruent_cell_goldens_cover_resolutions_and_pentagons() {
    assert!(CELL_GOLDENS.len() >= 32);
    let mut resolutions = [false; 16];
    let mut base_cells = [false; 122];

    for &(lat_bits, lng_bits, resolution, expected) in CELL_GOLDENS {
        let actual = cell(
            f64::from_bits(lat_bits),
            f64::from_bits(lng_bits),
            resolution,
        )
        .unwrap();
        assert_eq!(
            actual.raw(),
            expected,
            "cell golden ({lat_bits:#018x}, {lng_bits:#018x}, r{resolution})"
        );
        resolutions[usize::from(resolution)] = true;
        base_cells[usize::from(actual.base_cell())] = true;
    }

    assert!(resolutions.into_iter().all(|covered| covered));
    let covered_pentagons = PENTAGON_BASE_CELLS
        .into_iter()
        .filter(|base_cell| base_cells[usize::from(*base_cell)])
        .count();
    assert!(covered_pentagons >= 2);
}

#[test]
fn atom_is_the_finest_cell_assignment() {
    for (lat, lng) in [(0.0, 0.0), (90.0, 0.0), (-90.0, 0.0), (31.25, -180.0)] {
        assert_eq!(atom(lat, lng).unwrap(), cell(lat, lng, 15).unwrap());
    }
    assert!(atom(f64::NAN, 0.0).is_err());
    assert!(cell(0.0, 0.0, 16).is_err());
    assert!(h3_compat_cell(0.0, 0.0, 16).is_err());
}

fn assert_atom_membership(
    sample_cell: CellIndex,
    candidate: CellIndex,
    range: (u64, u64),
    sample: usize,
) {
    let raw_is_inside = range.0 <= candidate.raw() && candidate.raw() <= range.1;
    let truncates_to_cell = candidate.truncate_to(sample_cell.resolution()).unwrap() == sample_cell;
    assert_eq!(
        raw_is_inside,
        truncates_to_cell,
        "range/truncation disagreement for cell sample {sample}: cell={:#x}, atom={:#x}",
        sample_cell.raw(),
        candidate.raw()
    );
}

fn assert_oracle_neighbors(
    first: CellIndex,
    second: CellIndex,
    sample: usize,
    lat: f64,
    lng: f64,
    resolution: u8,
) {
    let first = OracleCell::try_from(first.raw()).unwrap();
    let second = OracleCell::try_from(second.raw()).unwrap();
    assert!(
        first.is_neighbor_with(second).unwrap(),
        "non-neighbor divergence at sample {sample}, ({lat}, {lng}), r{resolution}: \
         direct={:#x}, congruent={:#x}",
        u64::from(first),
        u64::from(second)
    );
}

fn oracle_cell_at(lat: f64, lng: f64, resolution: u8) -> OracleCell {
    LatLng::new(lat, lng)
        .unwrap()
        .to_cell(Resolution::try_from(resolution).unwrap())
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

    fn uniform_sphere(&mut self) -> (f64, f64) {
        // z = sin(latitude) is uniform on [-1, 1); longitude is uniform on
        // [-180, 180). High 53-bit fractions retain exact binary64 spacing.
        let z = 2.0 * unit_interval(self.next()) - 1.0;
        let latitude = z.asin().to_degrees();
        let longitude = 360.0 * unit_interval(self.next()) - 180.0;
        (latitude, longitude)
    }
}

fn unit_interval(value: u64) -> f64 {
    (value >> 11) as f64 * (1.0 / ((1_u64 << 53) as f64))
}
