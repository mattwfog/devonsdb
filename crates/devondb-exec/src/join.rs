//! Hash-join execution over two [`crate::source::ChunkSource`] inputs.
//!
//! The right input is the blocking build side. The left input is streamed in
//! probe order, and every build bucket retains right-input insertion order.

use std::{collections::HashMap, sync::Arc};

use devondb_plan::{
    expr::Expr,
    ops::{JoinKey, JoinType},
};
use devondb_storage::budget::MemoryBudget;
use devondb_types::{
    DevonError, DevonResult, decimal::Decimal128, logical_type::LogicalType, value::Value,
};

use crate::{
    chunk::{Chunk, ChunkBuilder},
    column::Column,
    eval::evaluate,
    source::ChunkSource,
};

/// Writer-policy estimate for one buffered build row and allocator metadata.
/// Value payloads are charged separately through [`Value::approx_bytes`].
const BUFFERED_ROW_OVERHEAD_BYTES: usize = 64;

/// The expression lookup map and ordered physical types for one join input.
#[derive(Clone, Debug)]
pub struct JoinLayout {
    columns: HashMap<String, usize>,
    types: Vec<LogicalType>,
}

impl JoinLayout {
    /// Creates a layout from an expression column map and column-ordered types.
    #[must_use]
    pub fn new(columns: HashMap<String, usize>, types: Vec<LogicalType>) -> Self {
        Self { columns, types }
    }
}

/// A right-build, left-probe equality join with deterministic output order.
pub struct HashJoin {
    left: Box<dyn ChunkSource>,
    right: Box<dyn ChunkSource>,
    on: Vec<JoinKey>,
    join: JoinType,
    left_layout: JoinLayout,
    right_layout: JoinLayout,
    output_types: Vec<LogicalType>,
    budget: Arc<MemoryBudget>,
    build: Option<BuildTable>,
    probe: Option<ProbeChunk>,
    pending: Option<PendingProbe>,
    initialized: bool,
    finished: bool,
}

impl HashJoin {
    /// Creates a join whose output layout is `left` followed by `right`.
    #[must_use]
    pub fn new(
        left: Box<dyn ChunkSource>,
        right: Box<dyn ChunkSource>,
        on: Vec<JoinKey>,
        join: JoinType,
        left_layout: JoinLayout,
        right_layout: JoinLayout,
        budget: Arc<MemoryBudget>,
    ) -> Self {
        let output_types = left_layout
            .types
            .iter()
            .chain(&right_layout.types)
            .copied()
            .collect();
        Self {
            left,
            right,
            on,
            join,
            left_layout,
            right_layout,
            output_types,
            budget,
            build: None,
            probe: None,
            pending: None,
            initialized: false,
            finished: false,
        }
    }

    fn initialize(&mut self) -> DevonResult<()> {
        if self.on.is_empty() {
            return Err(invalid_argument(
                "HashJoin requires at least one equality key",
            ));
        }
        let mut build = BuildTable::new(Arc::clone(&self.budget));
        while let Some(chunk) = self.right.next_chunk()? {
            validate_layout(&chunk, &self.right_layout.types, "right")?;
            let keys = evaluate_keys(&self.on, &chunk, &self.right_layout.columns, Side::Right)?;
            build.reserve_rows(chunk.row_count());
            append_build_chunk(&mut build, &chunk, &keys)?;
        }
        self.build = Some(build);
        self.initialized = true;
        Ok(())
    }

    fn next_probe_row(&mut self) -> DevonResult<Option<PendingProbe>> {
        loop {
            if let Some(probe) = self.probe.as_mut()
                && let Some(row) = probe.next_row()?
            {
                return Ok(Some(row));
            }
            let Some(chunk) = self.left.next_chunk()? else {
                self.probe = None;
                return Ok(None);
            };
            validate_layout(&chunk, &self.left_layout.types, "left")?;
            let keys = evaluate_keys(&self.on, &chunk, &self.left_layout.columns, Side::Left)?;
            self.probe = Some(ProbeChunk::new(chunk, keys));
        }
    }

