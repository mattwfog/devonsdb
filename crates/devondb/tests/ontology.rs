//! Real-database ontology DDL, introspection, validation, and reopen coverage.

use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use devondb::{
    Database, DevonError,
    introspect::{
        ClassesSummary, InterfaceColumnSummary, InterfaceSummary, NodeClassSummary, RelClassSummary,
    },
    text::parser::{Parsed, parse},
};

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
            .expect("clock after epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "devondb-ontology-{label}-{}-{timestamp}-{sequence}",
            std::process::id()
        ));
        fs::create_dir(&path).expect("create test directory");
        Self { path }
    }

    fn database(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn execute_text(database: &mut Database, input: &str) -> Result<(), DevonError> {
    match parse(input)? {
        Parsed::Statement(envelope) => database.execute(&envelope.stmt),
        Parsed::Query(_) => panic!("expected statement: {input}"),
    }
}

fn create_tables(database: &mut Database) {
    execute_text(
        database,
        "create node table Person (id Int64 primary key, name String, role String)",
    )
    .expect("create Person");
    execute_text(
        database,
        "create rel table Knows from Person to Person (since Int64)",
    )
    .expect("create Knows");
}

fn expected_classes() -> ClassesSummary {
    ClassesSummary {
        interfaces: vec![InterfaceSummary {
            name: "Nameable".to_owned(),
            columns: vec![InterfaceColumnSummary {
                name: "name".to_owned(),
                ty: "String".to_owned(),
            }],
        }],
        node_classes: vec![NodeClassSummary {
            table: "Person".to_owned(),
            display: "Person".to_owned(),
            plural: Some("people".to_owned()),
            label: Some("name".to_owned()),
            summary: vec!["name".to_owned(), "role".to_owned()],
            color: Some("#7aa2ff".to_owned()),
            description: Some("a human".to_owned()),
            implements: vec!["Nameable".to_owned()],
        }],
        rel_classes: vec![RelClassSummary {
            table: "Knows".to_owned(),
            verb: Some("knows".to_owned()),
            inverse: Some("is known by".to_owned()),
        }],
    }
}

fn assert_classes(database: &Database) {
    let summary = database.schema_summary();
    assert_eq!(summary.classes, Some(expected_classes()));
    let json = serde_json::to_string(&summary).expect("summary serializes");
    println!("ontology summary: {json}");
}

#[test]
fn ontology_ddl_persists_and_reopens_through_the_public_facade() {
    let directory = TestDirectory::new("reopen");
    let path = directory.database("db.devondb");
    let mut database = Database::create(&path, PAGE_SIZE).expect("create database");
    create_tables(&mut database);
    execute_text(&mut database, "create interface Nameable (name String)")
        .expect("declare interface");
    execute_text(
        &mut database,
        concat!(
            "create class for Person (plural \"people\", summary (name, role), ",
            "color \"#7aa2ff\", description \"a human\", implements (Nameable))"
        ),
    )
    .expect("declare node class");
    execute_text(
        &mut database,
        "create class for Knows (verb \"knows\", inverse \"is known by\")",
    )
    .expect("declare relationship class");
    assert_classes(&database);
    drop(database);

    let mut reopened = Database::open(&path).expect("open ontology database");
    assert_classes(&reopened);
    execute_text(
        &mut reopened,
        "insert into Person values (1, \"Ada\", \"mathematician\")",
    )
    .expect("recognized ontology feature remains writable after reopen");
    reopened.checkpoint().expect("checkpoint after reopen");
}

#[test]
fn ontology_ddl_validation_errors_are_exact() {
    let directory = TestDirectory::new("validation");
    let mut database =
        Database::create(directory.database("db.devondb"), PAGE_SIZE).expect("create database");
    create_tables(&mut database);

    let wrong_kind = execute_text(
        &mut database,
        "create class for Person (verb \"is a person\")",
    )
    .expect_err("node class must reject relationship clauses");
    assert_eq!(
        wrong_kind.to_string(),
        "invalid argument: node class for `Person` does not admit relationship clause(s): verb"
    );

    let unknown =
        execute_text(&mut database, "create class for Persn").expect_err("unknown table must fail");
    assert_eq!(
        unknown.to_string(),
        "invalid argument: class target table `Persn` was not found (did you mean `Person`?)"
    );

    execute_text(&mut database, "create interface Ageable (age Int64)")
        .expect("declare unsatisfied interface");
    let unsatisfied = execute_text(
        &mut database,
        "create class for Person (implements (Ageable))",
    )
    .expect_err("unsatisfied interface must fail");
    assert_eq!(
        unsatisfied.to_string(),
        concat!(
            "invalid argument: node table `Person` does not satisfy interface `Ageable`: ",
            "requires column `age` of type Int64"
        )
    );
}

#[test]
fn schema_without_ontology_derives_node_classes() {
    let directory = TestDirectory::new("absent");
    let mut database =
        Database::create(directory.database("db.devondb"), PAGE_SIZE).expect("create database");
    create_tables(&mut database);

    let summary = database.schema_summary();
    // ONTOLOGY.md:39-42: defaults are derived and deepen every database;
    // an ontology declaration is never required.
    assert_eq!(
        summary.classes,
        Some(ClassesSummary {
            interfaces: Vec::new(),
            node_classes: vec![NodeClassSummary {
                table: "Person".to_owned(),
                display: "Person".to_owned(),
                plural: Some("people".to_owned()),
                label: Some("name".to_owned()),
                summary: Vec::new(),
                color: None,
                description: None,
                implements: Vec::new(),
            }],
            rel_classes: Vec::new(),
        })
    );
    let json = serde_json::to_value(summary).expect("summary serializes");
    assert!(json.get("classes").is_some());
}
