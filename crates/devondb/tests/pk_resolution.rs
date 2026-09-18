use std::path::Path;

use devondb::{Database, Options, Plan, Statement};
use devondb_plan::statement::{RelRow, SetItem};
use devondb_storage::{
    catalog::{Catalog, TableStorage},
    node_group::{NODE_GROUP_CAPACITY, NodeGroup},
    pager::Pager,
};
use devondb_types::{
    logical_type::LogicalType,
    schema::{Column, NodeTableSchema},
    value::Value,
};
use tempfile::tempdir;

const PAGE_SIZE: u32 = 4096;
const FIXTURE_DB_ID: [u8; 16] = *b"pk-resolution-db";

#[test]
fn distinct_checkpointed_resolutions_do_one_build_pass_not_one_scan_each() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("severed-proof.devondb");
    write_checkpointed_fixture(&path, 3 * NODE_GROUP_CAPACITY);
    let mut database = Database::open(&path).unwrap();

    database.reset_page_read_count();
    let full_rows = database.run(&people_scan()).unwrap().rows;
    let full_scan_reads = database.page_read_count();
    assert_eq!(full_rows.len(), 3 * NODE_GROUP_CAPACITY);

    let mut transaction = database.begin().unwrap();
    database.reset_page_read_count();
    let mut resolved = 0_u64;
    for group in 0..3 {
        for row in 0..32 {
            let key = (group * NODE_GROUP_CAPACITY + row) as i64;
            transaction
                .execute(&update_name(key, &format!("updated-{key}")))
                .unwrap();
            resolved += 1;
        }
    }
    let resolution_reads = database.page_read_count();

    assert!(database.pk_resolution_index_present("Person"));
    assert!(
        resolution_reads <= full_scan_reads.saturating_mul(3),
        "{resolved} resolutions read {resolution_reads} pages; one full scan read {full_scan_reads}"
    );
    assert!(
        resolution_reads.saturating_mul(8) < full_scan_reads.saturating_mul(resolved),
        "resolution work still scales like one full scan per key"
    );
    transaction.abort();
}

#[test]
fn indexed_resolution_preserves_overlay_own_dml_and_endpoint_precedence() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("equivalence.devondb");
    let mut database = create_people_database(&path);
    database
        .execute(&insert_people(&[
            (1, "checkpointed", 10),
            (2, "delete-me", 20),
        ]))
        .unwrap();
    database.checkpoint().unwrap();

    let mut missing = database.begin().unwrap();
    let error = missing.execute(&update_rank(999, 999)).unwrap_err();
    assert_eq!(
        error.to_string(),
        "not found: node table `Person` primary key 999"
    );
    assert!(database.pk_resolution_index_present("Person"));
    missing.abort();

    database.execute(&update_name(1, "dml-updated")).unwrap();
    database.execute(&update_rank(1, 11)).unwrap();
    database
        .execute(&insert_people(&[(3, "overlay-inserted", 30)]))
        .unwrap();
    database.execute(&update_rank(3, 31)).unwrap();
    database.execute(&delete_person(2)).unwrap();

    let deleted_error = database.execute(&update_rank(2, 22)).unwrap_err();
    assert_eq!(
        deleted_error.to_string(),
        "not found: node table `Person` primary key 2"
    );

    let mut own = database.begin().unwrap();
    own.execute(&insert_people(&[
        (4, "own-inserted", 40),
        (5, "own-delete", 50),
    ]))
    .unwrap();
    own.execute(&update_rank(4, 41)).unwrap();
    own.execute(&delete_person(5)).unwrap();
    assert_eq!(
        sorted_people(own.run(&people_scan()).unwrap().rows),
        people(&[
            (1, "dml-updated", 11),
            (3, "overlay-inserted", 31),
            (4, "own-inserted", 41),
        ])
    );
    own.commit().unwrap();

    database.execute(&create_knows()).unwrap();
    database
        .execute(&insert_people(&[(7, "overlay-endpoint", 70)]))
        .unwrap();
    let missing_endpoint = database
        .execute(&insert_edges(&[(2, 7), (999, 7)]))
        .unwrap_err();
    assert_eq!(
        missing_endpoint.to_string(),
        "not found: node table `Person` primary key 2"
    );
    let unknown_endpoint = database.execute(&insert_edges(&[(999, 7)])).unwrap_err();
    assert_eq!(
        unknown_endpoint.to_string(),
        "not found: node table `Person` primary key 999"
    );

    let mut edges = database.begin().unwrap();
    edges
        .execute(&insert_people(&[(6, "own-endpoint", 60)]))
        .unwrap();
    edges.execute(&insert_edges(&[(1, 7), (7, 6)])).unwrap();
    edges.commit().unwrap();
    assert!(
        database
            .run(&people_scan())
            .unwrap()
            .rows
            .iter()
            .any(|row| row.first() == Some(&Value::Int64(6)))
    );
}

