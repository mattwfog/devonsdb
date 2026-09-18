//! The `Expand` pull operator: follow a rel table from bound input nodes
//! (`docs/PLAN_IR.md` § Query operators, binding).

use devondb_plan::ops::Direction;
use devondb_types::{DevonError, DevonResult, logical_type::LogicalType, value::Value};

use crate::{
    chunk::{Chunk, ChunkBuilder},
    source::{ChunkSource, NeighborSource},
};

/// A pull operator that follows a relationship from each bound input node.
pub struct Expand {
    upstream: Box<dyn ChunkSource>,
    neighbors: Box<dyn NeighborSource>,
    rel: String,
    direction: Direction,
    from_offset_column: usize,
    neighbor_types: Vec<LogicalType>,
    input: Option<Chunk>,
    next_input_row: usize,
    pending: Option<PendingRow>,
}

struct PendingRow {
    input_values: Vec<Value>,
    neighbor_offsets: Vec<u64>,
    next_neighbor: usize,
}

impl Expand {
    /// Creates an expand over `upstream` using node offsets from the selected column.
    #[must_use]
    pub fn new(
        upstream: Box<dyn ChunkSource>,
        neighbors: Box<dyn NeighborSource>,
        rel: String,
        direction: Direction,
        from_offset_column: usize,
        neighbor_types: Vec<LogicalType>,
    ) -> Self {
        Self {
            upstream,
            neighbors,
            rel,
            direction,
            from_offset_column,
            neighbor_types,
            input: None,
            next_input_row: 0,
            pending: None,
        }
    }

    fn pull_input(&mut self) -> DevonResult<bool> {
        let Some(chunk) = self.upstream.next_chunk()? else {
            return Ok(false);
        };
        self.input = Some(chunk);
        self.next_input_row = 0;
        Ok(true)
    }

    fn input_exhausted(&self) -> bool {
        self.input
            .as_ref()
            .is_none_or(|chunk| self.next_input_row == chunk.row_count())
    }

    fn prepare_row(&mut self) -> DevonResult<()> {
        let chunk = self
            .input
            .as_ref()
            .ok_or_else(|| corrupt("expand has no current input chunk"))?;
        let input_values = clone_row(chunk, self.next_input_row)?;
        let from = offset_from(&input_values, self.from_offset_column, self.next_input_row)?;
        let neighbor_offsets = self.neighbors.neighbors(&self.rel, self.direction, from)?;

        if neighbor_offsets.is_empty() {
            self.next_input_row += 1;
        } else {
            self.pending = Some(PendingRow {
                input_values,
                neighbor_offsets,
                next_neighbor: 0,
            });
        }
        Ok(())
    }

    fn emit_neighbor(&mut self, builder: &mut ChunkBuilder) -> DevonResult<()> {
        let neighbor_offset = self
            .pending
            .as_ref()
            .and_then(|pending| pending.neighbor_offsets.get(pending.next_neighbor))
            .copied()
            .ok_or_else(|| corrupt("expand has no pending neighbor"))?;
        let encoded_offset = i64::try_from(neighbor_offset).map_err(|_| {
            corrupt(format!(
                "neighbor offset {neighbor_offset} cannot be represented as Int64"
            ))
        })?;
        let properties = self
            .neighbors
            .node_row(&self.rel, self.direction, neighbor_offset)?;
        validate_neighbor_row(neighbor_offset, &properties, &self.neighbor_types)?;

        let mut output = self
            .pending
            .as_ref()
            .map(|pending| pending.input_values.clone())
            .ok_or_else(|| corrupt("expand lost its pending input row"))?;
        output.push(Value::Int64(encoded_offset));
        output.extend(properties);
        builder
            .push_row(output)
            .map_err(|error| corrupt(format!("invalid expand output row: {error}")))?;
        self.advance_neighbor();
        Ok(())
    }

    fn advance_neighbor(&mut self) {
        let finished = self.pending.as_mut().is_some_and(|pending| {
            pending.next_neighbor += 1;
            pending.next_neighbor == pending.neighbor_offsets.len()
        });
        if finished {
            self.pending = None;
            self.next_input_row += 1;
        }
    }

    fn output_types(&self) -> DevonResult<Vec<LogicalType>> {
        let chunk = self
            .input
            .as_ref()
            .ok_or_else(|| corrupt("cannot infer expand output without an input chunk"))?;
        let mut types = chunk.types().to_vec();
        types.push(LogicalType::Int64);
        types.extend_from_slice(&self.neighbor_types);
        Ok(types)
    }
}

