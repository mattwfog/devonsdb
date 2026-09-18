//! Disc covering compiler (`docs/GEO.md` §1): geo predicates compile to
//! contiguous atom-key ranges plus a conservative boundary re-check set.

use core::f64::consts::PI;

use crate::{
    GeoError,
    index::{CellIndex, PENTAGON_BASE_CELLS},
    math::{atan2_det, cos_det, sin_det, sqrt_det},
    projection,
};

const MAX_RESOLUTION: u8 = 15;
const MAX_BASE_CELL: u8 = 121;
const DEFAULT_CELL_INDEX: u64 = 0x0800_1fff_ffff_ffff;
const RESOLUTION_SHIFT: u32 = 52;
const RESOLUTION_MASK: u64 = 0b1111 << RESOLUTION_SHIFT;
const BASE_CELL_SHIFT: u32 = 45;
const DIGIT_MASK: u64 = 0b111;
const RADIANS_PER_DEGREE: f64 = PI / 180.0;

// H3 4.x `H3_EARTH_RADIUS_KM`, exposed verbatim by h3o 0.8.0 as
// `h3o::EARTH_RADIUS_KM` in `src/lib.rs`; WGS84 authalic radius.
const EARTH_RADIUS_M: f64 = 6_371_007.180_918_475;

/// Conservative maximum center-to-region distances for resolutions 0..=15.
///
/// Frozen from the h3o 0.8.0 oracle: every cell at resolutions 0..=5 was
/// measured from its center to every `cell_to_boundary` vertex, followed by
/// 65,536 SplitMix64-selected cells at each finer resolution. Each measured
/// maximum was multiplied by at least 1.3 and rounded upward. The factor also
/// covers DevonGrid's atom-union crinkle fringe (`docs/GEO.md` §1); the oracle
/// re-derivation test locks the frozen values.
pub const MAX_CENTER_TO_BOUNDARY_M: [f64; 16] = [
    1_800_000.0,
    690_000.0,
    262_000.0,
    99_000.0,
    37_500.0,
    14_200.0,
    5_400.0,
    2_050.0,
    770.0,
    290.0,
    110.0,
    42.0,
    16.0,
    6.0,
    2.3,
    0.85,
];

/// Rounding allowance added only when classifying a cell as fully contained.
///
/// Binary64 relative spacing is at most 2^-52. At the largest spherical
/// distance, `PI * EARTH_RADIUS_M`, a conservative 64-rounding error budget is
/// less than 0.3 micrometers
/// (`64 * 2^-52 * PI * 6_371_007.180_918_475`). One micrometer rounds that
/// bound upward and also covers the final add/comparison. The allowance can
/// only demote a formerly `full` cell to boundary refinement; it is
/// deliberately absent from the outside test.
pub const FULL_CONTAINMENT_EPSILON_M: f64 = 0.000_001;

/// A compiled congruent DevonGrid covering of a spherical disc.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiscCovering {
    /// Merged, sorted, non-overlapping inclusive atom-key ranges whose cells
    /// are entirely inside the disc; every stored atom returned by these
    /// ranges matches without an exact-distance re-check.
    pub full: Vec<(u64, u64)>,
    /// Sorted target-resolution cells conservatively straddling the disc
    /// edge; their stored atoms require an exact-distance re-check.
    pub boundary: Vec<CellIndex>,
}

#[derive(Clone, Copy)]
struct Disc {
    lat_deg: f64,
    lng_deg: f64,
    radius_m: f64,
    resolution: u8,
}

/// Returns the great-circle distance in meters between two degree coordinates.
///
/// The haversine calculation uses only DevonGrid's deterministic math kernels
/// and H3's exact WGS84 authalic Earth radius.
#[must_use]
pub fn great_circle_meters(lat_a: f64, lng_a: f64, lat_b: f64, lng_b: f64) -> f64 {
    let lat_a = lat_a * RADIANS_PER_DEGREE;
    let lat_b = lat_b * RADIANS_PER_DEGREE;
    let delta_lat = (lat_b - lat_a) * 0.5;
    let delta_lng = (lng_b - lng_a) * RADIANS_PER_DEGREE * 0.5;
    let sin_lat = sin_det(delta_lat);
    let sin_lng = sin_det(delta_lng);
    let haversine = sin_lat * sin_lat + cos_det(lat_a) * cos_det(lat_b) * sin_lng * sin_lng;
    let clamped = haversine.clamp(0.0, 1.0);
    let angle = 2.0 * atan2_det(sqrt_det(clamped), sqrt_det(1.0 - clamped));
    angle * EARTH_RADIUS_M
}

