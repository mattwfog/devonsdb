//! Point-to-cell projection and cell-to-center inverse for DevonGrid profile 0
//! (`docs/GEO.md` sections 1 and 4).

use core::f64::consts::{FRAC_PI_2, PI};

use crate::{
    GeoError,
    base_cells::{BASE_CELLS, FaceIjk, base_cell_rotation_from_face_ijk, home_face_ijk},
    coordijk::CoordIjk,
    faces::{CLASS_II_AXIS_AZIMUTHS_RAD, FACE_CENTER_VECTORS, face_center_lat_lng},
    index::CellIndex,
    math::{Vec3, acos_det, asin_det, atan2_det, cos_det, sin_det, sqrt_det},
};

const MAX_RESOLUTION: u8 = 15;
const DEFAULT_CELL_INDEX: u64 = 0x0800_1fff_ffff_ffff;
const TWO_PI: f64 = 2.0 * PI;
const DEGREES_PER_RADIAN: f64 = 180.0 / PI;
const RADIANS_PER_DEGREE: f64 = PI / 180.0;

// Projection constants transcribed from h3o 0.8.0 `coord/mod.rs`.
const EPSILON: f64 = 0.000_000_000_000_000_1;
const RES0_U_GNOMONIC: f64 = 0.381_966_011_250_105;
const INV_RES0_U_GNOMONIC: f64 = 2.618_033_988_749_896;
const AP7_ROT_RADS: f64 = 0.333_473_172_251_832_1;
const SQRT3_2: f64 = 0.866_025_403_784_438_6;

// Powers of sqrt(7), transcribed bit-for-bit from h3o 0.8.0
// `coord::SQRT7_POWERS` and `coord::INV_SQRT7_POWERS`.
const SQRT7_POWERS: [f64; 16] = [
    1.0,
    2.645_751_311_064_590_7,
    7.0,
    18.520_259_177_452_136,
    49.000_000_000_000_01,
    129.641_814_242_164_97,
    343.000_000_000_000_1,
    907.492_699_695_154_9,
    2_401.000_000_000_001,
    6_352.448_897_866_085,
    16_807.000_000_000_007,
    44_467.142_285_062_6,
    117_649.000_000_000_07,
    311_269.995_995_438_2,
    823_543.000_000_000_6,
    2_178_889.971_968_068,
];

const INV_SQRT7_POWERS: [f64; 16] = [
    1.0,
    0.377_964_473_009_227_2,
    0.142_857_142_857_142_85,
    0.053_994_924_715_603_88,
    0.020_408_163_265_306_12,
    0.007_713_560_673_657_697,
    0.002_915_451_895_043_731,
    0.001_101_937_239_093_956_5,
    0.000_416_493_127_863_390_1,
    0.000_157_419_605_584_850_93,
    0.000_059_499_018_266_198_58,
    0.000_022_488_515_083_550_13,
    0.000_008_499_859_752_314_082,
    0.000_003_212_645_011_935_733,
    0.000_001_214_265_678_902_011_5,
    0.000_000_458_949_287_419_390_3,
];

// Overage tables transcribed from h3o 0.8.0 `coord::faceijk`.
const MAX_DIM_BY_CLASS_II_RESOLUTION: [i32; 17] = [
    2, -1, 14, -1, 98, -1, 686, -1, 4_802, -1, 33_614, -1, 235_298, -1, 1_647_086, -1, 11_529_602,
];
const UNIT_SCALE_BY_CLASS_II_RESOLUTION: [i32; 17] = [
    1, -1, 7, -1, 49, -1, 343, -1, 2_401, -1, 16_807, -1, 117_649, -1, 823_543, -1, 5_764_801,
];

