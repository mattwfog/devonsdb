//! Deterministic ranked retrieval through real database snapshots and budgets.
#[cfg(feature = "fts")]
use devondb::Statement;
use devondb::{Database, Plan, text};
fn query(text: &str) -> Plan {
    let text::parser::Parsed::Query(plan) = text::parser::parse(text).unwrap() else {
        panic!("query")
    };
    plan
}
fn execute(db: &mut Database, text: &str) {
    let text::parser::Parsed::Statement(stmt) = text::parser::parse(text).unwrap() else {
        panic!("statement")
    };
    db.execute(&stmt.stmt).unwrap();
}
#[cfg(feature = "fts")]
fn statement(text: &str) -> Statement {
    let text::parser::Parsed::Statement(stmt) = text::parser::parse(text).unwrap() else {
        panic!("statement")
    };
    stmt.stmt
}
#[cfg(not(feature = "fts"))]
#[test]
fn capability_refuses_even_filtered_textscan() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = Database::create(dir.path().join("db"), 4096).unwrap();
    execute(
        &mut db,
        "create node table Document (id Int64 primary key, body String)",
    );
    for suffix in ["", " | filter d.id = 1"] {
        let err = db
            .run(&query(&format!(
                "textscan(Document.body, \"rust\", k=1) as d{suffix}"
            )))
            .unwrap_err();
        assert!(err.to_string().contains("fts feature"), "{err}");
    }
}
#[cfg(feature = "fts")]
mod enabled {
    use super::*;
    use devondb::{DevonError, Options, Value};
    fn setup() -> (tempfile::TempDir, Database) {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Database::create(dir.path().join("db"), 4096).unwrap();
        execute(
            &mut db,
            "create node table Document (id Int64 primary key, body String, score Int64, _score String)",
        );
        execute(
            &mut db,
            "insert into Document values (9, \"rust graph\", 90, \"nine\"), (1, \"rust graph\", 10, \"one\"), (2, \"other words\", 20, \"two\"), (3, null, 30, \"three\")",
        );
        (dir, db)
    }
    fn ranked() -> Plan {
        query("textscan(Document.body, \"rust\", k=10) as d | project d.id, scoreof(d) as rank")
    }
    #[test]
    fn bootstrap_pk_ties_checkpoint_reopen_and_empty_query() {
        let (dir, mut db) = setup();
        let before = db.run(&ranked()).unwrap();
        assert_eq!(
            before.rows.iter().map(|r| r[0].clone()).collect::<Vec<_>>(),
            vec![Value::Int64(1), Value::Int64(9)]
        );
        for row in &before.rows {
            assert!(matches!(row[1],Value::Float64(n) if n>0.0));
        }
        assert_eq!(
            db.run(&query(
                "textscan(Document.body, \"rust\", k=1) as d | project d.id"
            ))
            .unwrap()
            .rows,
            vec![vec![Value::Int64(1)]]
        );
        db.checkpoint().unwrap();
        assert_eq!(db.run(&ranked()).unwrap(), before);
        assert!(db.fulltext_index_present("Document", "body"));
        assert!(
            db.run(&query(
                "textscan(Document.body, \"!!!\", k=9223372036854775807) as d | project scoreof(d)"
            ))
            .unwrap()
            .rows
            .is_empty()
        );
        drop(db);
        let mut db = Database::open(dir.path().join("db")).unwrap();
        assert_eq!(db.run(&ranked()).unwrap(), before);
    }
    #[test]
    fn metadata_filter_order_projection_aggregation_join_and_scalars() {
        let (_dir, mut db) = setup();
        assert!(
            db.run(&query(
                "textscan(Document.body, \"rust\", k=1) as d | filter d.id = 9 | project d.id"
            ))
            .unwrap()
            .rows
            .is_empty()
        );
        let rows=db.run(&query("textscan(Document.body, \"rust\", k=1) as d | project d.score, d._score, scoreof(d) as score")).unwrap().rows;
        assert_eq!(
            rows[0][..2],
            [Value::Int64(10), Value::String("one".into())]
        );
        let expected = rows[0][2].clone();
        for text in [
            "textscan(Document.body, \"rust\", k=1) as d | project 7 as literal | project scoreof(d)",
            "textscan(Document.body, \"rust\", k=1) as d | project scalar(nodes(Document) as x | limit 1 | project scoreof(d))",
            "textscan(Document.body, \"rust\", k=1) as d | aggregate sum(scalar(nodes(Document) as x | limit 1 | project scoreof(d))) as total",
            "textscan(Document.body, \"rust\", k=1) as d | filter scalar(nodes(Document) as x | limit 1 | project scoreof(d)) > 0 | project scoreof(d)",
            "let other = textscan(Document.body, \"rust\", k=1) as r | project 2 as right_literal; textscan(Document.body, \"rust\", k=1) as l | project 1 as left_literal | join other on scoreof(r) = scoreof(l) | project scoreof(l)",
        ] {
            assert_eq!(
                db.run(&query(text)).unwrap().rows,
                vec![vec![expected.clone()]],
                "{text}"
            );
        }
    }
    #[test]
    fn own_writes_old_snapshot_detach_reinsert_and_novel_term() {
        let (_dir, mut db) = setup();
        db.checkpoint().unwrap();
        let old = db.snapshot();
        let before = old.run(&ranked()).unwrap();
        let mut tx = db.begin().unwrap();
        tx.execute(&statement(
            "update Document set body = \"novel\" where id = 1",
        ))
        .unwrap();
        tx.execute(&statement("detach delete from Document where id = 9"))
            .unwrap();
        tx.execute(&statement(
            "insert into Document values (9, \"novel novel\", 91, \"new\")",
        ))
        .unwrap();
        assert!(tx.run(&ranked()).unwrap().rows.is_empty());
        let novel =
            query("textscan(Document.body, \"novel\", k=10) as d | project d.id, scoreof(d)");
        let after = tx.run(&novel).unwrap();
        assert_eq!(after.rows.len(), 2);
        assert!(
            after
                .rows
                .iter()
                .all(|r| matches!(r[1],Value::Float64(n) if n>0.0))
        );
        tx.commit().unwrap();
        assert_eq!(db.run(&novel).unwrap(), after);
        assert_eq!(old.run(&ranked()).unwrap(), before);
        db.checkpoint().unwrap();
        assert_eq!(old.run(&ranked()).unwrap(), before);
    }
    #[test]
    fn narrow_projection_avoids_unused_payload_and_budget_releases() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db");
        let mut db = Database::create(&path, 4096).unwrap();
        execute(
            &mut db,
            "create node table Document (id Int64 primary key, body String, payload String)",
        );
        execute(
            &mut db,
            &format!(
                "insert into Document values (1, \"rust\", \"{}\")",
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
        assert_eq!(db.run(&ranked()).unwrap().rows.len(), 1);
        let full = query("textscan(Document.body, \"rust\", k=1) as d");
        for _ in 0..3 {
            assert!(matches!(
                db.run(&full),
                Err(DevonError::BudgetExceeded { .. })
            ));
        }
        assert_eq!(db.run(&ranked()).unwrap().rows.len(), 1);
    }

    #[test]
    fn own_insert_update_delete_sequences_match_scan_visibility() {
        for operations in [
            vec![
                "insert into Document values (4, \"rust\", 4, \"four\")",
                "update Document set body = \"novel\" where id = 4",
            ],
            vec![
                "insert into Document values (4, \"rust\", 4, \"four\")",
                "delete from Document where id = 4",
            ],
            vec![
                "detach delete from Document where id = 9",
                "insert into Document values (9, \"rust\", 9, \"new\")",
                "update Document set body = \"novel\" where id = 9",
            ],
            vec![
                "detach delete from Document where id = 9",
                "insert into Document values (9, \"rust\", 9, \"new\")",
                "delete from Document where id = 9",
            ],
        ] {
            let (_dir, mut db) = setup();
            db.checkpoint().unwrap();
            let old = db.snapshot();
            let original = old.run(&ranked()).unwrap();
            let mut tx = db.begin().unwrap();
            for op in operations {
                tx.execute(&statement(op)).unwrap();
            }
            let plain = query("nodes(Document) as d | filter d.body = \"novel\" | project d.id");
            let text = query("textscan(Document.body, \"novel\", k=10) as d | project d.id");
            let expected = tx.run(&plain).unwrap().rows;
            assert_eq!(tx.run(&text).unwrap().rows, expected);
            tx.commit().unwrap();
            assert_eq!(db.run(&text).unwrap().rows, expected);
            assert_eq!(old.run(&ranked()).unwrap(), original);
        }
    }
    #[test]
    fn cache_refusal_keeps_checkpoint_statistics_and_releases_pin_charges() {
        use devondb_storage::overlay::{FullTextResult, shed_fulltext_caches};
        let (_dir, mut db) = setup();
        db.checkpoint().unwrap();
        execute(
            &mut db,
            "update Document set body = \"rust rust rust\" where id = 2",
        );
        execute(&mut db, "delete from Document where id = 9");
        let before_count = db.fulltext_cached_rows_scored();
        let expected = db.run(&ranked()).unwrap();
        assert!(
            db.fulltext_cached_rows_scored() > before_count,
            "cached base postings must be consulted"
        );
        let FullTextResult::Indexed(index) = db.fulltext_index("Document", "body").unwrap() else {
            panic!("index")
        };
        let bytes = index.heap_bytes();
        let budget = db.memory_budget();
        let before = budget.charged();
        shed_fulltext_caches();
        assert_eq!(budget.charged(), before);
        drop(index);
        assert_eq!(budget.charged(), before - bytes);
        let held = budget.limit() - budget.charged() - 1;
        assert!(budget.try_charge(held));
        assert!(matches!(
            db.fulltext_index("Document", "body").unwrap(),
            FullTextResult::Unavailable
        ));
        budget.release(held);
        let count = db.fulltext_cached_rows_scored();
        assert_eq!(db.run(&ranked()).unwrap(), expected);
        assert_eq!(
            db.fulltext_cached_rows_scored(),
            count,
            "refused cache uses query-sized baseline statistics"
        );
    }
    #[test]
    fn string_primary_key_ties_and_zero_token_checkpoint_bootstrap() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Database::create(dir.path().join("db"), 4096).unwrap();
        execute(
            &mut db,
            "create node table Document (id String primary key, body String)",
        );
        execute(
            &mut db,
            "insert into Document values (\"z\", \"!!!\"), (\"a\", null)",
        );
        db.checkpoint().unwrap();
        execute(
            &mut db,
            "update Document set body = \"rust\" where id = \"z\"",
        );
        execute(
            &mut db,
            "update Document set body = \"rust\" where id = \"a\"",
        );
        let result = db
            .run(&query(
                "textscan(Document.body, \"rust\", k=1) as d | project d.id, scoreof(d)",
            ))
            .unwrap();
        assert_eq!(result.rows[0][0], Value::String("a".into()));
        assert!(matches!(result.rows[0][1],Value::Float64(n) if n>0.0));
    }