    fn take_pending_output(&mut self) -> DevonResult<Option<Vec<Value>>> {
        let Some(mut pending) = self.pending.take() else {
            return Ok(None);
        };
        let right_row = pending.key.as_ref().and_then(|key| {
            self.build
                .as_ref()
                .and_then(|build| build.get(key, pending.next_match))
                .cloned()
        });
        if let Some(right_row) = right_row {
            pending.next_match += 1;
            let has_more = pending.key.as_ref().is_some_and(|key| {
                self.build
                    .as_ref()
                    .is_some_and(|build| build.contains_index(key, pending.next_match))
            });
            if pending.left.is_none() {
                let probe = self
                    .probe
                    .as_ref()
                    .ok_or_else(|| invalid_argument("HashJoin pending probe has no probe chunk"))?;
                pending.left = Some(clone_row(&probe.chunk, pending.row_index)?);
            }
            let mut output = if has_more {
                pending
                    .left
                    .as_ref()
                    .cloned()
                    .ok_or_else(|| invalid_argument("HashJoin pending probe has no left row"))?
            } else {
                pending
                    .left
                    .take()
                    .ok_or_else(|| invalid_argument("HashJoin pending probe has no left row"))?
            };
            output.extend(right_row);
            if has_more {
                self.pending = Some(pending);
            }
            return Ok(Some(output));
        }
        if self.join == JoinType::Left {
            let probe = self
                .probe
                .as_ref()
                .ok_or_else(|| invalid_argument("HashJoin pending probe has no probe chunk"))?;
            let mut output = clone_row(&probe.chunk, pending.row_index)?;
            output.extend(std::iter::repeat_n(
                Value::Null,
                self.right_layout.types.len(),
            ));
            return Ok(Some(output));
        }
        Ok(None)
    }

    fn finish(&mut self) {
        self.finished = true;
        self.build = None;
        self.probe = None;
        self.pending = None;
    }
}

impl ChunkSource for HashJoin {
    fn next_chunk(&mut self) -> DevonResult<Option<Chunk>> {
        if self.finished {
            return Ok(None);
        }
        if !self.initialized {
            self.initialize()?;
        }
        let mut builder = ChunkBuilder::new(self.output_types.clone());
        let mut output_rows = 0;
        while !builder.is_full() {
            if self.pending.is_none() {
                let Some(pending) = self.next_probe_row()? else {
                    self.finish();
                    break;
                };
                self.pending = Some(pending);
            }
            if let Some(row) = self.take_pending_output()? {
                builder.push_row(row)?;
                output_rows += 1;
            }
        }
        Ok((output_rows != 0).then(|| builder.finish()))
    }
}

#[derive(Clone, Copy)]
enum Side {
    Left,
    Right,
}

fn evaluate_keys(
    on: &[JoinKey],
    chunk: &Chunk,
    columns: &HashMap<String, usize>,
    side: Side,
) -> DevonResult<Vec<EvaluatedKey>> {
    on.iter()
        .map(|key| {
            let expression = match side {
                Side::Left => &key.left,
                Side::Right => &key.right,
            };
            evaluate_key(expression, chunk, columns)
        })
        .collect()
}

fn evaluate_key(
    expression: &Expr,
    chunk: &Chunk,
    columns: &HashMap<String, usize>,
) -> DevonResult<EvaluatedKey> {
    let Expr::Col(reference) = expression else {
        return evaluate(expression, chunk, columns).map(EvaluatedKey::Materialized);
    };
    let index = columns
        .get(reference)
        .copied()
        .ok_or_else(|| invalid_argument(format!("unknown column reference `{reference}`")))?;
    if chunk.column(index).is_none() {
        return Err(invalid_argument(format!(
            "column reference `{reference}` maps to out-of-range chunk column {index}"
        )));
    }
    Ok(EvaluatedKey::Direct(index))
}

enum EvaluatedKey {
    Direct(usize),
    Materialized(Vec<Value>),
}

fn append_build_chunk(
    build: &mut BuildTable,
    chunk: &Chunk,
    key_columns: &[EvaluatedKey],
) -> DevonResult<()> {
    for row_index in 0..chunk.row_count() {
        let Some((key, key_charge)) =
            HashKey::from_evaluated(key_columns, chunk, row_index, "HashJoin build key")?
        else {
            continue;
        };
        let row = clone_row(chunk, row_index)?;
        let charge = build_row_charge(&row, key_charge)?;
        build.insert(key, row, charge)?;
    }
    Ok(())
}

