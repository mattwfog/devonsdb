//! Typed-chunk boundary tests (`docs/SCALE.md` §6).
//!
//! Every data set here is fixed and deterministic — no randomness anywhere.

use devondb_exec::chunk::{CHUNK_CAPACITY, Chunk, ChunkBuilder};
use devondb_exec::column::Column;
use devondb_types::logical_type::{B1Rescore, LogicalType, VectorEncoding};
use devondb_types::value::Value;
use devondb_types::{Decimal128, GeoPoint};

fn decimal(digits: i128, scale: u8) -> Value {
    Value::Decimal(Decimal128::new(digits, scale).unwrap())
}

fn geo() -> Value {
    Value::GeoPoint(GeoPoint::from_canonical(45.5, -122.625).unwrap())
}

/// `value_at ∘ from_values` is identity for every value the type admits,
/// including NULL (`docs/SCALE.md` §6.4).
#[test]
fn value_at_round_trips_every_admitted_value_including_null() {
    let cases: Vec<(LogicalType, Vec<Value>)> = vec![
        (
            LogicalType::Int64,
            vec![
                Value::Int64(0),
                Value::Null,
                Value::Int64(i64::MIN),
                Value::Int64(i64::MAX),
            ],
        ),
        (
            LogicalType::Float64,
            vec![
                Value::Float64(0.0),
                Value::Null,
                Value::Float64(-0.0),
                Value::Float64(f64::MAX),
                Value::Float64(f64::MIN_POSITIVE),
            ],
        ),
        (
            LogicalType::Bool,
            vec![Value::Bool(true), Value::Null, Value::Bool(false)],
        ),
        (
            LogicalType::Timestamp,
            vec![
                Value::Timestamp(0),
                Value::Null,
                Value::Timestamp(-1_000_000),
                Value::Timestamp(1_723_161_600_123_456),
            ],
        ),
        (
            LogicalType::Decimal {
                precision: 10,
                scale: 2,
            },
            vec![
                decimal(0, 2),
                Value::Null,
                decimal(-250, 2),
                decimal(1999, 2),
            ],
        ),
        (
            LogicalType::String,
            vec![
                Value::String("edge".into()),
                Value::Null,
                Value::String(String::new()),
            ],
        ),
        (
            LogicalType::Bytes,
            vec![Value::Bytes(vec![0x00, 0xff]), Value::Null],
        ),
        (
            LogicalType::Json,
            vec![Value::Json("{\"k\":1}".into()), Value::Null],
        ),
        (
            LogicalType::Vector { dim: 2 },
            vec![Value::Vector(vec![0.1, 0.2]), Value::Null],
        ),
        (
            LogicalType::VectorEncoded {
                dim: 2,
                encoding: VectorEncoding::F16,
            },
            vec![Value::Vector(vec![0.1, 0.2]), Value::Null],
        ),
        (LogicalType::GeoPoint, vec![geo(), Value::Null]),
    ];
    for (ty, values) in cases {
        let column = Column::from_values(&ty, values.clone());
        assert_eq!(column.len(), values.len(), "row count must survive {ty}");
        for (row, expected) in values.iter().enumerate() {
            assert_eq!(
                column.value_at(row),
                *expected,
                "{ty} row {row} must round-trip"
            );
        }
    }
}

/// §6.3 pin: NULL slots hold ZERO in the typed vector, and the validity
/// bitmap clears exactly those slots; an all-valid column carries no bitmap.
#[test]
fn null_slots_hold_zero_in_typed_vectors() {
    let column = Column::from_values(
        &LogicalType::Int64,
        vec![Value::Int64(7), Value::Null, Value::Int64(9)],
    );
    let Column::Int64 { values, validity } = &column else {
        panic!("Int64 values must produce a typed column: {column:?}");
    };
    assert_eq!(values, &[7, 0, 9], "the NULL slot must hold zero");
    let validity = validity.as_ref().expect("a NULL row forces a bitmap");
    assert!(validity.is_valid(0));
    assert!(!validity.is_valid(1));
    assert!(validity.is_valid(2));

    let decimal_column = Column::from_values(
        &LogicalType::Decimal {
            precision: 10,
            scale: 2,
        },
        vec![Value::Null, decimal(100, 2)],
    );
    let Column::Decimal {
        values, validity, ..
    } = &decimal_column
    else {
        panic!("Decimal values must produce a typed column: {decimal_column:?}");
    };
    assert_eq!(values, &[0, 100], "the NULL slot must hold zero digits");
    assert!(!validity.as_ref().expect("bitmap").is_valid(0));

    let all_valid = Column::from_values(
        &LogicalType::Timestamp,
        vec![Value::Timestamp(1), Value::Timestamp(2)],
    );
    let Column::Timestamp { validity, .. } = &all_valid else {
        panic!("Timestamp values must produce a typed column: {all_valid:?}");
    };
    assert!(validity.is_none(), "no NULLs means no bitmap (§6.3)");
}

