//! Icosahedron face tables for DevonGrid profile 0 (`docs/GEO.md` §3).

use std::sync::LazyLock;

use crate::math::Vec3;

/// Number of icosahedron faces in profile 0.
pub const FACE_COUNT: usize = 20;

/// A latitude/longitude pair in radians.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LatLngRad {
    /// Latitude in radians.
    pub lat: f64,
    /// Longitude in radians.
    pub lng: f64,
}

impl LatLngRad {
    const fn new(lat: f64, lng: f64) -> Self {
        Self { lat, lng }
    }
}

/// Icosahedron face centers as latitude/longitude pairs in radians.
///
/// Provenance: `h3o-0.8.0::face::CENTER_GEO`.
#[rustfmt::skip]
pub const FACE_CENTERS_LAT_LNG_RAD: [LatLngRad; FACE_COUNT] = [
    LatLngRad::new( 0.80358264971899,     1.2483974196173961),
    LatLngRad::new( 1.3077478834556382,   2.5369450098779214),
    LatLngRad::new( 1.054751253523952,   -1.3475173589003966),
    LatLngRad::new( 0.6001915955381868,  -0.45060390946975576),
    LatLngRad::new( 0.49171542819877384,  0.40198820291130694),
    LatLngRad::new( 0.1727453274156187,   1.6781468852804338),
    LatLngRad::new( 0.6059293215713507,   2.9539233298124117),
    LatLngRad::new( 0.42737051832897965, -1.8888762003362853),
    LatLngRad::new(-0.07906611854921283, -0.7334295133808677),
    LatLngRad::new(-0.23096164445538364,  0.506495587332349),
    LatLngRad::new( 0.07906611854921283,  2.4081631402089254),
    LatLngRad::new( 0.23096164445538364, -2.635097066257444),
    LatLngRad::new(-0.1727453274156187,  -1.4634457683093596),
    LatLngRad::new(-0.6059293215713507,  -0.18766932377738163),
    LatLngRad::new(-0.42737051832897965,  1.2527164532535078),
    LatLngRad::new(-0.6001915955381868,   2.6909887441200375),
    LatLngRad::new(-0.49171542819877384, -2.7396044506784865),
    LatLngRad::new(-0.80358264971899,    -1.8931952339723972),
    LatLngRad::new(-1.3077478834556382,  -0.6046476437118721),
    LatLngRad::new(-1.054751253523952,    1.7940752946893965),
];

/// Icosahedron face centers as Cartesian unit-sphere vectors.
///
/// Provenance: derived from `h3o-0.8.0::face::CENTER_GEO` through
/// [`Vec3::from_lat_lng_rad`]. The Cartesian values are deliberately not a
/// second frozen float table.
pub static FACE_CENTER_VECTORS: LazyLock<[Vec3; FACE_COUNT]> = LazyLock::new(|| {
    FACE_CENTERS_LAT_LNG_RAD.map(|center| Vec3::from_lat_lng_rad(center.lat, center.lng))
});

/// Class-II IJK-axis azimuths in radians for every icosahedron face.
///
/// Each row contains the azimuth from the face center to IJK vertices 0, 1,
/// and 2. Provenance: `h3o-0.8.0::face::AXES_AZ_RADS_CII`.
#[rustfmt::skip]
pub const CLASS_II_AXIS_AZIMUTHS_RAD: [[f64; 3]; FACE_COUNT] = [
    [5.6199582685239395,  3.5255631661307447, 1.4311680637375488],
    [5.7603390817141875,  3.665943979320992,  1.571548876927796],
    [0.78021365439343,    4.969003859179821,  2.8746087567866256],
    [0.4304693639799999,  4.619259568766391,  2.5248644663731956],
    [6.130269123335111,   4.0358740209419155, 1.9414789185487202],
    [2.692877706530643,   0.5984826041374471, 4.787272808923838],
    [2.982963003477244,   0.8885679010840484, 5.07735810587044],
    [3.532912002790141,   1.4385169003969456, 5.627307105183337],
    [3.494305004259568,   1.3999099018663728, 5.588700106652764],
    [3.0032141694995382,  0.908819067106343,  5.0976092718927335],
    [5.930472956509812,   3.836077854116616,  1.7416827517234204],
    [0.13837848409025486, 4.327168688876646,  2.23277358648345],
    [0.4487149470591504,  4.6375051518455415, 2.543110049452346],
    [0.15862965011254937, 4.3474198548989405, 2.2530247525057447],
    [5.891865957979238,   3.797470855586043,  1.7030757531928475],
    [2.711123289609793,   0.6167281872165977, 4.8055183920029885],
    [3.294508837434268,   1.2001137350410729, 5.388903939827464],
    [3.80481969224544,    1.7104245898522445, 5.8992147946386355],
    [3.6644388790551923,  1.570043776661997,  5.758833981448388],
    [2.361378999196363,   0.2669838968031676, 4.455774101589559],
];

/// Returns a face center's latitude/longitude pair, or `None` for an invalid
/// face number.
#[must_use]
pub fn face_center_lat_lng(face: u8) -> Option<LatLngRad> {
    FACE_CENTERS_LAT_LNG_RAD.get(usize::from(face)).copied()
}

/// Returns a face center's Cartesian vector, or `None` for an invalid face
/// number.
#[must_use]
pub fn face_center_vector(face: u8) -> Option<Vec3> {
    FACE_CENTER_VECTORS.get(usize::from(face)).copied()
}
