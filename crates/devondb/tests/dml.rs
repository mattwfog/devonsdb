use std::{
    ffi::OsString,
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::Duration,
};

use devondb::{Database, DevonError, Plan, Statement};
use devondb_plan::{expr::Metric, statement::SetItem};
use devondb_storage::{node_group::NODE_GROUP_CAPACITY, pager::Pager, superblock::DML_WAL_FLAG};
use devondb_types::{logical_type::LogicalType, schema::Column, value::Value};
use tempfile::tempdir;

const PAGE_SIZE: u32 = 4096;
const CRASH_CHILD_ENV: &str = "DEVONDB_DML_CRASH_CHILD";
const CRASH_PATH_ENV: &str = "DEVONDB_DML_CRASH_PATH";

#[test]
fn update_delete_are_visible_across_snapshots_checkpoints_and_reopen() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("visibility.devondb");
    let mut database = people_database(&path, &[(1, "Ada"), (2, "Grace")]);
    database.checkpoint().unwrap();

    let before_update = database.snapshot();
    database.execute(&update_name(1, "Augusta")).unwrap();
    assert_eq!(
        person_rows(&mut database),
        rows(&[(1, "Augusta"), (2, "Grace")])
    );
    assert_eq!(
        sorted_rows(before_update.run(&people_scan()).unwrap().rows),
        rows(&[(1, "Ada"), (2, "Grace")])
    );

    database.checkpoint().unwrap();
    assert_eq!(
        person_rows(&mut database),
        rows(&[(1, "Augusta"), (2, "Grace")])
    );
    let before_delete = database.snapshot();
    database.execute(&delete_person(2)).unwrap();
    assert_eq!(person_rows(&mut database), rows(&[(1, "Augusta")]));
    assert_eq!(
        sorted_rows(before_delete.run(&people_scan()).unwrap().rows),
        rows(&[(1, "Augusta"), (2, "Grace")])
    );

    database.checkpoint().unwrap();
    drop(database);
    let mut reopened = Database::open(&path).unwrap();
    assert_eq!(person_rows(&mut reopened), rows(&[(1, "Augusta")]));
}

#[test]
fn own_insert_updates_fold_and_own_insert_delete_disappears() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("own-writes.devondb");
    let mut database = people_database(&path, &[]);
    let mut transaction = database.begin().unwrap();
    transaction
        .execute(&insert_people(&[(1, "one"), (2, "doomed")]))
        .unwrap();
    transaction.execute(&update_name(1, "two")).unwrap();
    transaction.execute(&update_name(1, "three")).unwrap();
    transaction.execute(&delete_person(2)).unwrap();
    assert_eq!(
        transaction.run(&people_scan()).unwrap().rows,
        rows(&[(1, "three")])
    );
    transaction.commit().unwrap();
    assert_eq!(person_rows(&mut database), rows(&[(1, "three")]));
}

#[test]
fn same_pk_dml_conflict_matrix_is_first_committer_wins() {
    run_conflict_case(
        update_name(1, "winner"),
        update_name(1, "loser"),
        Some("winner"),
    );
    run_conflict_case(update_name(1, "winner"), delete_person(1), Some("winner"));
    run_conflict_case(delete_person(1), delete_person(1), None);
}

#[test]
fn concurrent_insert_conflicts_with_an_older_update_of_the_same_pk() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("insert-update-conflict.devondb");
    let mut database = people_database(&path, &[(1, "old")]);
    database.checkpoint().unwrap();
    let mut old_update = database.begin().unwrap();
    old_update.execute(&update_name(1, "stale")).unwrap();
    database.execute(&delete_person(1)).unwrap();
    database.execute(&insert_people(&[(1, "winner")])).unwrap();

    assert_conflict(old_update.commit().unwrap_err());
    assert_eq!(person_rows(&mut database), rows(&[(1, "winner")]));
}

