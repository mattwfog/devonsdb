//! Exact `WithinScan` covering consumer (`docs/GEO.md` §7).

use devondb_geo::{
    covering::{DiscCovering, MAX_CENTER_TO_BOUNDARY_M, cover_disc, great_circle_meters},
    grid,
};
use devondb_types::{DevonError, DevonResult, GeoPoint, logical_type::LogicalType, value::Value};

use crate::{
    chunk::{Chunk, ChunkBuilder},
    source::ChunkSource,
};

/// A streaming source that emits rows inside an inclusive great-circle disc.
///
/// The disc covering is compiled once at construction. Full atom ranges emit
/// without a distance calculation; boundary cells use the pinned deterministic
/// great-circle kernel as the exact membership check.
pub struct WithinScan {
    source: Box<dyn ChunkSource>,
    point_column: usize,
    center: GeoPoint,
    meters: f64,
    resolution: u8,
    covering: DiscCovering,
}

impl WithinScan {
    /// Compiles the operator's disc covering into inclusive atom-key ranges.
    ///
    /// The returned ranges include both fully contained cells and boundary
    /// cells. They are plain `u64` pairs so storage-facing callers can use
    /// the operator's exact covering for conservative group pruning without
    /// depending on geo types.
    ///
    /// # Errors
    ///
    /// Returns an invalid-argument error when the center or radius cannot be
    /// compiled into a disc covering.
    pub fn covering_ranges(center: GeoPoint, meters: f64) -> DevonResult<Vec<(u64, u64)>> {
        let (_, covering) = compile_covering(center, meters)?;
        Ok(possibly_matching_ranges(&covering))
    }

    /// Creates a covering consumer over a table scan source.
    ///
    /// `point_column` is the declaration-order GeoPoint column index. The
    /// output retains the input schema and row order without adding a column.
    ///
    /// # Errors
    ///
    /// Returns an invalid-argument error when the center or radius cannot be
    /// compiled into a disc covering.
    pub fn new(
        source: Box<dyn ChunkSource>,
        point_column: usize,
        center: GeoPoint,
        meters: f64,
    ) -> DevonResult<Self> {
        let (resolution, covering) = compile_covering(center, meters)?;
        Ok(Self {
            source,
            point_column,
            center,
            meters,
            resolution,
            covering,
        })
    }

    fn filter_chunk(&self, chunk: &Chunk) -> DevonResult<Chunk> {
        self.validate_chunk(chunk)?;
        let mut output = ChunkBuilder::new(chunk.types().to_vec());
        for row in 0..chunk.row_count() {
            if self.row_matches(chunk, row)? {
                output.push_row(clone_row(chunk, row)?)?;
            }
        }
        Ok(output.finish())
    }

    fn validate_chunk(&self, chunk: &Chunk) -> DevonResult<()> {
        let Some(column_type) = chunk.types().get(self.point_column) else {
            return Err(invalid_argument(format!(
                "WithinScan GeoPoint column {} is out of range for {} source columns",
                self.point_column,
                chunk.column_count()
            )));
        };
        if *column_type != LogicalType::GeoPoint {
            return Err(invalid_argument(format!(
                "WithinScan column {} has type {column_type}; expected GeoPoint",
                self.point_column
            )));
        }
        Ok(())
    }

    fn row_matches(&self, chunk: &Chunk, row: usize) -> DevonResult<bool> {
        let value = chunk.value(row, self.point_column).ok_or_else(|| {
            invalid_argument(format!(
                "WithinScan row {row} is missing GeoPoint column {}",
                self.point_column
            ))
        })?;
        let point = match value {
            Value::Null => return Ok(false),
            Value::GeoPoint(point) => point,
            other => {
                return Err(invalid_argument(format!(
                    "WithinScan row {row} column {} has non-GeoPoint value {other}",
                    self.point_column
                )));
            }
        };

        let atom = grid::atom(point.lat_deg(), point.lng_deg()).map_err(|error| {
            invalid_argument(format!(
                "WithinScan cannot assign row {row} to an atom: {error}"
            ))
        })?;
        if range_contains(&self.covering.full, atom.raw()) {
            return Ok(true);
        }
        let cell = atom.truncate_to(self.resolution).map_err(|error| {
            invalid_argument(format!(
                "WithinScan cannot truncate row {row} atom to resolution {}: {error}",
                self.resolution
            ))
        })?;
        if self.covering.boundary.binary_search(&cell).is_err() {
            return Ok(false);
        }
        Ok(great_circle_meters(
            point.lat_deg(),
            point.lng_deg(),
            self.center.lat_deg(),
            self.center.lng_deg(),
        ) <= self.meters)
    }
}

