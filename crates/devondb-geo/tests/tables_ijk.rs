use devondb_geo::{
    base_cells::{BASE_CELL_COUNT, BASE_CELLS, base_cell_from_face_ijk},
    coordijk::{CoordIjk, DIGIT_UNIT_VECTORS},
    faces::{FACE_CENTER_VECTORS, FACE_COUNT},
    index::PENTAGON_BASE_CELLS,
    math::sqrt_det,
};

#[test]
fn pentagon_flags_match_frozen_set_and_oracle() {
    let table_pentagons = BASE_CELLS
        .iter()
        .enumerate()
        .filter_map(|(base_cell, data)| data.is_pentagon.then_some(base_cell as u8))
        .collect::<Vec<_>>();
    assert_eq!(table_pentagons, PENTAGON_BASE_CELLS);

    for (base_cell, oracle) in h3o::CellIndex::base_cells().enumerate() {
        assert_eq!(u8::from(oracle.base_cell()), base_cell as u8);
        assert_eq!(BASE_CELLS[base_cell].is_pentagon, oracle.is_pentagon());
    }
}

#[test]
fn home_face_belongs_to_each_oracle_base_cell() {
    for (base_cell, oracle) in h3o::CellIndex::base_cells().enumerate() {
        let face = h3o::Face::try_from(BASE_CELLS[base_cell].home.face)
            .expect("table face must be in range");
        assert!(
            oracle.icosahedron_faces().contains(face),
            "base cell {base_cell} home face {face} is not an oracle member"
        );
    }
}

#[test]
fn polar_pentagons_are_the_only_ones_without_cw_offsets() {
    let without_offsets = BASE_CELLS
        .iter()
        .enumerate()
        .filter_map(|(base_cell, data)| {
            (data.is_pentagon && data.cw_offset_faces.is_none()).then_some(base_cell as u8)
        })
        .collect::<Vec<_>>();
    assert_eq!(without_offsets, [4, 117]);
}

#[test]
fn face_centers_have_icosahedron_symmetry() {
    assert_eq!(FACE_CENTER_VECTORS.len(), FACE_COUNT);
    for (face, center) in FACE_CENTER_VECTORS.iter().enumerate() {
        let length = sqrt_det(center.dot(*center));
        assert!(
            ulp_distance(length, 1.0) <= 1,
            "face {face} length {length:?} is more than one ulp from unity"
        );
    }

    let mut pairwise_dots = Vec::with_capacity(FACE_COUNT * (FACE_COUNT - 1) / 2);
    for first in 0..FACE_COUNT {
        for second in (first + 1)..FACE_COUNT {
            pairwise_dots.push(FACE_CENTER_VECTORS[first].dot(FACE_CENTER_VECTORS[second]));
        }
    }
    pairwise_dots.sort_by(f64::total_cmp);
    let clusters = cluster_counts(&pairwise_dots);
    assert_eq!(clusters, [10, 30, 60, 60, 30]);

    for face in 0..FACE_COUNT {
        let dots = (0..FACE_COUNT)
            .filter(|other| *other != face)
            .map(|other| FACE_CENTER_VECTORS[face].dot(FACE_CENTER_VECTORS[other]))
            .collect::<Vec<_>>();
        let adjacent_dot = dots.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        let adjacent_count = dots
            .iter()
            .filter(|dot| (**dot - adjacent_dot).abs() <= 32.0 * f64::EPSILON)
            .count();
        assert_eq!(adjacent_count, 3, "face {face} edge-neighbor count");
    }
}

#[test]
fn normalize_is_canonical_and_idempotent() {
    let mut random = SplitMix64::new(0x7a91_02d4_efa3_51c8);
    for _ in 0..10_000 {
        let coordinate = random.small_coord();
        let normalized = coordinate.normalize();
        assert!(normalized.i >= 0 && normalized.j >= 0 && normalized.k >= 0);
        assert!(normalized.i == 0 || normalized.j == 0 || normalized.k == 0);
        assert_eq!(normalized.normalize(), normalized);
    }
}

#[test]
fn rotations_have_order_six() {
    let mut random = SplitMix64::new(0x3f06_35d2_ba17_c849);
    for _ in 0..10_000 {
        let expected = random.small_coord().normalize();
        let mut clockwise = expected;
        let mut counterclockwise = expected;
        for _ in 0..6 {
            clockwise = clockwise.rotate60_cw();
            counterclockwise = counterclockwise.rotate60_ccw();
        }
        assert_eq!(clockwise, expected);
        assert_eq!(counterclockwise, expected);
    }
}

#[test]
fn digit_vectors_and_aperture_seven_roundtrip() {
    let expected = [
        CoordIjk::new(0, 0, 0),
        CoordIjk::new(0, 0, 1),
        CoordIjk::new(0, 1, 0),
        CoordIjk::new(0, 1, 1),
        CoordIjk::new(1, 0, 0),
        CoordIjk::new(1, 0, 1),
        CoordIjk::new(1, 1, 0),
    ];
    assert_eq!(DIGIT_UNIT_VECTORS, expected);

    for digit in 0..=6 {
        let unit = CoordIjk::from_digit(digit).expect("digit is in range");
        assert_eq!(unit.to_digit(), Some(digit));
        assert_eq!(unit.down_ap7().up_ap7().to_digit(), Some(digit));
        assert_eq!(unit.down_ap7r().up_ap7r().to_digit(), Some(digit));
        for other in (digit + 1)..=6 {
            assert_ne!(unit, DIGIT_UNIT_VECTORS[usize::from(other)]);
        }
    }
    assert_eq!(CoordIjk::from_digit(7), None);
}

#[test]
fn home_face_ijk_lookup_is_bijective() {
    assert_eq!(BASE_CELLS.len(), BASE_CELL_COUNT);
    for (base_cell, data) in BASE_CELLS.iter().enumerate() {
        assert_eq!(
            base_cell_from_face_ijk(data.home),
            Some(base_cell as u8),
            "base cell {base_cell}"
        );
    }
}

fn cluster_counts(sorted: &[f64]) -> Vec<usize> {
    let mut representatives = Vec::<f64>::new();
    let mut counts = Vec::<usize>::new();
    for value in sorted {
        if representatives
            .last()
            .is_some_and(|representative| (*value - representative).abs() <= 32.0 * f64::EPSILON)
        {
            let last = counts.last_mut().expect("a representative has a count");
            *last += 1;
        } else {
            representatives.push(*value);
            counts.push(1);
        }
    }
    counts
}

fn ulp_distance(first: f64, second: f64) -> u64 {
    assert!(first.is_sign_positive() && second.is_sign_positive());
    first.to_bits().abs_diff(second.to_bits())
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

    fn small_coord(&mut self) -> CoordIjk {
        CoordIjk::new(self.component(), self.component(), self.component())
    }

    fn component(&mut self) -> i32 {
        (self.next() % 129) as i32 - 64
    }
}
