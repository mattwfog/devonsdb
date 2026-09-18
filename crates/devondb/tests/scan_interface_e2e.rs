//! Severed-proof coverage for executable ontology interfaces.

use std::path::Path;

use devondb::{Database, DevonError, Plan, QueryResult, Statement, text};
use devondb_plan::{
    expr::Expr,
    ops::{Operator, ProjectionItem},
    statement::InterfaceColumn,
    typing::{InterfaceSchema, SchemaInput},
    validate::validate,
};
use devondb_storage::{
    catalog::{Catalog, InterfaceColumn as CatalogInterfaceColumn, InterfaceEntry},
    pager::Pager,
};
use devondb_types::{
    logical_type::LogicalType,
    schema::{Column, NodeTableSchema},
    value::Value,
};
use tempfile::TempDir;

const PAGE_SIZE: u32 = 4096;
const PIN_NAME: &str = "all names";

fn statement(input: &str) -> Statement {
    let text::parser::Parsed::Statement(statement) =
        text::parser::parse(input).expect("statement parses")
    else {
        panic!("expected statement: {input}");
    };
    statement.stmt
}

fn execute(database: &mut Database, input: &str) {
    database
        .execute(&statement(input))
        .unwrap_or_else(|error| panic!("execute {input:?}: {error}"));
}

fn interface_source(interface: &str, binding: &str) -> Operator {
    Operator::ScanInterface {
        interface: interface.to_owned(),
        binding: binding.to_owned(),
    }
}

fn projected_interface_plan() -> Plan {
    Plan {
        v: 0,
        plan: Operator::Project {
            exprs: vec![
                ProjectionItem {
                    expr: Expr::Col("entity.name".into()),
                    alias: "name".into(),
                },
                ProjectionItem {
                    expr: Expr::ClassOf("entity".into()),
                    alias: "class".into(),
                },
            ],
            input: Box::new(interface_source("Nameable", "entity")),
        },
    }
}

fn raw_interface_plan(interface: &str, binding: &str) -> Plan {
    Plan {
        v: 0,
        plan: interface_source(interface, binding),
    }
}

fn create_interface_fixture(path: &Path) -> Database {
    let mut database = Database::create(path, PAGE_SIZE).expect("create database");
    execute(
        &mut database,
        "create node table Person (id Int64 primary key, name String, role String)",
    );
    execute(
        &mut database,
        "create node table Project (id Int64 primary key, title String, name String)",
    );
    execute(&mut database, "create interface Nameable (name String)");
    execute(&mut database, "create interface Dormant (name String)");
    execute(
        &mut database,
        "create class for Project (implements (Nameable))",
    );
    execute(
        &mut database,
        "create class for Person (implements (Nameable))",
    );
    database
}

fn expected_rows() -> Vec<Vec<Value>> {
    vec![
        vec![Value::String("Ada".into()), Value::String("Person".into())],
        vec![
            Value::String("Grace".into()),
            Value::String("Person".into()),
        ],
        vec![
            Value::String("DevonDB".into()),
            Value::String("Project".into()),
        ],
        vec![
            Value::String("Atlas".into()),
            Value::String("Project".into()),
        ],
    ]
}

#[test]
fn scan_interface_e2e_preserves_order_overlay_reopen_zero_and_pin() {
    let directory = TempDir::new().expect("temporary directory");
    let path = directory.path().join("interfaces.devondb");
    let mut database = create_interface_fixture(&path);

    execute(
        &mut database,
        "insert into Person values (1, \"Ada\", \"mathematician\")",
    );
    execute(
        &mut database,
        "insert into Project values (10, \"database\", \"DevonDB\")",
    );
    database.checkpoint().expect("checkpoint base rows");
    execute(
        &mut database,
        "insert into Person values (2, \"Grace\", \"engineer\")",
    );
    execute(
        &mut database,
        "insert into Project values (11, \"browser\", \"Atlas\")",
    );
    let checkpoint_lsn = Pager::open(&path)
        .expect("inspect checkpoint before interface scan")
        .superblock()
        .checkpoint_lsn;
    assert!(
        database.observed_commit_lsn() > checkpoint_lsn,
        "fixture rows must still live in the committed overlay"
    );

    let plan = projected_interface_plan();
    let table_only_schemas = vec![
        node_schema("Person", "role"),
        node_schema("Project", "title"),
    ];
    let severed = validate(&plan, &table_only_schemas, &[])
        .expect_err("disconnecting the facade interface adapter must fail");
    assert!(
        severed.to_string().contains("interface `Nameable`"),
        "unexpected severed-adapter error: {severed}"
    );

    let overlay_result = database.run(&plan).expect("run interface over overlay");
    assert_eq!(
        overlay_result,
        QueryResult {
            columns: vec!["name".into(), "class".into()],
            rows: expected_rows(),
        }
    );
    assert_eq!(
        database
            .run(&raw_interface_plan("Dormant", "d"))
            .expect("zero-implementer scan"),
        QueryResult {
            columns: vec!["d.name".into()],
            rows: Vec::new(),
        }
    );

    database
        .pin(PIN_NAME, "nodes(Nameable) as entity", &plan)
        .expect("pin interface query");
    let pinned_before = database.run_pin(PIN_NAME).expect("run fresh pin");
    assert_eq!(pinned_before.rows, expected_rows());
    drop(database);

    let mut reopened = Database::open(&path).expect("reopen interface database");
    assert_eq!(
        reopened.run(&plan).expect("run after reopen"),
        overlay_result
    );
    assert_eq!(
        reopened.run_pin(PIN_NAME).expect("replay pin after reopen"),
        pinned_before
    );
    drop(reopened);

    assert_interface_limit_does_not_read_the_second_table(&path);
}

