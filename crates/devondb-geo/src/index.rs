//! DevonGrid cell-index format (`docs/GEO.md` §3).

use core::cmp::Ordering;

use crate::GeoError;

const MAX_RESOLUTION: u8 = 15;
const MAX_BASE_CELL: u8 = 121;
const RESERVED_BIT_MASK: u64 = 1 << 63;
const MODE_SHIFT: u32 = 59;
const MODE_MASK: u64 = 0b1111;
const CELL_MODE: u8 = 1;
const MODE_DEPENDENT_SHIFT: u32 = 56;
const MODE_DEPENDENT_MASK: u64 = 0b111;
const RESOLUTION_SHIFT: u32 = 52;
const RESOLUTION_MASK: u64 = 0b1111;
const BASE_CELL_SHIFT: u32 = 45;
const BASE_CELL_MASK: u64 = 0b111_1111;
const DIGIT_MASK: u64 = 0b111;

/// The twelve H3 pentagon base-cell numbers for DevonGrid profile 0.
///
/// These values are frozen from the pinned `h3o` oracle.
pub const PENTAGON_BASE_CELLS: [u8; 12] = [4, 14, 24, 38, 49, 58, 63, 72, 83, 97, 107, 117];

/// A validated profile-0 DevonGrid cell index.
///
/// Its raw `u64` spelling is identical to an H3 cell-mode index.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
#[repr(transparent)]
pub struct CellIndex(u64);

impl CellIndex {
    /// Returns the validated index's raw H3-compatible spelling.
    #[must_use]
    pub const fn raw(self) -> u64 {
        self.0
    }

    /// Returns this profile-0 cell's H3 index spelling.
    ///
    /// Conversion is the identity because DevonGrid profile 0 uses the H3
    /// cell-mode bit layout exactly (`docs/GEO.md` §2).
    #[must_use]
    pub const fn to_h3(self) -> u64 {
        self.0
    }

    /// Validates an H3 cell-mode index as a profile-0 DevonGrid cell.
    ///
    /// # Errors
    ///
    /// Returns an error when `raw` is not a valid profile-0 cell spelling.
    pub fn from_h3(raw: u64) -> Result<Self, GeoError> {
        Self::try_from(raw)
    }

    /// Returns the cell resolution in `0..=15`.
    #[must_use]
    pub const fn resolution(self) -> u8 {
        field(self.0, RESOLUTION_SHIFT, RESOLUTION_MASK)
    }

    /// Returns the base-cell number in `0..=121`.
    #[must_use]
    pub const fn base_cell(self) -> u8 {
        field(self.0, BASE_CELL_SHIFT, BASE_CELL_MASK)
    }

    /// Returns digit `position`, where positions are numbered `1..=15`.
    ///
    /// Digits beyond the cell's resolution are the unused value `7`.
    ///
    /// # Panics
    ///
    /// Panics if `position` is outside `1..=15`.
    #[must_use]
    pub fn digit(self, position: u8) -> u8 {
        assert!(
            (1..=MAX_RESOLUTION).contains(&position),
            "cell digit position must be 1..=15"
        );
        raw_digit(self.0, position)
    }

    /// Returns the immediate parent, or `None` for a resolution-zero cell.
    #[must_use]
    pub fn parent(self) -> Option<Self> {
        let resolution = self.resolution();
        (resolution > 0).then(|| Self(truncate_raw(self.0, resolution - 1)))
    }

    /// Truncates this cell to `resolution` using DevonGrid digit semantics.
    ///
    /// # Errors
    ///
    /// Returns an error if the resolution is above 15 or finer than this cell.
    pub fn truncate_to(self, resolution: u8) -> Result<Self, GeoError> {
        if resolution > MAX_RESOLUTION {
            return Err(GeoError::InvalidResolution { resolution });
        }
        let cell_resolution = self.resolution();
        if resolution > cell_resolution {
            return Err(GeoError::ResolutionAboveCell {
                resolution,
                cell_resolution,
            });
        }
        Ok(Self(truncate_raw(self.0, resolution)))
    }

    /// Returns whether this cell is a resolution-15 atom.
    #[must_use]
    pub const fn is_atom(self) -> bool {
        self.resolution() == MAX_RESOLUTION
    }

