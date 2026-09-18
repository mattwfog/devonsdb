//! Scalar-anchored KNN execution through the public embedded facade.

use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use devondb::{Database, Plan, QueryResult, Statement};
use devondb_plan::{
    expr::{Expr, Metric},
    ops::{KnnMode, KnnVectorSource, Operator, PLAN_VERSION},
    text::parser::{Parsed, parse},
};
use devondb_types::{logical_type::LogicalType, schema::Column, value::Value};

const PAGE_SIZE: u32 = 4096;
const DIMENSION: u32 = 8;

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
            "devondb-knn-scalar-{label}-{timestamp}-{sequence}-{}",
            std::process::id()
        ));
        fs::create_dir_all(&path).unwrap();
        Self { path }
    }

    fn database(&self) -> PathBuf {
        self.path.join("db.devondb")
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

fn create_documents(database: &mut Database) {
    database
        .execute(&Statement::CreateNodeTable {
            name: "Document".to_owned(),
            columns: vec![
                column("id", LogicalType::Int64, true),
                column("embedding", LogicalType::Vector { dim: DIMENSION }, false),
            ],
        })
        .unwrap();
}

fn insert_document(database: &mut Database, id: i64, embedding: Option<Vec<f32>>) {
    let value = match embedding {
        Some(vector) => Value::Vector(vector),
        None => Value::Null,
    };
    database
        .execute(&Statement::InsertNode {
            table: "Document".to_owned(),
            rows: vec![vec![Value::Int64(id), value]],
        })
        .unwrap();
}

fn deterministic_vector(id: i64) -> Vec<f32> {
    (0..DIMENSION)
        .map(|dimension| (id * 10 + dimension as i64) as f32)
        .collect()
}

fn scalar_plan() -> Operator {
    Operator::Project {
        exprs: vec![devondb_plan::ops::ProjectionItem {
            expr: Expr::Col("anchor.embedding".to_owned()),
            alias: "embedding".to_owned(),
        }],
        input: Box::new(Operator::Filter {
            predicate: Expr::Binary {
                op: devondb_plan::expr::BinaryOp::Eq,
                left: Box::new(Expr::Col("anchor.id".to_owned())),
                right: Box::new(Expr::Lit(Value::Int64(7))),
            },
            input: Box::new(Operator::ScanNodes {
                table: "Document".to_owned(),
                binding: "anchor".to_owned(),
            }),
        }),
    }
}

fn knn_query(source: KnnVectorSource, k: u64, mode: KnnMode) -> Plan {
    Plan {
        v: PLAN_VERSION,
        plan: Operator::KnnScan {
            table: "Document".to_owned(),
            column: "embedding".to_owned(),
            query: source,
            k,
            metric: Metric::Cosine,
            mode,
        },
    }
}

fn text_scalar_knn(k: u64) -> String {
    text_scalar_knn_anchored(k, 7)
}

/// The anchor subquery selects `Document` ids `7..=last_anchor_id`.
fn text_scalar_knn_anchored(k: u64, last_anchor_id: i64) -> String {
    format!(
        "knn(Document.embedding, scalar(nodes(Document) as anchor | filter anchor.id >= 7 and \
         anchor.id <= {last_anchor_id} | aggregate count(anchor.id) as id by anchor.id, \
         anchor.embedding | project anchor.embedding), {k}, cosine)"
    )
}

fn parse_query(source: &str) -> Plan {
    match parse(source).unwrap() {
        Parsed::Query(plan) => plan,
        Parsed::Statement(_) => panic!("expected a query"),
    }
}

fn ids(result: &QueryResult) -> Vec<i64> {
    result
        .rows
        .iter()
        .map(|row| match row.first() {
            Some(Value::Int64(id)) => *id,
            other => panic!("unexpected id value: {other:?}"),
        })
        .collect()
}

fn literal_vector(id: i64) -> Vec<f32> {
    deterministic_vector(id)
        .into_iter()
        .map(|component| component + 0.5)
        .collect()
}

#[test]
fn similar_to_returns_anchor_first_and_matches_literal_result() {
    let directory = TestDirectory::new("e2e");
    let mut database = Database::create(directory.database(), PAGE_SIZE).unwrap();
    create_documents(&mut database);
    for id in 1..=20 {
        insert_document(&mut database, id, Some(deterministic_vector(id)));
    }

    let scalar_text = text_scalar_knn(5);
    let scalar_result = database.run(&parse_query(&scalar_text)).unwrap();
    assert_eq!(ids(&scalar_result), [7, 8, 6, 9, 10]);
    assert_eq!(scalar_result.rows[0][2], Value::Float64(0.0));

    let anchor = literal_vector(7);
    let literal_text = format!(
        "knn(Document.embedding, [{components}], 5, cosine)",
        components = anchor
            .iter()
            .map(|component| component.to_string())
            .collect::<Vec<_>>()
            .join(", ")
    );
    let literal_result = database.run(&parse_query(&literal_text)).unwrap();
    assert_eq!(ids(&scalar_result), ids(&literal_result));
}

#[test]
fn json_scalar_source_matches_text_and_refusal_is_gone() {
    let directory = TestDirectory::new("json");
    let mut database = Database::create(directory.database(), PAGE_SIZE).unwrap();
    create_documents(&mut database);
    for id in 1..=12 {
        insert_document(&mut database, id, Some(deterministic_vector(id)));
    }

    let text_plan = parse_query(&text_scalar_knn(4));
    let json = text_plan.to_json().unwrap();
    let json_plan = Plan::from_json(&json).unwrap();
    assert_eq!(text_plan, json_plan);
    let expected = database.run(&text_plan).unwrap();
    assert_eq!(database.run(&json_plan).unwrap(), expected);
    assert_eq!(ids(&expected), [7, 8, 6, 9]);

    let direct_plan = knn_query(
        KnnVectorSource::Scalar {
            plan: Box::new(scalar_plan()),
        },
        4,
        KnnMode::Exact,
    );
    let direct_result = database.run(&direct_plan).unwrap();
    assert_eq!(direct_result, expected);
}

#[test]
fn null_anchor_is_an_evaluation_error_naming_the_subquery() {
    let directory = TestDirectory::new("null");
    let mut database = Database::create(directory.database(), PAGE_SIZE).unwrap();
    create_documents(&mut database);
    for id in 1..=3 {
        if id == 7 {
            insert_document(&mut database, id, None);
        } else {
            insert_document(&mut database, id, Some(deterministic_vector(id)));
        }
    }

    let plan = parse_query(&text_scalar_knn(2));
    println!("plan: {}", plan.to_json().unwrap());
    let outcome = database.run(&plan).map(|result| format!("{result:?}"));
    println!("outcome: {outcome:?}");
    assert!(outcome.is_err(), "expected an error, got {outcome:?}");
    let error = outcome.err().unwrap();
    assert_eq!(
        error.to_string(),
        "invalid argument: knn query vector is null"
    );
}

#[test]
fn zero_row_anchor_uses_a6b_null_rule_then_the_knn_null_error() {
    let directory = TestDirectory::new("zero");
    let mut database = Database::create(directory.database(), PAGE_SIZE).unwrap();
    create_documents(&mut database);
    for id in 1..=3 {
        insert_document(&mut database, id, Some(deterministic_vector(id)));
    }

    let error = database
        .run(&parse_query(&text_scalar_knn(2)))
        .err()
        .unwrap();
    assert_eq!(
        error.to_string(),
        "invalid argument: knn query vector is null"
    );
}

#[test]
fn two_row_anchor_uses_a6b_cardinality_error_exactly() {
    let directory = TestDirectory::new("two");
    let mut database = Database::create(directory.database(), PAGE_SIZE).unwrap();
    create_documents(&mut database);
    for id in 7..=8 {
        insert_document(&mut database, id, Some(deterministic_vector(id)));
    }

    let error = database
        .run(&parse_query(&text_scalar_knn_anchored(2, 8)))
        .err()
        .unwrap();
    assert_eq!(
        error.to_string(),
        "invalid argument: scalar subquery returned more than one row"
    );
}

#[test]
fn scalar_and_scan_share_one_transaction_snapshot() {
    let directory = TestDirectory::new("snapshot");
    let mut database = Database::create(directory.database(), PAGE_SIZE).unwrap();
    create_documents(&mut database);
    for id in 1..=10 {
        insert_document(&mut database, id, Some(deterministic_vector(id)));
    }

    database.checkpoint().unwrap();
    let snapshot = database.snapshot();
    drop(database);
    let mut writer = Database::open(directory.database()).unwrap();
    insert_document(&mut writer, 11, Some(vec![71.0; DIMENSION as usize]));
    drop(writer);

    let result = snapshot.run(&parse_query(&text_scalar_knn(5))).unwrap();
    assert!(!ids(&result).contains(&11));
    assert_eq!(ids(&result), [7, 8, 6, 9, 10]);
}

#[test]
fn scalar_source_works_in_exact_and_hnsw_modes() {
    let directory = TestDirectory::new("modes");
    let mut database = Database::create(directory.database(), PAGE_SIZE).unwrap();
    create_documents(&mut database);
    for id in 1..=10 {
        insert_document(&mut database, id, Some(deterministic_vector(id)));
    }
    database
        .execute(&Statement::CreateHnswIndex {
            name: "document_embedding_cosine".to_owned(),
            table: "Document".to_owned(),
            column: "embedding".to_owned(),
            metric: Metric::Cosine,
        })
        .unwrap();

    let exact = database
        .run(&knn_query(
            KnnVectorSource::Scalar {
                plan: Box::new(scalar_plan()),
            },
            1,
            KnnMode::Exact,
        ))
        .unwrap();
    let hnsw = database
        .run(&knn_query(
            KnnVectorSource::Scalar {
                plan: Box::new(scalar_plan()),
            },
            1,
            KnnMode::Approximate,
        ))
        .unwrap();
    assert_eq!(ids(&exact), [7]);
    assert_eq!(ids(&hnsw), [7]);
}

#[test]
fn canonical_scalar_plan_replays_identically() {
    let directory = TestDirectory::new("replay");
    let mut database = Database::create(directory.database(), PAGE_SIZE).unwrap();
    create_documents(&mut database);
    for id in 1..=8 {
        insert_document(&mut database, id, Some(deterministic_vector(id)));
    }

    let original = parse_query(&text_scalar_knn(3));
    let canonical = devondb_plan::text::printer::print_plan(&original).unwrap();
    let reparsed = parse_query(&canonical);
    assert_eq!(original, reparsed);
    assert_eq!(
        database.run(&reparsed).unwrap(),
        database.run(&original).unwrap()
    );
}