/// `Chunk` equality is unchanged: two chunks built from the same rows are
/// equal; differing in one NULL are not.
#[test]
fn chunk_equality_semantics_unchanged() {
    fn build(second_row_int: Value) -> Chunk {
        let mut builder = ChunkBuilder::new(vec![LogicalType::Int64, LogicalType::String]);
        builder
            .push_row(vec![Value::Int64(1), Value::String("a".into())])
            .unwrap();
        builder.push_row(vec![second_row_int, Value::Null]).unwrap();
        builder.finish()
    }

    assert_eq!(build(Value::Int64(2)), build(Value::Int64(2)));
    assert_ne!(
        build(Value::Int64(2)),
        build(Value::Null),
        "one differing NULL must make chunks unequal"
    );
    assert_ne!(
        build(Value::Int64(2)),
        build(Value::Int64(3)),
        "one differing value must make chunks unequal"
    );
}

/// Bitmap word math at word boundaries: rows 0, 63, 64, 127, 2047 of a full
/// 2048-row chunk (docs/SCALE.md §6.3).
#[test]
fn bitmap_word_math_at_boundaries() {
    const NULL_ROWS: [usize; 5] = [0, 63, 64, 127, CHUNK_CAPACITY - 1];
    let mut values = vec![Value::Int64(1); CHUNK_CAPACITY];
    for row in NULL_ROWS {
        values[row] = Value::Null;
    }
    let column = Column::from_values(&LogicalType::Int64, values);
    let Column::Int64 { validity, .. } = &column else {
        panic!("Int64 values must produce a typed column: {column:?}");
    };
    let bitmap = validity.as_ref().expect("NULL rows force a bitmap");

    assert_eq!(bitmap.len(), CHUNK_CAPACITY);
    assert_eq!(bitmap.as_words().len(), 32, "2048 rows = 32 words");
    for row in 0..CHUNK_CAPACITY {
        assert_eq!(
            bitmap.is_valid(row),
            !NULL_ROWS.contains(&row),
            "row {row} validity"
        );
    }
    // Word-level pins: word 0 loses bits 0 and 63; word 1 loses bits 0 and
    // 63 (rows 64 and 127); word 31 loses bit 63 (row 2047).
    assert_eq!(bitmap.as_words()[0], u64::MAX ^ 1 ^ (1 << 63));
    assert_eq!(bitmap.as_words()[1], u64::MAX ^ 1 ^ (1 << 63));
    assert_eq!(bitmap.as_words()[2], u64::MAX);
    assert_eq!(bitmap.as_words()[31], u64::MAX ^ (1 << 63));
}

/// §6.6 charge arithmetic: fixed-width columns charge `len × width` plus
/// bitmap words; `Boxed` charges exactly the per-value sum.
#[test]
fn charge_arithmetic_matches_the_budget_law() {
    let values = vec![Value::Int64(1); CHUNK_CAPACITY];
    let column = Column::from_values(&LogicalType::Int64, values);
    assert_eq!(
        column.approx_bytes(),
        CHUNK_CAPACITY * 8,
        "an all-Int64 2048-row column with no NULLs charges len × 8 (+0 bitmap)"
    );

    let mut values = vec![Value::Int64(1); CHUNK_CAPACITY];
    values[0] = Value::Null;
    let column = Column::from_values(&LogicalType::Int64, values);
    assert_eq!(
        column.approx_bytes(),
        CHUNK_CAPACITY * 8 + 32 * 8,
        "one NULL adds the full 32-word bitmap"
    );

    let mut values = vec![Value::Bool(true); CHUNK_CAPACITY];
    values[64] = Value::Null;
    let column = Column::from_values(&LogicalType::Bool, values);
    assert_eq!(column.approx_bytes(), CHUNK_CAPACITY + 32 * 8);

    // A chunk of strings charges Σ Value::approx_bytes =
    // 2048 × (32 + 4) = 73_728 for four-byte strings (MVCC.md §7.2
    // constants). The Boxed charge must match exactly.
    let column = Column::from_values(
        &LogicalType::String,
        vec![Value::String("edge".into()); CHUNK_CAPACITY],
    );
    assert_eq!(column.approx_bytes(), 73_728);
}

