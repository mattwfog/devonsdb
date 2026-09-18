use devondb_geo::{
    error::GeoError,
    index::{CellIndex, PENTAGON_BASE_CELLS},
    projection::{atom_of, cell_at, cell_center},
};
use h3o::{CellIndex as OracleCell, LatLng, Resolution};

const ASSIGNMENT_SEED: u64 = 0x104a_5519_4e2d_c3b7;
const TRUNCATION_SEED: u64 = 0x104c_0a97_72e8_614d;
const CENTER_SEED: u64 = 0x104c_e117_e12a_8fd3;
const ASSIGNMENT_SAMPLES: usize = 20_000;
const PROPERTY_SAMPLES: usize = 2_048;
const CENTER_TOLERANCE_DEGREES: f64 = 1e-11;

// Frozen from this implementation. Six non-center points cover every
// resolution and six base cells, including pentagons 4 and 14.
#[rustfmt::skip]
const DETERMINISM_GOLDENS: &[(u64, u64, u8, u64)] = &[
    (0x4053b783750a00e9, 0x404352ff0036dd29, 0, 0x08001fffffffffff),
    (0x4053b783750a00e9, 0x404352ff0036dd29, 1, 0x081003ffffffffff),
    (0x4053b783750a00e9, 0x404352ff0036dd29, 2, 0x0820007fffffffff),
    (0x4053b783750a00e9, 0x404352ff0036dd29, 3, 0x0830000fffffffff),
    (0x4053b783750a00e9, 0x404352ff0036dd29, 4, 0x0840000dffffffff),
    (0x4053b783750a00e9, 0x404352ff0036dd29, 5, 0x0850000c3fffffff),
    (0x4053b783750a00e9, 0x404352ff0036dd29, 6, 0x0860000c07ffffff),
    (0x4053b783750a00e9, 0x404352ff0036dd29, 7, 0x0870000c01ffffff),
    (0x4053b783750a00e9, 0x404352ff0036dd29, 8, 0x0880000c015fffff),
    (0x4053b783750a00e9, 0x404352ff0036dd29, 9, 0x0890000c014bffff),
    (0x4053b783750a00e9, 0x404352ff0036dd29, 10, 0x08a0000c014affff),
    (0x4053b783750a00e9, 0x404352ff0036dd29, 11, 0x08b0000c014a8fff),
    (0x4053b783750a00e9, 0x404352ff0036dd29, 12, 0x08c0000c014a83ff),
    (0x4053b783750a00e9, 0x404352ff0036dd29, 13, 0x08d0000c014a833f),
    (0x4053b783750a00e9, 0x404352ff0036dd29, 14, 0x08e0000c014a8337),
    (0x4053b783750a00e9, 0x404352ff0036dd29, 15, 0x08f0000c014a8334),
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
    (0x4038cdc56535eaf9, 0x4060370971982174, 0, 0x0804bfffffffffff),
    (0x4038cdc56535eaf9, 0x4060370971982174, 1, 0x0814a3ffffffffff),
    (0x4038cdc56535eaf9, 0x4060370971982174, 2, 0x0824a07fffffffff),
    (0x4038cdc56535eaf9, 0x4060370971982174, 3, 0x0834a04fffffffff),
    (0x4038cdc56535eaf9, 0x4060370971982174, 4, 0x0844a043ffffffff),
    (0x4038cdc56535eaf9, 0x4060370971982174, 5, 0x0854a043bfffffff),
    (0x4038cdc56535eaf9, 0x4060370971982174, 6, 0x0864a043a7ffffff),
    (0x4038cdc56535eaf9, 0x4060370971982174, 7, 0x0874a043a5ffffff),
    (0x4038cdc56535eaf9, 0x4060370971982174, 8, 0x0884a043a57fffff),
    (0x4038cdc56535eaf9, 0x4060370971982174, 9, 0x0894a043a567ffff),
    (0x4038cdc56535eaf9, 0x4060370971982174, 10, 0x08a4a043a575ffff),
    (0x4038cdc56535eaf9, 0x4060370971982174, 11, 0x08b4a043a575bfff),
    (0x4038cdc56535eaf9, 0x4060370971982174, 12, 0x08c4a043a575b5ff),
    (0x4038cdc56535eaf9, 0x4060370971982174, 13, 0x08d4a043a575b5bf),
    (0x4038cdc56535eaf9, 0x4060370971982174, 14, 0x08e4a043a575b58f),
    (0x4038cdc56535eaf9, 0x4060370971982174, 15, 0x08f4a043a575b58b),
    (0xc0274a57fcf59d5e, 0xc05a2d6d1a4e99a6, 0, 0x08093fffffffffff),
    (0xc0274a57fcf59d5e, 0xc05a2d6d1a4e99a6, 1, 0x081923ffffffffff),
    (0xc0274a57fcf59d5e, 0xc05a2d6d1a4e99a6, 2, 0x0829207fffffffff),
    (0xc0274a57fcf59d5e, 0xc05a2d6d1a4e99a6, 3, 0x0839201fffffffff),
    (0xc0274a57fcf59d5e, 0xc05a2d6d1a4e99a6, 4, 0x08492015ffffffff),
    (0xc0274a57fcf59d5e, 0xc05a2d6d1a4e99a6, 5, 0x08592014ffffffff),
    (0xc0274a57fcf59d5e, 0xc05a2d6d1a4e99a6, 6, 0x08692014c7ffffff),
    (0xc0274a57fcf59d5e, 0xc05a2d6d1a4e99a6, 7, 0x08792014c0ffffff),
    (0xc0274a57fcf59d5e, 0xc05a2d6d1a4e99a6, 8, 0x08892014c01fffff),
    (0xc0274a57fcf59d5e, 0xc05a2d6d1a4e99a6, 9, 0x08992014c01bffff),
    (0xc0274a57fcf59d5e, 0xc05a2d6d1a4e99a6, 10, 0x08a92014c0187fff),
    (0xc0274a57fcf59d5e, 0xc05a2d6d1a4e99a6, 11, 0x08b92014c0183fff),
    (0xc0274a57fcf59d5e, 0xc05a2d6d1a4e99a6, 12, 0x08c92014c018edff),
    (0xc0274a57fcf59d5e, 0xc05a2d6d1a4e99a6, 13, 0x08d92014c018edbf),
    (0xc0274a57fcf59d5e, 0xc05a2d6d1a4e99a6, 14, 0x08e92014c018ed9f),
    (0xc0274a57fcf59d5e, 0xc05a2d6d1a4e99a6, 15, 0x08f92014c018ed9c),
    (0xc053bf83750a00e9, 0xc061d3403ff248b7, 0, 0x080f3fffffffffff),
    (0xc053bf83750a00e9, 0xc061d3403ff248b7, 1, 0x081f23ffffffffff),
    (0xc053bf83750a00e9, 0xc061d3403ff248b7, 2, 0x082f207fffffffff),
    (0xc053bf83750a00e9, 0xc061d3403ff248b7, 3, 0x083f200fffffffff),
    (0xc053bf83750a00e9, 0xc061d3403ff248b7, 4, 0x084f2009ffffffff),
    (0xc053bf83750a00e9, 0xc061d3403ff248b7, 5, 0x085f2008ffffffff),
    (0xc053bf83750a00e9, 0xc061d3403ff248b7, 6, 0x086f2008efffffff),
    (0xc053bf83750a00e9, 0xc061d3403ff248b7, 7, 0x087f2008eaffffff),
    (0xc053bf83750a00e9, 0xc061d3403ff248b7, 8, 0x088f2008ea1fffff),
    (0xc053bf83750a00e9, 0xc061d3403ff248b7, 9, 0x089f2008ea03ffff),
    (0xc053bf83750a00e9, 0xc061d3403ff248b7, 10, 0x08af2008ea007fff),
    (0xc053bf83750a00e9, 0xc061d3403ff248b7, 11, 0x08bf2008ea006fff),
    (0xc053bf83750a00e9, 0xc061d3403ff248b7, 12, 0x08cf2008ea006dff),
    (0xc053bf83750a00e9, 0xc061d3403ff248b7, 13, 0x08df2008ea006cff),
    (0xc053bf83750a00e9, 0xc061d3403ff248b7, 14, 0x08ef2008ea006cf7),
    (0xc053bf83750a00e9, 0xc061d3403ff248b7, 15, 0x08ff2008ea006cf0),
];

