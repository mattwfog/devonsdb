//! Behavior pins for the join and aggregate hot paths.
//!
//! The deterministic 10k-row fixtures pin complete result rows across both
//! row-at-a-time and typed-column execution, including NULL join keys,
//! duplicate-match ordering, NULL group ordering, Boxed String extrema, and
//! exact Decimal sums at scale two.

use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    sync::Arc,
};

use devondb_exec::{
    chunk::{CHUNK_CAPACITY, Chunk, ChunkBuilder},
    join::{HashJoin, JoinLayout},
    operators::{Aggregate, SpillConfig},
    source::ChunkSource,
};
use devondb_plan::{
    expr::Expr,
    ops::{AggregateFunction, JoinKey, JoinType},
};
use devondb_storage::budget::MemoryBudget;
use devondb_types::{DevonResult, decimal::Decimal128, logical_type::LogicalType, value::Value};

const FIXTURE_ROWS: usize = 10_000;

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

fn chunks(types: &[LogicalType], rows: &[Vec<Value>]) -> Vec<Chunk> {
    rows.chunks(CHUNK_CAPACITY)
        .map(|rows| {
            let mut builder = ChunkBuilder::new(types.to_vec());
            for row in rows {
                builder.push_row(row.clone()).unwrap();
            }
            builder.finish()
        })
        .collect()
}

fn collect_rows(source: &mut dyn ChunkSource) -> Vec<Vec<Value>> {
    let mut rows = Vec::new();
    while let Some(chunk) = source.next_chunk().unwrap() {
        rows.extend(chunk.rows());
    }
    rows
}

fn join_key(left: &str, right: &str) -> JoinKey {
    JoinKey {
        left: Expr::Col(left.to_owned()),
        right: Expr::Col(right.to_owned()),
    }
}

fn join_layout(names: &[&str], types: &[LogicalType]) -> JoinLayout {
    JoinLayout::new(
        names
            .iter()
            .enumerate()
            .map(|(index, name)| ((*name).to_owned(), index))
            .collect(),
        types.to_vec(),
    )
}

fn join_fixture() -> (Vec<Vec<Value>>, Vec<Vec<Value>>) {
    let left = (0..FIXTURE_ROWS)
        .map(|id| {
            let key = if id % 257 == 0 {
                Value::Null
            } else if id % 263 == 0 {
                Value::Int64(100)
            } else {
                Value::Int64((id % 100) as i64)
            };
            vec![
                Value::Int64(id as i64),
                key,
                Value::String(format!("left-{id:05}")),
            ]
        })
        .collect();
    let right = (0..300)
        .map(|sequence| {
            vec![
                Value::Int64((sequence % 100) as i64),
                Value::Int64(sequence as i64),
            ]
        })
        .collect();
    (left, right)
}

fn expected_join_rows(left: &[Vec<Value>], join_type: JoinType) -> Vec<Vec<Value>> {
    let mut expected = Vec::new();
    for left_row in left {
        let key = match left_row.get(1) {
            Some(Value::Int64(key @ 0..=99)) => Some(*key),
            Some(Value::Null | Value::Int64(_)) => None,
            other => panic!("unexpected generated join key {other:?}"),
        };
        if let Some(key) = key {
            for sequence in [key, key + 100, key + 200] {
                let mut row = left_row.clone();
                row.extend([Value::Int64(key), Value::Int64(sequence)]);
                expected.push(row);
            }
        } else if join_type == JoinType::Left {
            let mut row = left_row.clone();
            row.extend([Value::Null, Value::Null]);
            expected.push(row);
        }
    }
    expected
}

fn run_join(join_type: JoinType) -> (Vec<Vec<Value>>, Vec<Vec<Value>>) {
    let (left, right) = join_fixture();
    let left_types = vec![LogicalType::Int64, LogicalType::Int64, LogicalType::String];
    let right_types = vec![LogicalType::Int64, LogicalType::Int64];
    let mut join = HashJoin::new(
        Box::new(VecSource::new(chunks(&left_types, &left))),
        Box::new(VecSource::new(chunks(&right_types, &right))),
        vec![join_key("l.key", "r.key")],
        join_type,
        join_layout(&["l.id", "l.key", "l.label"], &left_types),
        join_layout(&["r.key", "r.sequence"], &right_types),
        Arc::new(MemoryBudget::unlimited()),
    );
    (
        collect_rows(&mut join),
        expected_join_rows(&left, join_type),
    )
}

