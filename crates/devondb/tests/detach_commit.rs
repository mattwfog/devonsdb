use std::path::{Path, PathBuf};

use devondb::{
    Database, DevonError, Plan, Statement,
    text::parser::{Parsed, parse},
};
use devondb_plan::{expr::Metric, statement::RelRow};
use devondb_storage::{
    pager::Pager,
    superblock::{DML_WAL_FLAG, REL_TOMBSTONE_WAL_FLAG, RESERVED_READ_SAFE_FLAG},
    txn_log::{RelEndpoint, WalPayload, decode_payload},
    wal::replay,
};
use devondb_types::{logical_type::LogicalType, schema::Column, value::Value};
use tempfile::tempdir;

const PAGE_SIZE: u32 = 4096;

#[test]
fn detach_removes_node_and_incident_edges_in_txn_fresh_and_recovered_views() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("round-trip.devondb");
    let mut database = graph_database(&path, &[1, 2, 3]);
    insert_edges(&mut database, &[(1, 2), (2, 1), (2, 3), (3, 1)]);
    database.checkpoint().unwrap();
    let old = database.snapshot();

    let mut transaction = database.begin().unwrap();
    transaction.execute(&detach("Person", 1)).unwrap();
    assert_eq!(
        node_ids(transaction.run(&node_scan()).unwrap().rows),
        [2, 3]
    );
    assert_eq!(
        edge_pairs(transaction.run(&edge_scan()).unwrap().rows),
        [(2, 3)]
    );
    transaction.commit().unwrap();

    assert_eq!(node_ids(database.run(&node_scan()).unwrap().rows), [2, 3]);
    assert_eq!(
        edge_pairs(database.run(&edge_scan()).unwrap().rows),
        [(2, 3)]
    );
    assert_eq!(node_ids(old.run(&node_scan()).unwrap().rows), [1, 2, 3]);
    assert_eq!(
        edge_pairs(old.run(&edge_scan()).unwrap().rows),
        [(1, 2), (2, 1), (2, 3), (3, 1)]
    );
    let flags = Pager::open(&path).unwrap().superblock().feature_flags;
    assert_eq!(
        flags & (DML_WAL_FLAG | REL_TOMBSTONE_WAL_FLAG),
        DML_WAL_FLAG | REL_TOMBSTONE_WAL_FLAG
    );
    assert_canonical_rel_delete_phase(&path);

    drop(old);
    drop(database);
    let mut recovered = Database::open(&path).unwrap();
    assert_eq!(node_ids(recovered.run(&node_scan()).unwrap().rows), [2, 3]);
    assert_eq!(
        edge_pairs(recovered.run(&edge_scan()).unwrap().rows),
        [(2, 3)]
    );
}

#[test]
fn statement_order_folds_earlier_edges_and_rejects_later_edges() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("statement-order.devondb");
    let mut database = graph_database(&path, &[1, 2]);

    let mut before = database.begin().unwrap();
    before.execute(&insert_edge(1, 2)).unwrap();
    before.execute(&detach("Person", 1)).unwrap();
    assert!(before.run(&edge_scan()).unwrap().rows.is_empty());
    before.commit().unwrap();
    assert!(database.run(&edge_scan()).unwrap().rows.is_empty());

    let path = directory.path().join("later-edge.devondb");
    let database = graph_database(&path, &[1, 2]);
    let mut after = database.begin().unwrap();
    after.execute(&detach("Person", 1)).unwrap();
    let error = after.execute(&insert_edge(1, 2)).unwrap_err();
    assert!(
        matches!(error, DevonError::NotFound { .. }),
        "unexpected error: {error}"
    );
    after.abort();
}

#[test]
fn own_insert_detach_folds_node_edges_and_wal_to_empty() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("own-insert.devondb");
    let mut database = graph_database(&path, &[1]);
    let wal = wal_path(&path);
    let before_len = std::fs::metadata(&wal).unwrap().len();
    let mut transaction = database.begin().unwrap();
    transaction.execute(&insert_nodes(&[2])).unwrap();
    transaction.execute(&insert_edge(2, 1)).unwrap();
    transaction.execute(&detach("Person", 2)).unwrap();
    assert_eq!(node_ids(transaction.run(&node_scan()).unwrap().rows), [1]);
    assert!(transaction.run(&edge_scan()).unwrap().rows.is_empty());
    transaction.commit().unwrap();

    assert_eq!(std::fs::metadata(wal).unwrap().len(), before_len);
    assert_eq!(node_ids(database.run(&node_scan()).unwrap().rows), [1]);
}