struct BuildTable {
    buckets: HashMap<HashKey, Vec<Vec<Value>>>,
    budget: Arc<MemoryBudget>,
    charged: usize,
}

impl BuildTable {
    fn new(budget: Arc<MemoryBudget>) -> Self {
        Self {
            buckets: HashMap::new(),
            budget,
            charged: 0,
        }
    }

    fn reserve_rows(&mut self, additional: usize) {
        self.buckets.reserve(additional);
    }

    fn insert(&mut self, key: HashKey, row: Vec<Value>, bytes: usize) -> DevonResult<()> {
        if !self.budget.charge_or_reclaim(bytes) {
            return Err(DevonError::BudgetExceeded {
                context: format!(
                    "HashJoin build row requests {bytes} bytes with {} charged against a {} byte limit",
                    self.budget.charged(),
                    self.budget.limit()
                ),
            });
        }
        let Some(charged) = self.charged.checked_add(bytes) else {
            self.budget.release(bytes);
            return Err(invalid_argument(
                "HashJoin build-side memory estimate exceeds usize::MAX",
            ));
        };
        self.charged = charged;
        self.buckets.entry(key).or_default().push(row);
        Ok(())
    }

    fn get(&self, key: &HashKey, index: usize) -> Option<&Vec<Value>> {
        self.buckets.get(key)?.get(index)
    }

    fn contains_index(&self, key: &HashKey, index: usize) -> bool {
        self.buckets.get(key).is_some_and(|rows| index < rows.len())
    }
}

impl Drop for BuildTable {
    fn drop(&mut self) {
        self.budget.release(self.charged);
    }
}

struct ProbeChunk {
    chunk: Chunk,
    key_columns: Vec<EvaluatedKey>,
    next_row: usize,
}

impl ProbeChunk {
    fn new(chunk: Chunk, key_columns: Vec<EvaluatedKey>) -> Self {
        Self {
            chunk,
            key_columns,
            next_row: 0,
        }
    }

    fn next_row(&mut self) -> DevonResult<Option<PendingProbe>> {
        if self.next_row == self.chunk.row_count() {
            return Ok(None);
        }
        let row_index = self.next_row;
        self.next_row += 1;
        let key = HashKey::from_evaluated(
            &self.key_columns,
            &self.chunk,
            row_index,
            "HashJoin probe key",
        )?
        .map(|(key, _)| key);
        Ok(Some(PendingProbe {
            left: None,
            row_index,
            key,
            next_match: 0,
        }))
    }
}

