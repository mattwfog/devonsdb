use devondb_geo::{
    GeoError,
    index::{CellIndex, PENTAGON_BASE_CELLS},
};
use h3o::{CellIndex as OracleCellIndex, Resolution};

const SWEEP_SEED: u64 = 0xd3e0_6db0_0100_c311;

#[test]
fn layout_matches_oracle_across_every_resolution_and_base_cell() {
    let cells = oracle_sweep();
    assert_eq!(cells.len(), 122 * 16);

    for oracle in cells {
        let raw = u64::from(oracle);
        let cell = CellIndex::try_from(raw).unwrap();
        assert_eq!(cell.raw(), raw);
        assert_eq!(cell.resolution(), u8::from(oracle.resolution()));
        assert_eq!(cell.base_cell(), u8::from(oracle.base_cell()));
        assert_eq!(cell.is_atom(), oracle.resolution() == Resolution::Fifteen);

        for position in 1..=15 {
            let resolution = Resolution::try_from(position).unwrap();
            let oracle_digit = oracle.direction_at(resolution).map_or(7, u8::from);
            assert_eq!(cell.digit(position), oracle_digit);
        }
    }
}

#[test]
fn hierarchy_matches_oracle_across_seeded_sweep() {
    for oracle in oracle_sweep() {
        let cell = CellIndex::try_from(u64::from(oracle)).unwrap();
        let resolution = u8::from(oracle.resolution());
        let oracle_parent = resolution.checked_sub(1).map(|parent_resolution| {
            let parent_resolution = Resolution::try_from(parent_resolution).unwrap();
            u64::from(oracle.parent(parent_resolution).unwrap())
        });
        assert_eq!(cell.parent().map(CellIndex::raw), oracle_parent);

        for target in 0..=resolution {
            let oracle_resolution = Resolution::try_from(target).unwrap();
            let expected = u64::from(oracle.parent(oracle_resolution).unwrap());
            assert_eq!(cell.truncate_to(target).unwrap().raw(), expected);
        }
    }

    let resolution_five = oracle_sweep()
        .into_iter()
        .find(|cell| cell.resolution() == Resolution::Five)
        .unwrap();
    let cell = CellIndex::try_from(u64::from(resolution_five)).unwrap();
    assert_eq!(
        cell.truncate_to(6),
        Err(GeoError::ResolutionAboveCell {
            resolution: 6,
            cell_resolution: 5,
        })
    );
    assert_eq!(
        cell.truncate_to(16),
        Err(GeoError::InvalidResolution { resolution: 16 })
    );
}

#[test]
fn atom_ranges_match_oracle_child_extremes() {
    let sweep = oracle_sweep();
    let mut samples = Vec::new();
    for (base_cell, resolution) in [(0, 10), (1, 11), (24, 12), (47, 13), (72, 14), (121, 15)] {
        samples.push(
            *sweep
                .iter()
                .find(|cell| {
                    u8::from(cell.base_cell()) == base_cell
                        && u8::from(cell.resolution()) == resolution
                })
                .unwrap(),
        );
    }

    let pentagon = OracleCellIndex::base_cells()
        .find(|cell| u8::from(cell.base_cell()) == PENTAGON_BASE_CELLS[0])
        .unwrap();
    let pentagon = center_descendant(pentagon, 10);
    assert!(pentagon.is_pentagon());
    samples.push(pentagon);

    for oracle in samples {
        let cell = CellIndex::try_from(u64::from(oracle)).unwrap();
        let children = oracle
            .children(Resolution::Fifteen)
            .map(u64::from)
            .collect::<Vec<_>>();
        let minimum = *children.iter().min().unwrap();
        let maximum = *children.iter().max().unwrap();
        let range = cell.atom_range();
        assert_eq!(range, (minimum, maximum));
        assert!(
            children
                .iter()
                .all(|child| range.0 <= *child && *child <= range.1)
        );
    }
}