#[test]
fn edge_insert_and_detach_conflict_in_both_commit_orders() {
    for detach_wins in [true, false] {
        let directory = tempdir().unwrap();
        let path = directory.path().join("conflict.devondb");
        let mut database = graph_database(&path, &[1, 2]);
        let mut detacher = database.begin().unwrap();
        let mut inserter = database.begin().unwrap();
        detacher.execute(&detach("Person", 1)).unwrap();
        inserter.execute(&insert_edge(1, 2)).unwrap();

        let error = if detach_wins {
            detacher.commit().unwrap();
            inserter.commit().unwrap_err()
        } else {
            inserter.commit().unwrap();
            // Conflict-only endpoint claims outlive the checkpoint-cleared
            // overlay while the older detacher still pins its snapshot.
            database.checkpoint().unwrap();
            detacher.commit().unwrap_err()
        };
        assert!(matches!(error, DevonError::TransactionConflict { .. }));
        assert!(error.to_string().contains("detach/incident-edge insert"));
    }
}

#[test]
fn zero_degree_referenced_and_unreferenced_detach_are_legal() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("zero-degree.devondb");
    let mut database = graph_database(&path, &[1, 2]);
    database.execute(&detach("Person", 1)).unwrap();
    assert_eq!(node_ids(database.run(&node_scan()).unwrap().rows), [2]);

    let path = directory.path().join("unreferenced.devondb");
    let mut database = Database::create(&path, PAGE_SIZE).unwrap();
    database.execute(&create_nodes("Solo", false)).unwrap();
    database.execute(&insert_into("Solo", &[7])).unwrap();
    database.execute(&detach("Solo", 7)).unwrap();
    assert!(database.run(&scan("Solo", "s")).unwrap().rows.is_empty());
    let records = replay(wal_path(&path)).unwrap();
    assert!(
        !records.iter().any(|(_, bytes)| {
            matches!(decode_payload(bytes), Ok(WalPayload::RelDelete { .. }))
        })
    );
}

#[test]
fn hnsw_detach_preserves_create_sole_statement_rule() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("other-endpoint-index.devondb");
    let mut database = Database::create(&path, PAGE_SIZE).unwrap();
    database.execute(&create_nodes("A", true)).unwrap();
    database.execute(&create_nodes("B", true)).unwrap();
    database.execute(&insert_vector_node("A", 1)).unwrap();
    database.execute(&insert_vector_node("B", 1)).unwrap();
    database
        .execute(&Statement::CreateRelTable {
            name: "R".into(),
            from: "A".into(),
            to: "B".into(),
            columns: Vec::new(),
        })
        .unwrap();
    database
        .execute(&Statement::CreateHnswIndex {
            name: "b_idx".into(),
            table: "B".into(),
            column: "embedding".into(),
            metric: Metric::L2,
        })
        .unwrap();
    database.execute(&detach("A", 1)).unwrap();

    let path = directory.path().join("pending-target-index.devondb");
    let database = vector_database(&path);
    let mut transaction = database.begin().unwrap();
    transaction
        .execute(&Statement::CreateHnswIndex {
            name: "a_idx".into(),
            table: "A".into(),
            column: "embedding".into(),
            metric: Metric::L2,
        })
        .unwrap();
    let error = transaction.execute(&detach("A", 1)).unwrap_err();
    assert!(error.to_string().contains("only statement"));
    transaction.abort();
}

#[test]
fn writable_reopen_heals_empty_two_bit_state() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("heal.devondb");
    let database = Database::create(&path, PAGE_SIZE).unwrap();
    drop(database);
    let pager = Pager::open(&path).unwrap();
    pager
        .commit_feature_flags(DML_WAL_FLAG | REL_TOMBSTONE_WAL_FLAG)
        .unwrap();
    drop(pager);

    let database = Database::open(&path).unwrap();
    drop(database);
    let flags = Pager::open(&path).unwrap().superblock().feature_flags;
    assert_eq!(flags & (DML_WAL_FLAG | REL_TOMBSTONE_WAL_FLAG), 0);

    let path = directory.path().join("read-only-does-not-heal.devondb");
    let database = Database::create(&path, PAGE_SIZE).unwrap();
    drop(database);
    let pager = Pager::open(&path).unwrap();
    pager
        .commit_feature_flags(DML_WAL_FLAG | REL_TOMBSTONE_WAL_FLAG | RESERVED_READ_SAFE_FLAG)
        .unwrap();
    drop(pager);

    let mut database = Database::open(&path).unwrap();
    assert!(matches!(
        database.execute(&create_nodes("NoWrite", false)),
        Err(DevonError::ReadOnly { .. })
    ));
    drop(database);
    let flags = Pager::open(&path).unwrap().superblock().feature_flags;
    assert_eq!(
        flags & (DML_WAL_FLAG | REL_TOMBSTONE_WAL_FLAG),
        DML_WAL_FLAG | REL_TOMBSTONE_WAL_FLAG
    );
}

