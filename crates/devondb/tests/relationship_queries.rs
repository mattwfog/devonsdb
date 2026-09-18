//! Property queries through live overlays, snapshots and durable CSR.
use devondb::{Database, DevonError, Options, Plan, Statement, text};
use devondb_types::value::Value;
use tempfile::TempDir;

fn query(input: &str) -> Plan {
    let text::parser::Parsed::Query(plan) = text::parser::parse(input).unwrap() else {
        panic!("query")
    };
    plan
}
fn statement(input: &str) -> Statement {
    let text::parser::Parsed::Statement(stmt) = text::parser::parse(input).unwrap() else {
        panic!("statement")
    };
    stmt.stmt
}
fn execute(db: &mut Database, input: &str) {
    db.execute(&statement(input)).unwrap();
}
fn setup() -> (TempDir, Database) {
    let dir = TempDir::new().unwrap();
    let mut db = Database::create(dir.path().join("graph.devondb"), 4096).unwrap();
    for stmt in [
        "create node table Person (id Int64 primary key, name String)",
        "create rel table Knows from Person to Person (weight Int64, note String)",
        "insert into Person values (1, \"Ada\"), (2, \"Bob\"), (3, \"Cam\")",
        "insert rel into Knows values (3 -> 1, 30, \"first\"), (1 -> 2, 10, \"parallel-a\"), (1 -> 1, 11, \"loop-a\"), (2 -> 1, 20, \"second\"), (1 -> 2, 12, \"parallel-b\"), (1 -> 1, 13, \"loop-b\")",
    ] {
        execute(&mut db, stmt);
    }
    (dir, db)
}
fn weights(db: &mut Database, direction: &str) -> Vec<i64> {
    db.run(&query(&format!("nodes(Person) as p | filter p.id = 1 | expand_rel Knows {direction} as q via e | project e.weight"))).unwrap().rows.into_iter().map(|row| {
        let Value::Int64(n) = row[0] else { panic!("integer") }; n
    }).collect()
}

#[test]
fn parallel_values_loops_and_incoming_order_survive_checkpoint_reopen() {
    let (dir, mut db) = setup();
    for checkpoint in [false, true] {
        if checkpoint {
            db.checkpoint().unwrap();
        }
        assert_eq!(weights(&mut db, "out"), [10, 11, 12, 13]);
        assert_eq!(weights(&mut db, "in"), [30, 11, 20, 13]);
        assert_eq!(weights(&mut db, "both"), [10, 11, 12, 13, 30, 20]);
    }
    drop(db);
    let mut db = Database::open(dir.path().join("graph.devondb")).unwrap();
    assert_eq!(weights(&mut db, "in"), [30, 11, 20, 13]);
    let result = db.run(&query("nodes(Person) as p | expand_rel Knows out as q via e | filter e.weight >= 20 | sort e.weight desc | project p.name, q.name, e.note")).unwrap();
    assert_eq!(
        result.rows,
        vec![
            vec![
                Value::String("Cam".into()),
                Value::String("Ada".into()),
                Value::String("first".into())
            ],
            vec![
                Value::String("Bob".into()),
                Value::String("Ada".into()),
                Value::String("second".into())
            ]
        ]
    );
    let sum = db.run(&query("nodes(Person) as p | expand_rel Knows out as q via e | aggregate sum(e.weight) as total")).unwrap();
    assert_eq!(sum.rows, vec![vec![Value::Int64(96)]]);
}

#[test]
fn own_writes_detach_reinsert_and_old_snapshot() {
    let (dir, mut db) = setup();
    db.checkpoint().unwrap();
    let plan = query(
        "nodes(Person) as p | expand_rel Knows out as q via e | project p.id, q.id, e.weight, e.note",
    );
    let old = db.snapshot();
    let before = old.run(&plan).unwrap();
    let mut tx = db.begin().unwrap();
    tx.execute(&statement(
        "insert rel into Knows values (2 -> 3, 99, null)",
    ))
    .unwrap();
    assert_eq!(tx.run(&plan).unwrap().rows.len(), before.rows.len() + 1);
    assert_eq!(db.run(&plan).unwrap(), before);
    tx.execute(&statement("detach delete from Person where id = 1"))
        .unwrap();
    let local = tx.run(&plan).unwrap();
    assert_eq!(
        local.rows,
        vec![vec![
            Value::Int64(2),
            Value::Int64(3),
            Value::Int64(99),
            Value::Null
        ]]
    );
    tx.commit().unwrap();
    execute(&mut db, "insert into Person values (1, \"New Ada\")");
    execute(
        &mut db,
        "insert rel into Knows values (1 -> 3, 100, \"new\")",
    );
    let after = db.run(&plan).unwrap();
    assert_eq!(after.rows.len(), 2);
    db.checkpoint().unwrap();
    assert_eq!(old.run(&plan).unwrap(), before);
    assert_eq!(db.run(&plan).unwrap(), after);
    drop(old);
    drop(db);
    let mut reopened = Database::open(dir.path().join("graph.devondb")).unwrap();
    assert_eq!(reopened.run(&plan).unwrap(), after);
}

