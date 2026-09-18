//! Base-cell data tables for DevonGrid profile 0 (`docs/GEO.md` §3).

use crate::coordijk::CoordIjk;

/// Number of profile-0 base cells.
pub const BASE_CELL_COUNT: usize = 122;

/// A face number and an IJK coordinate in that face's coordinate system.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct FaceIjk {
    /// Icosahedron face number in `0..=19`.
    pub face: u8,
    /// IJK coordinate on `face`.
    pub coord: CoordIjk,
}

impl FaceIjk {
    /// Creates a face-IJK address.
    #[must_use]
    pub const fn new(face: u8, coord: CoordIjk) -> Self {
        Self { face, coord }
    }
}

/// Frozen metadata for one profile-0 base cell.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BaseCellData {
    /// The base cell's canonical home face and IJK coordinate.
    pub home: FaceIjk,
    /// Whether the base cell is pentagonal.
    pub is_pentagon: bool,
    /// Clockwise-offset adjacent faces for a non-polar pentagon.
    ///
    /// The two polar pentagons have no such pair in the oracle.
    pub cw_offset_faces: Option<[u8; 2]>,
}

/// A resolution-zero face-IJK lookup result.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BaseCellRotation {
    /// Resolved base-cell number.
    pub base_cell: u8,
    /// Counterclockwise 60-degree rotations into the base cell's coordinate
    /// system.
    pub ccw_rotations: u8,
}

macro_rules! base_cell {
    ($home:literal, [$i:literal, $j:literal, $k:literal]) => {
        BaseCellData {
            home: FaceIjk::new($home, CoordIjk::new($i, $j, $k)),
            is_pentagon: false,
            cw_offset_faces: None,
        }
    };
    (pentagon $home:literal, [$i:literal, $j:literal, $k:literal]) => {
        BaseCellData {
            home: FaceIjk::new($home, CoordIjk::new($i, $j, $k)),
            is_pentagon: true,
            cw_offset_faces: None,
        }
    };
    ($home:literal, [$i:literal, $j:literal, $k:literal], ($first:literal, $second:literal)) => {
        BaseCellData {
            home: FaceIjk::new($home, CoordIjk::new($i, $j, $k)),
            is_pentagon: true,
            cw_offset_faces: Some([$first, $second]),
        }
    };
}

