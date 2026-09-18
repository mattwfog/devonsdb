//! The DevonGrid congruence API (`docs/GEO.md` §1) — the only assignment
//! surface engine code should use.
//!
//! Profile-0 DevonGrid cell indexes are H3 indexes: conversion to H3 is the
//! identity operation on their raw `u64` spelling (`docs/GEO.md` §2). Point
//! assignment differs near the documented crinkle band because DevonGrid
//! assigns only resolution-15 atoms directly, then truncates those atoms to
//! obtain every coarser cell. Use [`h3_compat_cell`] only when direct stock-H3
//! assignment is specifically required for interoperability checks.

use crate::{
    FORMAT_V1_PROFILE, GeoError, Profile, index::CellIndex, profile::require_implemented,
    projection,
};

/// Assigns canonical WGS84 degrees to their format-v1 DevonGrid atom.
///
/// This shorthand uses [`FORMAT_V1_PROFILE`] because format v1 permanently
/// pins profile 0 and has no stored profile field. Use [`atom_in`] when the
/// caller already carries an explicit profile.
///
/// # Errors
///
/// Returns an error when either coordinate is non-finite or out of range.
pub fn atom(lat_deg: f64, lng_deg: f64) -> Result<CellIndex, GeoError> {
    atom_in(FORMAT_V1_PROFILE, lat_deg, lng_deg)
}

/// Assigns canonical WGS84 degrees to their resolution-15 DevonGrid atom.
///
/// This is the only direct point-to-cell assignment used by DevonGrid
/// indexing (`docs/GEO.md` §1).
///
/// # Errors
///
/// Returns an error for an unsupported profile or invalid coordinate.
pub fn atom_in(profile: Profile, lat_deg: f64, lng_deg: f64) -> Result<CellIndex, GeoError> {
    require_implemented(profile)?;
    projection::atom_of(lat_deg, lng_deg)
}

/// Assigns canonical WGS84 degrees to a format-v1 DevonGrid cell at `res`.
///
/// This shorthand uses [`FORMAT_V1_PROFILE`] because format v1 permanently
/// pins profile 0 and has no stored profile field. Use [`cell_in`] when the
/// caller already carries an explicit profile.
///
/// # Errors
///
/// Returns an error for an invalid coordinate or a resolution above 15.
pub fn cell(lat_deg: f64, lng_deg: f64, res: u8) -> Result<CellIndex, GeoError> {
    cell_in(FORMAT_V1_PROFILE, lat_deg, lng_deg, res)
}

/// Assigns canonical WGS84 degrees to a congruent DevonGrid cell at `res`.
///
/// The point is first assigned to its resolution-15 [`atom_in`], which is then
/// digit-truncated. Direct projection at `res` is deliberately forbidden here:
/// atom truncation makes parent assignment congruent by construction, whereas
/// direct stock-H3 assignments disagree across resolutions in the documented
/// crinkle band (`docs/GEO.md` §1).
///
/// # Errors
///
/// Returns an error for an unsupported profile, invalid coordinate, or a
/// resolution above 15.
pub fn cell_in(
    profile: Profile,
    lat_deg: f64,
    lng_deg: f64,
    res: u8,
) -> Result<CellIndex, GeoError> {
    atom_in(profile, lat_deg, lng_deg)?.truncate_to(res)
}

/// Directly assigns format-v1 coordinates using stock-H3-compatible rules.
///
/// This shorthand uses [`FORMAT_V1_PROFILE`] because format v1 permanently
/// pins profile 0 and has no stored profile field. Use [`h3_compat_cell_in`]
/// when the caller already carries an explicit profile.
///
/// # Errors
///
/// Returns an error for an invalid coordinate or a resolution above 15.
pub fn h3_compat_cell(lat_deg: f64, lng_deg: f64, res: u8) -> Result<CellIndex, GeoError> {
    h3_compat_cell_in(FORMAT_V1_PROFILE, lat_deg, lng_deg, res)
}

/// Directly assigns canonical WGS84 degrees using stock-H3-compatible rules.
///
/// This non-congruent escape hatch is bit-compatible with stock H3 and exists
/// only for interoperability verification and migration checks. It must never
/// be used to build DevonGrid indexes: near the `docs/GEO.md` §1 crinkle band,
/// direct assignments can disagree with their own parent or child assignment.
///
/// # Errors
///
/// Returns an error for an unsupported profile, invalid coordinate, or a
/// resolution above 15.
pub fn h3_compat_cell_in(
    profile: Profile,
    lat_deg: f64,
    lng_deg: f64,
    res: u8,
) -> Result<CellIndex, GeoError> {
    require_implemented(profile)?;
    projection::cell_at(lat_deg, lng_deg, res)
}

/// Returns the format-v1 ordered-index interval containing `cell`'s atoms.
///
/// This shorthand uses [`FORMAT_V1_PROFILE`] because format v1 permanently
/// pins profile 0 and has no stored profile field. Use
/// [`descendant_atom_range_in`] when the caller already carries an explicit
/// profile.
#[must_use]
pub fn descendant_atom_range(cell: CellIndex) -> (u64, u64) {
    // FORMAT_V1_PROFILE is implemented by every format-v1-capable build.
    descendant_atom_range_in(FORMAT_V1_PROFILE, cell).unwrap_or_else(|_| unreachable!())
}

/// Returns the inclusive ordered-index interval containing `cell`'s atoms.
///
/// The bounds are raw resolution-15 cell-index spellings. Valid stored atom
/// keys inside `[lo, hi]` are exactly the descendants of `cell`; invalid raw
/// values in the numeric interval never occur in the ordered index
/// (`docs/GEO.md` §3, "Atom keys and descendant ranges").
///
/// # Errors
///
/// Returns an error when `profile` is not implemented by this build.
pub fn descendant_atom_range_in(profile: Profile, cell: CellIndex) -> Result<(u64, u64), GeoError> {
    require_implemented(profile)?;
    Ok(cell.atom_range())
}