// Sine/cosine pairs for h3o 0.8.0 `face::CENTER_GEO` latitudes. These are
// frozen from the oracle's coordinate-at inputs so the inverse never consults
// platform libm; `tests/projection.rs` locks the resulting centers to h3o.
const FACE_CENTER_SIN_COS_LAT: [(f64, f64); 20] = [
    (
        f64::from_bits(0x3fe7_08fd_b42b_5b79),
        f64::from_bits(0x3fe6_3654_bf8f_6b70),
    ),
    (
        f64::from_bits(0x3fee_e635_bb84_4530),
        f64::from_bits(0x3fd0_a441_5099_6918),
    ),
    (
        f64::from_bits(0x3feb_d537_a62c_73a9),
        f64::from_bits(0x3fdf_9496_8f82_3265),
    ),
    (
        f64::from_bits(0x3fe2_12d8_b1cf_dabc),
        f64::from_bits(0x3fea_6843_53db_15f9),
    ),
    (
        f64::from_bits(0x3fde_3785_8bfd_a732),
        f64::from_bits(0x3fec_3572_5076_09be),
    ),
    (
        f64::from_bits(0x3fc6_0068_87cb_b634),
        f64::from_bits(0x3fef_8613_3b6c_5919),
    ),
    (
        f64::from_bits(0x3fe2_398f_0220_8d9d),
        f64::from_bits(0x3fea_4d9a_b29b_dde7),
    ),
    (
        f64::from_bits(0x3fda_86d3_ff8a_19be),
        f64::from_bits(0x3fed_1f33_8c8c_5a40),
    ),
    (
        f64::from_bits(0xbfb4_3847_ae03_25c4),
        f64::from_bits(0x3fef_e668_4ae8_de54),
    ),
    (
        f64::from_bits(0xbfcd_4d0b_9f05_f3a1),
        f64::from_bits(0x3fef_2679_b7cb_38f7),
    ),
    (
        f64::from_bits(0x3fb4_3847_ae03_25c4),
        f64::from_bits(0x3fef_e668_4ae8_de54),
    ),
    (
        f64::from_bits(0x3fcd_4d0b_9f05_f3a1),
        f64::from_bits(0x3fef_2679_b7cb_38f7),
    ),
    (
        f64::from_bits(0xbfc6_0068_87cb_b634),
        f64::from_bits(0x3fef_8613_3b6c_5919),
    ),
    (
        f64::from_bits(0xbfe2_398f_0220_8d9d),
        f64::from_bits(0x3fea_4d9a_b29b_dde7),
    ),
    (
        f64::from_bits(0xbfda_86d3_ff8a_19be),
        f64::from_bits(0x3fed_1f33_8c8c_5a40),
    ),
    (
        f64::from_bits(0xbfe2_12d8_b1cf_dabc),
        f64::from_bits(0x3fea_6843_53db_15f9),
    ),
    (
        f64::from_bits(0xbfde_3785_8bfd_a732),
        f64::from_bits(0x3fec_3572_5076_09be),
    ),
    (
        f64::from_bits(0xbfe7_08fd_b42b_5b79),
        f64::from_bits(0x3fe6_3654_bf8f_6b70),
    ),
    (
        f64::from_bits(0xbfee_e635_bb84_4530),
        f64::from_bits(0x3fd0_a441_5099_6918),
    ),
    (
        f64::from_bits(0xbfeb_d537_a62c_73a9),
        f64::from_bits(0x3fdf_9496_8f82_3265),
    ),
];

#[derive(Clone, Copy)]
struct Vec2 {
    x: f64,
    y: f64,
}

#[derive(Clone, Copy)]
struct FaceTransform {
    face: u8,
    translate: CoordIjk,
    ccw_rotations: u8,
}

macro_rules! transform {
    ($face:literal, [$i:literal, $j:literal, $k:literal], $rotations:literal) => {
        FaceTransform {
            face: $face,
            translate: CoordIjk::new($i, $j, $k),
            ccw_rotations: $rotations,
        }
    };
}

