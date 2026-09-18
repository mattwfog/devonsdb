use devondb_storage::{
    node_group::{NODE_GROUP_CAPACITY, NodeGroup},
    pager::Pager,
    superblock::{COLUMN_ENCODINGS_FLAG, ZONE_MAPS_FLAG},
};
use devondb_types::{
    DevonError,
    logical_type::LogicalType,
    schema::{Column, NodeTableSchema},
    value::Value,
};
use proptest::{collection, prelude::*};
use tempfile::tempdir;

const PAGE_SIZE: u32 = 4096;
const DB_ID: [u8; 16] = *b"vector-props-db!";

fn finite_f32() -> impl Strategy<Value = f32> {
    any::<u32>()
        .prop_filter("f32 bit pattern must be finite", |bits| {
            f32::from_bits(*bits).is_finite()
        })
        .prop_map(f32::from_bits)
}

fn vector_rows(elements: Vec<f32>, dim: usize) -> Vec<Vec<Value>> {
    elements
        .chunks_exact(dim)
        .map(|vector| vec![Value::Vector(vector.to_vec())])
        .collect()
}

fn mixed_rows(elements: Vec<f32>, dim: usize) -> Vec<Vec<Value>> {
    elements
        .chunks_exact(dim)
        .enumerate()
        .map(|(row, vector)| {
            vec![
                Value::Bool(row % 2 == 0),
                Value::Vector(vector.to_vec()),
                Value::Int64(-(row as i64)),
                Value::String(format!("row-{row}")),
                Value::Float64(if row == 0 { -0.0 } else { row as f64 / 3.0 }),
            ]
        })
        .collect()
}

fn mixed_types(dim: usize) -> Vec<LogicalType> {
    vec![
        LogicalType::Bool,
        LogicalType::Vector { dim: dim as u32 },
        LogicalType::Int64,
        LogicalType::String,
        LogicalType::Float64,
    ]
}

/// Publishes `ZONE_MAPS` the way a real checkpoint's catalog save does.
///
/// Fixtures here write node groups straight through the pager and never
/// publish a catalog, so `Catalog::save` — the only code that claims the bit —
/// never runs. Since SCALE S-1 those groups carry a `ZONE_MAP_STATS` directory
/// section, and a set section bit whose governing feature bit is clear is
/// corruption by format law (`docs/FORMAT.md` § node groups). Without this the
/// fixture writes a file the reader must reject.
fn publish_zone_maps(pager: &Pager) {
    let mut superblock = pager.superblock();
    superblock.feature_flags |= ZONE_MAPS_FLAG | COLUMN_ENCODINGS_FLAG;
    // The pager enforces checkpoint-LSN monotonicity on every commit.
    superblock.checkpoint_lsn += 1;
    pager.commit_superblock(superblock).unwrap();
}

fn assert_round_trip(types: &[LogicalType], expected: &[Vec<Value>]) {
    assert!(!expected.is_empty());
    let directory = tempdir().unwrap();
    let path = directory.path().join("vectors.devondb");
    let pager = Pager::create(&path, PAGE_SIZE, DB_ID).unwrap();
    let mut directory_pages = Vec::new();

    for rows in expected.chunks(NODE_GROUP_CAPACITY) {
        let mut group = NodeGroup::new(types.to_vec()).unwrap();
        for row in rows {
            group.push_row(row.clone()).unwrap();
        }
        directory_pages.push(group.write(&pager).unwrap());
    }
    publish_zone_maps(&pager);
    drop(pager);

    let pager = Pager::open(path).unwrap();
    let mut actual = Vec::with_capacity(expected.len());
    for directory_page in directory_pages {
        let group = NodeGroup::read(&pager, directory_page, types).unwrap();
        for row in 0..group.row_count() {
            actual.push(
                (0..group.column_count())
                    .map(|column| group.value(row, column).unwrap().clone())
                    .collect::<Vec<_>>(),
            );
        }
    }

    assert_rows_bitwise_equal(expected, &actual);
}

