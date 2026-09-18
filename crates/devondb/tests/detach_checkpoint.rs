use std::path::{Path, PathBuf};

use devondb::{Database, DevonError, Options, Plan, Statement};
use devondb_plan::{
    statement::RelRow,
    text::parser::{Parsed, parse},
};
use devondb_storage::{
    catalog::Catalog,
    csr_group::csr_page_inventory,
    free_pages::prospective_page_count,
    pager::Pager,
    superblock::{DML_WAL_FLAG, REL_TOMBSTONE_WAL_FLAG},
};
use devondb_types::{logical_type::LogicalType, schema::Column, value::Value};
use tempfile::tempdir;

const PAGE_SIZE: u32 = 4096;

#[test]
fn zero_degree_delete_remaps_distant_edges_and_keeps_old_snapshot_pages_stable() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("zero-degree-remap.devondb");
    let node_ids = (0..2051).collect::<Vec<_>>();
    let mut database = self_graph(&path, &node_ids, &["Knows"]);
    insert_edges(&mut database, "Knows", &[(2048, 2050), (2050, 2048)], None);
    database.checkpoint().unwrap();
    let old_snapshot = database.snapshot();

    let pager = Pager::open(&path).unwrap();
    let catalog = Catalog::load(&pager).unwrap();
    let old_group = catalog.rel_storage("Knows").unwrap().fwd[1];
    let old_pages = csr_page_inventory(&pager, old_group, &[])
        .unwrap()
        .into_iter()
        .map(|page_id| (page_id, pager.read_page(page_id).unwrap()))
        .collect::<Vec<_>>();
    drop(pager);

    database.execute(&detach("Person", 0)).unwrap();
    database.checkpoint().unwrap();

    let expected = [(2048, 2050), (2050, 2048)];
    assert_eq!(
        pairs(database.run(&out_scan("Knows")).unwrap().rows),
        expected
    );
    assert_eq!(
        pairs(database.run(&in_scan("Knows")).unwrap().rows),
        expected
    );
    let fresh_ids = ids(database.run(&node_scan()).unwrap().rows);
    assert_eq!(fresh_ids.len(), 2050);
    assert_eq!(fresh_ids.first(), Some(&1));
    assert_eq!(fresh_ids.last(), Some(&2050));
    assert_eq!(
        pairs(old_snapshot.run(&out_scan("Knows")).unwrap().rows),
        expected
    );
    let old_ids = ids(old_snapshot.run(&node_scan()).unwrap().rows);
    assert_eq!(old_ids.len(), 2051);
    assert_eq!(old_ids.first(), Some(&0));

    let pager = Pager::open(&path).unwrap();
    for (page_id, bytes) in old_pages {
        assert_eq!(pager.read_page(page_id).unwrap(), bytes);
    }
    assert_eq!(
        pager.superblock().feature_flags & (DML_WAL_FLAG | REL_TOMBSTONE_WAL_FLAG),
        0
    );
    assert_eq!(std::fs::metadata(wal_path(&path)).unwrap().len(), 0);
    drop(pager);
    drop(old_snapshot);
    drop(database);

    let mut reopened = Database::open(&path).unwrap();
    assert_eq!(
        pairs(reopened.run(&out_scan("Knows")).unwrap().rows),
        expected
    );
    assert_eq!(
        pairs(reopened.run(&in_scan("Knows")).unwrap().rows),
        expected
    );
}

#[test]
fn incident_matrix_disappears_in_both_directions_after_checkpoint_reopen() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("matrix.devondb");
    let mut database = self_graph(&path, &[1, 2, 3, 4, 5], &["Knows", "Likes"]);
    insert_edges(
        &mut database,
        "Knows",
        &[(2, 1), (1, 2), (2, 2), (2, 3), (2, 3), (3, 4), (4, 5)],
        None,
    );
    insert_edges(&mut database, "Likes", &[(5, 2), (1, 3), (3, 1)], None);
    database.checkpoint().unwrap();
    database.execute(&detach("Person", 2)).unwrap();
    database.checkpoint().unwrap();
    drop(database);

    let mut reopened = Database::open(&path).unwrap();
    let knows = [(3, 4), (4, 5)];
    let likes = [(1, 3), (3, 1)];
    assert_eq!(pairs(reopened.run(&out_scan("Knows")).unwrap().rows), knows);
    assert_eq!(pairs(reopened.run(&in_scan("Knows")).unwrap().rows), knows);
    assert_eq!(pairs(reopened.run(&out_scan("Likes")).unwrap().rows), likes);
    assert_eq!(pairs(reopened.run(&in_scan("Likes")).unwrap().rows), likes);
}

