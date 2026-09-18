//! Indexed DML: raw-history visibility, checkpoint repair and durable fencing.
use devondb::{Database, DevonError, Options, Plan, Statement};
use devondb_plan::{
    expr::Metric,
    ops::{KnnMode, Operator, PLAN_VERSION},
    statement::SetItem,
};
use devondb_storage::{
    catalog::Catalog,
    hnsw::index::load_persisted_index,
    pager::Pager,
    superblock::{DML_WAL_FLAG, HNSW_MUTATION_WAL_FLAG, READ_SAFE_FLAG_MASK, SUPPORTED_FLAG_MASK},
};
use devondb_types::{logical_type::LogicalType, schema::Column, value::Value};
use std::{
    ffi::OsString,
    fs,
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::Duration,
};
use tempfile::tempdir;

fn create_index() -> Statement {
    Statement::CreateHnswIndex {
        name: "idx".into(),
        table: "Corpus".into(),
        column: "embedding".into(),
        metric: Metric::L2,
    }
}
fn insert(id: i64, vector: Value) -> Statement {
    Statement::InsertNode {
        table: "Corpus".into(),
        rows: vec![vec![
            Value::Int64(id),
            vector,
            Value::String(format!("row-{id}")),
        ]],
    }
}
fn update(id: i64, column: &str, value: Value) -> Statement {
    Statement::UpdateNode {
        table: "Corpus".into(),
        set: vec![SetItem {
            column: column.into(),
            value,
        }],
        key_column: "id".into(),
        key: Value::Int64(id),
    }
}
fn delete(id: i64) -> Statement {
    Statement::DeleteNode {
        table: "Corpus".into(),
        key_column: "id".into(),
        key: Value::Int64(id),
    }
}
fn vector(x: f32) -> Value {
    Value::Vector(vec![x, 0.0])
}
fn plan() -> Plan {
    Plan {
        v: PLAN_VERSION,
        plan: Operator::KnnScan {
            table: "Corpus".into(),
            column: "embedding".into(),
            query: vec![0.0, 0.0].into(),
            k: 20,
            metric: Metric::L2,
            mode: KnnMode::Approximate,
        },
    }
}
fn ids(rows: Vec<Vec<Value>>) -> Vec<i64> {
    rows.into_iter()
        .map(|r| match r[0] {
            Value::Int64(id) => id,
            _ => panic!("id"),
        })
        .collect()
}
fn seed(path: &Path, indexed: bool) -> Database {
    let mut db = Database::create(path, 4096).unwrap();
    db.execute(&Statement::CreateNodeTable {
        name: "Corpus".into(),
        columns: vec![
            Column {
                name: "id".into(),
                ty: LogicalType::Int64,
                primary_key: true,
            },
            Column {
                name: "embedding".into(),
                ty: LogicalType::Vector { dim: 2 },
                primary_key: false,
            },
            Column {
                name: "body".into(),
                ty: LogicalType::String,
                primary_key: false,
            },
        ],
    })
    .unwrap();
    for id in 1..=4 {
        db.execute(&insert(id, vector(id as f32))).unwrap();
    }
    if indexed {
        db.execute(&create_index()).unwrap();
    } else {
        db.checkpoint().unwrap();
    }
    db
}
fn flags(path: &Path) -> u64 {
    Pager::open(path).unwrap().superblock().feature_flags
}
fn root(path: &Path) -> (u64, u64) {
    let pager = Pager::open(path).unwrap();
    let cat = Catalog::load(&pager).unwrap();
    let root = cat.indexes()[0].root;
    (
        root,
        load_persisted_index(&pager, root)
            .unwrap()
            .root
            .covered_rows,
    )
}
fn wal(path: &Path) -> PathBuf {
    let mut s = OsString::from(path);
    s.push("-wal");
    s.into()
}

