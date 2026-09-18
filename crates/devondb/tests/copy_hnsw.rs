//! COPY into HNSW-indexed node tables (`docs/INDEX_BULK_LOAD.md`).
//!
//! Rows and every replacement root publish through one atomic catalog
//! publication: no reachable state has rows visible
//! with incomplete coverage; failures leave the pre-COPY state
//! authoritative; equal sort keys keep CSV record ordinal.

use std::{
    env, fs,
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use devondb::{Database, Options, Plan, QueryResult, Statement};
use devondb_plan::{
    expr::Metric,
    ops::{KnnMode, Operator, PLAN_VERSION},
    text::parser::{Parsed, parse},
};
use devondb_storage::{catalog::Catalog, hnsw::index::load_persisted_index, pager::Pager};
use devondb_types::value::Value;
use tempfile::tempdir;

const PAGE_SIZE: u32 = 4096;
const TIGHT_LIMIT: usize = 1024 * 1024;
const KILL_CHILD_ENV: &str = "DEVONDB_COPY_HNSW_KILL_CHILD";
const KILL_CHILD_PATH_ENV: &str = "DEVONDB_COPY_HNSW_KILL_PATH";
const KILL_CHILD_CSV_ENV: &str = "DEVONDB_COPY_HNSW_KILL_CSV";

fn parsed_statement(text: &str) -> Statement {
    match parse(text).unwrap() {
        Parsed::Statement(statement) => statement.stmt,
        Parsed::Query(_) => panic!("expected statement: {text}"),
    }
}

fn parsed_plan(text: &str) -> Plan {
    match parse(text).unwrap() {
        Parsed::Query(plan) => plan,
        Parsed::Statement(_) => panic!("expected query: {text}"),
    }
}

fn copy_statement(table: &str, path: &Path, sort_by: Option<&str>) -> Statement {
    let quoted = path
        .to_string_lossy()
        .replace('\\', "\\\\")
        .replace('"', "\\\"");
    let sort = sort_by.map_or_else(String::new, |column| format!(" sort by {column}"));
    parsed_statement(&format!("copy {table} from \"{quoted}\"{sort}"))
}

fn knn_plan(table: &str, column: &str, query: &[f32], k: u64, mode: KnnMode) -> Plan {
    Plan {
        v: PLAN_VERSION,
        plan: Operator::KnnScan {
            table: table.to_owned(),
            column: column.to_owned(),
            query: query.to_vec().into(),
            k,
            metric: Metric::L2,
            mode,
        },
    }
}

fn create_doc_table(database: &mut Database) {
    database
        .execute(&parsed_statement(
            "create node table Doc (id Int64 primary key, embedding Vector(4))",
        ))
        .unwrap();
    database
        .execute(&parsed_statement(
            "create hnsw index doc_ann on Doc.embedding metric l2",
        ))
        .unwrap();
}

fn write_doc_csv(path: &Path, ids: impl Iterator<Item = i64>, lane: f32) {
    let mut file = fs::File::create(path).unwrap();
    writeln!(file, "id,embedding").unwrap();
    for id in ids {
        writeln!(file, "{id},\"[{id}, {lane}, 0, 0]\"").unwrap();
    }
}

fn result_ids(result: &QueryResult) -> Vec<i64> {
    result
        .rows
        .iter()
        .map(|row| match &row[0] {
            Value::Int64(id) => *id,
            other => panic!("unexpected id value {other:?}"),
        })
        .collect()
}

/// Reads `(name, root page id, covered rows)` per index straight from the
/// closed file — the object itself, not the API that wrote it.
fn index_roots(path: &Path) -> Vec<(String, u64, u64)> {
    let pager = Pager::open(path).unwrap();
    let catalog = Catalog::load(&pager).unwrap();
    catalog
        .indexes()
        .iter()
        .map(|entry| {
            let index = load_persisted_index(&pager, entry.root).unwrap();
            (entry.name.clone(), entry.root, index.root.covered_rows)
        })
        .collect()
}

#[test]
fn copy_covers_all_rows_via_both_build_paths_and_serves_csv_neighbors() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("covered.devondb");
    let first_csv = directory.path().join("first.csv");
    let second_csv = directory.path().join("second.csv");
    write_doc_csv(&first_csv, 0..40, 0.0);
    write_doc_csv(&second_csv, 100..120, 1.0);
    let mut database = Database::create(&path, PAGE_SIZE).unwrap();
    create_doc_table(&mut database);

    // First COPY: the index covers zero rows — the batched from-scratch
    // build path.
    database
        .execute(&copy_statement("Doc", &first_csv, None))
        .unwrap();
    let query = [35.9_f32, 0.0, 0.0, 0.0];
    let approximate = database
        .run(&knn_plan(
            "Doc",
            "embedding",
            &query,
            3,
            KnnMode::Approximate,
        ))
        .unwrap();
    assert_eq!(result_ids(&approximate)[0], 36, "nearest must be a CSV row");
    let exact = database
        .run(&knn_plan("Doc", "embedding", &query, 3, KnnMode::Exact))
        .unwrap();
    assert_eq!(approximate, exact);

    // Second COPY: the root now covers 40 rows — the catch-up build path.
    database
        .execute(&copy_statement("Doc", &second_csv, None))
        .unwrap();
    let query = [110.1_f32, 1.0, 0.0, 0.0];
    let approximate = database
        .run(&knn_plan(
            "Doc",
            "embedding",
            &query,
            3,
            KnnMode::Approximate,
        ))
        .unwrap();
    assert_eq!(result_ids(&approximate)[0], 110);
    drop(database);

    let roots = index_roots(&path);
    assert_eq!(roots.len(), 1);
    assert_eq!(roots[0].2, 60, "root must cover every row (invariant 5)");
}