// Face transforms, in IJ/KI/JK order, transcribed from h3o 0.8.0
// `face::NEIGHBORS`. The central-face entries are intentionally omitted.
#[rustfmt::skip]
const FACE_NEIGHBORS: [[FaceTransform; 3]; 20] = [
    [transform!(4,  [2, 0, 2], 1), transform!(1,  [2, 2, 0], 5), transform!(5,  [0, 2, 2], 3)],
    [transform!(0,  [2, 0, 2], 1), transform!(2,  [2, 2, 0], 5), transform!(6,  [0, 2, 2], 3)],
    [transform!(1,  [2, 0, 2], 1), transform!(3,  [2, 2, 0], 5), transform!(7,  [0, 2, 2], 3)],
    [transform!(2,  [2, 0, 2], 1), transform!(4,  [2, 2, 0], 5), transform!(8,  [0, 2, 2], 3)],
    [transform!(3,  [2, 0, 2], 1), transform!(0,  [2, 2, 0], 5), transform!(9,  [0, 2, 2], 3)],
    [transform!(10, [2, 2, 0], 3), transform!(14, [2, 0, 2], 3), transform!(0,  [0, 2, 2], 3)],
    [transform!(11, [2, 2, 0], 3), transform!(10, [2, 0, 2], 3), transform!(1,  [0, 2, 2], 3)],
    [transform!(12, [2, 2, 0], 3), transform!(11, [2, 0, 2], 3), transform!(2,  [0, 2, 2], 3)],
    [transform!(13, [2, 2, 0], 3), transform!(12, [2, 0, 2], 3), transform!(3,  [0, 2, 2], 3)],
    [transform!(14, [2, 2, 0], 3), transform!(13, [2, 0, 2], 3), transform!(4,  [0, 2, 2], 3)],
    [transform!(5,  [2, 2, 0], 3), transform!(6,  [2, 0, 2], 3), transform!(15, [0, 2, 2], 3)],
    [transform!(6,  [2, 2, 0], 3), transform!(7,  [2, 0, 2], 3), transform!(16, [0, 2, 2], 3)],
    [transform!(7,  [2, 2, 0], 3), transform!(8,  [2, 0, 2], 3), transform!(17, [0, 2, 2], 3)],
    [transform!(8,  [2, 2, 0], 3), transform!(9,  [2, 0, 2], 3), transform!(18, [0, 2, 2], 3)],
    [transform!(9,  [2, 2, 0], 3), transform!(5,  [2, 0, 2], 3), transform!(19, [0, 2, 2], 3)],
    [transform!(16, [2, 0, 2], 1), transform!(19, [2, 2, 0], 5), transform!(10, [0, 2, 2], 3)],
    [transform!(17, [2, 0, 2], 1), transform!(15, [2, 2, 0], 5), transform!(11, [0, 2, 2], 3)],
    [transform!(18, [2, 0, 2], 1), transform!(16, [2, 2, 0], 5), transform!(12, [0, 2, 2], 3)],
    [transform!(19, [2, 0, 2], 1), transform!(17, [2, 2, 0], 5), transform!(13, [0, 2, 2], 3)],
    [transform!(15, [2, 0, 2], 1), transform!(18, [2, 2, 0], 5), transform!(14, [0, 2, 2], 3)],
];

/// Assigns a WGS84 point to the directly projected profile-0 cell at `res`.
///
/// `+180` degrees is accepted and folded to `-180` before projection. This is
/// the H3-compatible direct assignment primitive; DevonGrid indexing assigns
/// atoms with [`atom_of`] and obtains coarser cells by truncation.
///
/// # Errors
///
/// Returns an error for a non-finite or out-of-range coordinate, or a
/// resolution outside `0..=15`.
pub fn cell_at(lat_deg: f64, lng_deg: f64, res: u8) -> Result<CellIndex, GeoError> {
    validate_arguments(lat_deg, lng_deg, res)?;
    let lng_deg = if lng_deg == 180.0 { -180.0 } else { lng_deg };
    let lat = lat_deg * RADIANS_PER_DEGREE;
    let lng = lng_deg * RADIANS_PER_DEGREE;
    let face_ijk = point_to_face_ijk(lat, lng, res);
    face_ijk_to_cell(face_ijk, res)
}

/// Assigns a WGS84 point to its resolution-15 DevonGrid atom.
///
/// # Errors
///
/// Returns an error for a non-finite or out-of-range coordinate.
pub fn atom_of(lat_deg: f64, lng_deg: f64) -> Result<CellIndex, GeoError> {
    cell_at(lat_deg, lng_deg, MAX_RESOLUTION)
}