#[test]
fn scalar_vector_null_delete_and_pinned_snapshots_rebuild() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("db");
    let mut db = seed(&path, true);
    let before = db.snapshot();
    let old = before.run(&plan()).unwrap();
    let old_root = root(&path);
    db.execute(&update(2, "body", Value::String("changed".into())))
        .unwrap();
    db.execute(&update(4, "embedding", vector(0.0))).unwrap();
    assert_eq!(ids(db.run(&plan()).unwrap().rows)[0], 4);
    db.execute(&update(4, "embedding", Value::Null)).unwrap();
    assert_eq!(ids(db.run(&plan()).unwrap().rows), vec![1, 2, 3]);
    db.execute(&update(4, "embedding", vector(0.0))).unwrap();
    db.execute(&delete(1)).unwrap();
    db.execute(&insert(5, vector(0.1))).unwrap();
    assert_eq!(ids(db.run(&plan()).unwrap().rows), vec![4, 5, 2, 3]);
    assert_ne!(flags(&path) & HNSW_MUTATION_WAL_FLAG, 0);
    db.checkpoint().unwrap();
    assert_ne!(old_root.0, root(&path).0);
    assert_eq!(root(&path).1, 4);
    assert_eq!(flags(&path) & HNSW_MUTATION_WAL_FLAG, 0);
    assert_eq!(before.run(&plan()).unwrap(), old);
    assert_eq!(ids(db.run(&plan()).unwrap().rows), vec![4, 5, 2, 3]);
    drop(before);
    drop(db);
    assert_eq!(
        ids(Database::open(&path).unwrap().run(&plan()).unwrap().rows),
        vec![4, 5, 2, 3]
    );
}

#[test]
fn own_and_committed_delete_reinsert_history_survives_net_effects() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("db");
    let mut db = seed(&path, true);
    let mut tx = db.begin().unwrap();
    tx.execute(&delete(1)).unwrap();
    tx.execute(&insert(1, vector(9.0))).unwrap();
    assert_eq!(ids(tx.run(&plan()).unwrap().rows), vec![2, 3, 4, 1]);
    tx.commit().unwrap();
    db.execute(&insert(5, vector(0.0))).unwrap();
    assert_eq!(ids(db.run(&plan()).unwrap().rows), vec![5, 2, 3, 4, 1]);
    db.checkpoint().unwrap();
    assert_eq!(root(&path).1, 5);
    assert_eq!(ids(db.run(&plan()).unwrap().rows), vec![5, 2, 3, 4, 1]);
    // Overlay-only update/delete paths also preserve the tail's visible rows.
    db.execute(&insert(6, vector(6.0))).unwrap();
    db.execute(&update(6, "embedding", vector(-0.1))).unwrap();
    assert_eq!(ids(db.run(&plan()).unwrap().rows)[1], 6);
    db.execute(&delete(6)).unwrap();
    assert_eq!(ids(db.run(&plan()).unwrap().rows), vec![5, 2, 3, 4, 1]);
}

#[test]
fn delete_all_and_refill_replaces_entrypoint_and_empty_topology() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("db");
    let mut db = seed(&path, true);
    for id in 1..=4 {
        db.execute(&delete(id)).unwrap();
    }
    assert!(db.run(&plan()).unwrap().rows.is_empty());
    db.checkpoint().unwrap();
    assert_eq!(root(&path).1, 0);
    db.execute(&insert(8, vector(0.0))).unwrap();
    assert_eq!(ids(db.run(&plan()).unwrap().rows), vec![8]);
    db.checkpoint().unwrap();
    assert_eq!(root(&path).1, 1);
}

#[test]
fn create_after_dml_and_between_preparation_and_publication_is_serialized() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("db");
    let mut db = seed(&path, false);
    db.execute(&delete(1)).unwrap();
    db.execute(&update(4, "embedding", vector(0.0))).unwrap();
    let mut create = db.begin().unwrap();
    create.execute(&create_index()).unwrap();
    db.execute(&delete(2)).unwrap();
    db.execute(&insert(9, vector(-0.1))).unwrap();
    create.commit().unwrap();
    assert_eq!(ids(db.run(&plan()).unwrap().rows), vec![4, 9, 3]);
    assert_eq!(root(&path).1, 3);
}

