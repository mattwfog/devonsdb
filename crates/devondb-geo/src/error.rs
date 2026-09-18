//! devondb-geo error type.

use thiserror::Error;

/// Errors from DevonGrid cell-index operations (`docs/GEO.md`).
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum GeoError {
    /// A raw u64 failed cell-index validation (`docs/GEO.md` §3).
    #[error("invalid cell index {index:#018x}: {reason}")]
    InvalidCellIndex {
        /// The rejected raw value.
        index: u64,
        /// The first validation rule it broke, in spec words.
        reason: String,
    },
    /// A resolution outside `0..=15`.
    #[error("invalid resolution {resolution}: must be 0..=15")]
    InvalidResolution {
        /// The rejected resolution.
        resolution: u8,
    },
    /// A geographic argument (degrees or meters) failed validation.
    #[error("invalid geo argument {}: {reason}", f64::from_bits(*value_bits))]
    InvalidArgument {
        /// `f64::to_bits` of the rejected value (bits keep the enum `Eq`).
        value_bits: u64,
        /// The first validation rule it broke, in spec words.
        reason: String,
    },
    /// A truncation requested a resolution finer than its source cell.
    #[error("cannot truncate resolution-{cell_resolution} cell to finer resolution {resolution}")]
    ResolutionAboveCell {
        /// The rejected target resolution.
        resolution: u8,
        /// The source cell's resolution.
        cell_resolution: u8,
    },
}
