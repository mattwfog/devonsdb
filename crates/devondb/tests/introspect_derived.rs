//! Regression coverage for derived ontology defaults and pin diagnostics.

use devondb::{
    Database,
    text::parser::{Parsed, parse},
};
use devondb_storage::{
    catalog::{Catalog, PinEntry},
    pager::Pager,
};
use serde_json::json;
use tempfile::TempDir;

const IRREGULAR_PLURALS: [(&str, &str); 8] = [
    ("Person", "people"),
    ("Child", "children"),
    ("Man", "men"),
    ("Woman", "women"),
    ("Foot", "feet"),
    ("Tooth", "teeth"),
    ("Goose", "geese"),
    ("Mouse", "mice"),
];

fn database_path(directory: &TempDir) -> std::path::PathBuf {
    directory.path().join("db.devondb")
}

fn execute_text(database: &mut Database, input: &str) {
    match parse(input).expect("statement parses") {
        Parsed::Statement(envelope) => database
            .execute(&envelope.stmt)
            .expect("statement executes"),
        Parsed::Query(_) => panic!("expected statement: {input}"),
    }
}

fn create_node_table(database: &mut Database, table: &str) {
    execute_text(
        database,
        &format!("create node table {table} (id Int64 primary key)"),
    );
}

#[test]
fn plain_database_derives_a_node_class_for_every_node_table() {
    let directory = TempDir::new().expect("create test directory");
    let mut database = Database::create(database_path(&directory), 4096).expect("create database");
    execute_text(
        &mut database,
        "create node table Person (id Int64 primary key, Name String)",
    );
    create_node_table(&mut database, "Project");

    let classes = database
        .schema_summary()
        .classes
        .expect("derived classes are always exposed");
    assert!(classes.interfaces.is_empty());
    assert!(classes.rel_classes.is_empty());
    assert_eq!(classes.node_classes.len(), 2);
    assert_eq!(classes.node_classes[0].table, "Person");
    assert_eq!(classes.node_classes[0].display, "Person");
    assert_eq!(classes.node_classes[0].plural.as_deref(), Some("people"));
    assert_eq!(classes.node_classes[0].label.as_deref(), Some("Name"));
    assert_eq!(classes.node_classes[1].table, "Project");
    assert_eq!(classes.node_classes[1].plural.as_deref(), Some("projects"));
}

#[test]
fn derived_plurals_match_the_nl_irregular_and_suffix_rules() {
    let directory = TempDir::new().expect("create test directory");
    let mut database = Database::create(database_path(&directory), 4096).expect("create database");
    for (singular, _) in IRREGULAR_PLURALS {
        create_node_table(&mut database, singular);
    }
    let suffix_cases = [
        ("Category", "categories"),
        ("Bus", "buses"),
        ("Box", "boxes"),
        ("Quiz", "quizes"),
        ("Church", "churches"),
        ("Brush", "brushes"),
        ("Project", "projects"),
    ];
    for (singular, _) in suffix_cases {
        create_node_table(&mut database, singular);
    }

    let classes = database
        .schema_summary()
        .classes
        .expect("derived classes are exposed");
    let expected = IRREGULAR_PLURALS
        .into_iter()
        .chain(suffix_cases)
        .collect::<Vec<_>>();
    let actual = classes
        .node_classes
        .iter()
        .map(|class| {
            (
                class.table.as_str(),
                class.plural.as_deref().expect("derived plural"),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(actual, expected);
}

#[test]
fn declared_plural_wins_over_the_derived_plural() {
    let directory = TempDir::new().expect("create test directory");
    let mut database = Database::create(database_path(&directory), 4096).expect("create database");
    create_node_table(&mut database, "Mouse");
    execute_text(
        &mut database,
        "create class for Mouse (plural \"mouse records\")",
    );

    let summary = database.schema_summary();
    let class = &summary.classes.expect("classes").node_classes[0];
    assert_eq!(class.plural.as_deref(), Some("mouse records"));
}

#[test]
fn undecodable_stored_pin_surfaces_its_error() {
    let directory = TempDir::new().expect("create test directory");
    let path = database_path(&directory);
    let database = Database::create(&path, 4096).expect("create database");
    drop(database);

    let pager = Pager::open(&path).expect("open pager");
    let mut catalog = Catalog::load(&pager).expect("load catalog");
    let publish_lsn = pager.superblock().checkpoint_lsn + 1;
    catalog
        .pin(PinEntry {
            name: "broken".to_owned(),
            text: "stored invalid plan".to_owned(),
            plan: json!({"v": 0, "plan": {"op": "NotAnOperator"}}),
            created_lsn: publish_lsn,
        })
        .expect("storage accepts an opaque plan envelope");
    catalog
        .save(&pager, publish_lsn)
        .expect("persist opaque pin");
    drop(pager);

    let database = Database::open(&path).expect("open database with opaque pin");
    let summary = database.schema_summary();
    let pin = &summary.pins[0];
    assert_eq!(pin.canonical, "");
    let error = pin.error.as_deref().expect("decode error is surfaced");
    assert!(error.contains("NotAnOperator"), "{error}");
    assert_eq!(
        serde_json::to_value(pin).expect("pin serializes")["error"],
        error
    );
}