#[test]
fn relationship_columns_flow_through_join_projection_and_correlated_scalar() {
    let (_dir, mut db) = setup();
    let result = db.run(&query("let right = nodes(Person) as r; nodes(Person) as p | expand_rel Knows out as q via e | join right on q.id = r.id | filter e.weight = 10 | project e.note, scalar(nodes(Person) as z | filter z.id = q.id | project z.name) as name")).unwrap();
    assert_eq!(
        result.rows,
        vec![vec![
            Value::String("parallel-a".into()),
            Value::String("Bob".into())
        ]]
    );
    let result = db.run(&query("nodes(Person) as p | expand_rel Knows out as q via e | project e.weight as `value.weight` | filter value.weight = 10")).unwrap();
    assert_eq!(result.rows, vec![vec![Value::Int64(10)]]);
}

#[test]
fn properties_are_budgeted_and_failure_does_not_poison_queries() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("large.devondb");
    let mut db = Database::create(&path, 4096).unwrap();
    execute(&mut db, "create node table N (id Int64 primary key)");
    execute(&mut db, "create rel table R from N to N (note String)");
    execute(&mut db, "insert into N values (1), (2)");
    execute(
        &mut db,
        &format!(
            "insert rel into R values (1 -> 2, \"{}\")",
            "x".repeat(400_000)
        ),
    );
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
    let plan = query("nodes(N) as p | expand_rel R out as q via e | project e.note");
    for _ in 0..3 {
        assert!(matches!(
            db.run(&plan),
            Err(DevonError::BudgetExceeded { .. })
        ));
    }
    assert_eq!(
        db.run(&query("nodes(N) as p | project p.id"))
            .unwrap()
            .rows
            .len(),
        2
    );
}

#[test]
fn heterogeneous_both_empty_relationship_and_supported_property_types() {
    let dir = TempDir::new().unwrap();
    let mut db = Database::create(dir.path().join("types.devondb"), 4096).unwrap();
    for text in [
        "create node table A (id Int64 primary key)",
        "create node table B (id Int64 primary key)",
        "create rel table R from A to B (b Bool, i Int64, f Float64, s String, v Vector(2))",
        "create rel table Empty from A to B",
        "insert into A values (1)",
        "insert into B values (2)",
        "insert rel into R values (1 -> 2, true, 7, 1.5, \"hi\", [1, 2])",
        "insert rel into Empty values (1 -> 2)",
    ] {
        execute(&mut db, text);
    }
    let out =
        query("nodes(A) as a | expand_rel R both as b via e | project e.b, e.i, e.f, e.s, e.v");
    let incoming =
        query("nodes(B) as b | expand_rel R both as a via e | project e.b, e.i, e.f, e.s, e.v");
    let expected = db
        .visible_relationships("R")
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .values;
    for persist in [false, true] {
        if persist {
            db.checkpoint().unwrap();
        }
        assert_eq!(db.run(&out).unwrap().rows, vec![expected.clone()]);
        assert_eq!(db.run(&incoming).unwrap().rows, vec![expected.clone()]);
        let empty = db
            .run(&query("nodes(A) as a | expand_rel Empty out as b via e"))
            .unwrap();
        assert_eq!(empty.columns, ["a.id", "b.id"]);
        assert_eq!(empty.rows, vec![vec![Value::Int64(1), Value::Int64(2)]]);
    }
}

#[test]
fn high_degree_expansion_resumes_across_chunks() {
    let dir = TempDir::new().unwrap();
    let mut db = Database::create(dir.path().join("chunks.devondb"), 4096).unwrap();
    for stmt in [
        "create node table N (id Int64 primary key)",
        "create rel table R from N to N (weight Int64)",
        "insert into N values (1), (2)",
    ] {
        execute(&mut db, stmt);
    }
    let entries = (0..2055)
        .map(|n| format!("(1 -> 2, {n})"))
        .collect::<Vec<_>>()
        .join(", ");
    execute(&mut db, &format!("insert rel into R values {entries}"));
    db.checkpoint().unwrap();
    let rows = db
        .run(&query(
            "nodes(N) as p | expand_rel R out as q via e | project e.weight",
        ))
        .unwrap()
        .rows;
    assert_eq!(
        rows,
        (0..2055).map(|n| vec![Value::Int64(n)]).collect::<Vec<_>>()
    );
}

