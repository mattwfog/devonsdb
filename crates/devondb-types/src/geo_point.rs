//! The `GeoPoint` value: a WGS84 coordinate pair in canonical form
//! (`docs/GEO.md` §5).

use serde::de::{Deserializer, Error as DeError};
use serde::ser::{SerializeStruct, Serializer};
use serde::{Deserialize, Serialize};

use crate::{DevonError, DevonResult};

/// A WGS84 coordinate pair in degrees, always in canonical form
/// (`docs/GEO.md` §5): both components finite, latitude in `[-90, 90]`,
/// longitude in `[-180, 180)` (+180 spells as −180), and longitude 0 at
/// the poles.
///
/// Fields are private so every constructed value is canonical; decode
/// paths REJECT non-canonical input rather than normalizing it, so
/// persisted and pinned bytes can never drift.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GeoPoint {
    lat_deg: f64,
    lng_deg: f64,
}

impl GeoPoint {
    /// Builds a canonical point from already-canonical components.
    ///
    /// # Errors
    ///
    /// Returns [`DevonError::InvalidArgument`] naming the first violated
    /// canonical-form rule (`docs/GEO.md` §5) otherwise.
    pub fn from_canonical(lat_deg: f64, lng_deg: f64) -> DevonResult<Self> {
        if let Some(rule) = canonical_violation(lat_deg, lng_deg) {
            return Err(DevonError::InvalidArgument {
                context: format!("GeoPoint({lat_deg}, {lng_deg}): {rule}"),
            });
        }
        Ok(Self { lat_deg, lng_deg })
    }

    /// Convenience constructor: normalizes `+180` longitude to `−180` and
    /// pole longitudes to `0`, then validates.
    ///
    /// # Errors
    ///
    /// Returns [`DevonError::InvalidArgument`] for non-finite components
    /// or out-of-range latitude/longitude — normalization only folds the
    /// two canonical spellings, it never wraps.
    pub fn new(lat_deg: f64, lng_deg: f64) -> DevonResult<Self> {
        let lng_deg = if lng_deg == 180.0 { -180.0 } else { lng_deg };
        let lng_deg = if lat_deg == 90.0 || lat_deg == -90.0 {
            0.0
        } else {
            lng_deg
        };
        Self::from_canonical(lat_deg, lng_deg)
    }

    /// Latitude in degrees.
    #[must_use]
    pub const fn lat_deg(self) -> f64 {
        self.lat_deg
    }

    /// Longitude in degrees.
    #[must_use]
    pub const fn lng_deg(self) -> f64 {
        self.lng_deg
    }
}

/// Returns the first violated canonical-form rule, if any.
fn canonical_violation(lat_deg: f64, lng_deg: f64) -> Option<&'static str> {
    if !lat_deg.is_finite() || !lng_deg.is_finite() {
        return Some("components must be finite");
    }
    if !(-90.0..=90.0).contains(&lat_deg) {
        return Some("latitude must be in [-90, 90]");
    }
    if !(-180.0..180.0).contains(&lng_deg) {
        return Some("longitude must be in [-180, 180); +180 spells as -180");
    }
    if (lat_deg == 90.0 || lat_deg == -90.0) && lng_deg != 0.0 {
        return Some("longitude must be 0 at the poles");
    }
    None
}

impl Serialize for GeoPoint {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("GeoPoint", 2)?;
        state.serialize_field("lat_deg", &self.lat_deg)?;
        state.serialize_field("lng_deg", &self.lng_deg)?;
        state.end()
    }
}

/// Strict decode intermediate: unknown fields are rejected and the
/// canonical form is enforced before a [`GeoPoint`] exists.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GeoPointRepr {
    lat_deg: f64,
    lng_deg: f64,
}

impl<'de> Deserialize<'de> for GeoPoint {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let repr = GeoPointRepr::deserialize(deserializer)?;
        Self::from_canonical(repr.lat_deg, repr.lng_deg).map_err(D::Error::custom)
    }
}

impl std::fmt::Display for GeoPoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "geo({}, {})", self.lat_deg, self.lng_deg)
    }
}

#[cfg(test)]
mod tests {
    use super::GeoPoint;

    #[test]
    fn canonical_spelling_round_trips() {
        let point = GeoPoint::from_canonical(45.5, -122.625).unwrap();
        let json = serde_json::to_string(&point).unwrap();
        assert_eq!(json, r#"{"lat_deg":45.5,"lng_deg":-122.625}"#);
        let back: GeoPoint = serde_json::from_str(&json).unwrap();
        assert_eq!(back, point);
        assert_eq!(point.to_string(), "geo(45.5, -122.625)");
    }

    #[test]
    fn new_normalizes_only_the_two_canonical_spellings() {
        assert_eq!(GeoPoint::new(10.0, 180.0).unwrap().lng_deg(), -180.0);
        assert_eq!(GeoPoint::new(90.0, 55.0).unwrap().lng_deg(), 0.0);
        assert_eq!(GeoPoint::new(-90.0, -1.0).unwrap().lng_deg(), 0.0);
        assert!(GeoPoint::new(10.0, 180.5).is_err());
        assert!(GeoPoint::new(-91.0, 0.0).is_err());
    }

    #[test]
    fn decode_rejects_every_non_canonical_form() {
        let rejected = [
            r#"{"lat_deg":10.0,"lng_deg":180.0}"#,
            r#"{"lat_deg":90.0,"lng_deg":5.0}"#,
            r#"{"lat_deg":-90.5,"lng_deg":0.0}"#,
            r#"{"lat_deg":null,"lng_deg":0.0}"#,
            r#"{"lat_deg":10.0}"#,
            r#"{"lat_deg":10.0,"lng_deg":0.0,"alt":3.0}"#,
        ];
        for json in rejected {
            assert!(
                serde_json::from_str::<GeoPoint>(json).is_err(),
                "accepted non-canonical GeoPoint: {json}"
            );
        }
        let nan = format!(r#"{{"lat_deg":{},"lng_deg":0.0}}"#, "1e999");
        assert!(serde_json::from_str::<GeoPoint>(&nan).is_err());
    }

    #[test]
    fn from_canonical_rejects_what_new_normalizes() {
        assert!(GeoPoint::from_canonical(10.0, 180.0).is_err());
        assert!(GeoPoint::from_canonical(90.0, 5.0).is_err());
        assert!(GeoPoint::from_canonical(f64::NAN, 0.0).is_err());
    }
}