fn compile_covering(center: GeoPoint, meters: f64) -> DevonResult<(u8, DiscCovering)> {
    let resolution = covering_resolution(meters);
    let covering = cover_disc(center.lat_deg(), center.lng_deg(), meters, resolution)
        .map_err(|error| invalid_argument(format!("WithinScan cannot cover disc: {error}")))?;
    Ok((resolution, covering))
}

fn possibly_matching_ranges(covering: &DiscCovering) -> Vec<(u64, u64)> {
    let mut ranges = covering.full.clone();
    ranges.extend(covering.boundary.iter().map(|cell| cell.atom_range()));
    merge_ranges(&mut ranges);
    ranges
}

fn merge_ranges(ranges: &mut Vec<(u64, u64)>) {
    ranges.sort_unstable();
    let mut write = 0;
    for read in 0..ranges.len() {
        if write > 0 && ranges[read].0 <= ranges[write - 1].1.saturating_add(1) {
            ranges[write - 1].1 = ranges[write - 1].1.max(ranges[read].1);
        } else {
            ranges[write] = ranges[read];
            write += 1;
        }
    }
    ranges.truncate(write);
}

impl ChunkSource for WithinScan {
    fn next_chunk(&mut self) -> DevonResult<Option<Chunk>> {
        loop {
            let Some(chunk) = self.source.next_chunk()? else {
                return Ok(None);
            };
            let output = self.filter_chunk(&chunk)?;
            if output.row_count() != 0 {
                return Ok(Some(output));
            }
        }
    }
}

/// Chooses the coarsest covering resolution whose frozen maximum
/// center-to-boundary scale is no larger than `meters`; radii below the
/// finest scale use resolution 15. The rule depends on meters alone and its
/// exact thresholds are `MAX_CENTER_TO_BOUNDARY_M`, so it is deterministic.
/// Resolution affects covering work only, never membership semantics.
fn covering_resolution(meters: f64) -> u8 {
    MAX_CENTER_TO_BOUNDARY_M
        .iter()
        .position(|bound| *bound <= meters)
        .map_or(15, |resolution| resolution as u8)
}

fn range_contains(ranges: &[(u64, u64)], atom: u64) -> bool {
    let insertion = ranges.partition_point(|(lower, _)| *lower <= atom);
    insertion != 0 && atom <= ranges[insertion - 1].1
}

fn clone_row(chunk: &Chunk, row: usize) -> DevonResult<Vec<Value>> {
    (0..chunk.column_count())
        .map(|column| {
            chunk.value(row, column).ok_or_else(|| {
                invalid_argument(format!(
                    "WithinScan cannot copy missing row {row}, column {column}"
                ))
            })
        })
        .collect()
}