    #[test]
    fn reinserted_overlay_has_fresh_relationship_identity() {
        for checkpoint in [false, true] {
            let (_dir, mut db) = setup();
            execute(
                &mut db,
                "create rel table Link from Document to Document (weight Int64)",
            );
            execute(&mut db, "insert rel into Link values (9 -> 2, 10)");
            if checkpoint {
                db.checkpoint().unwrap();
            }
            let old = db.snapshot();
            let plan = query(
                "textscan(Document.body, \"rust\", k=10) as d | expand_rel Link out as q via e | project d.id, q.id, e.weight, scoreof(d)",
            );
            let old_result = old.run(&plan).unwrap();
            assert_eq!(old_result.rows.len(), 1);
            let mut tx = db.begin().unwrap();
            tx.execute(&statement("detach delete from Document where id = 9"))
                .unwrap();
            tx.execute(&statement(
                "insert into Document values (9, \"rust\", 99, \"new\")",
            ))
            .unwrap();
            tx.execute(&statement("insert rel into Link values (9 -> 1, 77)"))
                .unwrap();
            let own = tx.run(&plan).unwrap();
            assert_eq!(own.rows.len(), 1);
            assert_eq!(
                own.rows[0][..3],
                [Value::Int64(9), Value::Int64(1), Value::Int64(77)]
            );
            tx.commit().unwrap();
            assert_eq!(db.run(&plan).unwrap(), own);
            db.checkpoint().unwrap();
            let current = db.run(&plan).unwrap();
            assert_eq!(current.rows[0][..3], own.rows[0][..3]);
            assert_eq!(old.run(&plan).unwrap(), old_result);
        }
    }
    #[test]
    fn follower_refresh_preserves_old_ranked_snapshot() {
        let (dir, mut db) = setup();
        db.checkpoint().unwrap();
        drop(db);
        let path = dir.path().join("db");
        Database::activate_multiprocess(&path).unwrap();
        let mut writer = Database::open(&path).unwrap();
        let follower = Database::open_read_only(&path).unwrap();
        let old = follower.snapshot();
        let before = old.run(&ranked()).unwrap();
        execute(
            &mut writer,
            "update Document set body = \"other\" where id = 1",
        );
        follower.refresh().unwrap();
        assert_eq!(follower.snapshot().run(&ranked()).unwrap().rows.len(), 1);
        writer.checkpoint().unwrap();
        follower.refresh().unwrap();
        assert_eq!(
            follower.snapshot().run(&ranked()).unwrap().rows[0][0],
            Value::Int64(9)
        );
        assert_eq!(old.run(&ranked()).unwrap(), before);
    }
    #[test]
    fn wal_child() {
        let Ok(path) = std::env::var("DEVONDB_TEXT_WAL_CHILD") else {
            return;
        };
        let mut db = Database::create(&path, 4096).unwrap();
        execute(
            &mut db,
            "create node table Document (id Int64 primary key, body String)",
        );
        execute(
            &mut db,
            "create rel table Link from Document to Document (weight Int64)",
        );
        execute(
            &mut db,
            "insert into Document values (9, \"rust graph\"), (1, \"rust graph\")",
        );
        execute(&mut db, "insert rel into Link values (9 -> 1, 10)");
        db.checkpoint().unwrap();
        let text = "textscan(Document.body, \"rust\", k=10) as d | expand_rel Link out as q via e | project d.id, q.id, e.weight, scoreof(d)";
        db.pin("search", "search Document body for rust", &query(text))
            .unwrap();
        let mut tx = db.begin().unwrap();
        tx.execute(&statement("detach delete from Document where id = 9"))
            .unwrap();
        tx.execute(&statement(
            "insert into Document values (9, \"rust graph\")",
        ))
        .unwrap();
        tx.execute(&statement("insert rel into Link values (9 -> 1, 77)"))
            .unwrap();
        tx.commit().unwrap();
        let result = db.run_pin("search").unwrap();
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0][2], Value::Int64(77));
        assert!(
            std::fs::metadata(format!("{path}-wal")).unwrap().len() > 0,
            "mutation must remain in WAL"
        );
        println!("TEXT_WAL_READY");
        use std::io::Write;
        std::io::stdout().flush().unwrap();
        loop {
            std::thread::park();
        }
    }
    #[test]
    fn killed_wal_replay_keeps_pinned_search_and_fresh_edge() {
        use std::io::{BufRead, BufReader};
        use std::process::{Command, Stdio};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db");
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "enabled::wal_child", "--nocapture"])
            .env("DEVONDB_TEXT_WAL_CHILD", &path)
            .env("DEVONDB_AUTOCHECKPOINT", "off")
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
        loop {
            if lines.next().expect("child must reach marker").unwrap() == "TEXT_WAL_READY" {
                break;
            }
        }
        child.kill().unwrap();
        child.wait().unwrap();
        let mut db = Database::open(&path).unwrap();
        let recovered = db.run_pin("search").unwrap();
        assert_eq!(recovered.rows.len(), 1);
        assert_eq!(
            recovered.rows[0][..3],
            [Value::Int64(9), Value::Int64(1), Value::Int64(77)]
        );
        assert!(matches!(recovered.rows[0][3],Value::Float64(n) if n>0.0));
        db.checkpoint().unwrap();
        assert_eq!(db.run_pin("search").unwrap(), recovered);
    }
}