#[test]
fn oracle_assignment_lock() {
    let mut random = SplitMix64::new(ASSIGNMENT_SEED);
    let mut boundary_mismatches = 0_usize;
    for sample in 0..ASSIGNMENT_SAMPLES {
        let (lat, lng) = random.uniform_sphere();
        let resolution = (random.next() % 16) as u8;
        let oracle = oracle_cell_at(lat, lng, resolution);
        let actual = cell_at(lat, lng, resolution).unwrap();
        if actual.raw() != u64::from(oracle) {
            assert_boundary_mismatch(lat, lng, resolution, actual, oracle, sample);
            boundary_mismatches += 1;
        }
    }
    // Pinned-kernel/platform-libm differences may only move boundary points,
    // and no more than 0.05% (10 of this seeded 20,000-point sweep).
    assert!(
        boundary_mismatches * 10_000 <= ASSIGNMENT_SAMPLES * 5,
        "{boundary_mismatches} boundary mismatches in {ASSIGNMENT_SAMPLES} samples"
    );

    assert_directed_assignments();
}

#[test]
fn truncation_consistency_property() {
    let mut random = SplitMix64::new(TRUNCATION_SEED);
    let mut direct_divergences = 0_usize;
    let mut examples: Vec<(f64, f64, u8, u64, u64)> = Vec::new();
    let comparisons = PROPERTY_SAMPLES * 16;
    for sample in 0..PROPERTY_SAMPLES {
        let (lat, lng) = random.uniform_sphere();
        let atom = atom_of(lat, lng).unwrap();
        let direct_atom = cell_at(lat, lng, 15).unwrap();
        let oracle_atom = oracle_cell_at(lat, lng, 15);
        assert_eq!(atom, direct_atom, "atom/direct-res15 sample {sample}");
        assert_eq!(atom.raw(), u64::from(oracle_atom));
        for resolution in 0..=15 {
            let truncated = atom.truncate_to(resolution).unwrap();
            let oracle_resolution = Resolution::try_from(resolution).unwrap();
            let oracle_truncated = oracle_atom.parent(oracle_resolution).unwrap();
            assert_eq!(
                truncated,
                direct_atom.truncate_to(resolution).unwrap(),
                "truncation identity sample {sample} resolution {resolution}"
            );
            assert_eq!(
                truncated.raw(),
                u64::from(oracle_truncated),
                "oracle atom truncation sample {sample} resolution {resolution}"
            );
            let direct = cell_at(lat, lng, resolution).unwrap();
            assert_eq!(
                direct.raw(),
                u64::from(oracle_cell_at(lat, lng, resolution)),
                "oracle direct assignment sample {sample} resolution {resolution}"
            );
            if direct != truncated {
                assert_neighbors(direct, truncated, sample, resolution);
                if examples.len() < 3
                    && examples.iter().all(|(example_lat, example_lng, ..)| {
                        *example_lat != lat || *example_lng != lng
                    })
                {
                    examples.push((lat, lng, resolution, direct.raw(), truncated.raw()));
                }
                direct_divergences += 1;
            }
        }
    }
    assert!(
        direct_divergences * 100 >= comparisons && direct_divergences * 100 <= comparisons * 8,
        "{direct_divergences} direct/truncated divergences in {comparisons} comparisons; \
         first examples: {examples:?}"
    );
}