    /// Returns the inclusive raw-u64 interval for this cell's descendants.
    ///
    /// Valid atom keys inside the interval are exactly this cell's descendants.
    /// Invalid raw values also occupy the interval, but never occur in a
    /// DevonGrid ordered index.
    #[must_use]
    pub fn atom_range(self) -> (u64, u64) {
        let resolution = self.resolution();
        let mut lower = set_resolution(self.0, MAX_RESOLUTION);
        let mut upper = lower;
        for position in (resolution + 1)..=MAX_RESOLUTION {
            lower = set_digit(lower, position, 0);
            upper = set_digit(upper, position, 6);
        }
        (lower, upper)
    }
}

/// Cell indexes are ordered by their raw `u64` values. This is semantic:
/// resolution-15 atom keys sort into descendant-contiguous intervals.
impl Ord for CellIndex {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0.cmp(&other.0)
    }
}

impl PartialOrd for CellIndex {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl TryFrom<u64> for CellIndex {
    type Error = GeoError;

    fn try_from(raw: u64) -> Result<Self, Self::Error> {
        let (resolution, base_cell) = validate_header(raw)?;
        validate_digits(raw, resolution)?;
        validate_pentagon(raw, resolution, base_cell)?;
        Ok(Self(raw))
    }
}

fn validate_header(raw: u64) -> Result<(u8, u8), GeoError> {
    if raw & RESERVED_BIT_MASK != 0 {
        return Err(invalid(raw, "reserved bit must be zero"));
    }
    if field(raw, MODE_SHIFT, MODE_MASK) != CELL_MODE {
        return Err(invalid(raw, "mode must be 1 (cell)"));
    }
    if field(raw, MODE_DEPENDENT_SHIFT, MODE_DEPENDENT_MASK) != 0 {
        return Err(invalid(
            raw,
            "mode-dependent bits must be zero in cell mode",
        ));
    }
    let resolution = field(raw, RESOLUTION_SHIFT, RESOLUTION_MASK);
    if resolution > MAX_RESOLUTION {
        return Err(invalid(raw, "resolution must be 0..=15"));
    }
    let base_cell = field(raw, BASE_CELL_SHIFT, BASE_CELL_MASK);
    if base_cell > MAX_BASE_CELL {
        return Err(invalid(raw, "base cell must be 0..=121"));
    }
    Ok((resolution, base_cell))
}

fn validate_digits(raw: u64, resolution: u8) -> Result<(), GeoError> {
    for position in 1..=resolution {
        let digit = raw_digit(raw, position);
        if digit > 6 {
            let reason = format!("digit {position} must be 0..=6 at resolution {resolution}");
            return Err(invalid_owned(raw, reason));
        }
    }
    for position in (resolution + 1)..=MAX_RESOLUTION {
        if raw_digit(raw, position) != 7 {
            let reason = format!("digit {position} must be 7 beyond resolution {resolution}");
            return Err(invalid_owned(raw, reason));
        }
    }
    Ok(())
}

fn validate_pentagon(raw: u64, resolution: u8, base_cell: u8) -> Result<(), GeoError> {
    if !PENTAGON_BASE_CELLS.contains(&base_cell) {
        return Ok(());
    }
    for position in 1..=resolution {
        match raw_digit(raw, position) {
            0 => {}
            1 => {
                return Err(invalid(
                    raw,
                    "pentagon cell's leading non-center digit must not be 1",
                ));
            }
            _ => return Ok(()),
        }
    }
    Ok(())
}

const fn field(raw: u64, shift: u32, mask: u64) -> u8 {
    ((raw >> shift) & mask) as u8
}

const fn digit_shift(position: u8) -> u32 {
    (3 * (MAX_RESOLUTION - position)) as u32
}

const fn raw_digit(raw: u64, position: u8) -> u8 {
    field(raw, digit_shift(position), DIGIT_MASK)
}

const fn set_resolution(raw: u64, resolution: u8) -> u64 {
    let mask = RESOLUTION_MASK << RESOLUTION_SHIFT;
    (raw & !mask) | ((resolution as u64) << RESOLUTION_SHIFT)
}

const fn set_digit(raw: u64, position: u8, digit: u8) -> u64 {
    let shift = digit_shift(position);
    let mask = DIGIT_MASK << shift;
    (raw & !mask) | ((digit as u64) << shift)
}

fn truncate_raw(raw: u64, resolution: u8) -> u64 {
    let mut truncated = set_resolution(raw, resolution);
    for position in (resolution + 1)..=MAX_RESOLUTION {
        truncated = set_digit(truncated, position, 7);
    }
    truncated
}

fn invalid(index: u64, reason: &'static str) -> GeoError {
    invalid_owned(index, reason.to_owned())
}

fn invalid_owned(index: u64, reason: String) -> GeoError {
    GeoError::InvalidCellIndex { index, reason }
}