#[test]
fn dml_refusal_matrix_reports_honest_errors_and_suggestions() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("refusals.devondb");
    let mut database = people_database(&path, &[(1, "Ada")]);

    assert_error_contains(
        database.execute(&Statement::UpdateNode {
            table: "Person".into(),
            set: vec![set("name", Value::String("x".into()))],
            key_column: "name".into(),
            key: Value::String("Ada".into()),
        }),
        "predicate-driven bulk DML",
    );
    assert_error_contains(
        database.execute(&Statement::UpdateNode {
            table: "Person".into(),
            set: vec![set("id", Value::Int64(2))],
            key_column: "id".into(),
            key: Value::Int64(1),
        }),
        "primary key column `id`",
    );
    assert_error_contains(
        database.execute(&update_for("Persn", "name", 1)),
        "did you mean `Person`",
    );
    assert_error_contains(
        database.execute(&update_for("Person", "nme", 1)),
        "did you mean `name`",
    );
    assert_error_contains(
        database.execute(&Statement::DeleteNode {
            table: "Person".into(),
            key_column: "idd".into(),
            key: Value::Int64(1),
        }),
        "did you mean `id`",
    );
    assert_error_contains(database.execute(&update_name(999, "missing")), "not found");
    assert_error_contains(database.execute(&delete_person(999)), "not found");
    assert_error_contains(
        database.execute(&Statement::UpdateNode {
            table: "Person".into(),
            set: vec![set("name", Value::Int64(7))],
            key_column: "id".into(),
            key: Value::Int64(1),
        }),
        "expects String",
    );
}

#[test]
fn delete_refuses_any_relationship_endpoint_even_without_edges() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("endpoint-refusal.devondb");
    let mut database = people_database(&path, &[(1, "Ada")]);
    database.execute(&create_knows()).unwrap();
    assert_error_contains(database.execute(&delete_person(1)), "detach-delete");
}

#[test]
fn update_and_delete_hnsw_indexed_tables() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("hnsw-refusal.devondb");
    let mut database = vector_database(&path);
    database
        .execute(&Statement::CreateHnswIndex {
            name: "embedding_l2".into(),
            table: "Corpus".into(),
            column: "embedding".into(),
            metric: Metric::L2,
        })
        .unwrap();
    database
        .execute(&Statement::UpdateNode {
            table: "Corpus".into(),
            set: vec![set("embedding", Value::Vector(vec![2.0, 2.0]))],
            key_column: "id".into(),
            key: Value::Int64(1),
        })
        .unwrap();
    database
        .execute(&Statement::DeleteNode {
            table: "Corpus".into(),
            key_column: "id".into(),
            key: Value::Int64(1),
        })
        .unwrap();
    database.checkpoint().unwrap();
}

#[test]
fn zone_map_pruning_emits_replacement_from_a_pruned_group() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("zone-map.devondb");
    let mut database = Database::create(&path, PAGE_SIZE).unwrap();
    database.execute(&create_readings()).unwrap();
    let mut source = (0..NODE_GROUP_CAPACITY)
        .map(|id| vec![Value::Int64(id as i64), Value::Int64(0)])
        .collect::<Vec<_>>();
    source.push(vec![
        Value::Int64(NODE_GROUP_CAPACITY as i64),
        Value::Int64(100),
    ]);
    database
        .execute(&Statement::InsertNode {
            table: "Reading".into(),
            rows: source,
        })
        .unwrap();
    database.checkpoint().unwrap();
    database
        .execute(&Statement::UpdateNode {
            table: "Reading".into(),
            set: vec![set("score", Value::Int64(100))],
            key_column: "id".into(),
            key: Value::Int64(1),
        })
        .unwrap();

    let pruned = database.run(&score_plan(false)).unwrap().rows;
    let unpruned = database.run(&score_plan(true)).unwrap().rows;
    assert_eq!(pruned, unpruned);
    assert_eq!(pruned.len(), 2);
    assert!(
        pruned
            .iter()
            .any(|row| row.first() == Some(&Value::Int64(1)))
    );
}