#[test]
fn budget_refusal_keeps_index_absent_and_falls_back_correctly() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("budget-refusal.devondb");
    write_checkpointed_fixture(&path, 25_000);
    let options = Options {
        page_size: PAGE_SIZE,
        memory_limit: 1024 * 1024,
    };
    let database = Database::open_with(&path, options).unwrap();
    let mut transaction = database.begin().unwrap();

    transaction
        .execute(&update_name(24_999, "resolved-by-fallback"))
        .unwrap();
    assert!(!database.pk_resolution_index_present("Person"));
    transaction.abort();
}

#[test]
fn checkpoint_publishes_a_fresh_index_for_rows_that_shifted_offsets() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("post-checkpoint.devondb");
    let mut database = create_people_database(&path);
    let rows = (0..=NODE_GROUP_CAPACITY)
        .map(|id| (id as i64, format!("person-{id}"), id as i64))
        .collect::<Vec<_>>();
    database
        .execute(&Statement::InsertNode {
            table: "Person".into(),
            rows: rows
                .iter()
                .map(|(id, name, rank)| person(*id, name, *rank))
                .collect(),
        })
        .unwrap();
    database.checkpoint().unwrap();

    let mut initial = database.begin().unwrap();
    initial.execute(&update_rank(1, 101)).unwrap();
    initial.abort();
    assert!(database.pk_resolution_index_present("Person"));

    database.execute(&delete_person(0)).unwrap();
    let mut before_checkpoint = database.begin().unwrap();
    before_checkpoint
        .execute(&update_rank(NODE_GROUP_CAPACITY as i64, 9999))
        .unwrap();
    before_checkpoint.abort();
    assert!(database.pk_resolution_index_present("Person"));

    database.checkpoint().unwrap();
    assert!(!database.pk_resolution_index_present("Person"));

    let mut after_checkpoint = database.begin().unwrap();
    after_checkpoint.execute(&update_rank(1, 111)).unwrap();
    assert!(database.pk_resolution_index_present("Person"));
    let shifted = after_checkpoint
        .run(&people_scan())
        .unwrap()
        .rows
        .into_iter()
        .find(|row| row.first() == Some(&Value::Int64(1)))
        .unwrap();
    assert_eq!(shifted, person(1, "person-1", 111));
    after_checkpoint.abort();
}

fn write_checkpointed_fixture(path: &Path, row_count: usize) {
    let pager = Pager::create(path, PAGE_SIZE, FIXTURE_DB_ID).unwrap();
    let schema = people_schema();
    let types = schema
        .columns()
        .iter()
        .map(|column| column.ty)
        .collect::<Vec<_>>();
    let mut groups = Vec::new();
    for start in (0..row_count).step_by(NODE_GROUP_CAPACITY) {
        let end = row_count.min(start + NODE_GROUP_CAPACITY);
        let mut group = NodeGroup::new(types.clone()).unwrap();
        for id in start..end {
            group
                .push_row(person(id as i64, &format!("person-{id}"), id as i64))
                .unwrap();
        }
        groups.push(group.write(&pager).unwrap());
    }
    let mut catalog = Catalog::default();
    catalog.add_node_table(schema).unwrap();
    catalog
        .set_table_storage("Person", TableStorage { groups })
        .unwrap();
    catalog.save(&pager, 1).unwrap();
}