impl ChunkSource for Expand {
    fn next_chunk(&mut self) -> DevonResult<Option<Chunk>> {
        loop {
            if self.input.is_none() && !self.pull_input()? {
                return Ok(None);
            }

            let mut builder = ChunkBuilder::new(self.output_types()?);
            let mut emitted = 0;
            while !builder.is_full() {
                if self.pending.is_some() {
                    self.emit_neighbor(&mut builder)?;
                    emitted += 1;
                } else if self.input_exhausted() {
                    self.input = None;
                    break;
                } else {
                    self.prepare_row()?;
                }
            }

            if emitted > 0 {
                return Ok(Some(builder.finish()));
            }
        }
    }
}

fn clone_row(chunk: &Chunk, row: usize) -> DevonResult<Vec<Value>> {
    (0..chunk.column_count())
        .map(|column| {
            chunk.value(row, column).ok_or_else(|| {
                corrupt(format!("missing input value at row {row}, column {column}"))
            })
        })
        .collect()
}

fn offset_from(values: &[Value], column: usize, row: usize) -> DevonResult<u64> {
    match values.get(column) {
        Some(Value::Int64(offset)) if *offset >= 0 => Ok(*offset as u64),
        Some(value) => Err(corrupt(format!(
            "expand offset at row {row}, column {column} must be a non-negative Int64, got {value}"
        ))),
        None => Err(corrupt(format!(
            "expand offset column {column} is missing from input row {row}"
        ))),
    }
}

fn validate_neighbor_row(offset: u64, values: &[Value], types: &[LogicalType]) -> DevonResult<()> {
    if values.len() != types.len() {
        return Err(corrupt(format!(
            "neighbor node {offset} has {} properties, expected {}",
            values.len(),
            types.len()
        )));
    }
    for (column, (value, logical_type)) in values.iter().zip(types).enumerate() {
        if !value.matches_type(logical_type) {
            return Err(corrupt(format!(
                "neighbor node {offset} property {column} value {value} does not match expected type {logical_type}"
            )));
        }
    }
    Ok(())
}

fn corrupt(context: impl Into<String>) -> DevonError {
    DevonError::Corrupt {
        context: context.into(),
    }
}

#[cfg(test)]
mod tests {
    use std::{
        cell::{Cell, RefCell},
        collections::{HashMap, VecDeque},
        rc::Rc,
    };

    use devondb_plan::ops::Direction;
    use devondb_types::{DevonError, DevonResult, logical_type::LogicalType, value::Value};