#[test]
fn follower_refresh_preserves_pinned_properties_across_checkpoint() {
    let (dir, mut db) = setup();
    db.checkpoint().unwrap();
    drop(db);
    let path = dir.path().join("graph.devondb");
    Database::activate_multiprocess(&path).unwrap();
    let mut writer = Database::open(&path).unwrap();
    let follower = Database::open_read_only(&path).unwrap();
    let pinned = follower.snapshot();
    let plan = query("nodes(Person) as p | expand_rel Knows out as q via e | project e.note");
    let before = pinned.run(&plan).unwrap();
    execute(
        &mut writer,
        "insert rel into Knows values (2 -> 3, 123, \"fresh\")",
    );
    follower.refresh().unwrap();
    assert_eq!(
        follower.snapshot().run(&plan).unwrap().rows.len(),
        before.rows.len() + 1
    );
    execute(&mut writer, "detach delete from Person where id = 1");
    writer.checkpoint().unwrap();
    follower.refresh().unwrap();
    assert_eq!(
        follower.snapshot().run(&plan).unwrap().rows,
        vec![vec![Value::String("fresh".into())]]
    );
    assert_eq!(pinned.run(&plan).unwrap(), before);
}

#[test]
fn large_node_string_bytes_and_vector_decode_is_precharged() {
    for (ty, value) in [
        ("String".to_owned(), format!("\"{}\"", "x".repeat(200_000))),
        (
            "Bytes".to_owned(),
            format!("bytes(\"{}\")", "ab".repeat(200_000)),
        ),
        (
            "Vector(50000)".to_owned(),
            format!("[{}]", vec!["1"; 50000].join(",")),
        ),
    ] {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("nodes.devondb");
        let mut db = Database::create(&path, 4096).unwrap();
        execute(&mut db, "create node table A (id Int64 primary key)");
        execute(
            &mut db,
            &format!("create node table B (id Int64 primary key, payload {ty})"),
        );
        execute(&mut db, "create rel table R from A to B (weight Int64)");
        execute(&mut db, "insert into A values (1)");
        execute(&mut db, &format!("insert into B values (2, {value})"));
        execute(&mut db, "insert rel into R values (1 -> 2, 7)");
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
        let plan = query("nodes(A) as a | expand_rel R out as b via e | project e.weight");
        for _ in 0..2 {
            assert!(
                matches!(db.run(&plan), Err(DevonError::BudgetExceeded { .. })),
                "{ty}"
            );
        }
        let source = query("nodes(B) as b | expand_rel R in as a via e | project b.payload");
        let error = db.run(&source).unwrap_err();
        assert!(
            matches!(error, DevonError::BudgetExceeded { .. }),
            "{ty}: {error}"
        );
        assert!(error.to_string().contains("ExpandRel"), "{ty}: {error}");
        assert_eq!(
            db.run(&query("nodes(A) as a | project a.id"))
                .unwrap()
                .rows
                .len(),
            1
        );
    }
}

#[test]
fn relationship_wal_child() {
    let Ok(path) = std::env::var("DEVONDB_REL_QUERY_WAL_CHILD") else {
        return;
    };
    let mut db = Database::create(&path, 4096).unwrap();
    for stmt in [
        "create node table N (id Int64 primary key)",
        "create rel table R from N to N (weight Int64)",
        "insert into N values (1), (2)",
    ] {
        execute(&mut db, stmt);
    }
    db.checkpoint().unwrap();
    execute(&mut db, "insert rel into R values (1 -> 2, 77)");
    use std::io::Write;
    assert!(
        std::fs::metadata(format!("{path}-wal")).unwrap().len() > 0,
        "edge mutation must remain in WAL"
    );
    println!("REL_WAL_READY");
    std::io::stdout().flush().unwrap();
    loop {
        std::thread::park();
    }
}

#[test]
fn uncheckpointed_properties_recover_after_real_process_kill() {
    use std::io::{BufRead, BufReader};
    use std::process::{Command, Stdio};
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("wal.devondb");
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "relationship_wal_child", "--nocapture"])
        .env("DEVONDB_REL_QUERY_WAL_CHILD", &path)
        .env("DEVONDB_AUTOCHECKPOINT", "off")
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    loop {
        let line = lines
            .next()
            .expect("child exited before WAL marker")
            .unwrap();
        if line == "REL_WAL_READY" {
            break;
        }
    }
    child.kill().unwrap();
    child.wait().unwrap();
    let mut db = Database::open(&path).unwrap();
    let plan = query("nodes(N) as p | expand_rel R out as q via e | project e.weight");
    assert_eq!(db.run(&plan).unwrap().rows, vec![vec![Value::Int64(77)]]);
    db.checkpoint().unwrap();
    assert_eq!(db.run(&plan).unwrap().rows, vec![vec![Value::Int64(77)]]);
}

#[test]
fn small_property_query_runs_with_one_megabyte() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("small.devondb");
    let mut db = Database::create(&path, 4096).unwrap();
    for stmt in [
        "create node table N (id Int64 primary key)",
        "create rel table R from N to N (weight Int64)",
        "insert into N values (1), (2)",
        "insert rel into R values (1 -> 2, 7)",
    ] {
        execute(&mut db, stmt);
    }
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
    assert_eq!(
        db.run(&query(
            "nodes(N) as p | expand_rel R out as q via e | project e.weight"
        ))
        .unwrap()
        .rows,
        vec![vec![Value::Int64(7)]]
    );
}