#[test]
fn center_inverse_lock() {
    for oracle in OracleCell::base_cells() {
        assert_center_lock(oracle);
    }

    let bases = OracleCell::base_cells().collect::<Vec<_>>();
    let mut random = SplitMix64::new(CENTER_SEED);
    for sample in 0..PROPERTY_SAMPLES {
        let resolution_u8 = (sample % 16) as u8;
        let resolution = Resolution::try_from(resolution_u8).unwrap();
        let base = bases[(random.next() % bases.len() as u64) as usize];
        let count = base.children_count(resolution);
        let oracle = base.child_at(random.next() % count, resolution).unwrap();
        assert_center_lock(oracle);
    }
}

#[test]
fn determinism_goldens() {
    assert!(DETERMINISM_GOLDENS.len() >= 64);
    let mut resolutions = [false; 16];
    let mut base_cells = [false; 122];
    for &(lat_bits, lng_bits, resolution, expected) in DETERMINISM_GOLDENS {
        let actual = cell_at(
            f64::from_bits(lat_bits),
            f64::from_bits(lng_bits),
            resolution,
        )
        .unwrap();
        assert_eq!(
            actual.raw(),
            expected,
            "determinism golden ({lat_bits:#018x}, {lng_bits:#018x}, r{resolution})"
        );
        resolutions[usize::from(resolution)] = true;
        base_cells[usize::from(actual.base_cell())] = true;
    }
    assert!(resolutions.into_iter().all(|covered| covered));
    assert!(base_cells.into_iter().filter(|covered| *covered).count() >= 6);
    assert!(base_cells[4] && base_cells[14]);
}