fn create_people_database(path: &Path) -> Database {
    let mut database = Database::create(path, PAGE_SIZE).unwrap();
    database
        .execute(&Statement::CreateNodeTable {
            name: "Person".into(),
            columns: people_schema().columns().to_vec(),
        })
        .unwrap();
    database
}

fn people_schema() -> NodeTableSchema {
    NodeTableSchema::new(
        "Person".into(),
        vec![
            column("id", LogicalType::Int64, true),
            column("name", LogicalType::String, false),
            column("rank", LogicalType::Int64, false),
        ],
    )
    .unwrap()
}

fn insert_people(rows: &[(i64, &str, i64)]) -> Statement {
    Statement::InsertNode {
        table: "Person".into(),
        rows: rows
            .iter()
            .map(|(id, name, rank)| person(*id, name, *rank))
            .collect(),
    }
}

fn update_name(id: i64, name: &str) -> Statement {
    Statement::UpdateNode {
        table: "Person".into(),
        set: vec![SetItem {
            column: "name".into(),
            value: Value::String(name.into()),
        }],
        key_column: "id".into(),
        key: Value::Int64(id),
    }
}

fn update_rank(id: i64, rank: i64) -> Statement {
    Statement::UpdateNode {
        table: "Person".into(),
        set: vec![SetItem {
            column: "rank".into(),
            value: Value::Int64(rank),
        }],
        key_column: "id".into(),
        key: Value::Int64(id),
    }
}

fn delete_person(id: i64) -> Statement {
    Statement::DeleteNode {
        table: "Person".into(),
        key_column: "id".into(),
        key: Value::Int64(id),
    }
}

fn create_knows() -> Statement {
    Statement::CreateRelTable {
        name: "Knows".into(),
        from: "Person".into(),
        to: "Person".into(),
        columns: Vec::new(),
    }
}

fn insert_edges(edges: &[(i64, i64)]) -> Statement {
    Statement::InsertRel {
        table: "Knows".into(),
        rows: edges
            .iter()
            .map(|(from, to)| RelRow {
                from_key: Value::Int64(*from),
                to_key: Value::Int64(*to),
                values: Vec::new(),
            })
            .collect(),
    }
}