fn node_schema(name: &str, third_column: &str) -> NodeTableSchema {
    NodeTableSchema::new(
        name.to_owned(),
        vec![
            Column {
                name: "id".into(),
                ty: LogicalType::Int64,
                primary_key: true,
            },
            Column {
                name: if name == "Project" {
                    third_column.to_owned()
                } else {
                    "name".into()
                },
                ty: LogicalType::String,
                primary_key: false,
            },
            Column {
                name: if name == "Project" {
                    "name".into()
                } else {
                    third_column.to_owned()
                },
                ty: LogicalType::String,
                primary_key: false,
            },
        ],
    )
    .expect("fixture schema")
}

fn assert_interface_limit_does_not_read_the_second_table(path: &Path) {
    let limited_plan = Plan {
        v: 0,
        plan: Operator::Limit {
            count: 1,
            offset: None,
            input: Box::new(interface_source("Nameable", "entity")),
        },
    };
    let mut limited = Database::open(path).expect("open for limited read proof");
    limited.reset_page_read_count();
    assert_eq!(
        limited
            .run(&limited_plan)
            .expect("limited interface")
            .rows
            .len(),
        1
    );
    let limited_reads = limited.page_read_count();
    drop(limited);

    let mut full = Database::open(path).expect("open for full read proof");
    full.reset_page_read_count();
    assert_eq!(
        full.run(&raw_interface_plan("Nameable", "entity"))
            .expect("full interface")
            .rows
            .len(),
        4
    );
    let full_reads = full.page_read_count();
    assert!(
        limited_reads < full_reads,
        "limited interface read {limited_reads} pages; full union read {full_reads}"
    );
}

#[test]
fn interface_and_node_table_ddl_refuse_new_folded_collisions() {
    let directory = TempDir::new().expect("temporary directory");
    let path = directory.path().join("ddl-collisions.devondb");
    let mut database = Database::create(path, PAGE_SIZE).expect("create database");
    execute(
        &mut database,
        "create node table Existing (id Int64 primary key, name String)",
    );

    let interface_error = database
        .execute(&statement("create interface existing (name String)"))
        .expect_err("interface must not collide with a node table");
    assert_eq!(
        interface_error.to_string(),
        "invalid argument: interface `existing` conflicts with node table `Existing` under folded name resolution"
    );

    execute(&mut database, "create interface Reserved (name String)");
    let table_error = database
        .execute(&statement(
            "create node table reserved (id Int64 primary key, name String)",
        ))
        .expect_err("node table must not collide with an interface");
    assert_eq!(
        table_error.to_string(),
        "invalid argument: node table `reserved` conflicts with interface `Reserved` under folded name resolution"
    );
}

#[test]
fn released_catalog_collision_keeps_table_shadow_and_targets_explicit_interface() {
    let directory = TempDir::new().expect("temporary directory");
    let path = directory.path().join("released-collision.devondb");
    let mut database = Database::create(&path, PAGE_SIZE).expect("create database");
    execute(
        &mut database,
        "create node table Shadow (id Int64 primary key, name String)",
    );
    database.checkpoint().expect("persist table catalog");
    drop(database);

    inject_released_interface_collision(&path);

    let pager = Pager::open(&path).expect("open raw collided catalog");
    let catalog = Catalog::load(&pager).expect("load collided catalog");
    let interfaces = vec![InterfaceSchema::new(
        "shadow".into(),
        vec![InterfaceColumn {
            name: "name".into(),
            ty: LogicalType::String,
        }],
    )];
    let schema = SchemaInput::new(catalog.node_tables(), catalog.rel_tables(), &interfaces);
    let text::parser::Parsed::Query(parsed) =
        text::parser::parse_with_schema("nodes(SHADOW) as item", &schema)
            .expect("table-shadowed text resolves")
    else {
        panic!("expected query");
    };
    assert!(matches!(parsed.plan, Operator::ScanNodes { .. }));
    drop(catalog);
    drop(pager);

    let mut database = Database::open(&path).expect("facade opens released collision");
    assert!(
        database.run(&parsed).is_ok(),
        "the shadowing table stays readable"
    );
    let error = database
        .run(&raw_interface_plan("shadow", "item"))
        .expect_err("explicit shadowed interface must fail");
    assert!(matches!(error, DevonError::InvalidArgument { .. }));
    assert_eq!(
        error.to_string(),
        "invalid argument: ScanInterface: interface `shadow` is shadowed by node table `Shadow` under folded name resolution"
    );
}

fn inject_released_interface_collision(path: &Path) {
    let pager = Pager::open(path).expect("open raw database");
    let mut catalog = Catalog::load(&pager).expect("load raw catalog");
    catalog
        .declare_interface(InterfaceEntry {
            name: "shadow".into(),
            columns: vec![CatalogInterfaceColumn {
                name: "name".into(),
                ty: LogicalType::String,
            }],
        })
        .expect("storage decode law permits released collision");
    let publish_lsn = pager
        .superblock()
        .checkpoint_lsn
        .checked_add(1)
        .expect("fixture LSN");
    catalog
        .save(&pager, publish_lsn)
        .expect("publish collided catalog");
}