fn invalid_argument(context: impl Into<String>) -> DevonError {
    DevonError::InvalidArgument {
        context: context.into(),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use devondb_geo::covering::{MAX_CENTER_TO_BOUNDARY_M, great_circle_meters};
    use devondb_types::{
        DevonError, DevonResult, GeoPoint, logical_type::LogicalType, value::Value,
    };

    use super::{
        WithinScan, compile_covering, covering_resolution, possibly_matching_ranges, range_contains,
    };
    use crate::{
        chunk::{Chunk, ChunkBuilder},
        source::ChunkSource,
    };

    struct VecSource {
        chunks: VecDeque<Chunk>,
    }

    impl VecSource {
        fn new(chunks: Vec<Chunk>) -> Self {
            Self {
                chunks: chunks.into(),
            }
        }
    }

    impl ChunkSource for VecSource {
        fn next_chunk(&mut self) -> DevonResult<Option<Chunk>> {
            Ok(self.chunks.pop_front())
        }
    }

    #[test]
    fn within_preserves_input_order_and_skips_nulls_and_outside_rows() {
        let center = geo(0.0, 0.0);
        let rows = [
            row(1, Some(center)),
            row(2, None),
            row(3, Some(geo(0.0, 0.005))),
            row(4, Some(geo(0.0, 1.0))),
            row(5, Some(geo(0.002, 0.0))),
        ];
        let chunks = vec![fixture_chunk(&rows[..2]), fixture_chunk(&rows[2..])];
        let output =
            collect(WithinScan::new(Box::new(VecSource::new(chunks)), 2, center, 1_000.0).unwrap());

        assert_eq!(ids(&output), vec![1, 3, 5]);
        assert_eq!(output[0].types(), fixture_types());
    }

    #[test]
    fn accelerated_membership_matches_naive_distance_at_scale_transitions() {
        let center = geo(12.345, 67.89);
        for meters in MAX_CENTER_TO_BOUNDARY_M {
            let latitude_delta = meters / 6_371_007.180_918_475 * 180.0 / core::f64::consts::PI;
            let points = [
                center,
                geo(center.lat_deg() - latitude_delta * 0.999, center.lng_deg()),
                geo(center.lat_deg() - latitude_delta * 1.001, center.lng_deg()),
            ];
            let rows = points
                .into_iter()
                .enumerate()
                .map(|(index, point)| row(index as i64, Some(point)))
                .collect::<Vec<_>>();
            let expected = rows
                .iter()
                .filter_map(|row| match &row[2] {
                    Value::GeoPoint(point)
                        if great_circle_meters(
                            point.lat_deg(),
                            point.lng_deg(),
                            center.lat_deg(),
                            center.lng_deg(),
                        ) <= meters =>
                    {
                        Some(row[0].clone())
                    }
                    _ => None,
                })
                .collect::<Vec<_>>();
            let source = VecSource::new(vec![fixture_chunk(&rows)]);
            let actual = collect(WithinScan::new(Box::new(source), 2, center, meters).unwrap());
            let actual = ids(&actual)
                .into_iter()
                .map(Value::Int64)
                .collect::<Vec<_>>();
            assert_eq!(actual, expected, "radius {meters}");
        }
    }

    #[test]
    fn resolution_rule_is_pinned_to_the_frozen_scale_table() {
        for (resolution, meters) in MAX_CENTER_TO_BOUNDARY_M.into_iter().enumerate() {
            assert_eq!(covering_resolution(meters), resolution as u8);
            if resolution != 0 {
                assert_eq!(covering_resolution(meters.next_up()), resolution as u8);
            }
        }
        assert_eq!(covering_resolution(0.1), 15);
    }

    #[test]
    fn inclusive_range_search_handles_edges_and_gaps() {
        let ranges = [(10, 20), (30, 40)];
        for atom in [10, 11, 20, 30, 40] {
            assert!(range_contains(&ranges, atom));
        }
        for atom in [0, 9, 21, 29, 41, u64::MAX] {
            assert!(!range_contains(&ranges, atom));
        }
    }

    #[test]
    fn pruning_ranges_include_full_and_boundary_atom_intervals() {
        let center = geo(45.5152, -122.6784);
        let (_, full_covering) = compile_covering(center, 25_000_000.0).unwrap();
        assert!(!full_covering.full.is_empty());
        let full_ranges = possibly_matching_ranges(&full_covering);
        for (min, max) in full_covering.full {
            assert!(range_contains(&full_ranges, min));
            assert!(range_contains(&full_ranges, max));
        }

        let (_, boundary_covering) = compile_covering(center, 1_000.0).unwrap();
        assert!(!boundary_covering.boundary.is_empty());
        let boundary_ranges = possibly_matching_ranges(&boundary_covering);
        for cell in boundary_covering.boundary {
            let (min, max) = cell.atom_range();
            assert!(range_contains(&boundary_ranges, min));
            assert!(range_contains(&boundary_ranges, max));
        }
    }

    #[test]
    fn wrong_source_column_type_is_rejected() {
        let mut builder = ChunkBuilder::new(vec![LogicalType::Int64]);
        builder.push_row(vec![Value::Int64(1)]).unwrap();
        let source = VecSource::new(vec![builder.finish()]);
        let mut within = WithinScan::new(Box::new(source), 0, geo(0.0, 0.0), 1.0).unwrap();

        let DevonError::InvalidArgument { context } = within.next_chunk().unwrap_err() else {
            panic!("expected InvalidArgument");
        };
        assert!(context.contains("expected GeoPoint"));
    }

    fn geo(lat_deg: f64, lng_deg: f64) -> GeoPoint {
        GeoPoint::from_canonical(lat_deg, lng_deg).unwrap()
    }

    fn row(id: i64, point: Option<GeoPoint>) -> Vec<Value> {
        vec![
            Value::Int64(id),
            Value::String(format!("row-{id}")),
            point.map_or(Value::Null, Value::GeoPoint),
        ]
    }

    fn fixture_types() -> Vec<LogicalType> {
        vec![
            LogicalType::Int64,
            LogicalType::String,
            LogicalType::GeoPoint,
        ]
    }

    fn fixture_chunk(rows: &[Vec<Value>]) -> Chunk {
        let mut builder = ChunkBuilder::new(fixture_types());
        for row in rows {
            builder.push_row(row.clone()).unwrap();
        }
        builder.finish()
    }

    fn collect(mut source: WithinScan) -> Vec<Chunk> {
        let mut chunks = Vec::new();
        while let Some(chunk) = source.next_chunk().unwrap() {
            chunks.push(chunk);
        }
        chunks
    }

    fn ids(chunks: &[Chunk]) -> Vec<i64> {
        chunks
            .iter()
            .flat_map(Chunk::rows)
            .map(|row| match &row[0] {
                Value::Int64(value) => *value,
                other => panic!("expected Int64 id, got {other}"),
            })
            .collect()
    }
}
