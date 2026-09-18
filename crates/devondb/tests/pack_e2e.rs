//! DEVONPACK facade end to end (`docs/SCALE.md` §7): a database
//! exercising every read path is packed, reopened by magic, and must answer
//! identically; every mutator hits the read-only fence; a `memory_limit`
//! below one frame refuses honestly; the golden corpus packs and reads back
//! equal. Without the `pack` feature, a DEVONPACK file is refused as
//! `invalid_argument`, never as superblock corruption.

/// Everything in this suite needs the container codec.
#[cfg(feature = "pack")]
mod with_feature {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    use devondb::text::parser::{Parsed, parse};
    use devondb::{Database, DevonError, Options, Plan, Statement};
    use devondb_plan::{
        expr::Metric,
        ops::{KnnMode, Operator},
        statement::RelRow,
    };
    use devondb_types::{
        Decimal128, GeoPoint, logical_type::LogicalType, schema::Column, value::Value,
    };

    const PAGE_SIZE: u32 = 4096;
    const DIMENSION: u32 = 4;
    const PEOPLE: i64 = 2500;
    const PIN_NAME: &str = "named person";

    static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

    struct TestDirectory {
        path: PathBuf,
    }

    impl TestDirectory {
        fn new(label: &str) -> Self {
            let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let timestamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "devondb-pack-e2e-{label}-{}-{timestamp}-{sequence}",
                std::process::id()
            ));
            fs::create_dir(&path).unwrap();
            Self { path }
        }

        fn file(&self, name: &str) -> PathBuf {
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

    fn query_plan(text: &str) -> Plan {
        let Parsed::Query(plan) = parse(text).expect("query parses") else {
            panic!("expected query: {text}")
        };
        plan
    }

    /// Builds the every-read-path source: four node tables (Int64/String;
    /// Timestamp/Decimal; GeoPoint; Vector + HNSW), a rel table, ≥ 3 node
    /// groups after checkpoint, ontology, and a pinned plan.
    fn build_source(path: &Path) -> Database {
        let mut database = Database::create(path, PAGE_SIZE).unwrap();
        database
            .execute(&Statement::CreateNodeTable {
                name: "Person".to_owned(),
                columns: vec![
                    column("id", LogicalType::Int64, true),
                    column("name", LogicalType::String, false),
                    column("age", LogicalType::Int64, false),
                ],
            })
            .unwrap();
        database
            .execute(&Statement::CreateNodeTable {
                name: "Scalar".to_owned(),
                columns: vec![
                    column("id", LogicalType::Int64, true),
                    column("created_at", LogicalType::Timestamp, false),
                    column(
                        "amount",
                        LogicalType::Decimal {
                            precision: 38,
                            scale: 0,
                        },
                        false,
                    ),
                ],
            })
            .unwrap();
        database
            .execute(&Statement::CreateNodeTable {
                name: "Place".to_owned(),
                columns: vec![
                    column("id", LogicalType::Int64, true),
                    column("loc", LogicalType::GeoPoint, false),
                ],
            })
            .unwrap();
        database
            .execute(&Statement::CreateNodeTable {
                name: "Corpus".to_owned(),
                columns: vec![
                    column("id", LogicalType::Int64, true),
                    column("embedding", LogicalType::Vector { dim: DIMENSION }, false),
                ],
            })
            .unwrap();
        database
            .execute(&Statement::CreateRelTable {
                name: "Knows".to_owned(),
                from: "Person".to_owned(),
                to: "Person".to_owned(),
                columns: vec![column("since", LogicalType::Int64, false)],
            })
            .unwrap();

        // Two node groups for Person (capacity 2048): the first is uniformly
        // age 30 so a selective filter prunes it by zone map.
        let people: Vec<Vec<Value>> = (0..PEOPLE)
            .map(|id| {
                vec![
                    Value::Int64(id),
                    Value::String(format!("person-{id}")),
                    Value::Int64(if id < 2048 { 30 } else { 90 }),
                ]
            })
            .collect();
        database
            .execute(&Statement::InsertNode {
                table: "Person".to_owned(),
                rows: people,
            })
            .unwrap();
        database
            .execute(&Statement::InsertNode {
                table: "Scalar".to_owned(),
                rows: (0..10_i64)
                    .map(|id| {
                        vec![
                            Value::Int64(id),
                            Value::Timestamp(1_700_000_000_000_000 + id),
                            Value::Decimal(Decimal128::new(i128::from(id) * 100, 0).unwrap()),
                        ]
                    })
                    .collect(),
            })
            .unwrap();
        database
            .execute(&Statement::InsertNode {
                table: "Place".to_owned(),
                rows: vec![
                    vec![
                        Value::Int64(1),
                        Value::GeoPoint(GeoPoint::from_canonical(45.5, -122.625).unwrap()),
                    ],
                    vec![Value::Int64(2), Value::Null],
                    vec![
                        Value::Int64(3),
                        Value::GeoPoint(GeoPoint::from_canonical(-90.0, 0.0).unwrap()),
                    ],
                ],
            })
            .unwrap();
        database
            .execute(&Statement::InsertNode {
                table: "Corpus".to_owned(),
                rows: (0..64_i64)
                    .map(|id| {
                        vec![
                            Value::Int64(id),
                            Value::Vector(vec![id as f32, 1.0, 2.0, 3.0]),
                        ]
                    })
                    .collect(),
            })
            .unwrap();
        database
            .execute(&Statement::InsertRel {
                table: "Knows".to_owned(),
                rows: (1..5_i64)
                    .map(|from| RelRow {
                        from_key: Value::Int64(from),
                        to_key: Value::Int64(from + 1),
                        values: vec![Value::Int64(2000 + from)],
                    })
                    .collect(),
            })
            .unwrap();
        database
            .execute(&Statement::CreateHnswIndex {
                name: "corpus_embedding_l2".to_owned(),
                table: "Corpus".to_owned(),
                column: "embedding".to_owned(),
                metric: Metric::L2,
            })
            .unwrap();
        database.checkpoint().unwrap();

        for text in [
            "create interface Nameable (name String)",
            "create class for Person (plural \"people\", summary (name), description \"a human\", implements (Nameable))",
            "create class for Knows (verb \"knows\", inverse \"is known by\")",
        ] {
            let Parsed::Statement(envelope) = parse(text).expect("statement parses") else {
                panic!("expected statement: {text}")
            };
            database.execute(&envelope.stmt).unwrap();
        }
        let pin_text = "nodes(Person) as person | filter person.name = \"person-7\" | project person.name as name";
        database
            .pin(PIN_NAME, pin_text, &query_plan(pin_text))
            .unwrap();
        database
    }

    /// Packs `source` next to it and returns the container path.
    fn pack_database(source: &mut Database, out: &Path, frame_pages: u32) -> PathBuf {
        source.pack(out, frame_pages).unwrap();
        out.to_path_buf()
    }

    fn assert_same(source: &mut Database, packed: &mut Database, plan: &Plan, label: &str) {
        let expected = source.run(plan).unwrap();
        let actual = packed.run(plan).unwrap();
        assert_eq!(actual.columns, expected.columns, "{label} columns differ");
        assert_rows_bit_identical(&expected.rows, &actual.rows, label);
    }

    /// NaN-aware row equality (`tests/zone_maps.rs` pattern): NaN != NaN under
    /// `PartialEq`, but a pack must reproduce the source bits exactly.
    fn assert_rows_bit_identical(expected: &[Vec<Value>], actual: &[Vec<Value>], label: &str) {
        assert_eq!(actual.len(), expected.len(), "{label} row count differs");
        for (row, (expected, actual)) in expected.iter().zip(actual).enumerate() {
            assert_eq!(
                actual.len(),
                expected.len(),
                "{label} row {row} width differs"
            );
            for (column, (expected, actual)) in expected.iter().zip(actual).enumerate() {
                let equal = match (expected, actual) {
                    (Value::Float64(expected), Value::Float64(actual)) => {
                        expected.to_bits() == actual.to_bits()
                    }
                    _ => expected == actual,
                };
                assert!(equal, "{label} row {row} column {column} differs");
            }
        }
    }

    #[test]
    fn packed_database_answers_every_read_path_identically() {
        let directory = TestDirectory::new("roundtrip");
        let source_path = directory.file("source.devondb");
        let mut source = build_source(&source_path);
        let pack_path = pack_database(&mut source, &directory.file("packed.devondb"), 256);

        let magic = fs::read(&pack_path).unwrap();
        assert_eq!(&magic[..9], b"DEVONPACK", "container magic at byte 0");

        let mut packed = Database::open(&pack_path).unwrap();
        assert_same(
            &mut source,
            &mut packed,
            &query_plan("nodes(Person) as person"),
            "Person scan",
        );
        assert_same(
            &mut source,
            &mut packed,
            &query_plan("nodes(Scalar) as s"),
            "Scalar scan (Timestamp/Decimal)",
        );
        assert_same(
            &mut source,
            &mut packed,
            &query_plan("nodes(Place) as place"),
            "Place scan (GeoPoint)",
        );
        assert_same(
            &mut source,
            &mut packed,
            &query_plan("nodes(Corpus) as corpus"),
            "Corpus scan (Vector)",
        );
        assert_same(
            &mut source,
            &mut packed,
            &query_plan(
                "nodes(Person) as p | expand Knows out as friend | project p.name as src, friend.name as dst",
            ),
            "Knows expand over CSR",
        );

        // Zone-map pruning: the selective filter must skip the first Person
        // group on the pack exactly as on the source.
        let selective = Plan::from_json(
        r#"{"v":0,"plan":{"op":"Filter","predicate":{"ge":[{"col":"p.age"},{"lit":80}]},"input":{"op":"ScanNodes","table":"Person","binding":"p"}}}"#,
    )
    .unwrap();
        let unprunable = Plan::from_json(
        r#"{"v":0,"plan":{"op":"Filter","predicate":{"ge":[{"add":[{"col":"p.age"},{"lit":0}]},{"lit":80}]},"input":{"op":"ScanNodes","table":"Person","binding":"p"}}}"#,
    )
    .unwrap();
        assert_same(&mut source, &mut packed, &selective, "pruned filter");
        packed.reset_page_read_count();
        let pruned_rows = packed.run(&selective).unwrap().rows.len();
        let pruned_reads = packed.page_read_count();
        packed.reset_page_read_count();
        assert_eq!(packed.run(&unprunable).unwrap().rows.len(), pruned_rows);
        let unpruned_reads = packed.page_read_count();
        assert!(
            pruned_reads < unpruned_reads,
            "pruned scan read {pruned_reads} pages, unprunable equivalent read {unpruned_reads}"
        );

        // HNSW-backed approximate knn and the exact scan agree with the source.
        for mode in [KnnMode::Approximate, KnnMode::Exact] {
            let knn = Plan {
                v: 0,
                plan: Operator::KnnScan {
                    table: "Corpus".to_owned(),
                    column: "embedding".to_owned(),
                    query: vec![10.0, 1.0, 2.0, 3.0].into(),
                    k: 5,
                    metric: Metric::L2,
                    mode,
                },
            };
            assert_same(&mut source, &mut packed, &knn, "knn scan");
        }

        // Pins and ontology live in the packed catalog.
        let expected = source.run_pin(PIN_NAME).unwrap();
        assert_eq!(packed.run_pin(PIN_NAME).unwrap(), expected, "pinned plan");
        assert_eq!(
            packed.schema_summary().classes,
            source.schema_summary().classes,
            "ontology classes"
        );
        assert_eq!(
            packed.schema_summary().pins.len(),
            1,
            "pin count in the packed catalog"
        );
    }

    #[test]
    fn every_mutator_hits_the_read_only_fence() {
        let directory = TestDirectory::new("fences");
        let source_path = directory.file("source.devondb");
        let mut source = build_source(&source_path);
        let pack_path = pack_database(&mut source, &directory.file("packed.devondb"), 256);
        drop(source);
        let mut packed = Database::open(&pack_path).unwrap();

        let assert_read_only = |result: Result<(), DevonError>, operation: &str| {
            assert!(
                matches!(&result, Err(DevonError::ReadOnly { context }) if context.contains("DEVONPACK")),
                "{operation} must be refused read-only, got {result:?}"
            );
        };
        assert_read_only(
            packed.execute(&Statement::InsertNode {
                table: "Person".to_owned(),
                rows: vec![vec![
                    Value::Int64(99_999),
                    Value::String("intruder".to_owned()),
                    Value::Int64(1),
                ]],
            }),
            "insert",
        );
        assert_read_only(
            packed.execute(&Statement::CreateNodeTable {
                name: "Extra".to_owned(),
                columns: vec![column("id", LogicalType::Int64, true)],
            }),
            "DDL",
        );
        assert_read_only(
            packed.pin("late", "late plan", &query_plan("nodes(Person) as person")),
            "pin",
        );
        assert_read_only(packed.unpin(PIN_NAME), "unpin");
        assert_read_only(packed.checkpoint(), "checkpoint");
        assert_read_only(
            packed.execute(&Statement::CopyNode {
                table: "Person".to_owned(),
                path: "nowhere.csv".to_owned(),
                sort_by: None,
            }),
            "copy",
        );
        assert_read_only(
            packed.execute(&Statement::CreateHnswIndex {
                name: "late_index".to_owned(),
                table: "Corpus".to_owned(),
                column: "embedding".to_owned(),
                metric: Metric::L2,
            }),
            "index build",
        );
        assert_read_only(packed.pack(directory.file("repack.devondb"), 256), "pack");
        assert!(
            matches!(packed.begin(), Err(DevonError::ReadOnly { .. })),
            "write transaction must be refused read-only"
        );

        // Reads still work after every refusal.
        assert_eq!(
            packed
                .run(&query_plan("nodes(Place) as place"))
                .unwrap()
                .rows
                .len(),
            3
        );
    }

    #[test]
    fn memory_limit_below_one_frame_refuses_honestly() {
        let directory = TestDirectory::new("budget");
        let source_path = directory.file("source.devondb");
        let mut source = build_source(&source_path);
        // 512 pages × 4096 bytes = 2 MiB frames.
        let pack_path = pack_database(&mut source, &directory.file("packed.devondb"), 512);
        drop(source);

        let error = match Database::open_with(
            &pack_path,
            Options {
                page_size: PAGE_SIZE,
                memory_limit: 1024 * 1024,
            },
        ) {
            Ok(_) => panic!("a 2 MiB frame must not fit a 1 MiB memory_limit"),
            Err(error) => error,
        };
        let message = match &error {
            DevonError::BudgetExceeded { context } => context.clone(),
            other => panic!("expected BudgetExceeded, got {other:?}"),
        };
        assert!(
            message.contains("2097152") && message.contains("1048576"),
            "the refusal must name the frame bytes and the limit: {message}"
        );

        // One frame fits the same limit when the writer chose smaller frames.
        let mut source = Database::open(&source_path).unwrap();
        let small = pack_database(&mut source, &directory.file("small.devondb"), 64);
        Database::open_with(
            &small,
            Options {
                page_size: PAGE_SIZE,
                memory_limit: 1024 * 1024,
            },
        )
        .unwrap();
    }

    /// Size-log helper: writes the end-to-end fixture to the directory
    /// named by `PACK_E2E_FIXTURE_OUT`, packed beside it. Ignored in normal
    /// runs; the size log invokes it explicitly.
    #[test]
    #[ignore = "size-log fixture writer"]
    fn write_size_fixture() {
        let Ok(out) = std::env::var("PACK_E2E_FIXTURE_OUT") else {
            return;
        };
        let directory = PathBuf::from(out);
        fs::create_dir_all(&directory).unwrap();
        let mut source = build_source(&directory.join("fixture.devondb"));
        source
            .pack(directory.join("fixture.devonpack"), 256)
            .unwrap();
    }

    #[test]
    fn golden_corpus_packs_and_reads_back_equal() {
        let corpus = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/golden");
        let mut files: Vec<PathBuf> = fs::read_dir(&corpus)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("devondb"))
            .collect();
        files.sort();
        assert!(files.len() >= 5, "golden corpus is missing: {files:?}");

        for file in files {
            // Pack a COPY: opening for pack creates a WAL sidecar, which must
            // never land next to the corpus files.
            let directory = TestDirectory::new("corpus");
            let source_copy = directory.file("source.devondb");
            fs::copy(&file, &source_copy).unwrap();
            let mut source = Database::open(&source_copy)
                .unwrap_or_else(|error| panic!("open {}: {error}", file.display()));
            let pack_path = directory.file("packed.devondb");
            source
                .pack(&pack_path, 256)
                .unwrap_or_else(|error| panic!("pack {}: {error}", file.display()));
            let mut packed = Database::open(&pack_path)
                .unwrap_or_else(|error| panic!("open packed {}: {error}", file.display()));

            assert_eq!(
                packed.schema_summary(),
                source.schema_summary(),
                "schema summary differs for {}",
                file.display()
            );
            let tables: Vec<String> = source
                .schema_summary()
                .node_tables
                .iter()
                .map(|table| table.name.clone())
                .collect();
            for table in tables {
                let plan = Plan::from_json(&format!(
                    r#"{{"v":0,"plan":{{"op":"ScanNodes","table":"{table}","binding":"row"}}}}"#
                ))
                .unwrap();
                assert_same(&mut source, &mut packed, &plan, "golden table scan");
            }
        }
    }
}

/// The no-feature law: a DEVONPACK file is `invalid_argument` naming the
/// missing feature, never a superblock-corruption error (`docs/SCALE.md`
/// §7.2).
#[cfg(not(feature = "pack"))]
#[test]
fn pack_file_without_feature_is_invalid_argument_not_corruption() {
    let path = std::env::temp_dir().join(format!(
        "devondb-pack-no-feature-{}-{}.devondb",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let mut bytes = b"DEVONPACK".to_vec();
    bytes.extend_from_slice(&[0_u8; 64]);
    std::fs::write(&path, &bytes).unwrap();

    let error = match devondb::Database::open(&path) {
        Ok(_) => panic!("a pack must not open without the `pack` feature"),
        Err(error) => error,
    };
    let _ = std::fs::remove_file(&path);
    assert!(
        matches!(&error, devondb::DevonError::InvalidArgument { context } if context == "pack files require the `pack` feature"),
        "expected the honest feature refusal, got {error:?}"
    );
}