#[test]
fn validates_projection_arguments_and_canonicalizes_seam() {
    assert!(cell_at(f64::NAN, 0.0, 0).is_err());
    assert!(cell_at(0.0, f64::INFINITY, 0).is_err());
    assert!(cell_at(-90.000_000_1, 0.0, 0).is_err());
    assert_eq!(
        cell_at(0.0, 180.000_000_1, 0),
        Err(GeoError::InvalidArgument {
            value_bits: 180.000_000_1_f64.to_bits(),
            reason: "longitude must be finite and in [-180, 180]".to_owned(),
        })
    );
    assert!(cell_at(0.0, 0.0, 16).is_err());
    for resolution in 0..=15 {
        assert_eq!(
            cell_at(31.25, 180.0, resolution).unwrap(),
            cell_at(31.25, -180.0, resolution).unwrap()
        );
    }
}

/// Adversarial probe of the closest-face assignment rule (2026-08-04 external
/// geo-projection review, finding 1): dense micro-jitter around every
/// icosahedral vertex (the 12 pentagon base cells) and every icosahedral
/// edge midpoint — the exact neighborhoods where "closest face center" and
/// "containing face" could disagree, and which a uniform random sweep
/// essentially never samples at the tightest radii. The law matches
/// `oracle_assignment_lock`: any mismatch must be a boundary neighbor, and
/// the mismatch rate stays under 0.5% even on this all-boundary set.
#[test]
fn vertex_and_edge_band_assignment_lock() {
    let vertices: Vec<(f64, f64)> = PENTAGON_BASE_CELLS
        .iter()
        .map(|base_cell| {
            let base = OracleCell::base_cells()
                .find(|cell| u8::from(cell.base_cell()) == *base_cell)
                .unwrap();
            let center = LatLng::from(base);
            (center.lat(), center.lng())
        })
        .collect();

    let to_xyz = |(lat, lng): (f64, f64)| {
        let (lat, lng) = (lat.to_radians(), lng.to_radians());
        [lat.cos() * lng.cos(), lat.cos() * lng.sin(), lat.sin()]
    };
    let to_lat_lng = |v: [f64; 3]| {
        let norm = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
        let (x, y, z) = (v[0] / norm, v[1] / norm, v[2] / norm);
        (z.asin().to_degrees(), y.atan2(x).to_degrees())
    };
    let mut probes = vertices.clone();
    for (index, first) in vertices.iter().enumerate() {
        for second in &vertices[index + 1..] {
            let (a, b) = (to_xyz(*first), to_xyz(*second));
            let dot = a[0] * b[0] + a[1] * b[1] + a[2] * b[2];
            if dot > 70.0_f64.to_radians().cos() {
                probes.push(to_lat_lng([a[0] + b[0], a[1] + b[1], a[2] + b[2]]));
            }
        }
    }

    let mut samples = 0_usize;
    let mut boundary_mismatches = 0_usize;
    for &(lat, lng) in &probes {
        for radius in [1e-6_f64, 1e-4, 1e-2, 0.5, 2.0] {
            for azimuth_step in 0..12 {
                let azimuth = f64::from(azimuth_step) * std::f64::consts::TAU / 12.0;
                let jittered_lat = radius.mul_add(azimuth.cos(), lat);
                if !(-90.0..=90.0).contains(&jittered_lat) {
                    continue;
                }
                let mut jittered_lng =
                    (radius * azimuth.sin() / jittered_lat.to_radians().cos().max(1e-9)) + lng;
                if jittered_lng >= 180.0 {
                    jittered_lng -= 360.0;
                } else if jittered_lng < -180.0 {
                    jittered_lng += 360.0;
                }
                for resolution in [0_u8, 15] {
                    samples += 1;
                    let oracle = oracle_cell_at(jittered_lat, jittered_lng, resolution);
                    let actual = cell_at(jittered_lat, jittered_lng, resolution).unwrap();
                    if actual.raw() != u64::from(oracle) {
                        assert_boundary_mismatch(
                            jittered_lat,
                            jittered_lng,
                            resolution,
                            actual,
                            oracle,
                            samples,
                        );
                        boundary_mismatches += 1;
                    }
                }
            }
        }
    }
    assert!(samples > 4_000, "probe generation collapsed: {samples}");
    assert!(
        boundary_mismatches * 1_000 <= samples * 5,
        "{boundary_mismatches} boundary mismatches in {samples} vertex/edge-band samples"
    );
}