#[test]
fn index_created_after_update_staged_sets_fence_from_current_catalog() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("db");
    let mut db = seed(&path, false);
    let mut update_tx = db.begin().unwrap();
    update_tx
        .execute(&update(4, "embedding", vector(0.0)))
        .unwrap();
    db.execute(&create_index()).unwrap();
    update_tx.commit().unwrap();
    assert_ne!(flags(&path) & HNSW_MUTATION_WAL_FLAG, 0);
    assert_eq!(ids(db.run(&plan()).unwrap().rows)[0], 4);
    let pager = Pager::open(&path).unwrap();
    let cat = Catalog::load(&pager).unwrap();
    let saved = root(&path).0;
    drop(pager);
    // Keep an uncheckpointed image for replay while normal handle drop cleans its own file.
    let copy = dir.path().join("replay");
    fs::copy(&path, &copy).unwrap();
    fs::copy(wal(&path), wal(&copy)).unwrap();
    assert_eq!(cat.indexes()[0].root, saved);
    assert_eq!(
        ids(Database::open(&copy).unwrap().run(&plan()).unwrap().rows)[0],
        4
    );
}

#[test]
fn missing_bit_is_corruption_and_empty_obsolete_wal_heals_only_writable() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("db");
    let mut db = seed(&path, true);
    db.execute(&update(4, "embedding", vector(0.0))).unwrap();
    let copy = dir.path().join("copy");
    fs::copy(&path, &copy).unwrap();
    fs::copy(wal(&path), wal(&copy)).unwrap();
    let pager = Pager::open(&copy).unwrap();
    pager
        .commit_feature_flags(pager.superblock().feature_flags & !HNSW_MUTATION_WAL_FLAG)
        .unwrap();
    drop(pager);
    assert!(matches!(
        Database::open(&copy),
        Err(DevonError::Corrupt { .. })
    ));
    let old_wal = fs::read(wal(&path)).unwrap();
    db.checkpoint().unwrap();
    drop(db);
    Database::activate_multiprocess(&path).unwrap();
    fs::write(wal(&path), &old_wal).unwrap();
    let pager = Pager::open(&path).unwrap();
    pager
        .commit_feature_flags(
            pager.superblock().feature_flags | HNSW_MUTATION_WAL_FLAG | DML_WAL_FLAG,
        )
        .unwrap();
    drop(pager);
    let read = Database::open_read_only(&path).unwrap();
    drop(read);
    assert_ne!(flags(&path) & HNSW_MUTATION_WAL_FLAG, 0);
    assert_eq!(fs::read(wal(&path)).unwrap(), old_wal);
    let mut healed = Database::open(&path).unwrap();
    assert_eq!(flags(&path) & HNSW_MUTATION_WAL_FLAG, 0);
    assert!(fs::read(wal(&path)).unwrap().is_empty());
    assert_eq!(ids(healed.run(&plan()).unwrap().rows)[0], 4);
}

#[test]
fn dirty_exact_precharges_large_variable_decode() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("db");
    let mut db = seed(&path, true);
    db.execute(&update(1, "body", Value::String("x".repeat(600_000))))
        .unwrap();
    db.checkpoint().unwrap();
    drop(db);
    let mut db = Database::open_with(
        &path,
        Options {
            page_size: 4096,
            memory_limit: 1024 * 1024,
        },
    )
    .unwrap();
    db.execute(&update(4, "embedding", vector(0.0))).unwrap();
    assert!(matches!(
        db.run(&plan()),
        Err(DevonError::BudgetExceeded { .. })
    ));
    assert!(db.memory_budget().charged() <= 1024 * 1024);
}

#[test]
fn mutation_bit_is_supported_and_non_read_safe() {
    assert_eq!(HNSW_MUTATION_WAL_FLAG, 1 << 14);
    assert_eq!(HNSW_MUTATION_WAL_FLAG & READ_SAFE_FLAG_MASK, 0);
    assert_ne!(HNSW_MUTATION_WAL_FLAG & SUPPORTED_FLAG_MASK, 0);
}