/// Metadata for all 122 profile-0 base cells, indexed by base-cell number.
///
/// Provenance: `h3o-0.8.0::base_cell::METADATA` for home coordinates and
/// clockwise offsets, and `h3o-0.8.0::base_cell::BASE_PENTAGONS` for flags.
#[rustfmt::skip]
pub const BASE_CELLS: [BaseCellData; BASE_CELL_COUNT] = [
    base_cell!(1,  [1, 0, 0]),
    base_cell!(2,  [1, 1, 0]),
    base_cell!(1,  [0, 0, 0]),
    base_cell!(2,  [1, 0, 0]),
    base_cell!(pentagon 0, [2, 0, 0]),
    base_cell!(1,  [1, 1, 0]),
    base_cell!(1,  [0, 0, 1]),
    base_cell!(2,  [0, 0, 0]),
    base_cell!(0,  [1, 0, 0]),
    base_cell!(2,  [0, 1, 0]),
    base_cell!(1,  [0, 1, 0]),
    base_cell!(1,  [0, 1, 1]),
    base_cell!(3,  [1, 0, 0]),
    base_cell!(3,  [1, 1, 0]),
    base_cell!(11, [2, 0, 0], (2, 6)),
    base_cell!(4,  [1, 0, 0]),
    base_cell!(0,  [0, 0, 0]),
    base_cell!(6,  [0, 1, 0]),
    base_cell!(0,  [0, 0, 1]),
    base_cell!(2,  [0, 1, 1]),
    base_cell!(7,  [0, 0, 1]),
    base_cell!(2,  [0, 0, 1]),
    base_cell!(0,  [1, 1, 0]),
    base_cell!(6,  [0, 0, 1]),
    base_cell!(10, [2, 0, 0], (1, 5)),
    base_cell!(6,  [0, 0, 0]),
    base_cell!(3,  [0, 0, 0]),
    base_cell!(11, [1, 0, 0]),
    base_cell!(4,  [1, 1, 0]),
    base_cell!(3,  [0, 1, 0]),
    base_cell!(0,  [0, 1, 1]),
    base_cell!(4,  [0, 0, 0]),
    base_cell!(5,  [0, 1, 0]),
    base_cell!(0,  [0, 1, 0]),
    base_cell!(7,  [0, 1, 0]),
    base_cell!(11, [1, 1, 0]),
    base_cell!(7,  [0, 0, 0]),
    base_cell!(10, [1, 0, 0]),
    base_cell!(12, [2, 0, 0], (3, 7)),
    base_cell!(6,  [1, 0, 1]),
    base_cell!(7,  [1, 0, 1]),
    base_cell!(4,  [0, 0, 1]),
    base_cell!(3,  [0, 0, 1]),
    base_cell!(3,  [0, 1, 1]),
    base_cell!(4,  [0, 1, 0]),
    base_cell!(6,  [1, 0, 0]),
    base_cell!(11, [0, 0, 0]),
    base_cell!(8,  [0, 0, 1]),
    base_cell!(5,  [0, 0, 1]),
    base_cell!(14, [2, 0, 0], (0, 9)),
    base_cell!(5,  [0, 0, 0]),
    base_cell!(12, [1, 0, 0]),
    base_cell!(10, [1, 1, 0]),
    base_cell!(4,  [0, 1, 1]),
    base_cell!(12, [1, 1, 0]),
    base_cell!(7,  [1, 0, 0]),
    base_cell!(11, [0, 1, 0]),
    base_cell!(10, [0, 0, 0]),
    base_cell!(13, [2, 0, 0], (4, 8)),
    base_cell!(10, [0, 0, 1]),
    base_cell!(11, [0, 0, 1]),
    base_cell!(9,  [0, 1, 0]),
    base_cell!(8,  [0, 1, 0]),
    base_cell!(6,  [2, 0, 0], (11, 15)),
    base_cell!(8,  [0, 0, 0]),
    base_cell!(9,  [0, 0, 1]),
    base_cell!(14, [1, 0, 0]),
    base_cell!(5,  [1, 0, 1]),
    base_cell!(16, [0, 1, 1]),
    base_cell!(8,  [1, 0, 1]),
    base_cell!(5,  [1, 0, 0]),
    base_cell!(12, [0, 0, 0]),
    base_cell!(7,  [2, 0, 0], (12, 16)),
    base_cell!(12, [0, 1, 0]),
    base_cell!(10, [0, 1, 0]),
    base_cell!(9,  [0, 0, 0]),
    base_cell!(13, [1, 0, 0]),
    base_cell!(16, [0, 0, 1]),
    base_cell!(15, [0, 1, 1]),
    base_cell!(15, [0, 1, 0]),
    base_cell!(16, [0, 1, 0]),
    base_cell!(14, [1, 1, 0]),
    base_cell!(13, [1, 1, 0]),
    base_cell!(5,  [2, 0, 0], (10, 19)),
    base_cell!(8,  [1, 0, 0]),
    base_cell!(14, [0, 0, 0]),
    base_cell!(9,  [1, 0, 1]),
    base_cell!(14, [0, 0, 1]),
    base_cell!(17, [0, 0, 1]),
    base_cell!(12, [0, 0, 1]),
    base_cell!(16, [0, 0, 0]),
    base_cell!(17, [0, 1, 1]),
    base_cell!(15, [0, 0, 1]),
    base_cell!(16, [1, 0, 1]),
    base_cell!(9,  [1, 0, 0]),
    base_cell!(15, [0, 0, 0]),
    base_cell!(13, [0, 0, 0]),
    base_cell!(8,  [2, 0, 0], (13, 17)),
    base_cell!(13, [0, 1, 0]),
    base_cell!(17, [1, 0, 1]),
    base_cell!(19, [0, 1, 0]),
    base_cell!(14, [0, 1, 0]),
    base_cell!(19, [0, 1, 1]),
    base_cell!(17, [0, 1, 0]),
    base_cell!(13, [0, 0, 1]),
    base_cell!(17, [0, 0, 0]),
    base_cell!(16, [1, 0, 0]),
    base_cell!(9,  [2, 0, 0], (14, 18)),
    base_cell!(15, [1, 0, 1]),
    base_cell!(15, [1, 0, 0]),
    base_cell!(18, [0, 1, 1]),
    base_cell!(18, [0, 0, 1]),
    base_cell!(19, [0, 0, 1]),
    base_cell!(17, [1, 0, 0]),
    base_cell!(19, [0, 0, 0]),
    base_cell!(18, [0, 1, 0]),
    base_cell!(18, [1, 0, 1]),
    base_cell!(pentagon 19, [2, 0, 0]),
    base_cell!(19, [1, 0, 0]),
    base_cell!(18, [0, 0, 0]),
    base_cell!(19, [1, 0, 1]),
    base_cell!(18, [1, 0, 0]),
];

