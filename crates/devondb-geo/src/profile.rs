//! DevonGrid profile identifiers and the format-v1 profile pin.

use crate::GeoError;

/// An explicit DevonGrid assignment profile.
///
/// Profile IDs are carried even when this build does not implement them so
/// format extensions can reject unsupported profiles at assignment entry
/// points instead of silently falling back to profile 0.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Profile(u8);

impl Profile {
    /// Profile 0: the frozen H3-compatible DevonGrid assignment profile.
    pub const H3_COMPATIBLE: Self = Self(0);

    /// Constructs a profile carrier from its format-level ID.
    ///
    /// Assignment entry points return [`GeoError`] when this build does not
    /// implement the carried ID.
    #[must_use]
    pub const fn from_id(id: u8) -> Self {
        Self(id)
    }

    /// Returns the format-level profile ID.
    #[must_use]
    pub const fn id(self) -> u8 {
        self.0
    }
}

/// The profile permanently pinned by the format-v1 geo storage freeze.
///
/// A future profile requires a feature-bit format extension that records its
/// ID; format-v1 databases have no profile field because they are profile 0.
pub const FORMAT_V1_PROFILE: Profile = Profile::H3_COMPATIBLE;

pub(crate) fn require_implemented(profile: Profile) -> Result<(), GeoError> {
    if profile == Profile::H3_COMPATIBLE {
        return Ok(());
    }
    Err(GeoError::InvalidArgument {
        value_bits: f64::from(profile.id()).to_bits(),
        reason: format!("profile {} is not implemented by this build", profile.id()),
    })
}