#[test]
fn killed_writer_replays_indexed_mutations_and_follower_sees_fence() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("db");
    let db = seed(&path, true);
    drop(db);
    Database::activate_multiprocess(&path).unwrap();
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "mutation_crash_child", "--nocapture"])
        .env("DEVONDB_HNSW_MUTATION_CHILD", &path)
        .env("DEVONDB_AUTOCHECKPOINT", "off")
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut output = BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    loop {
        line.clear();
        assert_ne!(output.read_line(&mut line).unwrap(), 0);
        if line.contains("MUTATION_READY") {
            break;
        }
    }
    let follower = Database::open_read_only(&path).unwrap();
    follower.refresh().unwrap();
    assert_eq!(
        ids(follower.snapshot().run(&plan()).unwrap().rows),
        vec![4, 2, 3]
    );
    child.kill().unwrap();
    child.wait().unwrap();
    drop(follower);
    let mut db = Database::open(&path).unwrap();
    assert_eq!(ids(db.run(&plan()).unwrap().rows), vec![4, 2, 3]);
    db.checkpoint().unwrap();
    assert_eq!(root(&path).1, 3);
}

#[test]
fn mutation_crash_child() {
    let Some(path) = std::env::var_os("DEVONDB_HNSW_MUTATION_CHILD") else {
        return;
    };
    let mut db = Database::open(PathBuf::from(path)).unwrap();
    db.execute(&update(4, "embedding", vector(0.0))).unwrap();
    db.execute(&delete(1)).unwrap();
    println!("MUTATION_READY");
    std::io::stdout().flush().unwrap();
    loop {
        std::thread::sleep(Duration::from_secs(1));
    }
}

#[test]
fn follower_refresh_extends_dirty_chain_then_rebases_repaired_root() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("db");
    drop(seed(&path, true));
    Database::activate_multiprocess(&path).unwrap();
    let mut writer = Database::open(&path).unwrap();
    let follower = Database::open_read_only(&path).unwrap();
    let pinned = follower.snapshot();
    let before = pinned.run(&plan()).unwrap();
    let lsn = Pager::open(&path).unwrap().superblock().checkpoint_lsn;
    writer
        .execute(&update(4, "embedding", vector(0.0)))
        .unwrap();
    assert!(!fs::read(wal(&path)).unwrap().is_empty());
    assert_eq!(Pager::open(&path).unwrap().superblock().checkpoint_lsn, lsn);
    follower.refresh().unwrap();
    assert_eq!(ids(follower.snapshot().run(&plan()).unwrap().rows)[0], 4);
    writer.checkpoint().unwrap();
    follower.refresh().unwrap();
    assert_eq!(ids(follower.snapshot().run(&plan()).unwrap().rows)[0], 4);
    assert_eq!(pinned.run(&plan()).unwrap(), before);
}

#[test]
fn tiny_dirty_query_reserves_final_output_capacity_and_releases_on_failure() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("db");
    drop(seed(&path, true));
    let mut db = Database::open_with(
        &path,
        Options {
            page_size: 4096,
            memory_limit: 1024 * 1024,
        },
    )
    .unwrap();
    db.execute(&update(4, "embedding", vector(0.0))).unwrap();
    let budget = db.memory_budget();
    assert!(budget.charge_or_reclaim(700_000));
    let mut one = plan();
    if let Operator::KnnScan { k, .. } = &mut one.plan {
        *k = 1;
    }
    assert!(matches!(
        db.run(&one),
        Err(DevonError::BudgetExceeded { .. })
    ));
    // First validation may populate clean pager frames; repeated failed scans
    // must release their statement reservation instead of accumulating it.
    let warmed = budget.charged();
    assert!(matches!(
        db.run(&one),
        Err(DevonError::BudgetExceeded { .. })
    ));
    assert!(budget.charged() <= warmed);
    budget.release(700_000);
    assert_eq!(ids(db.run(&one).unwrap().rows), vec![4]);
}