/// Every `LogicalType` variant maps to exactly one `Column` variant — the
/// exhaustive classification table (no wildcard arm in `from_values`).
#[test]
fn logical_type_classification_is_exhaustive() {
    let cases: Vec<(LogicalType, Value, &str)> = vec![
        (LogicalType::Bool, Value::Bool(true), "Bool"),
        (LogicalType::Int64, Value::Int64(1), "Int64"),
        (LogicalType::Float64, Value::Float64(1.0), "Float64"),
        (LogicalType::Timestamp, Value::Timestamp(1), "Timestamp"),
        (
            LogicalType::Decimal {
                precision: 10,
                scale: 2,
            },
            decimal(100, 2),
            "Decimal",
        ),
        (LogicalType::String, Value::String("a".into()), "Boxed"),
        (LogicalType::Bytes, Value::Bytes(vec![1]), "Boxed"),
        (LogicalType::Json, Value::Json("{}".into()), "Boxed"),
        (
            LogicalType::Vector { dim: 2 },
            Value::Vector(vec![0.1, 0.2]),
            "Boxed",
        ),
        (
            LogicalType::VectorEncoded {
                dim: 2,
                encoding: VectorEncoding::B1 {
                    rotation_seed: 7,
                    rescore: B1Rescore::None,
                },
            },
            Value::Vector(vec![0.1, 0.2]),
            "Boxed",
        ),
        (LogicalType::GeoPoint, geo(), "Boxed"),
    ];
    assert_eq!(
        cases.len(),
        11,
        "the table must name every LogicalType variant exactly once"
    );
    for (ty, value, expected) in cases {
        let column = Column::from_values(&ty, vec![value]);
        let variant = match &column {
            Column::Int64 { .. } => "Int64",
            Column::Float64 { .. } => "Float64",
            Column::Bool { .. } => "Bool",
            Column::Timestamp { .. } => "Timestamp",
            Column::Decimal { .. } => "Decimal",
            Column::Boxed(_) => "Boxed",
        };
        assert_eq!(variant, expected, "LogicalType {ty} classification");
    }
}

/// The builder bridge: chunks built row-at-a-time through `ChunkBuilder`
/// use typed storage and read back through the standard accessors.
#[test]
fn builder_bridges_rows_into_typed_columns() {
    let mut builder = ChunkBuilder::new(vec![
        LogicalType::Int64,
        LogicalType::Decimal {
            precision: 10,
            scale: 2,
        },
        LogicalType::String,
    ]);
    builder
        .push_row(vec![Value::Int64(1), decimal(1999, 2), Value::Null])
        .unwrap();
    builder
        .push_row(vec![Value::Null, Value::Null, Value::String("x".into())])
        .unwrap();
    let chunk = builder.finish();

    let Column::Int64 { values, validity } = chunk.column(0).unwrap() else {
        panic!("column 0 must be typed Int64");
    };
    assert_eq!(values, &[1, 0]);
    assert!(!validity.as_ref().expect("bitmap").is_valid(1));

    let Column::Decimal {
        values,
        scale,
        validity,
    } = chunk.column(1).unwrap()
    else {
        panic!("column 1 must be typed Decimal");
    };
    assert_eq!(values, &[1999, 0]);
    assert_eq!(*scale, 2);
    assert!(!validity.as_ref().expect("bitmap").is_valid(1));

    assert!(matches!(chunk.column(2), Some(Column::Boxed(_))));
    assert_eq!(
        chunk.rows().collect::<Vec<_>>(),
        vec![
            vec![Value::Int64(1), decimal(1999, 2), Value::Null],
            vec![Value::Null, Value::Null, Value::String("x".into())],
        ]
    );
    assert_eq!(chunk.value(1, 0), Some(Value::Null));
    assert_eq!(chunk.value(2, 0), None, "out-of-bounds row reads None");
}

/// `value_ref_boxed` lends a stored value only for the `Boxed` variant.
#[test]
fn value_ref_boxed_is_boxed_only() {
    let boxed = Column::from_values(
        &LogicalType::String,
        vec![Value::String("a".into()), Value::Null],
    );
    assert_eq!(boxed.value_ref_boxed(0), Some(&Value::String("a".into())));
    assert_eq!(boxed.value_ref_boxed(1), Some(&Value::Null));
    assert_eq!(boxed.value_ref_boxed(2), None);

    let typed = Column::from_values(&LogicalType::Int64, vec![Value::Int64(1)]);
    assert_eq!(typed.value_ref_boxed(0), None);
}