#[test]
fn hash_join_10k_pins_nulls_duplicate_matches_and_order() {
    let (inner, expected_inner) = run_join(JoinType::Inner);
    assert_eq!(inner, expected_inner);

    let (left, expected_left) = run_join(JoinType::Left);
    assert_eq!(left, expected_left);
}

#[derive(Clone)]
struct AggregateFixtureRow {
    group: Option<i64>,
    id: i64,
    amount: Option<i64>,
    score: Option<f64>,
    cents: Option<i128>,
    timestamp: Option<i64>,
    label: Option<String>,
}

impl AggregateFixtureRow {
    fn values(&self) -> Vec<Value> {
        vec![
            self.group.map_or(Value::Null, Value::Int64),
            Value::Int64(self.id),
            self.amount.map_or(Value::Null, Value::Int64),
            self.score.map_or(Value::Null, Value::Float64),
            self.cents.map_or(Value::Null, |digits| {
                Value::Decimal(Decimal128::new(digits, 2).unwrap())
            }),
            self.timestamp.map_or(Value::Null, Value::Timestamp),
            self.label
                .as_ref()
                .map_or(Value::Null, |label| Value::String(label.clone())),
        ]
    }
}

fn aggregate_fixture() -> Vec<AggregateFixtureRow> {
    (0..FIXTURE_ROWS)
        .map(|id| AggregateFixtureRow {
            group: (id % 97 != 0).then_some((id % 10) as i64),
            id: id as i64,
            amount: (id % 13 != 0).then_some((id % 201) as i64 - 100),
            score: (id % 17 != 0).then_some((id % 1_001) as f64 / 10.0),
            cents: (id % 19 != 0).then_some((id % 10_001) as i128 - 5_000),
            timestamp: (id % 23 != 0).then_some(1_900_000_000_000_000 - id as i64 * 31),
            label: (id % 29 != 0).then(|| format!("label-{id:05}")),
        })
        .collect()
}

fn aggregate_types() -> Vec<LogicalType> {
    vec![
        LogicalType::Int64,
        LogicalType::Int64,
        LogicalType::Int64,
        LogicalType::Float64,
        LogicalType::Decimal {
            precision: 18,
            scale: 2,
        },
        LogicalType::Timestamp,
        LogicalType::String,
    ]
}

fn aggregate_columns() -> HashMap<String, usize> {
    [
        ("r.group", 0),
        ("r.id", 1),
        ("r.amount", 2),
        ("r.score", 3),
        ("r.cents", 4),
        ("r.timestamp", 5),
        ("r.label", 6),
    ]
    .into_iter()
    .map(|(name, index)| (name.to_owned(), index))
    .collect()
}

fn aggregate_items(include_boxed: bool) -> Vec<(AggregateFunction, Expr)> {
    let mut items = vec![
        (AggregateFunction::Count, "r.id"),
        (AggregateFunction::Sum, "r.amount"),
        (AggregateFunction::Avg, "r.score"),
        (AggregateFunction::Sum, "r.cents"),
        (AggregateFunction::Min, "r.timestamp"),
        (AggregateFunction::Max, "r.score"),
    ];
    if include_boxed {
        items.push((AggregateFunction::Max, "r.label"));
    }
    items
        .into_iter()
        .map(|(function, column)| (function, Expr::Col(column.to_owned())))
        .collect()
}

fn aggregate_output_types(grouped: bool, include_boxed: bool) -> Vec<LogicalType> {
    let mut types = Vec::new();
    if grouped {
        types.push(LogicalType::Int64);
    }
    types.extend([
        LogicalType::Int64,
        LogicalType::Int64,
        LogicalType::Float64,
        LogicalType::Decimal {
            precision: 18,
            scale: 2,
        },
        LogicalType::Timestamp,
        LogicalType::Float64,
    ]);
    if include_boxed {
        types.push(LogicalType::String);
    }
    types
}