    use super::Expand;
    use crate::{
        chunk::{CHUNK_CAPACITY, Chunk, ChunkBuilder},
        source::{ChunkSource, NeighborSource},
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

    struct CountingSource {
        chunks: VecDeque<Chunk>,
        pulls: Rc<Cell<usize>>,
    }

    impl ChunkSource for CountingSource {
        fn next_chunk(&mut self) -> DevonResult<Option<Chunk>> {
            self.pulls.set(self.pulls.get() + 1);
            Ok(self.chunks.pop_front())
        }
    }

    struct FailingSource;

    impl ChunkSource for FailingSource {
        fn next_chunk(&mut self) -> DevonResult<Option<Chunk>> {
            Err(DevonError::NotFound {
                what: "upstream failure".into(),
            })
        }
    }

    #[derive(Debug, Default, PartialEq)]
    struct Calls {
        neighbors: Vec<(String, Direction, u64)>,
        node_rows: Vec<(String, Direction, u64)>,
    }

    struct MockNeighbors {
        adjacency: HashMap<u64, Vec<u64>>,
        rows: HashMap<u64, Vec<Value>>,
        calls: Rc<RefCell<Calls>>,
    }

    impl MockNeighbors {
        fn new(adjacency: HashMap<u64, Vec<u64>>, rows: HashMap<u64, Vec<Value>>) -> Self {
            Self {
                adjacency,
                rows,
                calls: Rc::new(RefCell::new(Calls::default())),
            }
        }

        fn with_calls(
            adjacency: HashMap<u64, Vec<u64>>,
            rows: HashMap<u64, Vec<Value>>,
            calls: Rc<RefCell<Calls>>,
        ) -> Self {
            Self {
                adjacency,
                rows,
                calls,
            }
        }
    }

    impl NeighborSource for MockNeighbors {
        fn neighbors(
            &mut self,
            rel: &str,
            direction: Direction,
            from: u64,
        ) -> DevonResult<Vec<u64>> {
            self.calls
                .borrow_mut()
                .neighbors
                .push((rel.to_owned(), direction, from));
            Ok(self.adjacency.get(&from).cloned().unwrap_or_default())
        }

        fn node_row(
            &mut self,
            rel: &str,
            direction: Direction,
            offset: u64,
        ) -> DevonResult<Vec<Value>> {
            self.calls
                .borrow_mut()
                .node_rows
                .push((rel.to_owned(), direction, offset));
            Ok(self.rows.get(&offset).cloned().unwrap_or_default())
        }
    }

    fn chunk(types: Vec<LogicalType>, rows: Vec<Vec<Value>>) -> Chunk {
        let mut builder = ChunkBuilder::new(types);
        for row in rows {
            builder.push_row(row).unwrap();
        }
        builder.finish()
    }

    fn offset_chunk(offsets: impl IntoIterator<Item = i64>) -> Chunk {
        chunk(
            vec![LogicalType::Int64],
            offsets
                .into_iter()
                .map(|offset| vec![Value::Int64(offset)])
                .collect(),
        )
    }

    fn collect_rows(source: &mut dyn ChunkSource) -> Vec<Vec<Value>> {
        let mut rows = Vec::new();
        while let Some(chunk) = source.next_chunk().unwrap() {
            rows.extend(chunk.rows().map(|row| row.into_iter().collect()));
        }
        rows
    }

    fn string_rows(offsets: impl IntoIterator<Item = u64>) -> HashMap<u64, Vec<Value>> {
        offsets
            .into_iter()
            .map(|offset| (offset, vec![Value::String(format!("node-{offset}"))]))
            .collect()
    }

    #[test]
    fn fan_out_preserves_input_chunk_row_and_edge_order() {
        let inputs = vec![
            chunk(
                vec![LogicalType::String, LogicalType::Int64],
                vec![
                    vec![Value::String("first".into()), Value::Int64(1)],
                    vec![Value::String("second".into()), Value::Int64(2)],
                ],
            ),
            chunk(
                vec![LogicalType::String, LogicalType::Int64],
                vec![vec![Value::String("third".into()), Value::Int64(3)]],
            ),
        ];
        let adjacency = HashMap::from([(1, vec![12, 11]), (2, vec![21]), (3, vec![32, 31])]);
        let rows = string_rows([11, 12, 21, 31, 32]);
        let mut expand = Expand::new(
            Box::new(VecSource::new(inputs)),
            Box::new(MockNeighbors::new(adjacency, rows)),
            "Knows".into(),
            Direction::Out,
            1,
            vec![LogicalType::String],
        );

        assert_eq!(
            collect_rows(&mut expand),
            vec![
                vec![
                    Value::String("first".into()),
                    Value::Int64(1),
                    Value::Int64(12),
                    Value::String("node-12".into()),
                ],
                vec![
                    Value::String("first".into()),
                    Value::Int64(1),
                    Value::Int64(11),
                    Value::String("node-11".into()),
                ],
                vec![
                    Value::String("second".into()),
                    Value::Int64(2),
                    Value::Int64(21),
                    Value::String("node-21".into()),
                ],
                vec![
                    Value::String("third".into()),
                    Value::Int64(3),
                    Value::Int64(32),
                    Value::String("node-32".into()),
                ],
                vec![
                    Value::String("third".into()),
                    Value::Int64(3),
                    Value::Int64(31),
                    Value::String("node-31".into()),
                ],
            ]
        );
    }

    #[test]
    fn rows_without_neighbors_are_dropped() {
        let adjacency = HashMap::from([(2, vec![20])]);
        let mut expand = Expand::new(
            Box::new(VecSource::new(vec![offset_chunk([1, 2, 3])])),
            Box::new(MockNeighbors::new(adjacency, HashMap::new())),
            "Knows".into(),
            Direction::Out,
            0,
            vec![],
        );

        assert_eq!(
            collect_rows(&mut expand),
            vec![vec![Value::Int64(2), Value::Int64(20)]]
        );
    }

    #[test]
    fn large_fan_out_splits_at_capacity_without_eager_upstream_pull() {
        let large_neighbors: Vec<u64> = (0..CHUNK_CAPACITY as u64 + 3).collect();
        let adjacency = HashMap::from([(1, large_neighbors.clone()), (2, vec![9_999])]);
        let pulls = Rc::new(Cell::new(0));
        let upstream = CountingSource {
            chunks: vec![offset_chunk([1]), offset_chunk([2])].into(),
            pulls: Rc::clone(&pulls),
        };
        let mut expand = Expand::new(
            Box::new(upstream),
            Box::new(MockNeighbors::new(adjacency, HashMap::new())),
            "Knows".into(),
            Direction::Out,
            0,
            vec![],
        );

        let first = expand.next_chunk().unwrap().unwrap();
        assert_eq!(first.row_count(), CHUNK_CAPACITY);
        assert_eq!(pulls.get(), 1);
        assert_eq!(first.value(0, 1), Some(Value::Int64(0)));
        assert_eq!(
            first.value(CHUNK_CAPACITY - 1, 1),
            Some(Value::Int64(CHUNK_CAPACITY as i64 - 1))
        );

        let second = expand.next_chunk().unwrap().unwrap();
        assert_eq!(second.row_count(), 3);
        assert_eq!(pulls.get(), 1);
        assert_eq!(
            second.column(1).unwrap(),
            &large_neighbors[CHUNK_CAPACITY..]
                .iter()
                .map(|offset| Value::Int64(*offset as i64))
                .collect::<Vec<_>>()
        );

        let third = expand.next_chunk().unwrap().unwrap();
        assert_eq!(third.row_count(), 1);
        assert_eq!(third.value(0, 1), Some(Value::Int64(9_999)));
        assert_eq!(pulls.get(), 2);
    }

    #[test]
    fn relationship_and_direction_are_forwarded_verbatim() {
        let calls = Rc::new(RefCell::new(Calls::default()));
        let neighbors = MockNeighbors::with_calls(
            HashMap::from([(7, vec![8])]),
            HashMap::new(),
            Rc::clone(&calls),
        );
        let mut expand = Expand::new(
            Box::new(VecSource::new(vec![offset_chunk([7])])),
            Box::new(neighbors),
            "MixedCaseRel".into(),
            Direction::Both,
            0,
            vec![],
        );

        assert_eq!(collect_rows(&mut expand).len(), 1);
        assert_eq!(
            *calls.borrow(),
            Calls {
                neighbors: vec![("MixedCaseRel".into(), Direction::Both, 7)],
                node_rows: vec![("MixedCaseRel".into(), Direction::Both, 8)],
            }
        );
    }

    #[test]
    fn appended_offset_can_drive_a_second_expand() {
        let first_neighbors = MockNeighbors::new(
            HashMap::from([(1, vec![10])]),
            HashMap::from([(10, vec![Value::String("middle".into())])]),
        );
        let first = Expand::new(
            Box::new(VecSource::new(vec![offset_chunk([1])])),
            Box::new(first_neighbors),
            "FirstHop".into(),
            Direction::Out,
            0,
            vec![LogicalType::String],
        );
        let second_neighbors = MockNeighbors::new(
            HashMap::from([(10, vec![20])]),
            HashMap::from([(20, vec![Value::String("end".into())])]),
        );
        let mut second = Expand::new(
            Box::new(first),
            Box::new(second_neighbors),
            "SecondHop".into(),
            Direction::Out,
            1,
            vec![LogicalType::String],
        );

        assert_eq!(
            collect_rows(&mut second),
            vec![vec![
                Value::Int64(1),
                Value::Int64(10),
                Value::String("middle".into()),
                Value::Int64(20),
                Value::String("end".into()),
            ]]
        );
    }

    #[test]
    fn invalid_input_offsets_are_corrupt() {
        let cases = [
            chunk(vec![LogicalType::Bool], vec![vec![Value::Bool(true)]]),
            offset_chunk([-1]),
        ];
        for input in cases {
            let mut expand = Expand::new(
                Box::new(VecSource::new(vec![input])),
                Box::new(MockNeighbors::new(HashMap::new(), HashMap::new())),
                "Knows".into(),
                Direction::Out,
                0,
                vec![],
            );

            assert!(matches!(
                expand.next_chunk(),
                Err(DevonError::Corrupt { .. })
            ));
        }
    }

    #[test]
    fn neighbor_property_shape_mismatches_are_corrupt() {
        let cases = [
            (
                vec![Value::String("only one".into())],
                vec![LogicalType::String, LogicalType::Bool],
            ),
            (vec![Value::Bool(true)], vec![LogicalType::String]),
        ];
        for (row, types) in cases {
            let neighbors =
                MockNeighbors::new(HashMap::from([(1, vec![10])]), HashMap::from([(10, row)]));
            let mut expand = Expand::new(
                Box::new(VecSource::new(vec![offset_chunk([1])])),
                Box::new(neighbors),
                "Knows".into(),
                Direction::Out,
                0,
                types,
            );

            assert!(matches!(
                expand.next_chunk(),
                Err(DevonError::Corrupt { .. })
            ));
        }
    }

    #[test]
    fn upstream_errors_propagate() {
        let mut expand = Expand::new(
            Box::new(FailingSource),
            Box::new(MockNeighbors::new(HashMap::new(), HashMap::new())),
            "Knows".into(),
            Direction::Out,
            0,
            vec![],
        );

        assert!(matches!(
            expand.next_chunk(),
            Err(DevonError::NotFound { what }) if what == "upstream failure"
        ));
    }
}