/// Returns metadata for `base_cell`, or `None` when it is outside `0..=121`.
#[must_use]
pub fn base_cell_data(base_cell: u8) -> Option<&'static BaseCellData> {
    BASE_CELLS.get(usize::from(base_cell))
}

/// Returns the canonical home face-IJK address for `base_cell`.
#[must_use]
pub fn home_face_ijk(base_cell: u8) -> Option<FaceIjk> {
    base_cell_data(base_cell).map(|data| data.home)
}

/// Resolves a resolution-zero face-IJK address to its base-cell number.
///
/// Coordinates in the oracle's resolution-zero neighborhood have components
/// in `0..=2`. Non-neighborhood coordinates and invalid faces return `None`.
#[must_use]
pub fn base_cell_from_face_ijk(address: FaceIjk) -> Option<u8> {
    base_cell_rotation_from_face_ijk(address).map(|result| result.base_cell)
}

/// Resolves a resolution-zero face-IJK address and its base-cell orientation.
///
/// Coordinates in the oracle's resolution-zero neighborhood have components
/// in `0..=2`. Non-neighborhood coordinates and invalid faces return `None`.
#[must_use]
pub fn base_cell_rotation_from_face_ijk(address: FaceIjk) -> Option<BaseCellRotation> {
    let i = component_index(address.coord.i)?;
    let j = component_index(address.coord.j)?;
    let k = component_index(address.coord.k)?;
    FACE_IJK_BASE_CELLS
        .get(usize::from(address.face))
        .map(|face| face[i][j][k])
}

fn component_index(component: i32) -> Option<usize> {
    usize::try_from(component).ok().filter(|value| *value <= 2)
}

macro_rules! bcr {
    ($base_cell:literal, $rotation:literal) => {
        BaseCellRotation {
            base_cell: $base_cell,
            ccw_rotations: $rotation,
        }
    };
}