#[test]
fn pentagon_base_cells_are_frozen_from_oracle() {
    let derived = OracleCellIndex::base_cells()
        .filter(|cell| cell.is_pentagon())
        .map(|cell| u8::from(cell.base_cell()))
        .collect::<Vec<_>>();
    assert_eq!(derived.as_slice(), PENTAGON_BASE_CELLS);
}

#[test]
fn invalid_raw_indexes_report_the_first_broken_rule() {
    let valid = u64::from(OracleCellIndex::try_from(0x08a1_fb46_622d_ffff).unwrap());

    assert_rejected(valid | (1 << 63), "reserved bit must be zero");
    assert_rejected(replace_field(valid, 59, 4, 2), "mode must be 1 (cell)");
    assert_rejected(
        replace_field(valid, 56, 3, 1),
        "mode-dependent bits must be zero in cell mode",
    );
    assert_rejected(
        replace_field(valid, 45, 7, 122),
        "base cell must be 0..=121",
    );
    assert_rejected(
        replace_digit(valid, 5, 7),
        "digit 5 must be 0..=6 at resolution 10",
    );
    assert_rejected(
        replace_digit(valid, 11, 6),
        "digit 11 must be 7 beyond resolution 10",
    );
    assert_rejected(
        replace_field(valid, 52, 4, 9),
        "digit 10 must be 7 beyond resolution 9",
    );
    assert_rejected(
        replace_field(valid, 52, 4, 11),
        "digit 11 must be 0..=6 at resolution 11",
    );

    let pentagon = OracleCellIndex::base_cells()
        .find(|cell| u8::from(cell.base_cell()) == PENTAGON_BASE_CELLS[0])
        .unwrap();
    let center = center_descendant(pentagon, 2);
    assert_rejected(
        replace_digit(u64::from(center), 2, 1),
        "pentagon cell's leading non-center digit must not be 1",
    );
}

#[test]
fn ordering_is_raw_u64_ordering() {
    let mut cells = oracle_sweep()
        .into_iter()
        .map(|cell| CellIndex::try_from(u64::from(cell)).unwrap())
        .collect::<Vec<_>>();
    let mut raw = cells.iter().map(|cell| cell.raw()).collect::<Vec<_>>();
    cells.sort();
    raw.sort();
    assert_eq!(cells.iter().map(|cell| cell.raw()).collect::<Vec<_>>(), raw);
}

fn oracle_sweep() -> Vec<OracleCellIndex> {
    let mut generator = SplitMix64::new(SWEEP_SEED);
    let mut cells = Vec::with_capacity(122 * 16);
    for base_cell in OracleCellIndex::base_cells() {
        let mut cell = base_cell;
        cells.push(cell);
        for resolution in 1..=15 {
            let resolution = Resolution::try_from(resolution).unwrap();
            let children = cell.children(resolution).collect::<Vec<_>>();
            let index = (generator.next() % children.len() as u64) as usize;
            cell = children[index];
            cells.push(cell);
        }
    }
    cells
}

fn center_descendant(mut cell: OracleCellIndex, target_resolution: u8) -> OracleCellIndex {
    for resolution in 1..=target_resolution {
        let resolution = Resolution::try_from(resolution).unwrap();
        cell = cell
            .children(resolution)
            .find(|child| child.is_pentagon())
            .unwrap();
    }
    cell
}

fn assert_rejected(raw: u64, expected_reason: &str) {
    assert!(OracleCellIndex::try_from(raw).is_err());
    assert_eq!(
        CellIndex::try_from(raw),
        Err(GeoError::InvalidCellIndex {
            index: raw,
            reason: expected_reason.to_owned(),
        })
    );
}

fn replace_digit(raw: u64, position: u8, digit: u8) -> u64 {
    replace_field(raw, u32::from(3 * (15 - position)), 3, u64::from(digit))
}

fn replace_field(raw: u64, shift: u32, width: u32, value: u64) -> u64 {
    let mask = ((1_u64 << width) - 1) << shift;
    (raw & !mask) | (value << shift)
}

struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    const fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    fn next(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut value = self.state;
        value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        value ^ (value >> 31)
    }
}