/// Returns the center of `cell` as canonical `(latitude, longitude)` degrees.
///
/// Longitude is in `[-180, 180)`, and a pole is spelled with longitude zero.
#[must_use]
pub fn cell_center(cell: CellIndex) -> (f64, f64) {
    let face_ijk = cell_to_face_ijk(cell);
    let (lat, lng) = face_ijk_to_lat_lng(face_ijk, cell.resolution());
    canonical_degrees(lat, lng)
}

fn validate_arguments(lat: f64, lng: f64, resolution: u8) -> Result<(), GeoError> {
    if resolution > MAX_RESOLUTION {
        return Err(GeoError::InvalidResolution { resolution });
    }
    if !lat.is_finite() || !(-90.0..=90.0).contains(&lat) {
        return Err(coordinate_error(
            lat,
            "latitude must be finite and in [-90, 90]",
        ));
    }
    if !lng.is_finite() || !(-180.0..=180.0).contains(&lng) {
        return Err(coordinate_error(
            lng,
            "longitude must be finite and in [-180, 180]",
        ));
    }
    Ok(())
}

fn coordinate_error(value: f64, reason: &'static str) -> GeoError {
    GeoError::InvalidArgument {
        value_bits: value.to_bits(),
        reason: reason.to_owned(),
    }
}

fn point_to_face_ijk(lat: f64, lng: f64, resolution: u8) -> FaceIjk {
    let point = Vec3::from_lat_lng_rad(lat, lng);
    let (face, distance) = closest_face(point);
    let vector = project_to_face(lat, lng, face, distance, resolution);
    FaceIjk::new(face, vec2_to_ijk(vector))
}

fn closest_face(point: Vec3) -> (u8, f64) {
    let mut closest = 0;
    let mut closest_distance = 5.0;
    for (face, center) in FACE_CENTER_VECTORS.iter().enumerate() {
        let x = point.x - center.x;
        let y = point.y - center.y;
        let z = point.z - center.z;
        let distance = x.mul_add(x, y.mul_add(y, z * z));
        if distance < closest_distance {
            closest = face as u8;
            closest_distance = distance;
        }
    }
    (closest, closest_distance)
}

fn project_to_face(lat: f64, lng: f64, face: u8, distance: f64, resolution: u8) -> Vec2 {
    let angular_distance = acos_det(1.0 - distance / 2.0);
    if angular_distance < EPSILON {
        return Vec2 { x: 0.0, y: 0.0 };
    }
    // h3o 0.8.0 uses tan(r). The deterministic equivalent is sin(r)/cos(r),
    // routed exclusively through DevonGrid's pinned math kernels.
    let tangent = sin_det(angular_distance) / cos_det(angular_distance);
    let radius = tangent * INV_RES0_U_GNOMONIC * SQRT7_POWERS[usize::from(resolution)];
    let theta = projection_theta(lat, lng, face, resolution);
    Vec2 {
        x: radius * cos_det(theta),
        y: radius * sin_det(theta),
    }
}

fn projection_theta(lat: f64, lng: f64, face: u8, resolution: u8) -> f64 {
    let center = face_center_lat_lng(face).unwrap_or_else(|| unreachable!("validated face"));
    let delta_lng = lng - center.lng;
    let y = cos_det(lat) * sin_det(delta_lng);
    let x = cos_det(center.lat).mul_add(
        sin_det(lat),
        -sin_det(center.lat) * cos_det(lat) * cos_det(delta_lng),
    );
    let azimuth = atan2_det(y, x);
    let mut theta = CLASS_II_AXIS_AZIMUTHS_RAD[usize::from(face)][0] - azimuth;
    if is_class_three(resolution) {
        theta -= AP7_ROT_RADS;
    }
    theta
}

#[allow(clippy::cast_possible_truncation)]
fn vec2_to_ijk(value: Vec2) -> CoordIjk {
    let absolute_x = value.x.abs();
    let absolute_y = value.y.abs();
    let x2 = absolute_y * (2.0 / SQRT3_2 / 2.0);
    let x1 = absolute_x + x2 / 2.0;
    let m1 = x1 as i32;
    let m2 = x2 as i32;
    let r1 = x1 - f64::from(m1);
    let r2 = x2 - f64::from(m2);
    let (mut i, mut j) = rounded_ij(m1, m2, r1, r2);
    fold_ijk_axes(value, &mut i, &mut j);
    CoordIjk::new(i, j, 0).normalize()
}

