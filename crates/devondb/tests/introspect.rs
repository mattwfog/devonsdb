//! Facade test: `Database::schema_summary` reflects committed DDL.

use std::{
    env, fs,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use devondb::{
    Database,
    text::parser::{Parsed, parse},
};

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct TestDirectory {
    path: PathBuf,
}

impl TestDirectory {
    fn new(label: &str) -> Self {
        let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        let path = env::temp_dir().join(format!(
            "devondb-introspect-{label}-{timestamp}-{sequence}-{}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("create test directory");
        Self { path }
    }

    fn db_path(&self) -> PathBuf {
        self.path.join("db.devondb")
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn execute_text(database: &mut Database, input: &str) {
    match parse(input).expect("statement parses") {
        Parsed::Statement(envelope) => database
            .execute(&envelope.stmt)
            .expect("statement executes"),
        Parsed::Query(_) => panic!("expected a statement, parsed a query"),
    }
}

#[test]
fn empty_database_has_empty_summary() {
    let directory = TestDirectory::new("empty");
    let database = Database::create(directory.db_path(), 4096).expect("create database");
    let summary = database.schema_summary();
    assert!(summary.node_tables.is_empty());
    assert!(summary.rel_tables.is_empty());
}

#[test]
fn summary_reflects_committed_ddl_with_plan_ir_type_spellings() {
    let directory = TestDirectory::new("ddl");
    let mut database = Database::create(directory.db_path(), 4096).expect("create database");
    execute_text(
        &mut database,
        "create node table Person (id Int64 primary key, name String, bio Vector(3))",
    );
    execute_text(
        &mut database,
        "create rel table Knows from Person to Person (since Int64)",
    );

    let summary = database.schema_summary();

    assert_eq!(summary.node_tables.len(), 1);
    let person = &summary.node_tables[0];
    assert_eq!(person.name, "Person");
    let columns: Vec<(&str, &str, bool)> = person
        .columns
        .iter()
        .map(|column| (column.name.as_str(), column.ty.as_str(), column.primary_key))
        .collect();
    assert_eq!(
        columns,
        vec![
            ("id", "Int64", true),
            ("name", "String", false),
            ("bio", "Vector(3)", false),
        ]
    );

    assert_eq!(summary.rel_tables.len(), 1);
    let knows = &summary.rel_tables[0];
    assert_eq!(knows.name, "Knows");
    assert_eq!(knows.from, "Person");
    assert_eq!(knows.to, "Person");
    assert_eq!(knows.columns.len(), 1);
    assert_eq!(knows.columns[0].name, "since");
    assert_eq!(knows.columns[0].ty, "Int64");
    assert!(!knows.columns[0].primary_key);
}

#[test]
fn summary_serializes_to_the_documented_json_shape() {
    let directory = TestDirectory::new("json");
    let mut database = Database::create(directory.db_path(), 4096).expect("create database");
    execute_text(
        &mut database,
        "create node table Person (id Int64 primary key, name String)",
    );

    let json = serde_json::to_value(database.schema_summary()).expect("summary serializes");
    assert_eq!(json["node_tables"][0]["name"], "Person");
    assert_eq!(json["node_tables"][0]["columns"][0]["name"], "id");
    assert_eq!(json["node_tables"][0]["columns"][0]["type"], "Int64");
    assert_eq!(json["node_tables"][0]["columns"][0]["primary_key"], true);
    assert_eq!(json["node_tables"][0]["columns"][1]["type"], "String");
    assert_eq!(json["rel_tables"], serde_json::json!([]));
}