/// Resolution-zero base-cell lookup for each face and IJK neighborhood.
///
/// Provenance: `h3o-0.8.0::coord::faceijk::FACE_IJK_BASE_CELLS`, including
/// both the base-cell values and counterclockwise rotation counts.
#[rustfmt::skip]
const FACE_IJK_BASE_CELLS: [[[[BaseCellRotation; 3]; 3]; 3]; 20] = [
    [
        [
            [bcr!(16, 0), bcr!(18, 0), bcr!(24, 0)],
            [bcr!(33, 0), bcr!(30, 0), bcr!(32, 3)],
            [bcr!(49, 1), bcr!(48, 3), bcr!(50, 3)],
        ], [
            [bcr!(8,  0), bcr!(5,  5), bcr!(10, 5)],
            [bcr!(22, 0), bcr!(16, 0), bcr!(18, 0)],
            [bcr!(41, 1), bcr!(33, 0), bcr!(30, 0)],
        ], [
            [bcr!(4,  0), bcr!(0,  5), bcr!(2,  5)],
            [bcr!(15, 1), bcr!(8,  0), bcr!(5,  5)],
            [bcr!(31, 1), bcr!(22, 0), bcr!(16, 0)],
        ],
    ], [
        [
            [bcr!(2,  0), bcr!(6,  0), bcr!(14, 0)],
            [bcr!(10, 0), bcr!(11, 0), bcr!(17, 3)],
            [bcr!(24, 1), bcr!(23, 3), bcr!(25, 3)],
        ], [
            [bcr!(0,  0), bcr!(1,  5), bcr!(9,  5)],
            [bcr!(5,  0), bcr!(2,  0), bcr!(6,  0)],
            [bcr!(18, 1), bcr!(10, 0), bcr!(11, 0)],
        ], [
            [bcr!(4,  1), bcr!(3, 5), bcr!(7, 5)],
            [bcr!(8,  1), bcr!(0, 0), bcr!(1, 5)],
            [bcr!(16, 1), bcr!(5, 0), bcr!(2, 0)],
        ],
    ], [
        [
            [bcr!(7,  0), bcr!(21, 0), bcr!(38, 0)],
            [bcr!(9,  0), bcr!(19, 0), bcr!(34, 3)],
            [bcr!(14, 1), bcr!(20, 3), bcr!(36, 3)],
        ], [
            [bcr!(3, 0), bcr!(13, 5), bcr!(29, 5)],
            [bcr!(1, 0), bcr!(7,  0), bcr!(21, 0)],
            [bcr!(6, 1), bcr!(9,  0), bcr!(19, 0)],
        ], [
            [bcr!(4, 2), bcr!(12, 5), bcr!(26, 5)],
            [bcr!(0, 1), bcr!(3,  0), bcr!(13, 5)],
            [bcr!(2, 1), bcr!(1,  0), bcr!(7,  0)],
        ]
    ], [
        [
            [bcr!(26, 0), bcr!(42, 0), bcr!(58, 0)],
            [bcr!(29, 0), bcr!(43, 0), bcr!(62, 3)],
            [bcr!(38, 1), bcr!(47, 3), bcr!(64, 3)],
        ], [
            [bcr!(12, 0), bcr!(28, 5), bcr!(44, 5)],
            [bcr!(13, 0), bcr!(26, 0), bcr!(42, 0)],
            [bcr!(21, 1), bcr!(29, 0), bcr!(43, 0)],
        ], [
            [bcr!(4, 3), bcr!(15, 5), bcr!(31, 5)],
            [bcr!(3, 1), bcr!(12, 0), bcr!(28, 5)],
            [bcr!(7, 1), bcr!(13, 0), bcr!(26, 0)],
        ]
    ], [
        [
            [bcr!(31, 0), bcr!(41, 0), bcr!(49, 0)],
            [bcr!(44, 0), bcr!(53, 0), bcr!(61, 3)],
            [bcr!(58, 1), bcr!(65, 3), bcr!(75, 3)],
        ], [
            [bcr!(15, 0), bcr!(22, 5), bcr!(33, 5)],
            [bcr!(28, 0), bcr!(31, 0), bcr!(41, 0)],
            [bcr!(42, 1), bcr!(44, 0), bcr!(53, 0)],
        ], [
            [bcr!(4,  4), bcr!(8,  5), bcr!(16, 5)],
            [bcr!(12, 1), bcr!(15, 0), bcr!(22, 5)],
            [bcr!(26, 1), bcr!(28, 0), bcr!(31, 0)],
        ]
    ], [
        [
            [bcr!(50, 0), bcr!(48, 0), bcr!(49, 3)],
            [bcr!(32, 0), bcr!(30, 3), bcr!(33, 3)],
            [bcr!(24, 3), bcr!(18, 3), bcr!(16, 3)],
        ], [
            [bcr!(70, 0), bcr!(67, 0), bcr!(66, 3)],
            [bcr!(52, 3), bcr!(50, 0), bcr!(48, 0)],
            [bcr!(37, 3), bcr!(32, 0), bcr!(30, 3)],
        ], [
            [bcr!(83, 0), bcr!(87, 3), bcr!(85, 3)],
            [bcr!(74, 3), bcr!(70, 0), bcr!(67, 0)],
            [bcr!(57, 1), bcr!(52, 3), bcr!(50, 0)],
        ]
    ], [
        [
            [bcr!(25, 0), bcr!(23, 0), bcr!(24, 3)],
            [bcr!(17, 0), bcr!(11, 3), bcr!(10, 3)],
            [bcr!(14, 3), bcr!(6,  3), bcr!(2,  3)],
        ], [
            [bcr!(45, 0), bcr!(39, 0), bcr!(37, 3)],
            [bcr!(35, 3), bcr!(25, 0), bcr!(23, 0)],
            [bcr!(27, 3), bcr!(17, 0), bcr!(11, 3)],
        ], [
            [bcr!(63, 0), bcr!(59, 3), bcr!(57, 3)],
            [bcr!(56, 3), bcr!(45, 0), bcr!(39, 0)],
            [bcr!(46, 3), bcr!(35, 3), bcr!(25, 0)],
        ]
    ], [
        [
            [bcr!(36, 0), bcr!(20, 0), bcr!(14, 3)],
            [bcr!(34, 0), bcr!(19, 3), bcr!(9,  3)],
            [bcr!(38, 3), bcr!(21, 3), bcr!(7,  3)],
        ], [
            [bcr!(55, 0), bcr!(40, 0), bcr!(27, 3)],
            [bcr!(54, 3), bcr!(36, 0), bcr!(20, 0)],
            [bcr!(51, 3), bcr!(34, 0), bcr!(19, 3)],
        ], [
            [bcr!(72, 0), bcr!(60, 3), bcr!(46, 3)],
            [bcr!(73, 3), bcr!(55, 0), bcr!(40, 0)],
            [bcr!(71, 3), bcr!(54, 3), bcr!(36, 0)],
        ]
    ], [
        [
            [bcr!(64, 0), bcr!(47, 0), bcr!(38, 3)],
            [bcr!(62, 0), bcr!(43, 3), bcr!(29, 3)],
            [bcr!(58, 3), bcr!(42, 3), bcr!(26, 3)],
        ], [
            [bcr!(84, 0), bcr!(69, 0), bcr!(51, 3)],
            [bcr!(82, 3), bcr!(64, 0), bcr!(47, 0)],
            [bcr!(76, 3), bcr!(62, 0), bcr!(43, 3)],
        ], [
            [bcr!(97, 0), bcr!(89, 3), bcr!(71, 3)],
            [bcr!(98, 3), bcr!(84, 0), bcr!(69, 0)],
            [bcr!(96, 3), bcr!(82, 3), bcr!(64, 0)],
        ]
    ], [
        [
            [bcr!(75, 0), bcr!(65, 0), bcr!(58, 3)],
            [bcr!(61, 0), bcr!(53, 3), bcr!(44, 3)],
            [bcr!(49, 3), bcr!(41, 3), bcr!(31, 3)],
        ], [
            [bcr!(94, 0), bcr!(86, 0), bcr!(76, 3)],
            [bcr!(81, 3), bcr!(75, 0), bcr!(65, 0)],
            [bcr!(66, 3), bcr!(61, 0), bcr!(53, 3)],
        ], [
            [bcr!(107, 0), bcr!(104, 3), bcr!(96, 3)],
            [bcr!(101, 3), bcr!(94,  0), bcr!(86, 0)],
            [bcr!(85,  3), bcr!(81,  3), bcr!(75, 0)],
        ]
    ], [
        [
            [bcr!(57, 0), bcr!(59, 0), bcr!(63, 3)],
            [bcr!(74, 0), bcr!(78, 3), bcr!(79, 3)],
            [bcr!(83, 3), bcr!(92, 3), bcr!(95, 3)],
        ], [
            [bcr!(37, 0), bcr!(39, 3), bcr!(45, 3)],
            [bcr!(52, 0), bcr!(57, 0), bcr!(59, 0)],
            [bcr!(70, 3), bcr!(74, 0), bcr!(78, 3)],
        ], [
            [bcr!(24, 0), bcr!(23, 3), bcr!(25, 3)],
            [bcr!(32, 3), bcr!(37, 0), bcr!(39, 3)],
            [bcr!(50, 3), bcr!(52, 0), bcr!(57, 0)],
        ]
    ], [
        [
            [bcr!(46, 0), bcr!(60, 0), bcr!(72, 3)],
            [bcr!(56, 0), bcr!(68, 3), bcr!(80, 3)],
            [bcr!(63, 3), bcr!(77, 3), bcr!(90, 3)],
        ], [
            [bcr!(27, 0), bcr!(40, 3), bcr!(55, 3)],
            [bcr!(35, 0), bcr!(46, 0), bcr!(60, 0)],
            [bcr!(45, 3), bcr!(56, 0), bcr!(68, 3)],
        ], [
            [bcr!(14, 0), bcr!(20, 3), bcr!(36, 3)],
            [bcr!(17, 3), bcr!(27, 0), bcr!(40, 3)],
            [bcr!(25, 3), bcr!(35, 0), bcr!(46, 0)],
        ]
    ], [
        [
            [bcr!(71, 0), bcr!(89, 0), bcr!(97,  3)],
            [bcr!(73, 0), bcr!(91, 3), bcr!(103, 3)],
            [bcr!(72, 3), bcr!(88, 3), bcr!(105, 3)],
        ], [
            [bcr!(51, 0), bcr!(69, 3), bcr!(84, 3)],
            [bcr!(54, 0), bcr!(71, 0), bcr!(89, 0)],
            [bcr!(55, 3), bcr!(73, 0), bcr!(91, 3)],
        ], [
            [bcr!(38, 0), bcr!(47, 3), bcr!(64, 3)],
            [bcr!(34, 3), bcr!(51, 0), bcr!(69, 3)],
            [bcr!(36, 3), bcr!(54, 0), bcr!(71, 0)],
        ]
    ], [
        [
            [bcr!(96, 0), bcr!(104, 0), bcr!(107, 3)],
            [bcr!(98, 0), bcr!(110, 3), bcr!(115, 3)],
            [bcr!(97, 3), bcr!(111, 3), bcr!(119, 3)],
        ], [
            [bcr!(76, 0), bcr!(86, 3), bcr!(94,  3)],
            [bcr!(82, 0), bcr!(96, 0), bcr!(104, 0)],
            [bcr!(84, 3), bcr!(98, 0), bcr!(110, 3)],
        ], [
            [bcr!(58, 0), bcr!(65, 3), bcr!(75, 3)],
            [bcr!(62, 3), bcr!(76, 0), bcr!(86, 3)],
            [bcr!(64, 3), bcr!(82, 0), bcr!(96, 0)],
        ]
    ], [
        [
            [bcr!(85,  0), bcr!(87,  0), bcr!(83,  3)],
            [bcr!(101, 0), bcr!(102, 3), bcr!(100, 3)],
            [bcr!(107, 3), bcr!(112, 3), bcr!(114, 3)],
        ], [
            [bcr!(66, 0), bcr!(67,  3), bcr!(70,  3)],
            [bcr!(81, 0), bcr!(85,  0), bcr!(87,  0)],
            [bcr!(94, 3), bcr!(101, 0), bcr!(102, 3)],
        ], [
            [bcr!(49, 0), bcr!(48, 3), bcr!(50, 3)],
            [bcr!(61, 3), bcr!(66, 0), bcr!(67, 3)],
            [bcr!(75, 3), bcr!(81, 0), bcr!(85, 0)],
        ]
    ], [
        [
            [bcr!(95, 0), bcr!(92, 0), bcr!(83, 0)],
            [bcr!(79, 0), bcr!(78, 0), bcr!(74, 3)],
            [bcr!(63, 1), bcr!(59, 3), bcr!(57, 3)],
        ], [
            [bcr!(109, 0), bcr!(108, 0), bcr!(100, 5)],
            [bcr!(93,  1), bcr!(95,  0), bcr!(92,  0)],
            [bcr!(77,  1), bcr!(79,  0), bcr!(78,  0)],
        ], [
            [bcr!(117, 4), bcr!(118, 5), bcr!(114, 5)],
            [bcr!(106, 1), bcr!(109, 0), bcr!(108, 0)],
            [bcr!(90,  1), bcr!(93,  1), bcr!(95,  0)],
        ]
    ], [
        [
            [bcr!(90, 0), bcr!(77, 0), bcr!(63, 0)],
            [bcr!(80, 0), bcr!(68, 0), bcr!(56, 3)],
            [bcr!(72, 1), bcr!(60, 3), bcr!(46, 3)],
        ], [
            [bcr!(106, 0), bcr!(93, 0), bcr!(79, 5)],
            [bcr!(99,  1), bcr!(90, 0), bcr!(77, 0)],
            [bcr!(88,  1), bcr!(80, 0), bcr!(68, 0)],
        ], [
            [bcr!(117, 3), bcr!(109, 5), bcr!(95, 5)],
            [bcr!(113, 1), bcr!(106, 0), bcr!(93, 0)],
            [bcr!(105, 1), bcr!(99,  1), bcr!(90, 0)],
        ]
    ], [
        [
            [bcr!(105, 0), bcr!(88, 0), bcr!(72, 0)],
            [bcr!(103, 0), bcr!(91, 0), bcr!(73, 3)],
            [bcr!(97,  1), bcr!(89, 3), bcr!(71, 3)],
        ], [
            [bcr!(113, 0), bcr!(99,  0), bcr!(80, 5)],
            [bcr!(116, 1), bcr!(105, 0), bcr!(88, 0)],
            [bcr!(111, 1), bcr!(103, 0), bcr!(91, 0)],
        ], [
            [bcr!(117, 2), bcr!(106, 5), bcr!(90, 5)],
            [bcr!(121, 1), bcr!(113, 0), bcr!(99, 0)],
            [bcr!(119, 1), bcr!(116, 1), bcr!(105, 0)],
        ]
    ], [
        [
            [bcr!(119, 0), bcr!(111, 0), bcr!(97, 0)],
            [bcr!(115, 0), bcr!(110, 0), bcr!(98, 3)],
            [bcr!(107, 1), bcr!(104, 3), bcr!(96, 3)],
        ], [
            [bcr!(121, 0), bcr!(116, 0), bcr!(103, 5)],
            [bcr!(120, 1), bcr!(119, 0), bcr!(111, 0)],
            [bcr!(112, 1), bcr!(115, 0), bcr!(110, 0)],
        ], [
            [bcr!(117, 1), bcr!(113, 5), bcr!(105, 5)],
            [bcr!(118, 1), bcr!(121, 0), bcr!(116, 0)],
            [bcr!(114, 1), bcr!(120, 1), bcr!(119, 0)],
        ]
    ], [
        [
            [bcr!(114, 0), bcr!(112, 0), bcr!(107, 0)],
            [bcr!(100, 0), bcr!(102, 0), bcr!(101, 3)],
            [bcr!(83,  1), bcr!(87,  3), bcr!(85,  3)],
        ], [
            [bcr!(118, 0), bcr!(120, 0), bcr!(115, 5)],
            [bcr!(108, 1), bcr!(114, 0), bcr!(112, 0)],
            [bcr!(92,  1), bcr!(100, 0), bcr!(102, 0)],
        ], [
            [bcr!(117, 0), bcr!(121, 5), bcr!(119, 5)],
            [bcr!(109, 1), bcr!(118, 0), bcr!(120, 0)],
            [bcr!(95,  1), bcr!(108, 1), bcr!(114, 0)],
        ]
    ]
];