#[test]
fn sorted_copy_equal_keys_keep_csv_ordinal() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("ordinal.devondb");
    let csv = directory.path().join("ordinal.csv");
    let mut file = fs::File::create(&csv).unwrap();
    writeln!(file, "id,bucket,embedding").unwrap();
    // Buckets interleave so an unstable sort genuinely permutes equals.
    for id in 0..100_i64 {
        let bucket = id % 2;
        writeln!(file, "{id},{bucket},\"[{id}, 0, 0, 0]\"").unwrap();
    }
    drop(file);
    let mut database = Database::create(&path, PAGE_SIZE).unwrap();
    database
        .execute(&parsed_statement(
            "create node table Keyed (id Int64 primary key, bucket Int64, embedding Vector(4))",
        ))
        .unwrap();
    database
        .execute(&parsed_statement(
            "create hnsw index keyed_ann on Keyed.embedding metric l2",
        ))
        .unwrap();

    database
        .execute(&copy_statement("Keyed", &csv, Some("bucket")))
        .unwrap();

    let stored = database
        .run(&parsed_plan("nodes(Keyed) as r | project r.id"))
        .unwrap();
    let mut expected: Vec<i64> = (0..100).filter(|id| id % 2 == 0).collect();
    expected.extend((0..100).filter(|id| id % 2 == 1));
    assert_eq!(
        result_ids(&stored),
        expected,
        "equal sort keys must keep original CSV record ordinal (docs/SCALE.md §5.4)"
    );
}

#[test]
fn held_snapshot_is_stable_across_indexed_copy() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("snapshot.devondb");
    let csv = directory.path().join("snapshot.csv");
    write_doc_csv(&csv, 10..30, 0.0);
    let mut database = Database::create(&path, PAGE_SIZE).unwrap();
    create_doc_table(&mut database);
    database
        .execute(&parsed_statement(
            "insert into Doc values (1, [1.0, 0.0, 0.0, 0.0])",
        ))
        .unwrap();

    let snapshot = database.snapshot();
    let scan = parsed_plan("nodes(Doc) as d | project d.id");
    let knn = knn_plan(
        "Doc",
        "embedding",
        &[15.2, 0.0, 0.0, 0.0],
        2,
        KnnMode::Approximate,
    );
    let rows_before = snapshot.run(&scan).unwrap();
    let knn_before = snapshot.run(&knn).unwrap();

    database
        .execute(&copy_statement("Doc", &csv, None))
        .unwrap();

    assert_eq!(snapshot.run(&scan).unwrap(), rows_before);
    assert_eq!(snapshot.run(&knn).unwrap(), knn_before);
    let fresh = database.run(&knn).unwrap();
    assert_eq!(result_ids(&fresh)[0], 15, "a fresh read sees the CSV rows");
    assert_eq!(database.run(&scan).unwrap().rows.len(), 21);
}