#[derive(Default)]
struct ReferenceAggregate {
    count: i64,
    amount: Option<i64>,
    score_sum: f64,
    score_count: u64,
    cents: Option<i128>,
    timestamp: Option<i64>,
    score_max: Option<f64>,
    label: Option<String>,
}

impl ReferenceAggregate {
    fn update(&mut self, row: &AggregateFixtureRow) {
        self.count += 1;
        if let Some(amount) = row.amount {
            self.amount = Some(self.amount.unwrap_or(0) + amount);
        }
        if let Some(score) = row.score {
            self.score_sum += score;
            self.score_count += 1;
            self.score_max = Some(self.score_max.map_or(score, |old| old.max(score)));
        }
        if let Some(cents) = row.cents {
            self.cents = Some(self.cents.unwrap_or(0) + cents);
        }
        if let Some(timestamp) = row.timestamp {
            self.timestamp = Some(self.timestamp.map_or(timestamp, |old| old.min(timestamp)));
        }
        if let Some(label) = &row.label
            && self.label.as_ref().is_none_or(|old| label > old)
        {
            self.label = Some(label.clone());
        }
    }

    fn finish(self, include_boxed: bool) -> Vec<Value> {
        let mut values = vec![
            Value::Int64(self.count),
            self.amount.map_or(Value::Null, Value::Int64),
            if self.score_count != 0 {
                Value::Float64(self.score_sum / self.score_count as f64)
            } else {
                Value::Null
            },
            self.cents.map_or(Value::Null, |digits| {
                Value::Decimal(Decimal128::new(digits, 2).unwrap())
            }),
            self.timestamp.map_or(Value::Null, Value::Timestamp),
            self.score_max.map_or(Value::Null, Value::Float64),
        ];
        if include_boxed {
            values.push(self.label.map_or(Value::Null, Value::String));
        }
        values
    }
}

fn expected_grouped_aggregates(
    fixture: &[AggregateFixtureRow],
    include_boxed: bool,
) -> Vec<Vec<Value>> {
    let mut groups = BTreeMap::<Option<i64>, ReferenceAggregate>::new();
    for row in fixture {
        groups.entry(row.group).or_default().update(row);
    }
    groups
        .into_iter()
        .map(|(group, state)| {
            let mut row = vec![group.map_or(Value::Null, Value::Int64)];
            row.extend(state.finish(include_boxed));
            row
        })
        .collect()
}

fn expected_global_aggregates(
    fixture: &[AggregateFixtureRow],
    include_boxed: bool,
) -> Vec<Vec<Value>> {
    let mut state = ReferenceAggregate::default();
    for row in fixture {
        state.update(row);
    }
    vec![state.finish(include_boxed)]
}

fn run_aggregate(
    fixture: &[AggregateFixtureRow],
    grouped: bool,
    include_boxed: bool,
) -> Vec<Vec<Value>> {
    let rows = fixture
        .iter()
        .map(AggregateFixtureRow::values)
        .collect::<Vec<_>>();
    let group_by = if grouped {
        vec![Expr::Col("r.group".to_owned())]
    } else {
        Vec::new()
    };
    let mut aggregate = Aggregate::new(
        Box::new(VecSource::new(chunks(&aggregate_types(), &rows))),
        group_by,
        aggregate_items(include_boxed),
        aggregate_columns(),
        aggregate_output_types(grouped, include_boxed),
        SpillConfig::unbounded(),
    );
    collect_rows(&mut aggregate)
}

#[test]
fn aggregate_10k_pins_grouped_and_global_exact_outputs() {
    let fixture = aggregate_fixture();
    assert_eq!(
        run_aggregate(&fixture, true, false),
        expected_grouped_aggregates(&fixture, false)
    );
    assert_eq!(
        run_aggregate(&fixture, true, true),
        expected_grouped_aggregates(&fixture, true)
    );
    assert_eq!(
        run_aggregate(&fixture, false, true),
        expected_global_aggregates(&fixture, true)
    );
}