fn rounded_ij(m1: i32, m2: i32, r1: f64, r2: f64) -> (i32, i32) {
    if r1 < 0.5 {
        if r1 < 1.0 / 3.0 {
            return (m1, m2 + i32::from(r2 >= (1.0 + r1) / 2.0));
        }
        let i = m1 + i32::from((1.0 - r1) <= r2 && r2 < 2.0 * r1);
        return (i, m2 + i32::from(r2 >= 1.0 - r1));
    }
    if r1 < 2.0 / 3.0 {
        let j = m2 + i32::from(r2 >= 1.0 - r1);
        let i = m1 + i32::from(r1.mul_add(2.0, -1.0) >= r2 || r2 >= 1.0 - r1);
        return (i, j);
    }
    (m1 + 1, m2 + i32::from(r2 >= r1 / 2.0))
}

fn fold_ijk_axes(value: Vec2, i: &mut i32, j: &mut i32) {
    if value.x < 0.0 {
        let offset = *j % 2;
        let axis_i = (*j + offset) / 2;
        let difference = *i - axis_i;
        *i -= 2 * difference + offset;
    }
    if value.y < 0.0 {
        *i -= (2 * *j + 1) / 2;
        *j = -*j;
    }
}

fn face_ijk_to_cell(mut face_ijk: FaceIjk, resolution: u8) -> Result<CellIndex, GeoError> {
    let mut bits = set_resolution(DEFAULT_CELL_INDEX, resolution);
    if resolution == 0 {
        let rotation = lookup_rotation(face_ijk)?;
        return CellIndex::try_from(set_base_cell(bits, rotation.base_cell));
    }
    face_ijk.coord = directions_from_ijk(face_ijk.coord, &mut bits, resolution)?;
    let rotation = lookup_rotation(face_ijk)?;
    bits = set_base_cell(bits, rotation.base_cell);
    bits = rotate_into_base_cell(bits, face_ijk.face, rotation);
    CellIndex::try_from(bits)
}

fn lookup_rotation(face_ijk: FaceIjk) -> Result<crate::base_cells::BaseCellRotation, GeoError> {
    base_cell_rotation_from_face_ijk(face_ijk).ok_or_else(|| GeoError::InvalidCellIndex {
        index: DEFAULT_CELL_INDEX,
        reason: "projected face-IJK address is outside the base-cell table".to_owned(),
    })
}

fn directions_from_ijk(
    mut coordinate: CoordIjk,
    bits: &mut u64,
    resolution: u8,
) -> Result<CoordIjk, GeoError> {
    for res in (1..=resolution).rev() {
        let last = coordinate;
        let center = if is_class_three(res) {
            coordinate = coordinate.up_ap7();
            coordinate.down_ap7()
        } else {
            coordinate = coordinate.up_ap7r();
            coordinate.down_ap7r()
        };
        let digit =
            (last - center)
                .normalize()
                .to_digit()
                .ok_or_else(|| GeoError::InvalidCellIndex {
                    index: *bits,
                    reason: "projection produced a non-unit IJK digit".to_owned(),
                })?;
        *bits = set_digit(*bits, res, digit);
    }
    Ok(coordinate)
}

fn rotate_into_base_cell(
    mut bits: u64,
    face: u8,
    rotation: crate::base_cells::BaseCellRotation,
) -> u64 {
    let metadata = BASE_CELLS[usize::from(rotation.base_cell)];
    if !metadata.is_pentagon {
        return rotate_bits_ccw(bits, rotation.ccw_rotations);
    }
    if first_digit(bits) == Some(1) {
        bits = if metadata
            .cw_offset_faces
            .is_some_and(|faces| faces.contains(&face))
        {
            rotate_bits_cw(bits, 1)
        } else {
            rotate_bits_ccw(bits, 1)
        };
    }
    for _ in 0..rotation.ccw_rotations {
        bits = pentagon_rotate_ccw(bits);
    }
    bits
}