/// Compiles a spherical disc to full atom-key ranges and boundary cells.
///
/// Refinement starts at the 122 resolution-zero cells and uses only the
/// congruent hierarchy. No neighbor or grid-disk operation is involved.
///
/// # Errors
///
/// Returns an error when a coordinate or radius is non-finite, latitude is
/// outside `[-90, 90]`, longitude is outside `[-180, 180]`, radius is not
/// positive, or `res` is above 15.
pub fn cover_disc(
    lat_deg: f64,
    lng_deg: f64,
    radius_m: f64,
    res: u8,
) -> Result<DiscCovering, GeoError> {
    validate_arguments(lat_deg, lng_deg, radius_m, res)?;
    let disc = Disc {
        lat_deg,
        lng_deg: if lng_deg == 180.0 { -180.0 } else { lng_deg },
        radius_m,
        resolution: res,
    };
    let mut full = Vec::new();
    let mut boundary = Vec::new();
    for base_cell in 0..=MAX_BASE_CELL {
        refine_cell(base_cell_index(base_cell)?, disc, &mut full, &mut boundary)?;
    }
    merge_ranges(&mut full);
    boundary.sort_unstable();
    boundary.dedup();
    Ok(DiscCovering { full, boundary })
}

fn validate_arguments(lat: f64, lng: f64, radius: f64, res: u8) -> Result<(), GeoError> {
    if res > MAX_RESOLUTION {
        return Err(GeoError::InvalidResolution { resolution: res });
    }
    if !lat.is_finite() || !(-90.0..=90.0).contains(&lat) {
        return Err(argument_error(
            lat,
            "latitude must be finite and in [-90, 90]",
        ));
    }
    if !lng.is_finite() || !(-180.0..=180.0).contains(&lng) {
        return Err(argument_error(
            lng,
            "longitude must be finite and in [-180, 180]",
        ));
    }
    if !radius.is_finite() || radius <= 0.0 {
        return Err(argument_error(radius, "radius must be finite and positive"));
    }
    Ok(())
}

fn argument_error(value: f64, reason: &'static str) -> GeoError {
    GeoError::InvalidArgument {
        value_bits: value.to_bits(),
        reason: reason.to_owned(),
    }
}

fn base_cell_index(base_cell: u8) -> Result<CellIndex, GeoError> {
    CellIndex::try_from(DEFAULT_CELL_INDEX | (u64::from(base_cell) << BASE_CELL_SHIFT))
}

fn refine_cell(
    cell: CellIndex,
    disc: Disc,
    full: &mut Vec<(u64, u64)>,
    boundary: &mut Vec<CellIndex>,
) -> Result<(), GeoError> {
    let (lat, lng) = projection::cell_center(cell);
    let center_distance = great_circle_meters(disc.lat_deg, disc.lng_deg, lat, lng);
    let bound = MAX_CENTER_TO_BOUNDARY_M[usize::from(cell.resolution())];
    if center_distance + bound + FULL_CONTAINMENT_EPSILON_M <= disc.radius_m {
        full.push(cell.atom_range());
    } else if center_distance - bound > disc.radius_m {
        return Ok(());
    } else if cell.resolution() == disc.resolution {
        boundary.push(cell);
    } else {
        refine_children(cell, disc, full, boundary)?;
    }
    Ok(())
}

fn refine_children(
    cell: CellIndex,
    disc: Disc,
    full: &mut Vec<(u64, u64)>,
    boundary: &mut Vec<CellIndex>,
) -> Result<(), GeoError> {
    for digit in 0..=6 {
        if digit == 1 && is_pentagon(cell) {
            continue;
        }
        refine_cell(child(cell, digit)?, disc, full, boundary)?;
    }
    Ok(())
}

fn is_pentagon(cell: CellIndex) -> bool {
    PENTAGON_BASE_CELLS.contains(&cell.base_cell())
        && (1..=cell.resolution()).all(|position| cell.digit(position) == 0)
}

fn child(cell: CellIndex, digit: u8) -> Result<CellIndex, GeoError> {
    let resolution = cell.resolution() + 1;
    let digit_shift = u32::from(3 * (MAX_RESOLUTION - resolution));
    let raw = (cell.raw() & !RESOLUTION_MASK) | (u64::from(resolution) << RESOLUTION_SHIFT);
    let raw = (raw & !(DIGIT_MASK << digit_shift)) | (u64::from(digit) << digit_shift);
    CellIndex::try_from(raw)
}

fn merge_ranges(ranges: &mut Vec<(u64, u64)>) {
    ranges.sort_unstable();
    let mut write = 0;
    for read in 0..ranges.len() {
        if write > 0 && ranges[read].0 <= ranges[write - 1].1.saturating_add(1) {
            ranges[write - 1].1 = ranges[write - 1].1.max(ranges[read].1);
        } else {
            ranges[write] = ranges[read];
            write += 1;
        }
    }
    ranges.truncate(write);
}
