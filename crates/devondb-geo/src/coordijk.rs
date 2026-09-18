//! Hex-grid IJK coordinate algebra for DevonGrid profile 0
//! (`docs/GEO.md` §3).

use core::ops::{Add, Mul, Sub};

/// An address in the three-axis coordinate system of a hexagonal grid.
///
/// Canonical coordinates are non-negative and have at least one zero
/// component. Call [`Self::normalize`] when accepting a non-canonical address.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct CoordIjk {
    /// I-axis component.
    pub i: i32,
    /// J-axis component.
    pub j: i32,
    /// K-axis component.
    pub k: i32,
}

impl CoordIjk {
    /// Creates an IJK coordinate without normalizing it.
    #[must_use]
    pub const fn new(i: i32, j: i32, k: i32) -> Self {
        Self { i, j, k }
    }

    /// Returns the unique canonical spelling of this coordinate.
    #[must_use]
    pub fn normalize(self) -> Self {
        let minimum = self.i.min(self.j).min(self.k);
        Self::new(self.i - minimum, self.j - minimum, self.k - minimum)
    }

    /// Adds two coordinates component by component.
    #[must_use]
    pub const fn add(self, other: Self) -> Self {
        Self::new(self.i + other.i, self.j + other.j, self.k + other.k)
    }

    /// Subtracts `other` component by component.
    #[must_use]
    pub const fn sub(self, other: Self) -> Self {
        Self::new(self.i - other.i, self.j - other.j, self.k - other.k)
    }

    /// Multiplies every component by `factor`.
    #[must_use]
    pub const fn scale(self, factor: i32) -> Self {
        Self::new(self.i * factor, self.j * factor, self.k * factor)
    }

    /// Returns the unit coordinate for an H3 digit in `0..=6`.
    #[must_use]
    pub const fn from_digit(digit: u8) -> Option<Self> {
        if digit <= 6 {
            Some(DIGIT_UNIT_VECTORS[digit as usize])
        } else {
            None
        }
    }

    /// Returns the H3 digit represented by this coordinate, if it is a unit
    /// coordinate after normalization.
    #[must_use]
    pub fn to_digit(self) -> Option<u8> {
        let normalized = self.normalize();
        DIGIT_UNIT_VECTORS
            .iter()
            .position(|coordinate| *coordinate == normalized)
            .map(|digit| digit as u8)
    }

    /// Rotates this coordinate 60 degrees clockwise and normalizes it.
    #[must_use]
    pub fn rotate60_cw(self) -> Self {
        let i = Self::new(1, 0, 1).scale(self.i);
        let j = Self::new(1, 1, 0).scale(self.j);
        let k = Self::new(0, 1, 1).scale(self.k);
        (i + j + k).normalize()
    }

    /// Rotates this coordinate 60 degrees counterclockwise and normalizes it.
    #[must_use]
    pub fn rotate60_ccw(self) -> Self {
        let i = Self::new(1, 1, 0).scale(self.i);
        let j = Self::new(0, 1, 1).scale(self.j);
        let k = Self::new(1, 0, 1).scale(self.k);
        (i + j + k).normalize()
    }

    /// Rounds this coordinate to its parent in the aperture-7 grid.
    ///
    /// Provenance: `h3o-0.8.0::coord::ijk::CoordIJK::up_aperture7::<CCW>`.
    /// The oracle performs these two divisions in binary64 and uses Rust's
    /// ties-away-from-zero `round`; this mirrors that float-based formula.
    #[must_use]
    #[allow(clippy::cast_possible_truncation)]
    pub fn up_ap7(self) -> Self {
        let (i, j) = self.ij();
        let parent_i = (f64::from(3 * i - j) / 7.0).round() as i32;
        let parent_j = (f64::from(i + 2 * j) / 7.0).round() as i32;
        Self::new(parent_i, parent_j, 0).normalize()
    }

    /// Rounds this coordinate to its parent in the rotated aperture-7 grid.
    ///
    /// Provenance: `h3o-0.8.0::coord::ijk::CoordIJK::up_aperture7::<CW>`.
    /// The oracle performs these two divisions in binary64 and uses Rust's
    /// ties-away-from-zero `round`; this mirrors that float-based formula.
    #[must_use]
    #[allow(clippy::cast_possible_truncation)]
    pub fn up_ap7r(self) -> Self {
        let (i, j) = self.ij();
        let parent_i = (f64::from(2 * i + j) / 7.0).round() as i32;
        let parent_j = (f64::from(3 * j - i) / 7.0).round() as i32;
        Self::new(parent_i, parent_j, 0).normalize()
    }

    /// Moves this coordinate to the next finer aperture-7 grid.
    #[must_use]
    pub fn down_ap7(self) -> Self {
        let i = Self::new(3, 0, 1).scale(self.i);
        let j = Self::new(1, 3, 0).scale(self.j);
        let k = Self::new(0, 1, 3).scale(self.k);
        (i + j + k).normalize()
    }

    /// Moves this coordinate to the next finer rotated aperture-7 grid.
    #[must_use]
    pub fn down_ap7r(self) -> Self {
        let i = Self::new(3, 1, 0).scale(self.i);
        let j = Self::new(0, 3, 1).scale(self.j);
        let k = Self::new(1, 0, 3).scale(self.k);
        (i + j + k).normalize()
    }

    const fn ij(self) -> (i32, i32) {
        (self.i - self.k, self.j - self.k)
    }
}

impl Add for CoordIjk {
    type Output = Self;

    fn add(self, other: Self) -> Self::Output {
        self.add(other)
    }
}

impl Sub for CoordIjk {
    type Output = Self;

    fn sub(self, other: Self) -> Self::Output {
        self.sub(other)
    }
}

impl Mul<i32> for CoordIjk {
    type Output = Self;

    fn mul(self, factor: i32) -> Self::Output {
        self.scale(factor)
    }
}

/// The seven profile-0 digit unit vectors, indexed by digit value.
///
/// Provenance: `h3o-0.8.0::Direction::coordinate` and the numeric
/// `h3o-0.8.0::Direction` discriminants.
pub const DIGIT_UNIT_VECTORS: [CoordIjk; 7] = [
    CoordIjk::new(0, 0, 0),
    CoordIjk::new(0, 0, 1),
    CoordIjk::new(0, 1, 0),
    CoordIjk::new(0, 1, 1),
    CoordIjk::new(1, 0, 0),
    CoordIjk::new(1, 0, 1),
    CoordIjk::new(1, 1, 0),
];