fn people_scan() -> Plan {
    Plan::from_json(r#"{"v":0,"plan":{"op":"ScanNodes","table":"Person","binding":"p"}}"#).unwrap()
}

fn sorted_people(mut rows: Vec<Vec<Value>>) -> Vec<Vec<Value>> {
    rows.sort_by_key(|row| match row.first() {
        Some(Value::Int64(id)) => *id,
        other => panic!("unexpected primary key: {other:?}"),
    });
    rows
}

fn people(rows: &[(i64, &str, i64)]) -> Vec<Vec<Value>> {
    rows.iter()
        .map(|(id, name, rank)| person(*id, name, *rank))
        .collect()
}

fn person(id: i64, name: &str, rank: i64) -> Vec<Value> {
    vec![
        Value::Int64(id),
        Value::String(name.into()),
        Value::Int64(rank),
    ]
}

fn column(name: &str, ty: LogicalType, primary_key: bool) -> Column {
    Column {
        name: name.into(),
        ty,
        primary_key,
    }
}

#[test]
fn pk_index_survives_ordinary_commits_and_rebuilds_at_checkpoint() {
    // Ordinary-commit chains punctuated by checkpoints must build the map once
    // across the chain
    // (adoption at the chain-Some publish) and rebuild after a checkpoint
    // (no adoption at the chain-None publish). Rebuilding per commit would
    // fail the small-read assertion below.
    let directory = tempdir().unwrap();
    let path = directory.path().join("epoch-carry.devondb");
    write_checkpointed_fixture(&path, 3 * NODE_GROUP_CAPACITY);
    let mut database = Database::open(&path).unwrap();

    // First resolution builds the map: one full group walk.
    database.reset_page_read_count();
    let mut transaction = database.begin().unwrap();
    transaction
        .execute(&update_name(1, "build-trigger"))
        .unwrap();
    transaction.commit().unwrap();
    let build_reads = database.page_read_count();
    assert!(database.pk_resolution_index_present("Person"));

    // N ordinary commits, each resolving a DISTINCT checkpointed key:
    // adoption must keep every resolution at O(1) page reads — far under
    // one build's worth EACH, and far under N builds total.
    const ORDINARY_COMMITS: u64 = 8;
    database.reset_page_read_count();
    for index in 0..ORDINARY_COMMITS {
        let key = (index + 2) as i64 * 17;
        let mut transaction = database.begin().unwrap();
        transaction
            .execute(&update_name(key, &format!("tick-{index}")))
            .unwrap();
        transaction.commit().unwrap();
    }
    let chain_reads = database.page_read_count();
    assert!(
        chain_reads < build_reads,
        "{ORDINARY_COMMITS} post-build commit resolutions read {chain_reads} pages; \
         one build read {build_reads} — the map is being rebuilt per commit"
    );

    // A checkpoint publishes fresh storage maps: the next resolution MUST
    // rebuild (reads on the order of a build, not an adopted lookup).
    database.checkpoint().unwrap();
    database.reset_page_read_count();
    let mut transaction = database.begin().unwrap();
    transaction
        .execute(&update_name(3, "post-checkpoint"))
        .unwrap();
    transaction.commit().unwrap();
    let post_checkpoint_reads = database.page_read_count();
    assert!(
        post_checkpoint_reads > chain_reads,
        "post-checkpoint resolution read {post_checkpoint_reads} pages vs {chain_reads} \
         across the whole ordinary chain — the map wrongly survived the checkpoint"
    );

    // The rows all landed, across both epochs.
    let rows = database.run(&people_scan()).unwrap().rows;
    assert_eq!(rows.len(), 3 * NODE_GROUP_CAPACITY);
}

#[test]
fn map_build_reads_fewer_pages_than_a_full_scan() {
    // The build decodes only the primary-key column. The fixture's three
    // columns (Int64 pk, String name, Int64 age) split payload pages
    // roughly a third each plus directories, so a pk-only build must read
    // strictly under half of what a full scan reads. Decoding every column's
    // payload pages would fail this assertion.
    let directory = tempdir().unwrap();
    let path = directory.path().join("pruned-build.devondb");
    write_checkpointed_fixture(&path, 3 * NODE_GROUP_CAPACITY);
    let mut database = Database::open(&path).unwrap();

    database.reset_page_read_count();
    let full_rows = database.run(&people_scan()).unwrap().rows;
    let full_scan_reads = database.page_read_count();
    assert_eq!(full_rows.len(), 3 * NODE_GROUP_CAPACITY);

    // A fresh open so the page cache does not subsidize the build's reads.
    drop(database);
    let database = Database::open(&path).unwrap();
    database.reset_page_read_count();
    let mut transaction = database.begin().unwrap();
    transaction
        .execute(&update_name(7, "pruned-build"))
        .unwrap();
    transaction.abort();
    let build_and_row_reads = database.page_read_count();
    assert!(database.pk_resolution_index_present("Person"));

    // Second update in a DIFFERENT group (the retained decoded group would
    // otherwise absorb the row read entirely): the map exists, so this pays
    // only the single-row group read. The difference isolates the BUILD.
    database.reset_page_read_count();
    let mut transaction = database.begin().unwrap();
    transaction
        .execute(&update_name(
            (2 * NODE_GROUP_CAPACITY + 9) as i64,
            "post-build",
        ))
        .unwrap();
    transaction.abort();
    let row_read_reads = database.page_read_count();

    let build_reads = build_and_row_reads.saturating_sub(row_read_reads);
    assert!(
        build_reads.saturating_mul(2) < full_scan_reads,
        "pk-only build read ~{build_reads} pages ({build_and_row_reads} incl. row read, \
         {row_read_reads} row read alone) vs {full_scan_reads} for a full scan — \
         the build is still decoding every column"
    );
}
