use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use devondb::{Database, DevonError, QueryResult};
use devondb_plan::{
    expr::{Expr, Metric},
    ops::{Direction, KnnMode, Operator, Plan, ProjectionItem},
    statement::Statement,
};
use devondb_types::{logical_type::LogicalType, schema::Column, value::Value};

const PAGE_SIZE: u32 = 4096;
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
            "devondb-case-insensitive-{label}-{}-{timestamp}-{sequence}",
            std::process::id()
        ));
        fs::create_dir(&path).unwrap();
        Self { path }
    }

    fn database(&self) -> PathBuf {
        self.path.join("case-insensitive.devondb")
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

fn create_person(database: &mut Database) {
    database
        .execute(&Statement::CreateNodeTable {
            name: "Person".to_owned(),
            columns: vec![
                column("id", LogicalType::Int64, true),
                column("name", LogicalType::String, false),
            ],
        })
        .unwrap();
}

fn plan(operator: Operator) -> Plan {
    Plan {
        v: 0,
        plan: operator,
    }
}

fn scan(table: &str, binding: &str) -> Operator {
    Operator::ScanNodes {
        table: table.to_owned(),
        binding: binding.to_owned(),
    }
}

fn assert_invalid_argument(result: Result<(), DevonError>) {
    assert!(
        matches!(result, Err(DevonError::InvalidArgument { .. })),
        "expected InvalidArgument, got {result:?}"
    );
}

fn first_ids(result: QueryResult) -> Vec<i64> {
    result
        .rows
        .into_iter()
        .map(|row| match row.first() {
            Some(Value::Int64(id)) => *id,
            other => panic!("expected leading Int64 id, got {other:?}"),
        })
        .collect()
}

#[test]
fn folded_table_binding_and_column_references_preserve_display_spelling() {
    let directory = TestDirectory::new("lookup-display");
    let mut database = Database::create(directory.database(), PAGE_SIZE).unwrap();
    create_person(&mut database);
    database
        .execute(&Statement::InsertNode {
            table: "pErSoN".to_owned(),
            rows: vec![vec![Value::Int64(1), Value::String("Ada".to_owned())]],
        })
        .unwrap();

    let result = database
        .run(&plan(Operator::Project {
            exprs: vec![ProjectionItem {
                expr: Expr::Col("P.NAME".to_owned()),
                alias: "display_name".to_owned(),
            }],
            input: Box::new(scan("person", "p")),
        }))
        .unwrap();

    assert_eq!(result.columns, vec!["display_name"]);
    assert_eq!(result.rows, vec![vec![Value::String("Ada".to_owned())]]);
    let summary = database.schema_summary();
    assert_eq!(summary.node_tables[0].name, "Person");
    assert_eq!(summary.node_tables[0].columns[1].name, "name");
}

#[test]
fn fold_equal_table_column_and_index_names_collide() {
    let directory = TestDirectory::new("ddl-collisions");
    let mut database = Database::create(directory.database(), PAGE_SIZE).unwrap();
    create_person(&mut database);

    assert_invalid_argument(database.execute(&Statement::CreateNodeTable {
        name: "person".to_owned(),
        columns: vec![column("id", LogicalType::Int64, true)],
    }));
    assert_invalid_argument(database.execute(&Statement::CreateNodeTable {
        name: "BadColumns".to_owned(),
        columns: vec![
            column("Name", LogicalType::Int64, true),
            column("name", LogicalType::String, false),
        ],
    }));

    database
        .execute(&Statement::CreateNodeTable {
            name: "VectorTable".to_owned(),
            columns: vec![
                column("id", LogicalType::Int64, true),
                column("FirstVector", LogicalType::Vector { dim: 2 }, false),
                column("SecondVector", LogicalType::Vector { dim: 2 }, false),
            ],
        })
        .unwrap();
    database
        .execute(&Statement::InsertNode {
            table: "vectortable".to_owned(),
            rows: vec![vec![
                Value::Int64(1),
                Value::Vector(vec![0.0, 0.0]),
                Value::Vector(vec![1.0, 1.0]),
            ]],
        })
        .unwrap();
    database
        .execute(&Statement::CreateHnswIndex {
            name: "VectorIndex".to_owned(),
            table: "VECTORTABLE".to_owned(),
            column: "firstvector".to_owned(),
            metric: Metric::L2,
        })
        .unwrap();

    assert_invalid_argument(database.execute(&Statement::CreateHnswIndex {
        name: "vectorindex".to_owned(),
        table: "VectorTable".to_owned(),
        column: "SecondVector".to_owned(),
        metric: Metric::L2,
    }));
    assert_invalid_argument(database.execute(&Statement::CreateHnswIndex {
        name: "OtherIndex".to_owned(),
        table: "vectortable".to_owned(),
        column: "FIRSTVECTOR".to_owned(),
        metric: Metric::L2,
    }));
}

#[test]
fn relationship_endpoints_and_expand_references_match_folded() {
    let directory = TestDirectory::new("relationships");
    let mut database = Database::create(directory.database(), PAGE_SIZE).unwrap();
    create_person(&mut database);
    database
        .execute(&Statement::CreateNodeTable {
            name: "City".to_owned(),
            columns: vec![
                column("id", LogicalType::Int64, true),
                column("name", LogicalType::String, false),
            ],
        })
        .unwrap();
    database
        .execute(&Statement::CreateRelTable {
            name: "LivesIn".to_owned(),
            from: "PERSON".to_owned(),
            to: "city".to_owned(),
            columns: vec![],
        })
        .unwrap();
    database
        .execute(&Statement::CreateRelTable {
            name: "Knows".to_owned(),
            from: "person".to_owned(),
            to: "PERSON".to_owned(),
            columns: vec![],
        })
        .unwrap();
    database
        .execute(&Statement::InsertNode {
            table: "person".to_owned(),
            rows: vec![
                vec![Value::Int64(1), Value::String("Ada".to_owned())],
                vec![Value::Int64(2), Value::String("Grace".to_owned())],
            ],
        })
        .unwrap();
    database
        .execute(&Statement::InsertRel {
            table: "KNOWS".to_owned(),
            rows: vec![devondb_plan::statement::RelRow {
                from_key: Value::Int64(1),
                to_key: Value::Int64(2),
                values: vec![],
            }],
        })
        .unwrap();
    database.checkpoint().unwrap();
    database
        .execute(&Statement::InsertNode {
            table: "CITY".to_owned(),
            rows: vec![vec![Value::Int64(10), Value::String("London".to_owned())]],
        })
        .unwrap();
    database
        .execute(&Statement::InsertRel {
            table: "livesin".to_owned(),
            rows: vec![devondb_plan::statement::RelRow {
                from_key: Value::Int64(1),
                to_key: Value::Int64(10),
                values: vec![],
            }],
        })
        .unwrap();
    database.checkpoint().unwrap();

    let result = database
        .run(&plan(Operator::Project {
            exprs: vec![ProjectionItem {
                expr: Expr::Col("c.NAME".to_owned()),
                alias: "city".to_owned(),
            }],
            input: Box::new(Operator::Expand {
                rel: "LIVESIN".to_owned(),
                direction: Direction::Out,
                from_binding: "p".to_owned(),
                binding: "C".to_owned(),
                input: Box::new(scan("PERSON", "P")),
            }),
        }))
        .unwrap();
    assert_eq!(result.rows, vec![vec![Value::String("London".to_owned())]]);

    let both = database
        .run(&plan(Operator::Project {
            exprs: vec![ProjectionItem {
                expr: Expr::Col("FRIEND.name".to_owned()),
                alias: "friend".to_owned(),
            }],
            input: Box::new(Operator::Expand {
                rel: "kNoWs".to_owned(),
                direction: Direction::Both,
                from_binding: "person".to_owned(),
                binding: "friend".to_owned(),
                input: Box::new(scan("person", "PERSON")),
            }),
        }))
        .unwrap();
    assert_eq!(
        both.rows,
        vec![
            vec![Value::String("Grace".to_owned())],
            vec![Value::String("Ada".to_owned())],
        ]
    );

    // Self-LOOP discriminator for the Both-direction dedup: `Knows` was
    // created `from person to PERSON` (fold-equal, byte-different), and
    // the dedup that suppresses a self-loop edge's duplicate incoming copy
    // must key on FOLDED endpoint equality. A byte-compare skips it and
    // Ada appears twice from her own loop.
    database
        .execute(&Statement::InsertRel {
            table: "knows".to_owned(),
            rows: vec![devondb_plan::statement::RelRow {
                from_key: Value::Int64(1),
                to_key: Value::Int64(1),
                values: vec![],
            }],
        })
        .unwrap();
    let with_loop = database
        .run(&plan(Operator::Project {
            exprs: vec![ProjectionItem {
                expr: Expr::Col("friend.name".to_owned()),
                alias: "friend".to_owned(),
            }],
            input: Box::new(Operator::Expand {
                rel: "Knows".to_owned(),
                direction: Direction::Both,
                from_binding: "p".to_owned(),
                binding: "friend".to_owned(),
                input: Box::new(scan("Person", "p")),
            }),
        }))
        .unwrap();
    assert_eq!(
        with_loop.rows,
        vec![
            vec![Value::String("Grace".to_owned())],
            vec![Value::String("Ada".to_owned())],
            vec![Value::String("Ada".to_owned())],
        ]
    );
}

#[test]
fn hnsw_ddl_and_approximate_knn_resolve_folded_references() {
    let directory = TestDirectory::new("hnsw");
    let path = directory.database();
    let mut database = Database::create(&path, PAGE_SIZE).unwrap();
    database
        .execute(&Statement::CreateNodeTable {
            name: "Corpus".to_owned(),
            columns: vec![
                column("id", LogicalType::Int64, true),
                column("embedding", LogicalType::Vector { dim: 2 }, false),
            ],
        })
        .unwrap();
    database
        .execute(&Statement::InsertNode {
            table: "CORPUS".to_owned(),
            rows: vec![
                vec![Value::Int64(1), Value::Vector(vec![0.0, 0.0])],
                vec![Value::Int64(2), Value::Vector(vec![10.0, 10.0])],
            ],
        })
        .unwrap();
    database
        .execute(&Statement::CreateHnswIndex {
            name: "CorpusEmbedding".to_owned(),
            table: "corpus".to_owned(),
            column: "EMBEDDING".to_owned(),
            metric: Metric::L2,
        })
        .unwrap();
    database
        .execute(&Statement::InsertNode {
            table: "CoRpUs".to_owned(),
            rows: vec![vec![Value::Int64(3), Value::Vector(vec![0.05, 0.05])]],
        })
        .unwrap();

    let query = plan(Operator::KnnScan {
        table: "cOrPuS".to_owned(),
        column: "Embedding".to_owned(),
        query: vec![0.1, 0.1].into(),
        k: 1,
        metric: Metric::L2,
        mode: KnnMode::Approximate,
    });
    assert_eq!(first_ids(database.run(&query).unwrap()), vec![3]);
    database.checkpoint().unwrap();
    drop(database);

    let mut reopened = Database::open(path).unwrap();
    assert_eq!(first_ids(reopened.run(&query).unwrap()), vec![3]);
}

#[test]
fn checkpoint_and_reopen_preserve_catalog_case_and_mixed_case_rows() {
    let directory = TestDirectory::new("reopen");
    let path = directory.database();
    let mut database = Database::create(&path, PAGE_SIZE).unwrap();
    database
        .execute(&Statement::CreateNodeTable {
            name: "MiXeDTable".to_owned(),
            columns: vec![
                column("Key", LogicalType::Int64, true),
                column("DisplayName", LogicalType::String, false),
            ],
        })
        .unwrap();
    database
        .execute(&Statement::InsertNode {
            table: "mixedtable".to_owned(),
            rows: vec![vec![Value::Int64(7), Value::String("preserved".to_owned())]],
        })
        .unwrap();
    database.checkpoint().unwrap();
    drop(database);

    let mut reopened = Database::open(path).unwrap();
    let summary = reopened.schema_summary();
    assert_eq!(summary.node_tables[0].name, "MiXeDTable");
    assert_eq!(summary.node_tables[0].columns[0].name, "Key");
    assert_eq!(summary.node_tables[0].columns[1].name, "DisplayName");
    let result = reopened.run(&plan(scan("MIXEDTABLE", "item"))).unwrap();
    assert_eq!(result.columns, vec!["item.Key", "item.DisplayName"]);
    assert_eq!(first_ids(result), vec![7]);
}

#[test]
fn mixed_case_primary_key_writers_conflict_across_transactions() {
    let directory = TestDirectory::new("conflict");
    let database = Database::create(directory.database(), PAGE_SIZE).unwrap();
    let mut setup = database.clone();
    create_person(&mut setup);

    let mut lower = database.begin().unwrap();
    let mut display = database.begin().unwrap();
    lower
        .execute(&Statement::InsertNode {
            table: "person".to_owned(),
            rows: vec![vec![Value::Int64(42), Value::String("lower".to_owned())]],
        })
        .unwrap();
    display
        .execute(&Statement::InsertNode {
            table: "Person".to_owned(),
            rows: vec![vec![Value::Int64(42), Value::String("display".to_owned())]],
        })
        .unwrap();

    lower.commit().unwrap();
    assert!(matches!(
        display.commit(),
        Err(DevonError::TransactionConflict { .. })
    ));
}
