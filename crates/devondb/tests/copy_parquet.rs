//! `copy <table> from "<file>.parquet"` end-to-end (`docs/SCALE.md` §5).
//! Fixtures are generated in-test with the `parquet` crate's
//! own writer — nothing is checked in. The magic (`PAR1`), not the
//! extension, selects the Parquet path.

use std::{
    env, fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use devondb::{Database, Plan, QueryResult, Statement};
use devondb_plan::text::parser::{Parsed, parse};
use devondb_types::{logical_type::LogicalType, schema::Column};

const PAGE_SIZE: u32 = 4096;
static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct TestDirectory {
    path: PathBuf,
}

impl TestDirectory {
    fn new() -> Self {
        let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = env::temp_dir().join(format!(
            "devondb-copy-parquet-test-{}-{timestamp}-{sequence}",
            std::process::id()
        ));
        fs::create_dir(&path).unwrap();
        Self { path }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn column(name: &str, ty: LogicalType, primary_key: bool) -> Column {
    Column {
        name: name.to_owned(),
        ty,
        primary_key,
    }
}

fn statement(text: &str) -> Statement {
    match parse(text).unwrap() {
        Parsed::Statement(statement) => statement.stmt,
        Parsed::Query(_) => panic!("expected statement: {text}"),
    }
}

fn plan(text: &str) -> Plan {
    match parse(text).unwrap() {
        Parsed::Query(plan) => plan,
        Parsed::Statement(_) => panic!("expected query: {text}"),
    }
}

fn copy_statement(table: &str, path: &Path, sort_by: Option<&str>) -> Statement {
    let path = path
        .to_string_lossy()
        .replace('\\', "\\\\")
        .replace('"', "\\\"");
    let sort = sort_by.map_or_else(String::new, |column| format!(" sort by {column}"));
    statement(&format!("copy {table} from \"{path}\"{sort}"))
}

fn scan(database: &mut Database, table: &str) -> QueryResult {
    database
        .run(&plan(&format!("nodes({table}) as t")))
        .unwrap()
}

/// Without the `parquet` feature a `PAR1` file is refused by name — never
/// misread as CSV.
#[cfg(not(feature = "parquet"))]
#[test]
fn parquet_magic_without_the_feature_is_refused() {
    let directory = TestDirectory::new();
    let file = directory.path("data.parquet");
    fs::write(&file, b"PAR1\x0f\x00\x00\x00").unwrap();
    let mut database = Database::create(directory.path("db.devondb"), PAGE_SIZE).unwrap();
    database
        .execute(&Statement::CreateNodeTable {
            name: "Person".to_owned(),
            columns: vec![
                column("id", LogicalType::Int64, true),
                column("name", LogicalType::String, false),
            ],
        })
        .unwrap();

    let error = database
        .execute(&copy_statement("Person", &file, None))
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "invalid argument: Parquet COPY requires the `parquet` feature"
    );
    assert!(scan(&mut database, "Person").rows.is_empty());
}

#[cfg(feature = "parquet")]
mod parquet_enabled {
    use std::sync::Arc;

    use devondb::{DevonError, Options};
    use devondb_types::{Decimal128, GeoPoint, value::Value};
    use parquet::{
        basic::{
            ConvertedType, LogicalType as ParquetLogical, Repetition, TimeUnit, Type as Physical,
        },
        data_type::{
            BoolType, ByteArray, ByteArrayType, DoubleType, FloatType, Int32Type, Int64Type,
        },
        file::{
            properties::WriterProperties,
            writer::{SerializedColumnWriter, SerializedFileWriter, SerializedRowGroupWriter},
        },
        schema::types::{Type as ParquetType, TypePtr},
    };

    use super::*;

    /// One top-level Parquet column: its schema field plus per-row data.
    struct ParquetColumn {
        field: TypePtr,
        data: ColumnData,
    }

    enum ColumnData {
        Bool(Vec<Option<bool>>),
        Int32(Vec<Option<i32>>),
        Int64(Vec<Option<i64>>),
        Float(Vec<Option<f32>>),
        Double(Vec<Option<f64>>),
        Bytes(Vec<Option<Vec<u8>>>),
        ListFloat(Vec<Option<Vec<f32>>>),
        ListDouble(Vec<Option<Vec<f64>>>),
        Geo(Vec<Option<(f64, f64)>>),
    }

    impl ColumnData {
        fn len(&self) -> usize {
            match self {
                Self::Bool(values) => values.len(),
                Self::Int32(values) => values.len(),
                Self::Int64(values) => values.len(),
                Self::Float(values) => values.len(),
                Self::Double(values) => values.len(),
                Self::Bytes(values) => values.len(),
                Self::ListFloat(values) => values.len(),
                Self::ListDouble(values) => values.len(),
                Self::Geo(values) => values.len(),
            }
        }
    }

    fn primitive(
        name: &str,
        physical: Physical,
    ) -> parquet::schema::types::PrimitiveTypeBuilder<'_> {
        ParquetType::primitive_type_builder(name, physical).with_repetition(Repetition::OPTIONAL)
    }

    fn utf8_field(name: &str) -> TypePtr {
        Arc::new(
            primitive(name, Physical::BYTE_ARRAY)
                .with_logical_type(Some(ParquetLogical::String))
                .with_converted_type(ConvertedType::UTF8)
                .build()
                .unwrap(),
        )
    }

    fn json_field(name: &str) -> TypePtr {
        Arc::new(
            primitive(name, Physical::BYTE_ARRAY)
                .with_logical_type(Some(ParquetLogical::Json))
                .with_converted_type(ConvertedType::JSON)
                .build()
                .unwrap(),
        )
    }

    fn timestamp_field(name: &str, unit: TimeUnit) -> TypePtr {
        let builder = primitive(name, Physical::INT64).with_logical_type(Some(
            ParquetLogical::Timestamp(parquet::basic::TimestampType {
                is_adjusted_to_u_t_c: true,
                unit,
            }),
        ));
        // Nanos have no legacy converted-type spelling.
        let builder = match unit {
            TimeUnit::MILLIS => builder.with_converted_type(ConvertedType::TIMESTAMP_MILLIS),
            TimeUnit::MICROS => builder.with_converted_type(ConvertedType::TIMESTAMP_MICROS),
            TimeUnit::NANOS => builder,
        };
        Arc::new(builder.build().unwrap())
    }

    fn decimal_i64_field(name: &str, precision: i32, scale: i32) -> TypePtr {
        Arc::new(
            primitive(name, Physical::INT64)
                .with_logical_type(Some(ParquetLogical::Decimal(parquet::basic::DecimalType {
                    scale,
                    precision,
                })))
                .with_converted_type(ConvertedType::DECIMAL)
                .with_precision(precision)
                .with_scale(scale)
                .build()
                .unwrap(),
        )
    }

    /// Standard three-level list: `group name (LIST) { repeated group list
    /// { optional <element> element } }`.
    fn list_field(name: &str, element: Physical) -> TypePtr {
        let element = Arc::new(primitive("element", element).build().unwrap());
        let list = Arc::new(
            ParquetType::group_type_builder("list")
                .with_repetition(Repetition::REPEATED)
                .with_fields(vec![element])
                .build()
                .unwrap(),
        );
        Arc::new(
            ParquetType::group_type_builder(name)
                .with_repetition(Repetition::OPTIONAL)
                .with_logical_type(Some(ParquetLogical::List))
                .with_converted_type(ConvertedType::LIST)
                .with_fields(vec![list])
                .build()
                .unwrap(),
        )
    }

    fn geo_field(name: &str) -> TypePtr {
        let component = |name: &str| {
            Arc::new(
                ParquetType::primitive_type_builder(name, Physical::DOUBLE)
                    .with_repetition(Repetition::REQUIRED)
                    .build()
                    .unwrap(),
            )
        };
        Arc::new(
            ParquetType::group_type_builder(name)
                .with_repetition(Repetition::OPTIONAL)
                .with_fields(vec![component("lat_deg"), component("lng_deg")])
                .build()
                .unwrap(),
        )
    }

    /// Writes `columns` to `path`, one row group per `group_rows` rows.
    fn write_parquet(path: &Path, columns: Vec<ParquetColumn>, group_rows: usize) {
        let row_count = columns.first().map_or(0, |column| column.data.len());
        assert!(row_count > 0, "fixtures always carry at least one row");
        assert!(
            columns.iter().all(|column| column.data.len() == row_count),
            "fixture columns disagree on row count"
        );
        let schema = Arc::new(
            ParquetType::group_type_builder("schema")
                .with_fields(columns.iter().map(|column| column.field.clone()).collect())
                .build()
                .unwrap(),
        );
        let file = fs::File::create(path).unwrap();
        let props = Arc::new(WriterProperties::builder().build());
        let mut writer = SerializedFileWriter::new(file, schema, props).unwrap();
        let mut start = 0;
        while start < row_count {
            let end = (start + group_rows).min(row_count);
            let mut row_group = writer.next_row_group().unwrap();
            for column in &columns {
                write_column(&mut row_group, &column.data, start, end);
            }
            row_group.close().unwrap();
            start = end;
        }
        writer.close().unwrap();
    }

    fn write_column(
        row_group: &mut SerializedRowGroupWriter<'_, fs::File>,
        data: &ColumnData,
        start: usize,
        end: usize,
    ) {
        match data {
            ColumnData::Bool(values) => {
                write_optional::<BoolType>(row_group, &values[start..end]);
            }
            ColumnData::Int32(values) => {
                write_optional::<Int32Type>(row_group, &values[start..end]);
            }
            ColumnData::Int64(values) => {
                write_optional::<Int64Type>(row_group, &values[start..end]);
            }
            ColumnData::Float(values) => {
                write_optional::<FloatType>(row_group, &values[start..end]);
            }
            ColumnData::Double(values) => {
                write_optional::<DoubleType>(row_group, &values[start..end]);
            }
            ColumnData::Bytes(values) => {
                let def: Vec<i16> = values[start..end]
                    .iter()
                    .map(|value| i16::from(value.is_some()))
                    .collect();
                let flat: Vec<ByteArray> = values[start..end]
                    .iter()
                    .flatten()
                    .map(|value| ByteArray::from(value.clone()))
                    .collect();
                let mut column = next_column(row_group);
                column
                    .typed::<ByteArrayType>()
                    .write_batch(&flat, Some(&def), None)
                    .unwrap();
                column.close().unwrap();
            }
            ColumnData::ListFloat(lists) => {
                let flat: Vec<f32> = list_values(lists, start, end);
                let (def, rep) = list_levels(lists, start, end);
                let mut column = next_column(row_group);
                column
                    .typed::<FloatType>()
                    .write_batch(&flat, Some(&def), Some(&rep))
                    .unwrap();
                column.close().unwrap();
            }
            ColumnData::ListDouble(lists) => {
                let flat: Vec<f64> = list_values(lists, start, end);
                let (def, rep) = list_levels(lists, start, end);
                let mut column = next_column(row_group);
                column
                    .typed::<DoubleType>()
                    .write_batch(&flat, Some(&def), Some(&rep))
                    .unwrap();
                column.close().unwrap();
            }
            ColumnData::Geo(points) => {
                for component in 0..2 {
                    let def: Vec<i16> = points[start..end]
                        .iter()
                        .map(|point| i16::from(point.is_some()))
                        .collect();
                    let flat: Vec<f64> = points[start..end]
                        .iter()
                        .flatten()
                        .map(|point| if component == 0 { point.0 } else { point.1 })
                        .collect();
                    let mut column = next_column(row_group);
                    column
                        .typed::<DoubleType>()
                        .write_batch(&flat, Some(&def), None)
                        .unwrap();
                    column.close().unwrap();
                }
            }
        }
    }

    fn next_column<'w>(
        row_group: &'w mut SerializedRowGroupWriter<'_, fs::File>,
    ) -> SerializedColumnWriter<'w> {
        row_group.next_column().unwrap().unwrap()
    }

    fn write_optional<T: parquet::data_type::DataType>(
        row_group: &mut SerializedRowGroupWriter<'_, fs::File>,
        values: &[Option<T::T>],
    ) where
        T::T: Clone,
    {
        let def: Vec<i16> = values
            .iter()
            .map(|value| i16::from(value.is_some()))
            .collect();
        let flat: Vec<T::T> = values.iter().flatten().cloned().collect();
        let mut column = next_column(row_group);
        column
            .typed::<T>()
            .write_batch(&flat, Some(&def), None)
            .unwrap();
        column.close().unwrap();
    }

    fn list_values<T: Copy>(lists: &[Option<Vec<T>>], start: usize, end: usize) -> Vec<T> {
        lists[start..end]
            .iter()
            .flatten()
            .flatten()
            .copied()
            .collect()
    }

    /// Definition/repetition levels for the three-level list: def 0 = null
    /// list, 2 = empty list, 3 = element present; rep 0 starts a list.
    fn list_levels<T>(lists: &[Option<Vec<T>>], start: usize, end: usize) -> (Vec<i16>, Vec<i16>) {
        let mut def = Vec::new();
        let mut rep = Vec::new();
        for list in &lists[start..end] {
            match list {
                None => {
                    def.push(0);
                    rep.push(0);
                }
                Some(items) if items.is_empty() => {
                    def.push(2);
                    rep.push(0);
                }
                Some(items) => {
                    for (index, _) in items.iter().enumerate() {
                        def.push(3);
                        rep.push(if index == 0 { 0 } else { 1 });
                    }
                }
            }
        }
        (def, rep)
    }

    fn all_types_columns() -> (Vec<Column>, Vec<ParquetColumn>) {
        let schema = vec![
            column("id", LogicalType::Int64, true),
            column("small", LogicalType::Int64, false),
            column("ratio", LogicalType::Float64, false),
            column("wide", LogicalType::Float64, false),
            column("active", LogicalType::Bool, false),
            column("name", LogicalType::String, false),
            column("payload", LogicalType::Bytes, false),
            column("at_micros", LogicalType::Timestamp, false),
            column("at_millis", LogicalType::Timestamp, false),
            column("at_nanos", LogicalType::Timestamp, false),
            column(
                "amount",
                LogicalType::Decimal {
                    precision: 10,
                    scale: 2,
                },
                false,
            ),
            column("embedding", LogicalType::Vector { dim: 2 }, false),
            column("embedding64", LogicalType::Vector { dim: 2 }, false),
            column("doc", LogicalType::Json, false),
            column("place", LogicalType::GeoPoint, false),
        ];
        let parquet = vec![
            ParquetColumn {
                field: Arc::new(primitive("id", Physical::INT64).build().unwrap()),
                data: ColumnData::Int64(vec![Some(1), Some(2), Some(3)]),
            },
            ParquetColumn {
                field: Arc::new(primitive("small", Physical::INT32).build().unwrap()),
                data: ColumnData::Int32(vec![Some(-7), None, Some(42)]),
            },
            ParquetColumn {
                field: Arc::new(primitive("ratio", Physical::DOUBLE).build().unwrap()),
                data: ColumnData::Double(vec![Some(1.5), None, Some(-0.0)]),
            },
            ParquetColumn {
                field: Arc::new(primitive("wide", Physical::FLOAT).build().unwrap()),
                data: ColumnData::Float(vec![Some(0.25), None, Some(-2.5)]),
            },
            ParquetColumn {
                field: Arc::new(primitive("active", Physical::BOOLEAN).build().unwrap()),
                data: ColumnData::Bool(vec![Some(true), None, Some(false)]),
            },
            ParquetColumn {
                field: utf8_field("name"),
                data: ColumnData::Bytes(vec![
                    Some(b"Ada".to_vec()),
                    None,
                    Some(String::new().into_bytes()),
                ]),
            },
            ParquetColumn {
                field: Arc::new(primitive("payload", Physical::BYTE_ARRAY).build().unwrap()),
                data: ColumnData::Bytes(vec![Some(vec![0x00, 0xff]), None, Some(vec![0x1a])]),
            },
            ParquetColumn {
                field: timestamp_field("at_micros", TimeUnit::MICROS),
                data: ColumnData::Int64(vec![Some(1_723_161_600_123_456), None, Some(0)]),
            },
            ParquetColumn {
                field: timestamp_field("at_millis", TimeUnit::MILLIS),
                data: ColumnData::Int64(vec![Some(1_723_161_600_123), None, Some(-1)]),
            },
            ParquetColumn {
                field: timestamp_field("at_nanos", TimeUnit::NANOS),
                data: ColumnData::Int64(vec![Some(1_723_161_600_123_456_000), None, Some(1_000)]),
            },
            ParquetColumn {
                field: decimal_i64_field("amount", 10, 2),
                data: ColumnData::Int64(vec![Some(1999), None, Some(-5)]),
            },
            ParquetColumn {
                field: list_field("embedding", Physical::FLOAT),
                data: ColumnData::ListFloat(vec![Some(vec![0.1, 0.2]), None, Some(vec![3.0, 4.0])]),
            },
            ParquetColumn {
                field: list_field("embedding64", Physical::DOUBLE),
                data: ColumnData::ListDouble(vec![
                    Some(vec![0.5, -0.25]),
                    None,
                    Some(vec![1.0, 2.0]),
                ]),
            },
            ParquetColumn {
                field: json_field("doc"),
                data: ColumnData::Bytes(vec![
                    Some(br#"{ "b": 1, "a": [2] }"#.to_vec()),
                    None,
                    Some(b"true".to_vec()),
                ]),
            },
            ParquetColumn {
                field: geo_field("place"),
                data: ColumnData::Geo(vec![Some((45.5, -122.625)), None, Some((0.0, 0.0))]),
            },
        ];
        (schema, parquet)
    }

    fn create_table(database: &mut Database, name: &str, columns: Vec<Column>) {
        database
            .execute(&Statement::CreateNodeTable {
                name: name.to_owned(),
                columns,
            })
            .unwrap();
    }

    /// Every mapped type, including NULLs, reads back
    /// exactly through `run`.
    #[test]
    fn all_mapped_types_and_nulls_round_trip() {
        let directory = TestDirectory::new();
        let file = directory.path("all-types.parquet");
        let (schema, parquet) = all_types_columns();
        write_parquet(&file, parquet, 2);
        let mut database = Database::create(directory.path("db.devondb"), PAGE_SIZE).unwrap();
        create_table(&mut database, "Reading", schema);

        database
            .execute(&copy_statement("Reading", &file, None))
            .unwrap();

        let decimal = |text: &str| Value::Decimal(text.parse::<Decimal128>().unwrap());
        assert_eq!(
            scan(&mut database, "Reading").rows,
            vec![
                vec![
                    Value::Int64(1),
                    Value::Int64(-7),
                    Value::Float64(1.5),
                    Value::Float64(0.25),
                    Value::Bool(true),
                    Value::String("Ada".to_owned()),
                    Value::Bytes(vec![0x00, 0xff]),
                    Value::Timestamp(1_723_161_600_123_456),
                    Value::Timestamp(1_723_161_600_123_000),
                    Value::Timestamp(1_723_161_600_123_456),
                    decimal("19.99"),
                    Value::Vector(vec![0.1, 0.2]),
                    Value::Vector(vec![0.5, -0.25]),
                    Value::Json(r#"{"a":[2],"b":1}"#.to_owned()),
                    Value::GeoPoint(GeoPoint::new(45.5, -122.625).unwrap()),
                ],
                vec![
                    Value::Int64(2),
                    Value::Null,
                    Value::Null,
                    Value::Null,
                    Value::Null,
                    Value::Null,
                    Value::Null,
                    Value::Null,
                    Value::Null,
                    Value::Null,
                    Value::Null,
                    Value::Null,
                    Value::Null,
                    Value::Null,
                    Value::Null,
                ],
                vec![
                    Value::Int64(3),
                    Value::Int64(42),
                    Value::Float64(-0.0),
                    Value::Float64(-2.5),
                    Value::Bool(false),
                    Value::String(String::new()),
                    Value::Bytes(vec![0x1a]),
                    Value::Timestamp(0),
                    Value::Timestamp(-1_000),
                    Value::Timestamp(1),
                    decimal("-0.05"),
                    Value::Vector(vec![3.0, 4.0]),
                    Value::Vector(vec![1.0, 2.0]),
                    Value::Json("true".to_owned()),
                    Value::GeoPoint(GeoPoint::new(0.0, 0.0).unwrap()),
                ],
            ]
        );
    }

    /// A nanos timestamp not divisible by 1000 is
    /// refused, naming row and column — never truncated.
    #[test]
    fn nanos_timestamp_not_divisible_by_1000_is_refused() {
        let directory = TestDirectory::new();
        let file = directory.path("nanos.parquet");
        write_parquet(
            &file,
            vec![
                ParquetColumn {
                    field: Arc::new(primitive("id", Physical::INT64).build().unwrap()),
                    data: ColumnData::Int64(vec![Some(1)]),
                },
                ParquetColumn {
                    field: timestamp_field("at", TimeUnit::NANOS),
                    data: ColumnData::Int64(vec![Some(1_000_000_001)]),
                },
            ],
            1,
        );
        let mut database = Database::create(directory.path("db.devondb"), PAGE_SIZE).unwrap();
        create_table(
            &mut database,
            "Reading",
            vec![
                column("id", LogicalType::Int64, true),
                column("at", LogicalType::Timestamp, false),
            ],
        );

        let error = database
            .execute(&copy_statement("Reading", &file, None))
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "invalid argument: Parquet row 1, column `at`: timestamp nanos value \
             1000000001 is not divisible by 1000; COPY never truncates"
        );
        assert!(scan(&mut database, "Reading").rows.is_empty());
    }

    /// A decimal whose Parquet scale disagrees with the
    /// column's declared scale is refused — never rescaled.
    #[test]
    fn decimal_scale_mismatch_is_refused() {
        let directory = TestDirectory::new();
        let file = directory.path("decimal.parquet");
        write_parquet(
            &file,
            vec![
                ParquetColumn {
                    field: Arc::new(primitive("id", Physical::INT64).build().unwrap()),
                    data: ColumnData::Int64(vec![Some(1)]),
                },
                ParquetColumn {
                    field: decimal_i64_field("amount", 10, 3),
                    data: ColumnData::Int64(vec![Some(1999)]),
                },
            ],
            1,
        );
        let mut database = Database::create(directory.path("db.devondb"), PAGE_SIZE).unwrap();
        create_table(
            &mut database,
            "Reading",
            vec![
                column("id", LogicalType::Int64, true),
                column(
                    "amount",
                    LogicalType::Decimal {
                        precision: 10,
                        scale: 2,
                    },
                    false,
                ),
            ],
        );

        let error = database
            .execute(&copy_statement("Reading", &file, None))
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "invalid argument: Parquet column `amount`: DECIMAL scale 3 does not match \
             column scale 2; COPY never rescales"
        );
        assert!(scan(&mut database, "Reading").rows.is_empty());
    }

    /// A list of the wrong length is refused, naming row
    /// and column.
    #[test]
    fn list_of_the_wrong_length_is_refused() {
        let directory = TestDirectory::new();
        let file = directory.path("vector.parquet");
        write_parquet(
            &file,
            vec![
                ParquetColumn {
                    field: Arc::new(primitive("id", Physical::INT64).build().unwrap()),
                    data: ColumnData::Int64(vec![Some(1), Some(2)]),
                },
                ParquetColumn {
                    field: list_field("embedding", Physical::FLOAT),
                    data: ColumnData::ListFloat(vec![Some(vec![0.1, 0.2]), Some(vec![1.0])]),
                },
            ],
            2,
        );
        let mut database = Database::create(directory.path("db.devondb"), PAGE_SIZE).unwrap();
        create_table(
            &mut database,
            "Reading",
            vec![
                column("id", LogicalType::Int64, true),
                column("embedding", LogicalType::Vector { dim: 2 }, false),
            ],
        );

        let error = database
            .execute(&copy_statement("Reading", &file, None))
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "invalid argument: Parquet row 2, column `embedding`: list has 1 elements; \
             expected 2"
        );
        assert!(scan(&mut database, "Reading").rows.is_empty());
    }

    /// A column-name mismatch is refused with the standard
    /// did-you-mean.
    #[test]
    fn column_name_mismatch_suggests_the_schema_spelling() {
        let directory = TestDirectory::new();
        let file = directory.path("drift.parquet");
        write_parquet(
            &file,
            vec![
                ParquetColumn {
                    field: Arc::new(primitive("id", Physical::INT64).build().unwrap()),
                    data: ColumnData::Int64(vec![Some(1)]),
                },
                ParquetColumn {
                    field: utf8_field("nane"),
                    data: ColumnData::Bytes(vec![Some(b"Ada".to_vec())]),
                },
            ],
            1,
        );
        let mut database = Database::create(directory.path("db.devondb"), PAGE_SIZE).unwrap();
        create_table(
            &mut database,
            "Person",
            vec![
                column("id", LogicalType::Int64, true),
                column("name", LogicalType::String, false),
            ],
        );

        let error = database
            .execute(&copy_statement("Person", &file, None))
            .unwrap_err()
            .to_string();
        assert!(error.contains("unknown Parquet column `nane`"), "{error}");
        assert!(error.contains("did you mean `name`"), "{error}");
        assert!(scan(&mut database, "Person").rows.is_empty());
    }

    /// A two-row-group file of 5,000 rows loads
    /// streaming under a small `memory_limit`; the same file through
    /// `sort by` exceeds the budget honestly, matching the CSV path.
    #[test]
    fn two_row_groups_stream_under_a_small_budget() {
        let directory = TestDirectory::new();
        let file = directory.path("stream.parquet");
        let payload = "x".repeat(128);
        let ids: Vec<Option<i64>> = (0..5_000).map(Some).collect();
        let names: Vec<Option<Vec<u8>>> = (0..5_000)
            .map(|id| Some(format!("{payload}{id}").into_bytes()))
            .collect();
        write_parquet(
            &file,
            vec![
                ParquetColumn {
                    field: Arc::new(primitive("id", Physical::INT64).build().unwrap()),
                    data: ColumnData::Int64(ids),
                },
                ParquetColumn {
                    field: utf8_field("name"),
                    data: ColumnData::Bytes(names),
                },
            ],
            2_500,
        );
        let mut database = Database::create_with(
            directory.path("db.devondb"),
            Options {
                page_size: PAGE_SIZE,
                memory_limit: 1024 * 1024,
            },
        )
        .unwrap();
        create_table(
            &mut database,
            "Person",
            vec![
                column("id", LogicalType::Int64, true),
                column("name", LogicalType::String, false),
            ],
        );

        let sorted = database
            .execute(&copy_statement("Person", &file, Some("id")))
            .unwrap_err();
        assert!(
            matches!(sorted, DevonError::BudgetExceeded { .. }),
            "{sorted}"
        );
        assert!(scan(&mut database, "Person").rows.is_empty());

        database
            .execute(&copy_statement("Person", &file, None))
            .unwrap();
        let count = database
            .run(&plan("nodes(Person) as p | aggregate count(p.id) as total"))
            .unwrap();
        assert_eq!(count.rows, vec![vec![Value::Int64(5_000)]]);
    }

    /// The magic decides, not the extension: a `.csv`
    /// whose bytes start with `PAR1` loads as Parquet.
    #[test]
    fn csv_extension_with_parquet_magic_loads_as_parquet() {
        let directory = TestDirectory::new();
        let file = directory.path("people.csv");
        write_parquet(
            &file,
            vec![
                ParquetColumn {
                    field: Arc::new(primitive("id", Physical::INT64).build().unwrap()),
                    data: ColumnData::Int64(vec![Some(1), Some(2)]),
                },
                ParquetColumn {
                    field: utf8_field("name"),
                    data: ColumnData::Bytes(vec![Some(b"Ada".to_vec()), Some(b"Grace".to_vec())]),
                },
            ],
            2,
        );
        assert_eq!(&fs::read(&file).unwrap()[..4], b"PAR1");
        let mut database = Database::create(directory.path("db.devondb"), PAGE_SIZE).unwrap();
        create_table(
            &mut database,
            "Person",
            vec![
                column("id", LogicalType::Int64, true),
                column("name", LogicalType::String, false),
            ],
        );

        database
            .execute(&copy_statement("Person", &file, None))
            .unwrap();
        assert_eq!(
            scan(&mut database, "Person").rows,
            vec![
                vec![Value::Int64(1), Value::String("Ada".to_owned())],
                vec![Value::Int64(2), Value::String("Grace".to_owned())],
            ]
        );
    }

    /// Relationship tables take the reserved `from`/`to`
    /// columns exactly as the CSV path does.
    #[test]
    fn relationship_table_loads_reserved_from_to_columns() {
        let directory = TestDirectory::new();
        let file = directory.path("knows.parquet");
        write_parquet(
            &file,
            vec![
                ParquetColumn {
                    field: Arc::new(primitive("to", Physical::INT64).build().unwrap()),
                    data: ColumnData::Int64(vec![Some(2), Some(3)]),
                },
                ParquetColumn {
                    field: Arc::new(primitive("since", Physical::INT64).build().unwrap()),
                    data: ColumnData::Int64(vec![Some(1843), Some(1991)]),
                },
                ParquetColumn {
                    field: Arc::new(primitive("from", Physical::INT64).build().unwrap()),
                    data: ColumnData::Int64(vec![Some(1), Some(1)]),
                },
            ],
            2,
        );
        let mut database = Database::create(directory.path("db.devondb"), PAGE_SIZE).unwrap();
        create_table(
            &mut database,
            "Person",
            vec![
                column("id", LogicalType::Int64, true),
                column("name", LogicalType::String, false),
            ],
        );
        database
            .execute(&statement(
                "insert into Person values (1, \"Ada\"), (2, \"Grace\"), (3, \"Edsger\")",
            ))
            .unwrap();
        database
            .execute(&statement(
                "create rel table Knows from Person to Person (since Int64)",
            ))
            .unwrap();

        // File column order is arbitrary (`to` first); exact-name mapping
        // must place every column regardless.
        database
            .execute(&copy_statement("Knows", &file, None))
            .unwrap();

        let rows = database
            .run(&plan(
                "nodes(Person) as p | expand Knows out as friend | project p.id, friend.id",
            ))
            .unwrap()
            .rows;
        assert_eq!(
            rows,
            vec![
                vec![Value::Int64(1), Value::Int64(2)],
                vec![Value::Int64(1), Value::Int64(3)],
            ]
        );
    }

    /// Endpoint reads fill clean cache frames before the Parquet iterator asks
    /// for its first buffer. The buffer must reclaim those frames, while an
    /// actual oversized row group still refuses without publishing any edges.
    #[test]
    fn relationship_row_group_reclaims_endpoint_cache_and_refuses_real_pressure() {
        const LIMIT: usize = 1024 * 1024;
        let directory = TestDirectory::new();
        let path = directory.path("cache-pressure.devondb");
        seed_wide_endpoints(&path);
        assert!(fs::metadata(&path).unwrap().len() > LIMIT as u64);
        let small = directory.path("small.parquet");
        let oversized = directory.path("oversized.parquet");
        write_property_edges(&small, 1_024);
        write_property_edges(&oversized, 16_384);
        let mut database = Database::open_with(
            &path,
            Options {
                page_size: PAGE_SIZE,
                memory_limit: LIMIT,
            },
        )
        .unwrap();
        database
            .execute(&copy_statement("Links", &small, None))
            .unwrap();
        assert!(database.memory_budget().charged() <= LIMIT);
        let error = database
            .execute(&copy_statement("Links", &oversized, None))
            .unwrap_err();
        assert!(
            matches!(error, DevonError::BudgetExceeded { .. }),
            "{error}"
        );
        assert!(
            error.to_string().contains("COPY Parquet row-group buffer"),
            "{error}"
        );
        assert!(database.memory_budget().charged() <= LIMIT);
        drop(database);
        let mut reopened = Database::open(&path).unwrap();
        let result = reopened
            .run(&plan(
                "nodes(Payload) as p | expand_rel Links out as q via e | project p.id, q.id, e.tag",
            ))
            .unwrap();
        let expected = (0..128)
            .map(|id| {
                vec![
                    Value::Int64(id),
                    Value::Int64((id + 1) % 128),
                    Value::String(property_tag(id, 1_024)),
                ]
            })
            .collect::<Vec<_>>();
        assert_eq!(result.rows, expected);
    }

    fn seed_wide_endpoints(path: &Path) {
        let mut database = Database::create(path, PAGE_SIZE).unwrap();
        create_table(
            &mut database,
            "Payload",
            vec![
                column("id", LogicalType::Int64, true),
                column("payload", LogicalType::Bytes, false),
            ],
        );
        let rows = (0..128)
            .map(|id| {
                let payload = (0..8_192)
                    .map(|offset| ((offset * 37 + id) % 256) as u8)
                    .collect();
                vec![Value::Int64(id), Value::Bytes(payload)]
            })
            .collect();
        database
            .execute(&Statement::InsertNode {
                table: "Payload".into(),
                rows,
            })
            .unwrap();
        database
            .execute(&statement(
                "create rel table Links from Payload to Payload (tag String)",
            ))
            .unwrap();
        database.checkpoint().unwrap();
    }

    fn property_tag(id: i64, bytes: usize) -> String {
        format!("{id:04}{}", "x".repeat(bytes - 4))
    }

    fn write_property_edges(path: &Path, bytes: usize) {
        write_parquet(
            path,
            vec![
                ParquetColumn {
                    field: Arc::new(primitive("from", Physical::INT64).build().unwrap()),
                    data: ColumnData::Int64((0..128).map(Some).collect()),
                },
                ParquetColumn {
                    field: Arc::new(primitive("to", Physical::INT64).build().unwrap()),
                    data: ColumnData::Int64((0..128).map(|id| Some((id + 1) % 128)).collect()),
                },
                ParquetColumn {
                    field: utf8_field("tag"),
                    data: ColumnData::Bytes(
                        (0..128)
                            .map(|id| Some(property_tag(id, bytes).into_bytes()))
                            .collect(),
                    ),
                },
            ],
            128,
        );
    }

    /// `sort by` orders the load exactly as for CSV,
    /// proven by a scan.
    #[test]
    fn sort_by_orders_the_loaded_rows() {
        let directory = TestDirectory::new();
        let file = directory.path("shuffled.parquet");
        let ids: Vec<Option<i64>> = (0..1_000_i64)
            .map(|offset| Some((offset * 7919) % 1_000))
            .collect();
        write_parquet(
            &file,
            vec![
                ParquetColumn {
                    field: Arc::new(primitive("id", Physical::INT64).build().unwrap()),
                    data: ColumnData::Int64(ids),
                },
                ParquetColumn {
                    field: Arc::new(primitive("value", Physical::INT64).build().unwrap()),
                    data: ColumnData::Int64(vec![Some(7); 1_000]),
                },
            ],
            500,
        );
        let mut database = Database::create(directory.path("db.devondb"), PAGE_SIZE).unwrap();
        create_table(
            &mut database,
            "Reading",
            vec![
                column("id", LogicalType::Int64, true),
                column("value", LogicalType::Int64, false),
            ],
        );

        database
            .execute(&copy_statement("Reading", &file, Some("id")))
            .unwrap();

        let ids: Vec<i64> = scan(&mut database, "Reading")
            .rows
            .into_iter()
            .map(|row| match row[0] {
                Value::Int64(id) => id,
                ref other => panic!("unexpected id: {other:?}"),
            })
            .collect();
        assert_eq!(ids, (0..1_000).collect::<Vec<_>>());
    }
}