fn cell_to_face_ijk(cell: CellIndex) -> FaceIjk {
    let mut bits = cell.raw();
    let base_cell = cell.base_cell();
    let resolution = cell.resolution();
    let metadata = BASE_CELLS[usize::from(base_cell)];
    if metadata.is_pentagon && first_digit(bits) == Some(5) {
        bits = rotate_bits_cw(bits, 1);
    }
    let mut face_ijk = face_ijk_from_bits(bits, resolution, base_cell);
    if possible_overage(face_ijk, resolution, metadata.is_pentagon) {
        adjust_cell_overage(&mut face_ijk, bits, resolution, metadata.is_pentagon);
    }
    face_ijk
}

fn face_ijk_from_bits(bits: u64, resolution: u8, base_cell: u8) -> FaceIjk {
    let mut face_ijk =
        home_face_ijk(base_cell).unwrap_or_else(|| unreachable!("validated base cell"));
    for res in 1..=resolution {
        face_ijk.coord = if is_class_three(res) {
            face_ijk.coord.down_ap7()
        } else {
            face_ijk.coord.down_ap7r()
        };
        let offset = CoordIjk::from_digit(get_digit(bits, res))
            .unwrap_or_else(|| unreachable!("validated active digit"));
        face_ijk.coord = (face_ijk.coord + offset).normalize();
    }
    face_ijk
}

fn possible_overage(face_ijk: FaceIjk, resolution: u8, is_pentagon: bool) -> bool {
    is_pentagon || resolution != 0 || face_ijk.coord != CoordIjk::new(0, 0, 0)
}

fn adjust_cell_overage(face_ijk: &mut FaceIjk, bits: u64, resolution: u8, pentagon: bool) {
    let original = face_ijk.coord;
    let class_two_resolution = if is_class_three(resolution) {
        face_ijk.coord = face_ijk.coord.down_ap7r();
        resolution + 1
    } else {
        resolution
    };
    let is_pentagon_leading_four = pentagon && first_digit(bits) == Some(4);
    if adjust_overage(face_ijk, class_two_resolution, is_pentagon_leading_four) {
        if pentagon {
            while adjust_overage(face_ijk, class_two_resolution, false) {}
        }
        if class_two_resolution != resolution {
            face_ijk.coord = face_ijk.coord.up_ap7r();
        }
    } else if class_two_resolution != resolution {
        face_ijk.coord = original;
    }
}

fn adjust_overage(
    face_ijk: &mut FaceIjk,
    class_two_resolution: u8,
    is_pentagon_four: bool,
) -> bool {
    let index = usize::from(class_two_resolution);
    let max_dimension = MAX_DIM_BY_CLASS_II_RESOLUTION[index];
    let dimension = face_ijk.coord.i + face_ijk.coord.j + face_ijk.coord.k;
    if dimension <= max_dimension {
        return false;
    }
    let quadrant = overage_quadrant(face_ijk, max_dimension, is_pentagon_four);
    let transform = FACE_NEIGHBORS[usize::from(face_ijk.face)][quadrant];
    face_ijk.face = transform.face;
    for _ in 0..transform.ccw_rotations {
        face_ijk.coord = face_ijk.coord.rotate60_ccw();
    }
    let scale = UNIT_SCALE_BY_CLASS_II_RESOLUTION[index];
    face_ijk.coord = (face_ijk.coord + transform.translate * scale).normalize();
    true
}

fn overage_quadrant(face_ijk: &mut FaceIjk, max_dimension: i32, pentagon_four: bool) -> usize {
    if face_ijk.coord.k == 0 {
        return 0;
    }
    if face_ijk.coord.j > 0 {
        return 2;
    }
    if pentagon_four {
        let origin = CoordIjk::new(max_dimension, 0, 0);
        face_ijk.coord = (face_ijk.coord - origin).rotate60_cw() + origin;
    }
    1
}

