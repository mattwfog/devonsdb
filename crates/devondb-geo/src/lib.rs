//! DevonGrid — devondb's geospatial cell system (`docs/GEO.md`).
//!
//! Profile 0 is H3-compatible: cell indexes ARE H3 indexes, while the
//! hierarchy is made congruent by atom-truncation semantics
//! (`docs/GEO.md` §1). Runtime code in this crate is dependency-free;
//! the `h3o` crate appears as a dev-dependency oracle only (§4). Tables and
//! kernels ported from `h3o` and `libm` carry `Provenance:` comments; their
//! licenses are reproduced in `THIRD_PARTY_LICENSES.md` beside this crate.

pub mod base_cells;
pub mod coordijk;
pub mod covering;
pub mod error;
pub mod faces;
pub mod grid;
pub mod index;
pub mod math;
pub mod profile;
pub mod projection;

pub use error::GeoError;
pub use profile::{FORMAT_V1_PROFILE, Profile};