#[test]
fn dirty_query_surfaces_corrupt_root_before_budget_or_exact_fallback() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("db");
    let mut db = seed(&path, true);
    db.execute(&update(4, "embedding", vector(0.0))).unwrap();
    let copy = dir.path().join("copy");
    fs::copy(&path, &copy).unwrap();
    fs::copy(wal(&path), wal(&copy)).unwrap();
    let pager = Pager::open(&copy).unwrap();
    let root = Catalog::load(&pager).unwrap().indexes()[0].root;
    let mut page = pager.read_page(root).unwrap();
    page[0] = 0;
    pager.write_page(root, &page).unwrap();
    pager.sync().unwrap();
    drop(pager);
    let mut reopened = Database::open(&copy).unwrap();
    assert!(matches!(
        reopened.run(&plan()),
        Err(DevonError::Corrupt { .. })
    ));
}

#[test]
fn indexed_detach_removes_incident_edges_and_rebuilds() {
    use devondb_plan::text::parser::{Parsed, parse};
    let dir = tempdir().unwrap();
    let path = dir.path().join("db");
    let mut db = seed(&path, true);
    for text in [
        "create rel table R from Corpus to Corpus",
        "insert rel into R values (1 -> 2), (2 -> 1), (1 -> 1), (2 -> 3)",
        "detach delete from Corpus where id = 1",
    ] {
        let Parsed::Statement(s) = parse(text).unwrap() else {
            panic!("statement")
        };
        db.execute(&s.stmt).unwrap();
    }
    assert_eq!(ids(db.run(&plan()).unwrap().rows), vec![2, 3, 4]);
    let Parsed::Query(edges) =
        parse("nodes(Corpus) as n | expand R out as m | project n.id, m.id").unwrap()
    else {
        panic!("query")
    };
    assert_eq!(
        db.run(&edges).unwrap().rows,
        vec![vec![Value::Int64(2), Value::Int64(3)]]
    );
    db.checkpoint().unwrap();
    assert_eq!(root(&path).1, 3);
    assert_eq!(
        db.run(&edges).unwrap().rows,
        vec![vec![Value::Int64(2), Value::Int64(3)]]
    );
}

#[test]
fn obsolete_mutation_wal_with_live_insert_retains_fence_until_checkpoint() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("db");
    let mut db = seed(&path, true);
    db.execute(&update(4, "embedding", vector(0.0))).unwrap();
    let mut combined = fs::read(wal(&path)).unwrap();
    db.checkpoint().unwrap();
    db.execute(&insert(5, vector(0.1))).unwrap();
    combined.extend(fs::read(wal(&path)).unwrap());
    let copy = dir.path().join("copy");
    fs::copy(&path, &copy).unwrap();
    fs::write(wal(&copy), &combined).unwrap();
    let pager = Pager::open(&copy).unwrap();
    pager
        .commit_feature_flags(
            pager.superblock().feature_flags | DML_WAL_FLAG | HNSW_MUTATION_WAL_FLAG,
        )
        .unwrap();
    drop(pager);
    let mut reopened = Database::open(&copy).unwrap();
    assert_ne!(flags(&copy) & HNSW_MUTATION_WAL_FLAG, 0);
    assert_eq!(fs::read(wal(&copy)).unwrap(), combined);
    assert_eq!(
        ids(reopened.run(&plan()).unwrap().rows),
        vec![4, 5, 1, 2, 3]
    );
    reopened.checkpoint().unwrap();
    assert_eq!(flags(&copy) & (DML_WAL_FLAG | HNSW_MUTATION_WAL_FLAG), 0);
    assert!(fs::read(wal(&copy)).unwrap().is_empty());
    assert_eq!(
        ids(reopened.run(&plan()).unwrap().rows),
        vec![4, 5, 1, 2, 3]
    );
}

#[test]
fn crash_before_records_heals_each_checkpoint_scoped_bit_independently() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("db");
    drop(seed(&path, true));
    for bit in [
        DML_WAL_FLAG,
        devondb_storage::superblock::REL_TOMBSTONE_WAL_FLAG,
        HNSW_MUTATION_WAL_FLAG,
    ] {
        let pager = Pager::open(&path).unwrap();
        pager
            .commit_feature_flags(pager.superblock().feature_flags | bit)
            .unwrap();
        drop(pager);
        let reopened = Database::open(&path).unwrap();
        assert_eq!(flags(&path) & bit, 0);
        drop(reopened);
    }
}