fn face_ijk_to_lat_lng(face_ijk: FaceIjk, resolution: u8) -> (f64, f64) {
    let i = f64::from(face_ijk.coord.i - face_ijk.coord.k);
    let j = f64::from(face_ijk.coord.j - face_ijk.coord.k);
    let vector = Vec2 {
        x: (-j).mul_add(0.5, i),
        y: j * SQRT3_2,
    };
    vec2_to_lat_lng(vector, face_ijk.face, resolution)
}

fn vec2_to_lat_lng(vector: Vec2, face: u8, resolution: u8) -> (f64, f64) {
    let magnitude = sqrt_det(vector.x * vector.x + vector.y * vector.y);
    let center = face_center_lat_lng(face).unwrap_or_else(|| unreachable!("validated face"));
    if magnitude < EPSILON {
        return (center.lat, center.lng);
    }
    let scaled = magnitude * INV_SQRT7_POWERS[usize::from(resolution)] * RES0_U_GNOMONIC;
    let distance = atan2_det(scaled, 1.0);
    let mut theta = atan2_det(vector.y, vector.x);
    if is_class_three(resolution) {
        theta = positive_angle(theta + AP7_ROT_RADS);
    }
    let azimuth = positive_angle(CLASS_II_AXIS_AZIMUTHS_RAD[usize::from(face)][0] - theta);
    coordinate_at(face, resolution, center.lat, center.lng, azimuth, distance)
}

fn coordinate_at(
    face: u8,
    resolution: u8,
    center_lat: f64,
    center_lng: f64,
    azimuth: f64,
    distance: f64,
) -> (f64, f64) {
    let due_north_south = azimuth.abs() <= EPSILON || (azimuth - PI).abs() <= EPSILON;
    let (sin_center_lat, cos_center_lat) = FACE_CENTER_SIN_COS_LAT[usize::from(face)];
    let sin_distance = sin_det(distance);
    let cos_distance = cos_det(distance);
    let (sin_azimuth, cos_azimuth) = if resolution > 0 && azimuth <= PI {
        normalized_sin_cos(azimuth)
    } else {
        (sin_det(azimuth), cos_det(azimuth))
    };
    let lat = if due_north_south {
        center_lat
            + if azimuth.abs() <= EPSILON {
                distance
            } else {
                -distance
            }
    } else {
        let value =
            sin_center_lat.mul_add(cos_distance, cos_center_lat * sin_distance * cos_azimuth);
        asin_det(value.clamp(-1.0, 1.0))
    };
    if (lat - FRAC_PI_2).abs() <= EPSILON {
        return (FRAC_PI_2, 0.0);
    }
    if (lat + FRAC_PI_2).abs() <= EPSILON {
        return (-FRAC_PI_2, 0.0);
    }
    let lng = if due_north_south {
        center_lng
    } else {
        coordinate_lng(
            center_lng,
            sin_center_lat,
            cos_center_lat,
            lat,
            sin_azimuth,
            sin_distance,
            cos_distance,
        )
    };
    (lat, signed_angle(lng))
}

fn coordinate_lng(
    center_lng: f64,
    sin_center_lat: f64,
    cos_center_lat: f64,
    lat: f64,
    sin_azimuth: f64,
    sin_distance: f64,
    cos_distance: f64,
) -> f64 {
    let sin_lat = sin_det(lat);
    let cos_lat = cos_det(lat);
    let sin_lng = (sin_azimuth * sin_distance / cos_lat).clamp(-1.0, 1.0);
    let cos_lng = sin_center_lat.mul_add(-sin_lat, cos_distance) / cos_center_lat / cos_lat;
    let delta = refined_atan2(sin_lng, cos_lng);
    center_lng + delta
}

fn normalized_sin_cos(angle: f64) -> (f64, f64) {
    let sine = sin_det(angle);
    let cosine = cos_det(angle);
    let norm_excess = unit_circle_excess(sine, cosine);
    if norm_excess > 0.0 {
        let scale = 1.0 - norm_excess * 0.5;
        (sine * scale, cosine * scale)
    } else {
        (sine, cosine)
    }
}