fn assert_rows_bitwise_equal(expected: &[Vec<Value>], actual: &[Vec<Value>]) {
    assert_eq!(actual.len(), expected.len());
    for (row, (expected_row, actual_row)) in expected.iter().zip(actual).enumerate() {
        assert_eq!(actual_row.len(), expected_row.len(), "row {row}");
        for (column, (expected_value, actual_value)) in
            expected_row.iter().zip(actual_row).enumerate()
        {
            match (expected_value, actual_value) {
                (Value::Vector(expected), Value::Vector(actual)) => {
                    assert_eq!(actual.len(), expected.len(), "row {row}, column {column}");
                    for (element, (expected, actual)) in expected.iter().zip(actual).enumerate() {
                        assert_eq!(
                            actual.to_bits(),
                            expected.to_bits(),
                            "row {row}, column {column}, vector element {element}"
                        );
                    }
                }
                (Value::Float64(expected), Value::Float64(actual)) => assert_eq!(
                    actual.to_bits(),
                    expected.to_bits(),
                    "row {row}, column {column}"
                ),
                _ => assert_eq!(actual_value, expected_value, "row {row}, column {column}"),
            }
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 24,
        failure_persistence: None,
        ..ProptestConfig::default()
    })]

    #[test]
    fn dimension_one_vectors_round_trip_bit_exact(
        elements in collection::vec(finite_f32(), 1..=96),
    ) {
        let rows = vector_rows(elements, 1);
        assert_round_trip(&[LogicalType::Vector { dim: 1 }], &rows);
    }

    #[test]
    fn arbitrary_dimensions_round_trip_bit_exact(
        (dim, elements) in (2usize..=64, 1usize..=24).prop_flat_map(|(dim, row_count)| {
            collection::vec(finite_f32(), dim * row_count)
                .prop_map(move |elements| (dim, elements))
        }),
    ) {
        let rows = vector_rows(elements, dim);
        assert_round_trip(&[LogicalType::Vector { dim: dim as u32 }], &rows);
    }

    #[test]
    fn vector_rows_cross_a_node_group_boundary(
        elements in collection::vec(
            finite_f32(),
            (NODE_GROUP_CAPACITY + 1)..=(NODE_GROUP_CAPACITY + 32),
        ),
    ) {
        let rows = vector_rows(elements, 1);
        assert_round_trip(&[LogicalType::Vector { dim: 1 }], &rows);
    }

    #[test]
    fn vectors_round_trip_interleaved_with_other_column_types(
        (dim, elements) in (1usize..=32, 1usize..=32).prop_flat_map(|(dim, row_count)| {
            collection::vec(finite_f32(), dim * row_count)
                .prop_map(move |elements| (dim, elements))
        }),
    ) {
        let rows = mixed_rows(elements, dim);
        assert_round_trip(&mixed_types(dim), &rows);
    }

    #[test]
    fn large_vectors_round_trip_bit_exact(
        (dim, elements) in (2048usize..=2064, 1usize..=4).prop_flat_map(|(dim, row_count)| {
            collection::vec(finite_f32(), dim * row_count)
                .prop_map(move |elements| (dim, elements))
        }),
    ) {
        let rows = vector_rows(elements, dim);
        assert_round_trip(&[LogicalType::Vector { dim: dim as u32 }], &rows);
    }

    #[test]
    fn nullable_vectors_round_trip_bit_exact(
        vectors in collection::vec(
            prop::option::of(collection::vec(finite_f32(), 8)),
            1..=64,
        ),
    ) {
        let rows = vectors
            .into_iter()
            .map(|vector| vec![vector.map_or(Value::Null, Value::Vector)])
            .collect::<Vec<_>>();
        assert_round_trip(&[LogicalType::Vector { dim: 8 }], &rows);
    }

    #[test]
    fn mismatched_vector_dimensions_are_invalid_arguments(
        (declared_dim, vector) in (0usize..=64, any::<bool>()).prop_flat_map(
            |(declared_dim, use_shorter)| {
                let actual_dim = if use_shorter && declared_dim > 0 {
                    declared_dim - 1
                } else {
                    declared_dim + 1
                };
                collection::vec(finite_f32(), actual_dim)
                    .prop_map(move |vector| (declared_dim, vector))
            },
        ),
    ) {
        let mut group = NodeGroup::new(vec![LogicalType::Vector {
            dim: declared_dim as u32,
        }])
        .unwrap();

        let error = group.push_row(vec![Value::Vector(vector)]).unwrap_err();

        assert!(
            matches!(error, DevonError::InvalidArgument { .. }),
            "expected InvalidArgument, got {error}"
        );
        prop_assert_eq!(group.row_count(), 0);
    }
}

#[test]
fn schema_permitted_zero_dimension_vectors_round_trip() {
    let schema = NodeTableSchema::new(
        "ZeroVector".to_owned(),
        vec![
            Column {
                name: "id".to_owned(),
                ty: LogicalType::Int64,
                primary_key: true,
            },
            Column {
                name: "embedding".to_owned(),
                ty: LogicalType::Vector { dim: 0 },
                primary_key: false,
            },
        ],
    )
    .unwrap();
    let types = schema
        .columns()
        .iter()
        .map(|column| column.ty)
        .collect::<Vec<_>>();
    let rows = vec![
        vec![Value::Int64(1), Value::Vector(Vec::new())],
        vec![Value::Int64(2), Value::Vector(Vec::new())],
    ];

    assert_round_trip(&types, &rows);
}