#[test]
fn both_endpoint_roles_remap_heterogeneous_csr_in_both_directions() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("both-role-remap.devondb");
    let mut database = Database::create(&path, PAGE_SIZE).unwrap();
    create_people(&mut database, &[1, 2, 3, 4]);
    database
        .execute(&Statement::CreateNodeTable {
            name: "Company".into(),
            columns: vec![column("id", LogicalType::Int64, true)],
        })
        .unwrap();
    database
        .execute(&Statement::InsertNode {
            table: "Company".into(),
            rows: [10, 20, 30, 40]
                .into_iter()
                .map(|id| vec![Value::Int64(id)])
                .collect(),
        })
        .unwrap();
    database
        .execute(&Statement::CreateRelTable {
            name: "WorksAt".into(),
            from: "Person".into(),
            to: "Company".into(),
            columns: Vec::new(),
        })
        .unwrap();
    database
        .execute(&Statement::CreateRelTable {
            name: "Sponsors".into(),
            from: "Company".into(),
            to: "Person".into(),
            columns: Vec::new(),
        })
        .unwrap();
    insert_edges(
        &mut database,
        "WorksAt",
        &[(1, 10), (2, 40), (3, 30), (4, 40)],
        None,
    );
    insert_edges(
        &mut database,
        "Sponsors",
        &[(10, 1), (40, 2), (30, 3), (40, 4)],
        None,
    );
    database.checkpoint().unwrap();

    database.execute(&detach("Person", 2)).unwrap();
    database.checkpoint().unwrap();
    drop(database);

    let mut reopened = Database::open(&path).unwrap();
    let works_at = [(1, 10), (3, 30), (4, 40)];
    let sponsors = [(10, 1), (30, 3), (40, 4)];
    assert_eq!(
        pairs(
            reopened
                .run(&out_scan_from("Person", "WorksAt"))
                .unwrap()
                .rows
        ),
        works_at
    );
    assert_eq!(
        pairs(
            reopened
                .run(&in_scan_from("Company", "WorksAt"))
                .unwrap()
                .rows
        ),
        works_at
    );
    assert_eq!(
        pairs(
            reopened
                .run(&out_scan_from("Company", "Sponsors"))
                .unwrap()
                .rows
        ),
        sponsors
    );
    assert_eq!(
        pairs(
            reopened
                .run(&in_scan_from("Person", "Sponsors"))
                .unwrap()
                .rows
        ),
        sponsors
    );
}

#[test]
fn reinsertion_tail_maps_after_the_deleted_overlay_hole() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("reinsertion-tail.devondb");
    let mut database = self_graph(&path, &[1, 2, 3], &["Knows"]);
    insert_edges(&mut database, "Knows", &[(3, 2)], None);
    database.checkpoint().unwrap();

    database.execute(&detach("Person", 2)).unwrap();
    database
        .execute(&Statement::InsertNode {
            table: "Person".into(),
            rows: vec![vec![Value::Int64(2)]],
        })
        .unwrap();
    insert_edges(&mut database, "Knows", &[(1, 2)], None);
    database.checkpoint().unwrap();
    drop(database);

    let mut reopened = Database::open(&path).unwrap();
    assert_eq!(ids(reopened.run(&node_scan()).unwrap().rows), [1, 3, 2]);
    assert_eq!(
        pairs(reopened.run(&out_scan("Knows")).unwrap().rows),
        [(1, 2)]
    );
    assert_eq!(
        pairs(reopened.run(&in_scan("Knows")).unwrap().rows),
        [(1, 2)]
    );
}

#[test]
fn blocked_group_backs_off_and_failed_generation_retires_on_success() {
    const LOW_LIMIT: usize = 1024 * 1024;
    const HIGH_LIMIT: usize = 32 * LOW_LIMIT;
    const EDGE_COUNT: i64 = 450;
    const PROPERTY_BYTES: usize = 3072;

    let directory = tempdir().unwrap();
    let path = directory.path().join("blocked.devondb");
    let mut database = Database::create_with(
        &path,
        Options {
            page_size: PAGE_SIZE,
            memory_limit: HIGH_LIMIT,
        },
    )
    .unwrap();
    create_people(&mut database, &(0..EDGE_COUNT + 2).collect::<Vec<_>>());
    database
        .execute(&Statement::CreateRelTable {
            name: "Heavy".into(),
            from: "Person".into(),
            to: "Person".into(),
            columns: vec![column("payload", LogicalType::String, false)],
        })
        .unwrap();
    let rows = (2..EDGE_COUNT + 2)
        .map(|from| RelRow {
            from_key: Value::Int64(from),
            to_key: Value::Int64(1),
            values: vec![Value::String("x".repeat(PROPERTY_BYTES))],
        })
        .collect();
    database
        .execute(&Statement::InsertRel {
            table: "Heavy".into(),
            rows,
        })
        .unwrap();
    database.checkpoint().unwrap();
    drop(database);

    let mut database = Database::open_with(
        &path,
        Options {
            page_size: PAGE_SIZE,
            memory_limit: LOW_LIMIT,
        },
    )
    .unwrap();
    database.execute(&detach("Person", 0)).unwrap();
    let error = database.checkpoint().unwrap_err();
    let DevonError::BudgetExceeded { context } = &error else {
        panic!("expected BudgetExceeded, got {error}");
    };
    for field in [
        "relationship=Heavy",
        "direction=fwd",
        "group=0",
        "requested=",
        "charged=",
        "limit=",
    ] {
        assert!(context.contains(field), "missing `{field}` in {context}");
    }
    let db_id = Pager::open(&path).unwrap().superblock().db_id;
    let queued = prospective_page_count(db_id);
    assert!(queued > 0, "failed node/CSR pages must be queued");
    let failed_len = std::fs::metadata(&path).unwrap().len();

    let retry = database.checkpoint().unwrap_err();
    assert!(matches!(retry, DevonError::BudgetExceeded { .. }));
    assert_eq!(std::fs::metadata(&path).unwrap().len(), failed_len);
    assert_eq!(prospective_page_count(db_id), queued);
    assert!(std::fs::metadata(wal_path(&path)).unwrap().len() > 0);
    drop(database);

    let mut database = Database::open_with(
        &path,
        Options {
            page_size: PAGE_SIZE,
            memory_limit: HIGH_LIMIT,
        },
    )
    .unwrap();
    database.checkpoint().unwrap();
    assert_eq!(prospective_page_count(db_id), 0);
    let pager = Pager::open(&path).unwrap();
    assert!(pager.free_pages_extension().unwrap().retired_total >= queued as u64);
    assert_eq!(
        pager.superblock().feature_flags & (DML_WAL_FLAG | REL_TOMBSTONE_WAL_FLAG),
        0
    );
    assert_eq!(std::fs::metadata(wal_path(&path)).unwrap().len(), 0);
}