fn unit_circle_excess(sine: f64, cosine: f64) -> f64 {
    let sine_square = sine * sine;
    let cosine_square = cosine * cosine;
    let sum = sine_square + cosine_square;
    let virtual_cosine = sum - sine_square;
    let sum_error = (sine_square - (sum - virtual_cosine)) + (cosine_square - virtual_cosine);
    let product_error = sine.mul_add(sine, -sine_square) + cosine.mul_add(cosine, -cosine_square);
    (sum - 1.0) + sum_error + product_error
}

fn refined_atan2(y: f64, x: f64) -> f64 {
    let angle = atan2_det(y, x);
    let sine = sin_det(angle);
    let cosine = cos_det(angle);
    let residual = (y * cosine - x * sine) / (x * cosine + y * sine);
    angle + residual
}

fn canonical_degrees(lat: f64, lng: f64) -> (f64, f64) {
    let lat = lat * DEGREES_PER_RADIAN;
    if lat == 90.0 || lat == -90.0 {
        return (lat, 0.0);
    }
    let mut lng = lng * DEGREES_PER_RADIAN;
    if lng >= 180.0 {
        lng -= 360.0;
    }
    (lat, lng)
}

const fn is_class_three(resolution: u8) -> bool {
    resolution % 2 == 1
}

fn positive_angle(mut angle: f64) -> f64 {
    if angle < 0.0 {
        angle += TWO_PI;
    } else if angle >= TWO_PI {
        angle -= TWO_PI;
    }
    angle
}

fn signed_angle(mut angle: f64) -> f64 {
    while angle > PI {
        angle -= TWO_PI;
    }
    while angle < -PI {
        angle += TWO_PI;
    }
    angle
}

fn pentagon_rotate_ccw(bits: u64) -> u64 {
    let count = if first_digit(bits) == Some(3) { 2 } else { 1 };
    rotate_bits_ccw(bits, count)
}

fn rotate_bits_ccw(mut bits: u64, count: u8) -> u64 {
    for resolution in 1..=get_resolution(bits) {
        bits = set_digit(
            bits,
            resolution,
            rotate_digit_ccw(get_digit(bits, resolution), count),
        );
    }
    bits
}

fn rotate_bits_cw(mut bits: u64, count: u8) -> u64 {
    for resolution in 1..=get_resolution(bits) {
        bits = set_digit(
            bits,
            resolution,
            rotate_digit_cw(get_digit(bits, resolution), count),
        );
    }
    bits
}

fn rotate_digit_ccw(mut digit: u8, count: u8) -> u8 {
    const ROTATE: [u8; 7] = [0, 5, 3, 1, 6, 4, 2];
    for _ in 0..count {
        digit = ROTATE[usize::from(digit)];
    }
    digit
}

fn rotate_digit_cw(mut digit: u8, count: u8) -> u8 {
    const ROTATE: [u8; 7] = [0, 3, 6, 2, 5, 1, 4];
    for _ in 0..count {
        digit = ROTATE[usize::from(digit)];
    }
    digit
}

fn first_digit(bits: u64) -> Option<u8> {
    (1..=get_resolution(bits))
        .map(|resolution| get_digit(bits, resolution))
        .find(|digit| *digit != 0)
}

const fn get_resolution(bits: u64) -> u8 {
    ((bits >> 52) & 0b1111) as u8
}

const fn set_resolution(bits: u64, resolution: u8) -> u64 {
    (bits & !(0b1111 << 52)) | ((resolution as u64) << 52)
}

const fn set_base_cell(bits: u64, base_cell: u8) -> u64 {
    (bits & !(0b111_1111 << 45)) | ((base_cell as u64) << 45)
}

const fn digit_offset(resolution: u8) -> u32 {
    (3 * (MAX_RESOLUTION - resolution)) as u32
}

const fn get_digit(bits: u64, resolution: u8) -> u8 {
    ((bits >> digit_offset(resolution)) & 0b111) as u8
}

const fn set_digit(bits: u64, resolution: u8, digit: u8) -> u64 {
    let offset = digit_offset(resolution);
    (bits & !(0b111 << offset)) | ((digit as u64) << offset)
}