/// Canonical-form fence for `cell_center` output (2026-08-04 external
/// geo-projection review, finding 2): every returned center must satisfy
/// `lat ∈ [-90, 90]`, `lng ∈ [-180, 180)`, and the pole spelling
/// `|lat| == 90 ⇒ lng == 0` — over every base cell's center child at every
/// resolution, plus directed pole and antimeridian cells.
#[test]
fn cell_center_output_is_canonical() {
    let mut checked = 0_usize;
    let mut assert_canonical = |cell: CellIndex| {
        let (lat, lng) = cell_center(cell);
        assert!(
            (-90.0..=90.0).contains(&lat),
            "non-canonical latitude {lat} for cell {:#x}",
            cell.raw()
        );
        assert!(
            (-180.0..180.0).contains(&lng),
            "non-canonical longitude {lng} for cell {:#x}",
            cell.raw()
        );
        if lat == 90.0 || lat == -90.0 {
            assert_eq!(lng, 0.0, "pole center must spell longitude 0");
        }
        checked += 1;
    };
    for base in OracleCell::base_cells() {
        for resolution in 0..=15_u8 {
            let child = base
                .center_child(Resolution::try_from(resolution).unwrap())
                .unwrap();
            assert_canonical(CellIndex::try_from(u64::from(child)).unwrap());
        }
    }
    for (lat, lng) in [(90.0, 0.0), (-90.0, 0.0), (0.0, 180.0), (45.0, -180.0)] {
        for resolution in 0..=15_u8 {
            assert_canonical(cell_at(lat, lng, resolution).unwrap());
        }
    }
    assert_eq!(checked, 122 * 16 + 4 * 16);
}

