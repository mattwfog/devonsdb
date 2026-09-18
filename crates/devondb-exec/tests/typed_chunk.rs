//! Typed-chunk validation tests (`docs/SCALE.md` §6.5):
//! `Chunk::from_columns` builds a chunk directly from typed columns with the
//! same validation `ChunkBuilder::push_row` applies to rows, and the result
//! equals a builder-built chunk value-for-value.

use devondb_exec::{
    chunk::{CHUNK_CAPACITY, Chunk, ChunkBuilder},
    column::{Bitmap, Column},
};
use devondb_types::{DevonError, logical_type::LogicalType, value::Value};

fn sample_types() -> Vec<LogicalType> {
    vec![
        LogicalType::Int64,
        LogicalType::String,
        LogicalType::Bool,
        LogicalType::Decimal {
            precision: 10,
            scale: 2,
        },
    ]
}

fn sample_rows() -> Vec<Vec<Value>> {
    vec![
        vec![
            Value::Int64(7),
            Value::String("ada".into()),
            Value::Bool(true),
            Value::Decimal(devondb_types::Decimal128::new(1234, 2).unwrap()),
        ],
        vec![Value::Null, Value::Null, Value::Null, Value::Null],
        vec![
            Value::Int64(-3),
            Value::String(String::new()),
            Value::Bool(false),
            Value::Decimal(devondb_types::Decimal128::new(-99, 2).unwrap()),
        ],
    ]
}

fn builder_chunk(types: &[LogicalType], rows: &[Vec<Value>]) -> Chunk {
    let mut builder = ChunkBuilder::new(types.to_vec());
    for row in rows {
        builder.push_row(row.clone()).unwrap();
    }
    builder.finish()
}

#[test]
fn from_columns_equals_builder_built_chunk() {
    let types = sample_types();
    let rows = sample_rows();
    let columns = rows
        .iter()
        .fold(vec![Vec::new(); types.len()], |mut acc, row| {
            for (index, value) in row.iter().enumerate() {
                acc[index].push(value.clone());
            }
            acc
        })
        .into_iter()
        .zip(&types)
        .map(|(values, ty)| Column::from_values(ty, values))
        .collect::<Vec<_>>();

    let typed = Chunk::from_columns(types.clone(), columns).unwrap();
    let built = builder_chunk(&types, &rows);

    assert_eq!(typed, built);
    assert_eq!(typed.row_count(), rows.len());
    assert_eq!(
        typed.rows().collect::<Vec<_>>(),
        built.rows().collect::<Vec<_>>()
    );
    assert_eq!(typed.types(), types.as_slice());
}

#[test]
fn from_columns_accepts_typed_variants_with_bitmaps() {
    let types = vec![LogicalType::Int64, LogicalType::Bool];
    let mut validity = Bitmap::all_valid(3);
    validity.clear(1);
    let columns = vec![
        Column::Int64 {
            values: vec![10, 0, 12],
            validity: Some(validity.clone()),
        },
        Column::Bool {
            values: vec![true, false, false],
            validity: Some(validity),
        },
    ];
    let chunk = Chunk::from_columns(types, columns).unwrap();

    assert_eq!(chunk.row_count(), 3);
    assert_eq!(chunk.value(1, 0), Some(Value::Null));
    assert_eq!(chunk.value(1, 1), Some(Value::Null));
    assert_eq!(chunk.value(0, 0), Some(Value::Int64(10)));
    assert_eq!(chunk.value(2, 1), Some(Value::Bool(false)));
}

#[test]
fn from_columns_rejects_arity_mismatch() {
    let error = Chunk::from_columns(
        sample_types(),
        vec![Column::Int64 {
            values: vec![1],
            validity: None,
        }],
    )
    .unwrap_err();
    let DevonError::InvalidArgument { context } = error else {
        panic!("expected InvalidArgument, got {error}");
    };
    assert!(context.contains("expected 4"), "{context}");
    assert!(context.contains("actual 1"), "{context}");
}

#[test]
fn from_columns_rejects_ragged_columns() {
    let error = Chunk::from_columns(
        vec![LogicalType::Int64, LogicalType::Int64],
        vec![
            Column::Int64 {
                values: vec![1, 2, 3],
                validity: None,
            },
            Column::Int64 {
                values: vec![1],
                validity: None,
            },
        ],
    )
    .unwrap_err();
    let DevonError::InvalidArgument { context } = error else {
        panic!("expected InvalidArgument, got {error}");
    };
    assert!(context.contains("column 1"), "{context}");
    assert!(context.contains("has 1 rows, expected 3"), "{context}");
}

#[test]
fn from_columns_rejects_variant_type_mismatch() {
    let error = Chunk::from_columns(
        vec![LogicalType::Int64],
        vec![Column::Float64 {
            values: vec![1.0],
            validity: None,
        }],
    )
    .unwrap_err();
    let DevonError::InvalidArgument { context } = error else {
        panic!("expected InvalidArgument, got {error}");
    };
    assert!(context.contains("column 0"), "{context}");
    assert!(context.contains("Int64"), "{context}");
}

#[test]
fn from_columns_rejects_decimal_scale_mismatch() {
    let error = Chunk::from_columns(
        vec![LogicalType::Decimal {
            precision: 10,
            scale: 3,
        }],
        vec![Column::Decimal {
            values: vec![100],
            scale: 2,
            validity: None,
        }],
    )
    .unwrap_err();
    let DevonError::InvalidArgument { context } = error else {
        panic!("expected InvalidArgument, got {error}");
    };
    assert!(context.contains("column 0"), "{context}");
}

#[test]
fn from_columns_checks_boxed_values_like_push_row() {
    // Same error text `ChunkBuilder::push_row` raises for a mismatched value.
    let error = Chunk::from_columns(
        vec![LogicalType::String],
        vec![Column::Boxed(vec![
            Value::String("ok".into()),
            Value::Int64(5),
        ])],
    )
    .unwrap_err();
    let DevonError::InvalidArgument { context } = error else {
        panic!("expected InvalidArgument, got {error}");
    };
    assert!(
        context.contains("column 0 value 5 does not match expected type"),
        "{context}"
    );
}

#[test]
fn from_columns_enforces_chunk_capacity() {
    let error = Chunk::from_columns(
        vec![LogicalType::Int64],
        vec![Column::Int64 {
            values: (0..=(CHUNK_CAPACITY as i64)).collect(),
            validity: None,
        }],
    )
    .unwrap_err();
    let DevonError::InvalidArgument { context } = error else {
        panic!("expected InvalidArgument, got {error}");
    };
    assert!(context.contains("capacity"), "{context}");
}

#[test]
fn from_columns_accepts_full_capacity_chunk() {
    let chunk = Chunk::from_columns(
        vec![LogicalType::Int64],
        vec![Column::Int64 {
            values: (0..CHUNK_CAPACITY as i64).collect(),
            validity: None,
        }],
    )
    .unwrap();
    assert_eq!(chunk.row_count(), CHUNK_CAPACITY);
}

#[test]
fn from_columns_builds_empty_chunk() {
    let chunk = Chunk::from_columns(
        vec![LogicalType::Int64],
        vec![Column::Int64 {
            values: Vec::new(),
            validity: None,
        }],
    )
    .unwrap();
    assert_eq!(chunk.row_count(), 0);
    assert_eq!(chunk.column_count(), 1);
    assert_eq!(chunk.value(0, 0), None);
}