fn self_graph(path: &Path, node_ids: &[i64], relationships: &[&str]) -> Database {
    let mut database = Database::create(path, PAGE_SIZE).unwrap();
    create_people(&mut database, node_ids);
    for relationship in relationships {
        database
            .execute(&Statement::CreateRelTable {
                name: (*relationship).to_owned(),
                from: "Person".into(),
                to: "Person".into(),
                columns: Vec::new(),
            })
            .unwrap();
    }
    database
}

fn create_people(database: &mut Database, node_ids: &[i64]) {
    database
        .execute(&Statement::CreateNodeTable {
            name: "Person".into(),
            columns: vec![column("id", LogicalType::Int64, true)],
        })
        .unwrap();
    database
        .execute(&Statement::InsertNode {
            table: "Person".into(),
            rows: node_ids.iter().map(|id| vec![Value::Int64(*id)]).collect(),
        })
        .unwrap();
}

fn insert_edges(
    database: &mut Database,
    relationship: &str,
    edges: &[(i64, i64)],
    property: Option<Value>,
) {
    database
        .execute(&Statement::InsertRel {
            table: relationship.to_owned(),
            rows: edges
                .iter()
                .map(|(from, to)| RelRow {
                    from_key: Value::Int64(*from),
                    to_key: Value::Int64(*to),
                    values: property.iter().cloned().collect(),
                })
                .collect(),
        })
        .unwrap();
}

fn detach(table: &str, id: i64) -> Statement {
    Statement::DetachDeleteNode {
        table: table.into(),
        key_column: "id".into(),
        key: Value::Int64(id),
    }
}

fn column(name: &str, ty: LogicalType, primary_key: bool) -> Column {
    Column {
        name: name.into(),
        ty,
        primary_key,
    }
}

fn node_scan() -> Plan {
    plan("nodes(Person) as p")
}

fn out_scan(relationship: &str) -> Plan {
    out_scan_from("Person", relationship)
}

fn out_scan_from(table: &str, relationship: &str) -> Plan {
    plan(&format!(
        "nodes({table}) as p | expand {relationship} out as q | project p.id as source, q.id as target"
    ))
}

fn in_scan(relationship: &str) -> Plan {
    in_scan_from("Person", relationship)
}

fn in_scan_from(table: &str, relationship: &str) -> Plan {
    plan(&format!(
        "nodes({table}) as p | expand {relationship} in as q | project q.id as source, p.id as target"
    ))
}

fn plan(text: &str) -> Plan {
    let Parsed::Query(plan) = parse(text).unwrap() else {
        panic!("expected query")
    };
    plan
}

fn ids(rows: Vec<Vec<Value>>) -> Vec<i64> {
    rows.into_iter()
        .map(|row| match row.as_slice() {
            [Value::Int64(id)] => *id,
            other => panic!("unexpected node row {other:?}"),
        })
        .collect()
}

fn pairs(rows: Vec<Vec<Value>>) -> Vec<(i64, i64)> {
    let mut pairs = rows
        .into_iter()
        .map(|row| match row.as_slice() {
            [Value::Int64(from), Value::Int64(to)] => (*from, *to),
            other => panic!("unexpected relationship row {other:?}"),
        })
        .collect::<Vec<_>>();
    pairs.sort_unstable();
    pairs
}

fn wal_path(path: &Path) -> PathBuf {
    let mut path = path.as_os_str().to_os_string();
    path.push("-wal");
    PathBuf::from(path)
}