#[test]
fn kill_after_dml_ack_recovers_and_checkpoint_clears_dml_wal_bit() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("crash.devondb");
    let mut database = people_database(&path, &[(1, "old"), (2, "doomed")]);
    database.checkpoint().unwrap();
    drop(database);

    let mut child = spawn_crash_child(&path);
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    read_until_ack(&mut stdout);
    child.kill().unwrap();
    assert!(!child.wait().unwrap().success());
    assert_child_stderr_empty(&mut child);

    let wal = wal_path(&path);
    assert!(std::fs::metadata(&wal).unwrap().len() > 0);
    let pager = Pager::open(&path).unwrap();
    assert_ne!(pager.superblock().feature_flags & DML_WAL_FLAG, 0);
    drop(pager);

    let mut recovered = Database::open(&path).unwrap();
    assert_eq!(person_rows(&mut recovered), rows(&[(1, "recovered")]));
    recovered.checkpoint().unwrap();
    drop(recovered);
    assert_eq!(std::fs::metadata(&wal).unwrap().len(), 0);
    let pager = Pager::open(&path).unwrap();
    assert_eq!(pager.superblock().feature_flags & DML_WAL_FLAG, 0);
}

#[test]
fn dml_crash_child_process() {
    if std::env::var_os(CRASH_CHILD_ENV).is_none() {
        return;
    }
    let path = PathBuf::from(std::env::var_os(CRASH_PATH_ENV).unwrap());
    let mut database = Database::open(path).unwrap();
    database.execute(&update_name(1, "recovered")).unwrap();
    database.execute(&delete_person(2)).unwrap();
    println!("DML_ACK");
    std::io::stdout().flush().unwrap();
    thread::sleep(Duration::from_secs(60));
}

fn run_conflict_case(first: Statement, second: Statement, winner_name: Option<&str>) {
    let directory = tempdir().unwrap();
    let path = directory.path().join("conflict.devondb");
    let mut database = people_database(&path, &[(1, "old")]);
    database.checkpoint().unwrap();
    let mut winner = database.begin().unwrap();
    let mut loser = database.begin().unwrap();
    winner.execute(&first).unwrap();
    loser.execute(&second).unwrap();
    winner.commit().unwrap();
    assert_conflict(loser.commit().unwrap_err());
    let expected = winner_name.map_or_else(Vec::new, |name| rows(&[(1, name)]));
    assert_eq!(person_rows(&mut database), expected);
}

fn people_database(path: &Path, initial: &[(i64, &str)]) -> Database {
    let mut database = Database::create(path, PAGE_SIZE).unwrap();
    database.execute(&create_people()).unwrap();
    if !initial.is_empty() {
        database.execute(&insert_people(initial)).unwrap();
    }
    database
}

fn vector_database(path: &Path) -> Database {
    let mut database = Database::create(path, PAGE_SIZE).unwrap();
    database
        .execute(&Statement::CreateNodeTable {
            name: "Corpus".into(),
            columns: vec![
                column("id", LogicalType::Int64, true),
                column("embedding", LogicalType::Vector { dim: 2 }, false),
            ],
        })
        .unwrap();
    database
        .execute(&Statement::InsertNode {
            table: "Corpus".into(),
            rows: vec![vec![Value::Int64(1), Value::Vector(vec![1.0, 1.0])]],
        })
        .unwrap();
    database
}

fn create_people() -> Statement {
    Statement::CreateNodeTable {
        name: "Person".into(),
        columns: vec![
            column("id", LogicalType::Int64, true),
            column("name", LogicalType::String, false),
        ],
    }
}