fn assert_directed_assignments() {
    for oracle in OracleCell::base_cells() {
        assert_point_matches_oracle(LatLng::from(oracle), oracle.resolution());
    }
    for (lat, lng) in [(90.0, 0.0), (-90.0, 0.0)] {
        for resolution in [Resolution::Zero, Resolution::Five, Resolution::Fifteen] {
            assert_point_matches_oracle(LatLng::new(lat, lng).unwrap(), resolution);
        }
    }
    for lat in [-80.0, -45.0, 0.0, 45.0, 80.0] {
        for lng in [-180.0, 180.0] {
            for resolution in [Resolution::Zero, Resolution::Seven, Resolution::Fifteen] {
                assert_point_matches_oracle(LatLng::new(lat, lng).unwrap(), resolution);
            }
        }
    }
    for base_cell in PENTAGON_BASE_CELLS {
        let base = OracleCell::base_cells()
            .find(|cell| u8::from(cell.base_cell()) == base_cell)
            .unwrap();
        for resolution in [Resolution::Zero, Resolution::Five, Resolution::Fifteen] {
            let pentagon = base.center_child(resolution).unwrap();
            assert!(pentagon.is_pentagon());
            assert_point_matches_oracle(LatLng::from(pentagon), resolution);
        }
    }
}

fn assert_point_matches_oracle(point: LatLng, resolution: Resolution) {
    let expected = point.to_cell(resolution);
    let actual = cell_at(point.lat(), point.lng(), resolution.into()).unwrap();
    assert_eq!(actual.raw(), u64::from(expected));
}

fn assert_boundary_mismatch(
    lat: f64,
    lng: f64,
    resolution: u8,
    actual: CellIndex,
    oracle: OracleCell,
    sample: usize,
) {
    let actual_oracle = OracleCell::try_from(actual.raw()).unwrap();
    assert!(
        actual_oracle.is_neighbor_with(oracle).unwrap(),
        "non-neighbor mismatch at sample {sample}: ({lat}, {lng}) r{resolution}: \
         ours={:#x}, oracle={:#x}",
        actual.raw(),
        u64::from(oracle)
    );
    let center = LatLng::from(oracle);
    assert_eq!(
        cell_at(center.lat(), center.lng(), resolution)
            .unwrap()
            .raw(),
        u64::from(oracle),
        "oracle center did not reproduce mismatch cell at sample {sample}"
    );
}

fn assert_neighbors(first: CellIndex, second: CellIndex, sample: usize, resolution: u8) {
    let first = OracleCell::try_from(first.raw()).unwrap();
    let second = OracleCell::try_from(second.raw()).unwrap();
    assert!(
        first.is_neighbor_with(second).unwrap(),
        "non-neighbor truncation divergence at sample {sample} resolution {resolution}: \
         direct={:#x}, truncated={:#x}",
        u64::from(first),
        u64::from(second)
    );
}

fn assert_center_lock(oracle: OracleCell) {
    let cell = CellIndex::try_from(u64::from(oracle)).unwrap();
    let expected = LatLng::from(oracle);
    let actual = cell_center(cell);
    assert_within_absolute_degrees(actual.0, expected.lat(), "latitude", oracle);
    assert_within_absolute_degrees(actual.1, expected.lng(), "longitude", oracle);
    assert_eq!(
        cell_at(actual.0, actual.1, cell.resolution()).unwrap(),
        cell,
        "center escaped cell {:#x}",
        cell.raw()
    );
}

fn assert_within_absolute_degrees(actual: f64, expected: f64, component: &str, cell: OracleCell) {
    let distance = (actual - expected).abs();
    assert!(
        distance <= CENTER_TOLERANCE_DEGREES,
        "{component} center for {:#x}: actual={actual:?}, expected={expected:?}, \
         absolute degrees={distance:?}",
        u64::from(cell)
    );
}

fn oracle_cell_at(lat: f64, lng: f64, resolution: u8) -> OracleCell {
    let resolution = Resolution::try_from(resolution).unwrap();
    LatLng::new(lat, lng).unwrap().to_cell(resolution)
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
        // Uniform z=sin(latitude) in [-1,1), uniform longitude in [-180,180).
        // Taking the high 53 bits gives the exact binary64 fraction k/2^53.
        let z = 2.0 * unit_interval(self.next()) - 1.0;
        let latitude = z.asin().to_degrees();
        let longitude = 360.0 * unit_interval(self.next()) - 180.0;
        (latitude, longitude)
    }
}

fn unit_interval(value: u64) -> f64 {
    (value >> 11) as f64 * (1.0 / ((1_u64 << 53) as f64))
}