fn graph_database(path: &Path, ids: &[i64]) -> Database {
    let mut database = Database::create(path, PAGE_SIZE).unwrap();
    database.execute(&create_nodes("Person", false)).unwrap();
    database
        .execute(&Statement::CreateRelTable {
            name: "Knows".into(),
            from: "Person".into(),
            to: "Person".into(),
            columns: Vec::new(),
        })
        .unwrap();
    database.execute(&insert_nodes(ids)).unwrap();
    database
}

fn vector_database(path: &Path) -> Database {
    let mut database = Database::create(path, PAGE_SIZE).unwrap();
    database.execute(&create_nodes("A", true)).unwrap();
    database.execute(&insert_vector_node("A", 1)).unwrap();
    database
}

fn create_nodes(table: &str, vector: bool) -> Statement {
    let mut columns = vec![column("id", LogicalType::Int64, true)];
    if vector {
        columns.push(column("embedding", LogicalType::Vector { dim: 2 }, false));
    }
    Statement::CreateNodeTable {
        name: table.into(),
        columns,
    }
}

fn insert_nodes(ids: &[i64]) -> Statement {
    insert_into("Person", ids)
}

fn insert_into(table: &str, ids: &[i64]) -> Statement {
    Statement::InsertNode {
        table: table.into(),
        rows: ids.iter().map(|id| vec![Value::Int64(*id)]).collect(),
    }
}

fn insert_vector_node(table: &str, id: i64) -> Statement {
    Statement::InsertNode {
        table: table.into(),
        rows: vec![vec![Value::Int64(id), Value::Vector(vec![1.0, 2.0])]],
    }
}

fn insert_edges(database: &mut Database, edges: &[(i64, i64)]) {
    database.execute(&insert_edge_rows(edges)).unwrap();
}

fn insert_edge(from: i64, to: i64) -> Statement {
    insert_edge_rows(&[(from, to)])
}

fn insert_edge_rows(edges: &[(i64, i64)]) -> Statement {
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

fn scan(table: &str, binding: &str) -> Plan {
    plan(&format!("nodes({table}) as {binding}"))
}

fn node_scan() -> Plan {
    scan("Person", "p")
}

fn edge_scan() -> Plan {
    plan("nodes(Person) as p | expand Knows out as q | project p.id as source, q.id as target")
}

fn plan(input: &str) -> Plan {
    let Parsed::Query(plan) = parse(input).unwrap() else {
        panic!("expected query")
    };
    plan
}

fn node_ids(rows: Vec<Vec<Value>>) -> Vec<i64> {
    let mut ids = rows
        .into_iter()
        .map(|row| match row.first() {
            Some(Value::Int64(id)) => *id,
            other => panic!("unexpected node row {other:?}"),
        })
        .collect::<Vec<_>>();
    ids.sort_unstable();
    ids
}

fn edge_pairs(rows: Vec<Vec<Value>>) -> Vec<(i64, i64)> {
    let mut edges = rows
        .into_iter()
        .map(|row| match row.as_slice() {
            [Value::Int64(from), Value::Int64(to)] => (*from, *to),
            other => panic!("unexpected edge row {other:?}"),
        })
        .collect::<Vec<_>>();
    edges.sort_unstable();
    edges
}

fn assert_canonical_rel_delete_phase(path: &Path) {
    let payloads = replay(wal_path(path))
        .unwrap()
        .into_iter()
        .map(|(_, bytes)| decode_payload(&bytes).unwrap())
        .collect::<Vec<_>>();
    let rel_deletes = payloads
        .iter()
        .filter_map(|payload| match payload {
            WalPayload::RelDelete {
                rel,
                endpoint,
                offset,
            } => Some((rel.as_str(), *endpoint, *offset)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        rel_deletes,
        [
            ("Knows", RelEndpoint::From, 0),
            ("Knows", RelEndpoint::To, 0)
        ]
    );
    let rel_delete_index = payloads
        .iter()
        .position(|payload| matches!(payload, WalPayload::RelDelete { .. }))
        .unwrap();
    let node_delete_index = payloads
        .iter()
        .position(|payload| matches!(payload, WalPayload::NodeDelete { .. }))
        .unwrap();
    assert!(rel_delete_index < node_delete_index);
}

fn wal_path(path: &Path) -> PathBuf {
    let mut path = path.as_os_str().to_os_string();
    path.push("-wal");
    PathBuf::from(path)
}