#[test]
fn budget_refusal_is_measured_and_leaves_pre_copy_state_authoritative() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("refusal.devondb");
    let csv = directory.path().join("refusal.csv");
    let mut database = Database::create(&path, PAGE_SIZE).unwrap();
    database
        .execute(&parsed_statement(
            "create node table Wide (id Int64 primary key, embedding Vector(16384))",
        ))
        .unwrap();
    database
        .execute(&parsed_statement(
            "create hnsw index wide_ann on Wide.embedding metric l2",
        ))
        .unwrap();
    drop(database);

    let mut file = fs::File::create(&csv).unwrap();
    writeln!(file, "id,embedding").unwrap();
    for id in 0..3_i64 {
        let mut components = vec!["1".to_owned()];
        components.resize(16384, "0".to_owned());
        writeln!(file, "{id},\"[{}]\"", components.join(", ")).unwrap();
    }
    drop(file);

    let mut tight = Database::open_with(
        &path,
        Options {
            page_size: PAGE_SIZE,
            memory_limit: TIGHT_LIMIT,
        },
    )
    .unwrap();
    let error = tight
        .execute(&copy_statement("Wide", &csv, None))
        .unwrap_err()
        .to_string();
    for needle in ["wide_ann", "required=", "limit=", "dominant="] {
        assert!(
            error.contains(needle),
            "measured refusal missing `{needle}`: {error}"
        );
    }

    // The pre-COPY state stays authoritative: no rows, index intact.
    assert!(
        tight
            .run(&parsed_plan("nodes(Wide) as w | project w.id"))
            .unwrap()
            .rows
            .is_empty()
    );
    drop(tight);
    let roots = index_roots(&path);
    assert_eq!(roots.len(), 1);
    assert_eq!(roots[0].2, 0, "the old zero-coverage root is unchanged");
}

#[test]
fn multi_index_failure_on_the_last_index_publishes_nothing() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("all-or-nothing.devondb");
    let csv = directory.path().join("all-or-nothing.csv");
    let mut database = Database::create(&path, PAGE_SIZE).unwrap();
    database
        .execute(&parsed_statement(
            "create node table Pair (id Int64 primary key, small Vector(4), big Vector(16384))",
        ))
        .unwrap();
    database
        .execute(&parsed_statement(
            "create hnsw index small_ann on Pair.small metric l2",
        ))
        .unwrap();
    database
        .execute(&parsed_statement(
            "create hnsw index big_ann on Pair.big metric l2",
        ))
        .unwrap();
    drop(database);
    let roots_before = index_roots(&path);
    assert_eq!(roots_before.len(), 2);

    let mut file = fs::File::create(&csv).unwrap();
    writeln!(file, "id,small,big").unwrap();
    for id in 0..3_i64 {
        let mut components = vec!["1".to_owned()];
        components.resize(16384, "0".to_owned());
        writeln!(
            file,
            "{id},\"[{id}, 0, 0, 0]\",\"[{}]\"",
            components.join(", ")
        )
        .unwrap();
    }
    drop(file);

    let mut tight = Database::open_with(
        &path,
        Options {
            page_size: PAGE_SIZE,
            memory_limit: TIGHT_LIMIT,
        },
    )
    .unwrap();
    // small_ann (catalog order first) rebuilds fine under the tight limit;
    // big_ann fails its measured bound — and NOTHING publishes.
    let error = tight
        .execute(&copy_statement("Pair", &csv, None))
        .unwrap_err()
        .to_string();
    assert!(error.contains("big_ann"), "{error}");
    assert!(
        tight
            .run(&parsed_plan("nodes(Pair) as p | project p.id"))
            .unwrap()
            .rows
            .is_empty()
    );
    drop(tight);

    assert_eq!(
        index_roots(&path),
        roots_before,
        "neither the rows nor small_ann's replacement root may be visible (invariant 7)"
    );
}