struct PendingProbe {
    left: Option<Vec<Value>>,
    row_index: usize,
    key: Option<HashKey>,
    next_match: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum HashKey {
    Single(HashValue),
    Composite(Vec<HashValue>),
}

impl HashKey {
    fn from_evaluated(
        keys: &[EvaluatedKey],
        chunk: &Chunk,
        row: usize,
        context: &str,
    ) -> DevonResult<Option<(Self, usize)>> {
        if let [key] = keys {
            return Ok(evaluated_hash_value(key, chunk, row, context)?
                .map(|(value, charge)| (Self::Single(value), charge)));
        }
        let mut values = Vec::with_capacity(keys.len());
        let mut charge = 0_usize;
        for key in keys {
            let Some((value, value_charge)) = evaluated_hash_value(key, chunk, row, context)?
            else {
                return Ok(None);
            };
            charge = charge.checked_add(value_charge).ok_or_else(|| {
                invalid_argument("HashJoin build-side memory estimate exceeds usize::MAX")
            })?;
            values.push(value);
        }
        Ok(Some((Self::Composite(values), charge)))
    }
}

fn evaluated_hash_value(
    key: &EvaluatedKey,
    chunk: &Chunk,
    row: usize,
    context: &str,
) -> DevonResult<Option<(HashValue, usize)>> {
    match key {
        EvaluatedKey::Direct(index) => {
            let column = chunk.column(*index).ok_or_else(|| {
                invalid_argument(format!("{context} column {index} is out of range"))
            })?;
            HashValue::from_column(column, row)
        }
        EvaluatedKey::Materialized(values) => {
            let value = values.get(row).ok_or_else(|| {
                invalid_argument(format!("{context} returned fewer than {} rows", row + 1))
            })?;
            Ok(HashValue::from_value(value)?.map(|key| (key, value.approx_bytes())))
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum HashValue {
    Bool(bool),
    Int64(i64),
    Float64(u64),
    String(String),
    Timestamp(i64),
    Bytes(Vec<u8>),
    Decimal(Decimal128),
    Json(String),
}

impl HashValue {
    fn from_column(column: &Column, row: usize) -> DevonResult<Option<(Self, usize)>> {
        match column {
            Column::Int64 { values, validity } => Ok(valid_value(validity, row)
                .then(|| (Self::Int64(values[row]), Value::Int64(0).approx_bytes()))),
            Column::Float64 { values, validity } => {
                let value = values[row];
                Ok((valid_value(validity, row) && !value.is_nan()).then(|| {
                    (
                        Self::Float64(float_key_bits(value)),
                        Value::Float64(0.0).approx_bytes(),
                    )
                }))
            }
            Column::Bool { values, validity } => Ok(valid_value(validity, row)
                .then(|| (Self::Bool(values[row]), Value::Bool(false).approx_bytes()))),
            Column::Timestamp { values, validity } => Ok(valid_value(validity, row).then(|| {
                (
                    Self::Timestamp(values[row]),
                    Value::Timestamp(0).approx_bytes(),
                )
            })),
            Column::Decimal {
                values,
                scale,
                validity,
            } => {
                if !valid_value(validity, row) {
                    return Ok(None);
                }
                let decimal = Decimal128::new(values[row], *scale)?;
                Ok(Some((
                    Self::Decimal(decimal),
                    Value::Decimal(decimal).approx_bytes(),
                )))
            }
            Column::Boxed(values) => {
                let value = values.get(row).ok_or_else(|| {
                    invalid_argument(format!("HashJoin key row {row} is out of range"))
                })?;
                Ok(Self::from_value(value)?.map(|key| (key, value.approx_bytes())))
            }
        }
    }

    fn from_value(value: &Value) -> DevonResult<Option<Self>> {
        match value {
            Value::Null => Ok(None),
            Value::Bool(value) => Ok(Some(Self::Bool(*value))),
            Value::Int64(value) => Ok(Some(Self::Int64(*value))),
            Value::Float64(value) if value.is_nan() => Ok(None),
            Value::Float64(value) => Ok(Some(Self::Float64(float_key_bits(*value)))),
            Value::String(value) => Ok(Some(Self::String(value.clone()))),
            Value::Timestamp(value) => Ok(Some(Self::Timestamp(*value))),
            Value::Bytes(value) => Ok(Some(Self::Bytes(value.clone()))),
            Value::Decimal(value) => Ok(Some(Self::Decimal(*value))),
            Value::Json(value) => Ok(Some(Self::Json(value.clone()))),
            Value::Vector(_) | Value::GeoPoint(_) => Err(invalid_argument(format!(
                "HashJoin key does not support value {value}"
            ))),
        }
    }
}

fn valid_value(validity: &Option<devondb_types::column::Bitmap>, row: usize) -> bool {
    validity.as_ref().is_none_or(|bitmap| bitmap.is_valid(row))
}

fn float_key_bits(value: f64) -> u64 {
    if value == 0.0 {
        0.0_f64.to_bits()
    } else {
        value.to_bits()
    }
}

fn build_row_charge(row: &[Value], key_charge: usize) -> DevonResult<usize> {
    row.iter()
        .try_fold(BUFFERED_ROW_OVERHEAD_BYTES, |total, value| {
            total.checked_add(value.approx_bytes()).ok_or_else(|| {
                invalid_argument("HashJoin build-side memory estimate exceeds usize::MAX")
            })
        })?
        .checked_add(key_charge)
        .ok_or_else(|| invalid_argument("HashJoin build-side memory estimate exceeds usize::MAX"))
}

fn validate_layout(chunk: &Chunk, expected: &[LogicalType], side: &str) -> DevonResult<()> {
    if chunk.types() == expected {
        return Ok(());
    }
    Err(invalid_argument(format!(
        "HashJoin {side} chunk types {:?} do not match supplied layout {expected:?}",
        chunk.types()
    )))
}

fn clone_row(chunk: &Chunk, row: usize) -> DevonResult<Vec<Value>> {
    (0..chunk.column_count())
        .map(|column| {
            chunk.value(row, column).ok_or_else(|| {
                invalid_argument(format!(
                    "cannot copy missing row {row}, column {column} from join input"
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
    use std::{collections::VecDeque, sync::Arc};

    use devondb_plan::{
        expr::Expr,
        ops::{JoinKey, JoinType},
    };
    use devondb_storage::budget::MemoryBudget;
    use devondb_types::{
        DevonError, DevonResult, decimal::Decimal128, logical_type::LogicalType, value::Value,
    };

    use super::{HashJoin, JoinLayout};
    use crate::{
        chunk::{CHUNK_CAPACITY, Chunk, ChunkBuilder},
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

    fn chunk(types: Vec<LogicalType>, rows: Vec<Vec<Value>>) -> Chunk {
        let mut builder = ChunkBuilder::new(types);
        for row in rows {
            builder.push_row(row).unwrap();
        }
        builder.finish()
    }

    fn chunks(types: Vec<LogicalType>, rows: Vec<Vec<Value>>) -> Vec<Chunk> {
        rows.chunks(CHUNK_CAPACITY)
            .map(|rows| chunk(types.clone(), rows.to_vec()))
            .collect()
    }

    fn layout(names: &[&str], types: Vec<LogicalType>) -> JoinLayout {
        JoinLayout::new(
            names
                .iter()
                .enumerate()
                .map(|(index, name)| ((*name).to_owned(), index))
                .collect(),
            types,
        )
    }

    fn key(left: &str, right: &str) -> JoinKey {
        JoinKey {
            left: Expr::Col(left.into()),
            right: Expr::Col(right.into()),
        }
    }

    fn join(
        left: Vec<Chunk>,
        right: Vec<Chunk>,
        on: Vec<JoinKey>,
        join: JoinType,
        left_layout: JoinLayout,
        right_layout: JoinLayout,
    ) -> HashJoin {
        HashJoin::new(
            Box::new(VecSource::new(left)),
            Box::new(VecSource::new(right)),
            on,
            join,
            left_layout,
            right_layout,
            Arc::new(MemoryBudget::unlimited()),
        )
    }

    fn collect_chunks(source: &mut dyn ChunkSource) -> Vec<Chunk> {
        let mut chunks = Vec::new();
        while let Some(chunk) = source.next_chunk().unwrap() {
            chunks.push(chunk);
        }
        chunks
    }

    fn collect_rows(source: &mut dyn ChunkSource) -> Vec<Vec<Value>> {
        collect_chunks(source)
            .into_iter()
            .flat_map(|chunk| {
                (0..chunk.row_count())
                    .map(|row| {
                        (0..chunk.column_count())
                            .map(|column| chunk.value(row, column).unwrap().clone())
                            .collect::<Vec<_>>()
                    })
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    fn int_pair_layout(prefix: &str) -> JoinLayout {
        let key = format!("{prefix}.key");
        let sequence = format!("{prefix}.sequence");
        layout(
            &[&key, &sequence],
            vec![LogicalType::Int64, LogicalType::Int64],
        )
    }

    #[test]
    fn join_inner_streams_more_than_one_chunk_in_left_order() {
        let row_count = CHUNK_CAPACITY + 3;
        let left_rows = (0..row_count as i64)
            .map(|value| vec![Value::Int64(value), Value::Int64(value)])
            .collect();
        let right_rows = (0..row_count as i64)
            .map(|value| vec![Value::Int64(value), Value::Int64(value * 10)])
            .collect();
        let types = vec![LogicalType::Int64, LogicalType::Int64];
        let mut join = join(
            chunks(types.clone(), left_rows),
            chunks(types, right_rows),
            vec![key("l.key", "r.key")],
            JoinType::Inner,
            int_pair_layout("l"),
            int_pair_layout("r"),
        );

        let output = collect_chunks(&mut join);
        assert_eq!(output.len(), 2);
        assert_eq!(output[0].row_count(), CHUNK_CAPACITY);
        assert_eq!(output[1].row_count(), 3);
        assert_eq!(output[0].value(0, 1), Some(Value::Int64(0)));
        assert_eq!(output[0].value(0, 3), Some(Value::Int64(0)));
        assert_eq!(
            output[1].value(2, 1),
            Some(Value::Int64(row_count as i64 - 1))
        );
        assert_eq!(
            output[1].value(2, 3),
            Some(Value::Int64((row_count as i64 - 1) * 10))
        );
    }

    #[test]
    fn join_left_crosses_chunks_and_pads_the_unmatched_tail() {
        let row_count = CHUNK_CAPACITY + 2;
        let left_rows = (0..row_count as i64)
            .map(|value| vec![Value::Int64(value), Value::Int64(value)])
            .collect();
        let mut right_rows = (0..row_count as i64 - 1)
            .map(|value| vec![Value::Int64(value), Value::Int64(value * 10)])
            .collect::<Vec<_>>();
        right_rows.push(vec![Value::Int64(50_000), Value::Int64(7)]);
        let types = vec![LogicalType::Int64, LogicalType::Int64];
        let mut join = join(
            chunks(types.clone(), left_rows),
            chunks(types, right_rows),
            vec![key("l.key", "r.key")],
            JoinType::Left,
            int_pair_layout("l"),
            int_pair_layout("r"),
        );

        let output = collect_chunks(&mut join);
        assert_eq!(output.len(), 2);
        assert_eq!(output[0].row_count(), CHUNK_CAPACITY);
        assert_eq!(output[1].row_count(), 2);
        assert_eq!(
            output[1].value(1, 1),
            Some(Value::Int64(row_count as i64 - 1))
        );
        assert_eq!(output[1].value(1, 2), Some(Value::Null));
        assert_eq!(output[1].value(1, 3), Some(Value::Null));
    }

    #[test]
    fn join_duplicate_keys_preserve_left_then_right_insertion_order() {
        let types = vec![LogicalType::Int64, LogicalType::String];
        let left = chunk(
            types.clone(),
            vec![
                vec![Value::Int64(1), Value::String("l0".into())],
                vec![Value::Int64(2), Value::String("l1".into())],
                vec![Value::Int64(1), Value::String("l2".into())],
            ],
        );
        let right = chunk(
            types.clone(),
            vec![
                vec![Value::Int64(1), Value::String("r0".into())],
                vec![Value::Int64(1), Value::String("r1".into())],
                vec![Value::Int64(2), Value::String("r2".into())],
                vec![Value::Int64(1), Value::String("r3".into())],
            ],
        );
        let mut join = join(
            vec![left],
            vec![right],
            vec![key("l.key", "r.key")],
            JoinType::Inner,
            layout(&["l.key", "l.name"], types.clone()),
            layout(&["r.key", "r.name"], types),
        );

        let pairs = collect_rows(&mut join)
            .into_iter()
            .map(|row| (row[1].clone(), row[3].clone()))
            .collect::<Vec<_>>();
        assert_eq!(
            pairs,
            vec![
                (Value::String("l0".into()), Value::String("r0".into())),
                (Value::String("l0".into()), Value::String("r1".into())),
                (Value::String("l0".into()), Value::String("r3".into())),
                (Value::String("l1".into()), Value::String("r2".into())),
                (Value::String("l2".into()), Value::String("r0".into())),
                (Value::String("l2".into()), Value::String("r1".into())),
                (Value::String("l2".into()), Value::String("r3".into())),
            ]
        );
    }

    #[test]
    fn join_null_keys_never_match_for_inner_or_left() {
        let types = vec![LogicalType::Int64, LogicalType::String];
        let left = chunk(
            types.clone(),
            vec![
                vec![Value::Null, Value::String("null-left".into())],
                vec![Value::Int64(1), Value::String("one-left".into())],
            ],
        );
        let right = chunk(
            types.clone(),
            vec![
                vec![Value::Null, Value::String("null-right".into())],
                vec![Value::Int64(1), Value::String("one-right".into())],
            ],
        );
        let make = |join_type| {
            join(
                vec![left.clone()],
                vec![right.clone()],
                vec![key("l.key", "r.key")],
                join_type,
                layout(&["l.key", "l.name"], types.clone()),
                layout(&["r.key", "r.name"], types.clone()),
            )
        };
        let mut inner = make(JoinType::Inner);
        let mut left_join = make(JoinType::Left);

        assert_eq!(
            collect_rows(&mut inner),
            vec![vec![
                Value::Int64(1),
                Value::String("one-left".into()),
                Value::Int64(1),
                Value::String("one-right".into()),
            ]]
        );
        assert_eq!(
            collect_rows(&mut left_join),
            vec![
                vec![
                    Value::Null,
                    Value::String("null-left".into()),
                    Value::Null,
                    Value::Null,
                ],
                vec![
                    Value::Int64(1),
                    Value::String("one-left".into()),
                    Value::Int64(1),
                    Value::String("one-right".into()),
                ],
            ]
        );
    }

    #[test]
    fn join_empty_build_side_obeys_inner_and_left_semantics() {
        let types = vec![LogicalType::Int64];
        let left = chunk(
            types.clone(),
            vec![vec![Value::Int64(1)], vec![Value::Int64(2)]],
        );
        let make = |join_type| {
            join(
                vec![left.clone()],
                vec![],
                vec![key("l.key", "r.key")],
                join_type,
                layout(&["l.key"], types.clone()),
                layout(&["r.key"], types.clone()),
            )
        };
        let mut inner = make(JoinType::Inner);
        let mut left_join = make(JoinType::Left);

        assert!(collect_rows(&mut inner).is_empty());
        assert_eq!(
            collect_rows(&mut left_join),
            vec![
                vec![Value::Int64(1), Value::Null],
                vec![Value::Int64(2), Value::Null],
            ]
        );
    }

    #[test]
    fn join_empty_probe_side_emits_nothing_and_releases_build_budget() {
        let budget = Arc::new(MemoryBudget::new(1_000));
        let types = vec![LogicalType::Int64];
        let right = chunk(types.clone(), vec![vec![Value::Int64(1)]]);
        let mut join = HashJoin::new(
            Box::new(VecSource::new(vec![])),
            Box::new(VecSource::new(vec![right])),
            vec![key("l.key", "r.key")],
            JoinType::Inner,
            layout(&["l.key"], types.clone()),
            layout(&["r.key"], types),
            Arc::clone(&budget),
        );

        assert!(join.next_chunk().unwrap().is_none());
        assert_eq!(budget.charged(), 0);
    }

    #[test]
    fn join_composite_keys_require_every_component_to_match() {
        let types = vec![LogicalType::String, LogicalType::Int64, LogicalType::String];
        let left = chunk(
            types.clone(),
            vec![
                vec![
                    Value::String("a".into()),
                    Value::Int64(1),
                    Value::String("l0".into()),
                ],
                vec![
                    Value::String("a".into()),
                    Value::Int64(2),
                    Value::String("l1".into()),
                ],
            ],
        );
        let right = chunk(
            types.clone(),
            vec![
                vec![
                    Value::String("a".into()),
                    Value::Int64(2),
                    Value::String("r0".into()),
                ],
                vec![
                    Value::String("b".into()),
                    Value::Int64(1),
                    Value::String("r1".into()),
                ],
                vec![
                    Value::String("a".into()),
                    Value::Int64(1),
                    Value::String("r2".into()),
                ],
            ],
        );
        let mut join = join(
            vec![left],
            vec![right],
            vec![key("l.group", "r.group"), key("l.key", "r.key")],
            JoinType::Inner,
            layout(&["l.group", "l.key", "l.name"], types.clone()),
            layout(&["r.group", "r.key", "r.name"], types),
        );

        let labels = collect_rows(&mut join)
            .into_iter()
            .map(|row| (row[2].clone(), row[5].clone()))
            .collect::<Vec<_>>();
        assert_eq!(
            labels,
            vec![
                (Value::String("l0".into()), Value::String("r2".into())),
                (Value::String("l1".into()), Value::String("r0".into())),
            ]
        );
    }

    #[test]
    fn join_timestamp_and_decimal_keys_use_exact_scalar_equality() {
        let decimal_type = LogicalType::Decimal {
            precision: 6,
            scale: 2,
        };
        let types = vec![LogicalType::Timestamp, decimal_type, LogicalType::String];
        let decimal = |digits| Value::Decimal(Decimal128::new(digits, 2).unwrap());
        let left = chunk(
            types.clone(),
            vec![
                vec![
                    Value::Timestamp(-1),
                    decimal(125),
                    Value::String("l0".into()),
                ],
                vec![
                    Value::Timestamp(5),
                    decimal(-200),
                    Value::String("l1".into()),
                ],
            ],
        );
        let right = chunk(
            types.clone(),
            vec![
                vec![
                    Value::Timestamp(5),
                    decimal(-200),
                    Value::String("r0".into()),
                ],
                vec![
                    Value::Timestamp(-1),
                    decimal(126),
                    Value::String("r1".into()),
                ],
                vec![
                    Value::Timestamp(-1),
                    decimal(125),
                    Value::String("r2".into()),
                ],
            ],
        );
        let mut join = join(
            vec![left],
            vec![right],
            vec![key("l.ts", "r.ts"), key("l.amount", "r.amount")],
            JoinType::Inner,
            layout(&["l.ts", "l.amount", "l.name"], types.clone()),
            layout(&["r.ts", "r.amount", "r.name"], types),
        );

        let labels = collect_rows(&mut join)
            .into_iter()
            .map(|row| (row[2].clone(), row[5].clone()))
            .collect::<Vec<_>>();
        assert_eq!(
            labels,
            vec![
                (Value::String("l0".into()), Value::String("r2".into())),
                (Value::String("l1".into()), Value::String("r0".into())),
            ]
        );
    }

    #[test]
    fn join_float_keys_match_signed_zero_but_never_nan() {
        let nan = f64::from_bits(0x7ff8_0000_0000_0001);
        let types = vec![LogicalType::Float64, LogicalType::Int64];
        let left = chunk(
            types.clone(),
            vec![
                vec![Value::Float64(-0.0), Value::Int64(0)],
                vec![Value::Float64(nan), Value::Int64(1)],
                vec![Value::Float64(1.5), Value::Int64(2)],
            ],
        );
        let right = chunk(
            types.clone(),
            vec![
                vec![Value::Float64(0.0), Value::Int64(10)],
                vec![Value::Float64(nan), Value::Int64(11)],
                vec![Value::Float64(1.5), Value::Int64(12)],
            ],
        );
        let mut join = join(
            vec![left],
            vec![right],
            vec![key("l.key", "r.key")],
            JoinType::Inner,
            layout(&["l.key", "l.sequence"], types.clone()),
            layout(&["r.key", "r.sequence"], types),
        );

        let sequences = collect_rows(&mut join)
            .into_iter()
            .map(|row| (row[1].clone(), row[3].clone()))
            .collect::<Vec<_>>();
        assert_eq!(
            sequences,
            vec![
                (Value::Int64(0), Value::Int64(10)),
                (Value::Int64(2), Value::Int64(12)),
            ]
        );
    }

    #[test]
    fn join_build_side_budget_exceeded_releases_prior_charges() {
        let budget = Arc::new(MemoryBudget::new(1));
        let types = vec![LogicalType::Int64, LogicalType::String];
        let right = chunk(
            types.clone(),
            vec![vec![Value::Int64(1), Value::String("build payload".into())]],
        );
        let mut join = HashJoin::new(
            Box::new(VecSource::new(vec![])),
            Box::new(VecSource::new(vec![right])),
            vec![key("l.key", "r.key")],
            JoinType::Inner,
            layout(&["l.key"], vec![LogicalType::Int64]),
            layout(&["r.key", "r.payload"], types),
            Arc::clone(&budget),
        );

        let DevonError::BudgetExceeded { context } = join.next_chunk().unwrap_err() else {
            panic!("expected BudgetExceeded");
        };
        assert!(context.contains("HashJoin build row"));
        assert!(context.contains("1 byte limit"));
        assert_eq!(budget.charged(), 0);
    }
}