fn create_readings() -> Statement {
    Statement::CreateNodeTable {
        name: "Reading".into(),
        columns: vec![
            column("id", LogicalType::Int64, true),
            column("score", LogicalType::Int64, false),
        ],
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

fn insert_people(input: &[(i64, &str)]) -> Statement {
    Statement::InsertNode {
        table: "Person".into(),
        rows: rows(input),
    }
}

fn update_name(id: i64, name: &str) -> Statement {
    Statement::UpdateNode {
        table: "Person".into(),
        set: vec![set("name", Value::String(name.into()))],
        key_column: "id".into(),
        key: Value::Int64(id),
    }
}

fn update_for(table: &str, column: &str, id: i64) -> Statement {
    Statement::UpdateNode {
        table: table.into(),
        set: vec![set(column, Value::String("value".into()))],
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

fn set(column: &str, value: Value) -> SetItem {
    SetItem {
        column: column.into(),
        value,
    }
}

fn column(name: &str, ty: LogicalType, primary_key: bool) -> Column {
    Column {
        name: name.into(),
        ty,
        primary_key,
    }
}

fn people_scan() -> Plan {
    Plan::from_json(r#"{"v":0,"plan":{"op":"ScanNodes","table":"Person","binding":"p"}}"#).unwrap()
}

fn score_plan(unprunable: bool) -> Plan {
    let predicate = if unprunable {
        r#"{"gt":[{"add":[{"col":"r.score"},{"lit":0}]},{"lit":50}]}"#
    } else {
        r#"{"gt":[{"col":"r.score"},{"lit":50}]}"#
    };
    Plan::from_json(&format!(
        r#"{{"v":0,"plan":{{"op":"Filter","predicate":{predicate},"input":{{"op":"ScanNodes","table":"Reading","binding":"r"}}}}}}"#
    ))
    .unwrap()
}

fn person_rows(database: &mut Database) -> Vec<Vec<Value>> {
    sorted_rows(database.run(&people_scan()).unwrap().rows)
}

fn sorted_rows(mut rows: Vec<Vec<Value>>) -> Vec<Vec<Value>> {
    rows.sort_by_key(|row| match row.first() {
        Some(Value::Int64(id)) => *id,
        other => panic!("unexpected primary key: {other:?}"),
    });
    rows
}

fn rows(input: &[(i64, &str)]) -> Vec<Vec<Value>> {
    input
        .iter()
        .map(|(id, name)| vec![Value::Int64(*id), Value::String((*name).into())])
        .collect()
}

fn assert_conflict(error: DevonError) {
    assert!(matches!(error, DevonError::TransactionConflict { .. }));
    assert!(error.to_string().contains("committed at LSN"));
}

fn assert_error_contains(result: Result<(), DevonError>, expected: &str) {
    let error = result.unwrap_err().to_string();
    assert!(
        error.contains(expected),
        "`{error}` does not contain `{expected}`"
    );
}

fn spawn_crash_child(path: &Path) -> Child {
    Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "dml_crash_child_process",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CRASH_CHILD_ENV, "1")
        .env(CRASH_PATH_ENV, path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}

fn read_until_ack(stdout: &mut BufReader<impl Read>) {
    loop {
        let mut line = String::new();
        assert_ne!(stdout.read_line(&mut line).unwrap(), 0);
        if line.contains("DML_ACK") {
            return;
        }
    }
}

fn assert_child_stderr_empty(child: &mut Child) {
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    assert!(stderr.is_empty(), "unexpected child stderr: {stderr}");
}

fn wal_path(path: &Path) -> PathBuf {
    let mut with_suffix = OsString::from(path.as_os_str());
    with_suffix.push("-wal");
    PathBuf::from(with_suffix)
}

/// A same-transaction DELETE of a committed key followed by INSERT of that
/// key must leave the NEW row visible after commit and reopen — the
/// "delete plus insert" recipe the PK-update refusal itself recommends.
#[test]
fn same_transaction_delete_then_insert_of_one_pk_keeps_the_new_row() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("delete-then-insert.devondb");
    let mut database = people_database(&path, &[(7, "old")]);
    database.checkpoint().unwrap();

    let mut transaction = database.begin().unwrap();
    transaction.execute(&delete_person(7)).unwrap();
    transaction.execute(&insert_people(&[(7, "new")])).unwrap();
    assert_eq!(
        transaction.run(&people_scan()).unwrap().rows,
        rows(&[(7, "new")]),
        "own-transaction read after delete→insert"
    );
    transaction.commit().unwrap();
    assert_eq!(
        person_rows(&mut database),
        rows(&[(7, "new")]),
        "post-commit read"
    );
    database.checkpoint().unwrap();
    drop(database);
    let mut reopened = Database::open(&path).unwrap();
    assert_eq!(
        person_rows(&mut reopened),
        rows(&[(7, "new")]),
        "after reopen"
    );
}