#[test]
fn same_csv_from_cloned_pre_copy_files_builds_identical_answers() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("determinism.devondb");
    let csv = directory.path().join("determinism.csv");
    write_doc_csv(&csv, 0..64, 0.0);
    let mut database = Database::create(&path, PAGE_SIZE).unwrap();
    create_doc_table(&mut database);
    database.checkpoint().unwrap();
    drop(database);

    // Clones share db_id, so both builds run the same level_seed over the
    // same insertion order (docs/INDEX_BULK_LOAD.md § Determinism).
    let clones = ["a", "b"].map(|label| {
        let clone = directory.path().join(format!("clone-{label}.devondb"));
        fs::copy(&path, &clone).unwrap();
        let mut wal_source = path.as_os_str().to_owned();
        wal_source.push(".wal");
        let mut wal_clone = clone.as_os_str().to_owned();
        wal_clone.push(".wal");
        if fs::metadata::<&Path>(PathBuf::from(&wal_source).as_path()).is_ok() {
            fs::copy(PathBuf::from(wal_source), PathBuf::from(wal_clone)).unwrap();
        }
        clone
    });
    let mut answers = Vec::new();
    for clone in &clones {
        let mut database = Database::open(clone).unwrap();
        database
            .execute(&copy_statement("Doc", &csv, None))
            .unwrap();
        let mut per_query = Vec::new();
        for id in [3_i64, 17, 42, 63] {
            let query = [id as f32 + 0.1, 0.0, 0.0, 0.0];
            per_query.push(
                database
                    .run(&knn_plan(
                        "Doc",
                        "embedding",
                        &query,
                        8,
                        KnnMode::Approximate,
                    ))
                    .unwrap(),
            );
        }
        drop(database);
        answers.push((per_query, index_roots(clone)[0].2));
    }
    assert_eq!(answers[0].0, answers[1].0, "same CSV + same seed diverged");
    assert_eq!(answers[0].1, 64);
    assert_eq!(answers[1].1, 64);
}

#[test]
fn kill_mid_indexed_copy_keeps_pre_copy_state_and_retry_succeeds() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("kill.devondb");
    let csv = directory.path().join("kill.csv");
    let mut database = Database::create(&path, PAGE_SIZE).unwrap();
    create_doc_table(&mut database);
    database
        .execute(&parsed_statement(
            "insert into Doc values (1000000, [0.5, 0.0, 0.0, 0.0])",
        ))
        .unwrap();
    database.checkpoint().unwrap();
    drop(database);
    // 3,000 rows: the first bulk page (~row 2048) opens the kill window
    // long before the child's index build or publication, while the
    // retry's debug-mode from-scratch build stays test-suite fast.
    write_doc_csv(&csv, 0..3_000, 0.0);
    let baseline_len = fs::metadata(&path).unwrap().len();
    let roots_before = index_roots(&path);

    let mut child = Command::new(env::current_exe().unwrap())
        .args([
            "--exact",
            "copy_hnsw_kill_child_process",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(KILL_CHILD_ENV, "1")
        .env(KILL_CHILD_PATH_ENV, &path)
        .env(KILL_CHILD_CSV_ENV, &csv)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut ready = String::new();
    loop {
        let mut line = String::new();
        let bytes = stdout.read_line(&mut line).unwrap();
        assert_ne!(bytes, 0, "kill child exited before announcing readiness");
        ready.push_str(&line);
        if line.contains("COPY_READY") {
            break;
        }
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if fs::metadata(&path).unwrap().len() > baseline_len {
            break;
        }
        assert!(
            child.try_wait().unwrap().is_none(),
            "kill child finished before writing an unpublished page"
        );
        assert!(Instant::now() < deadline, "kill child wrote no page");
        thread::sleep(Duration::from_millis(1));
    }
    child.kill().unwrap();
    let status = child.wait().unwrap();
    assert!(!status.success(), "kill child completed before SIGKILL");

    // C0 is authoritative: the original row, the original root.
    let mut recovered = Database::open(&path).unwrap();
    assert_eq!(
        result_ids(
            &recovered
                .run(&parsed_plan("nodes(Doc) as d | project d.id"))
                .unwrap()
        ),
        vec![1_000_000]
    );
    drop(recovered);
    assert_eq!(index_roots(&path), roots_before);

    // Retry completes and serves a CSV neighbor with full coverage.
    let mut recovered = Database::open(&path).unwrap();
    recovered
        .execute(&copy_statement("Doc", &csv, None))
        .unwrap();
    let nearest = recovered
        .run(&knn_plan(
            "Doc",
            "embedding",
            &[123.4, 0.0, 0.0, 0.0],
            1,
            KnnMode::Approximate,
        ))
        .unwrap();
    assert_eq!(result_ids(&nearest), vec![123]);
    drop(recovered);
    assert_eq!(index_roots(&path)[0].2, 3_001);
}

#[test]
fn copy_hnsw_kill_child_process() {
    if env::var_os(KILL_CHILD_ENV).is_none() {
        return;
    }
    let path = PathBuf::from(env::var_os(KILL_CHILD_PATH_ENV).unwrap());
    let csv = PathBuf::from(env::var_os(KILL_CHILD_CSV_ENV).unwrap());
    let mut database = Database::open(path).unwrap();
    println!("COPY_READY");
    std::io::stdout().flush().unwrap();
    database
        .execute(&copy_statement("Doc", &csv, None))
        .unwrap();
}
