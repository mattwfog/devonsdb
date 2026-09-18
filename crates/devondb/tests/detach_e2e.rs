//! Public-facade, crash-image, budget, and follower gates for detach-delete.

use std::{
    env,
    ffi::OsString,
    fs,
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::Duration,
};

use devondb::{Database, DevonError, Options, Plan, QueryResult};
use devondb_plan::text::parser::{Parsed, parse};
use devondb_storage::{
    pager::Pager,
    superblock::{DML_WAL_FLAG, READ_SAFE_FLAG_MASK, REL_TOMBSTONE_WAL_FLAG, SUPPORTED_FLAG_MASK},
    txn_log::{RelEndpoint, WalPayload, decode_payload, encode_payload},
    wal::{WalWriter, replay},
};
use devondb_types::{DevonResult, value::Value};
use tempfile::tempdir;

const PAGE_SIZE: u32 = 4096;
const LOW_LIMIT: usize = 1024 * 1024;
const HIGH_LIMIT: usize = 32 * LOW_LIMIT;
const CRASH_CHILD_ENV: &str = "DEVONDB_DETACH_CRASH_CHILD";
const CRASH_PATH_ENV: &str = "DEVONDB_DETACH_CRASH_PATH";
const CRASH_PHASE_ENV: &str = "DEVONDB_DETACH_CRASH_PHASE";
const FOLLOWER_CHILD_ENV: &str = "DEVONDB_DETACH_FOLLOWER_CHILD";
const FOLLOWER_PATH_ENV: &str = "DEVONDB_DETACH_FOLLOWER_PATH";
const AUTOCHECKPOINT_ENV: &str = "DEVONDB_AUTOCHECKPOINT";
const RECOVERY_CHILD_ENV: &str = "DEVONDB_DETACH_DOWNLEVEL_RECOVERY_CHILD";
const READY_MARKER: &str = "DETACH_CRASH_READY";
const FOLLOWER_MARKER: &str = "DETACH_FOLLOWER";

#[derive(Debug, PartialEq)]
struct ConsumerReads {
    people: QueryResult,
    knows: QueryResult,
    works_at: QueryResult,
    sponsors: QueryResult,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct CrashReads {
    people: Vec<i64>,
    edges: Vec<(i64, i64)>,
}

#[test]
fn execute_text_consumer_story_is_exact_before_and_after_checkpoint_reopen() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("consumer.devondb");
    let mut database = Database::create(&path, PAGE_SIZE).unwrap();
    build_consumer_graph(&mut database);
    database.checkpoint().unwrap();
    let pinned = database.snapshot();
    let before = consumer_reads_with(|input| pinned.run(&query_plan(input)).unwrap());

    command(&mut database, "detach delete from Person where id = 2");
    let expected = expected_consumer_reads();
    assert_eq!(read_consumer(&mut database), expected);
    assert_eq!(
        consumer_reads_with(|input| pinned.run(&query_plan(input)).unwrap()),
        before
    );

    database.checkpoint().unwrap();
    let checkpointed = read_consumer(&mut database);
    assert_eq!(checkpointed, expected);
    drop(pinned);
    drop(database);

    let mut reopened = Database::open(&path).unwrap();
    assert_eq!(read_consumer(&mut reopened), checkpointed);
}

#[test]
fn execute_text_plain_delete_keeps_its_schema_refusal_and_graph_unchanged() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("plain-refusal.devondb");
    let mut database = crash_graph(&path);

    let error = execute_text(&mut database, "delete from Person where id = 2").unwrap_err();

    assert!(
        error
            .to_string()
            .contains("relationship table names it as an endpoint")
    );
    assert_eq!(crash_reads(&mut database), old_crash_reads());
}

#[cfg(unix)]
#[test]
fn kill9_detach_matrix_recovers_only_whole_old_or_new_graph_states() {
    for phase in [
        "bit-set",
        "mid-group",
        "post-fsync",
        "catalog-publication",
        "wal-truncate",
        "bit-clear",
    ] {
        run_crash_phase(phase);
    }
}

#[test]
fn detach_crash_child_process() {
    if env::var_os(CRASH_CHILD_ENV).is_none() {
        return;
    }
    let path = PathBuf::from(env::var_os(CRASH_PATH_ENV).unwrap());
    let phase = env::var(CRASH_PHASE_ENV).unwrap();
    prepare_crash_phase(&path, &phase);
    println!("{READY_MARKER} {phase}");
    std::io::stdout().flush().unwrap();
    thread::sleep(Duration::from_secs(60));
}

#[test]
fn bit6_aware_bit10_unaware_fixture_refuses_before_pending_wal_decode() {
    if run_autocheckpoint_off_child(
        "bit6_aware_bit10_unaware_fixture_refuses_before_pending_wal_decode",
        RECOVERY_CHILD_ENV,
    ) {
        return;
    }
    const UNKNOWN_TO_CURRENT_BINARY: u64 = 1 << 11;
    let directory = tempdir().unwrap();
    let path = directory.path().join("downlevel.devondb");
    let mut database = crash_graph(&path);
    command(&mut database, "detach delete from Person where id = 2");
    drop(database);

    let records = replay(wal_path(&path)).unwrap();
    assert!(
        records.iter().any(|(_, bytes)| {
            matches!(decode_payload(bytes), Ok(WalPayload::RelDelete { .. }))
        })
    );
    let pager = Pager::open(&path).unwrap();
    let flags = pager.superblock().feature_flags;
    assert_eq!(flags & DML_WAL_FLAG, DML_WAL_FLAG);
    assert_eq!(flags & REL_TOMBSTONE_WAL_FLAG, REL_TOMBSTONE_WAL_FLAG);

    let downlevel_supported = SUPPORTED_FLAG_MASK & !REL_TOMBSTONE_WAL_FLAG;
    assert_eq!(downlevel_supported & DML_WAL_FLAG, DML_WAL_FLAG);
    assert_eq!(READ_SAFE_FLAG_MASK & REL_TOMBSTONE_WAL_FLAG, 0);
    assert_ne!(flags & !(READ_SAFE_FLAG_MASK | downlevel_supported), 0);
    pager
        .commit_feature_flags((flags & !REL_TOMBSTONE_WAL_FLAG) | UNKNOWN_TO_CURRENT_BINARY)
        .unwrap();
    drop(pager);

    let error = Database::open(&path).err().unwrap();
    assert!(matches!(error, DevonError::VersionMismatch { .. }));
    assert!(error.to_string().contains("upgrade devondb"));
    assert!(!matches!(error, DevonError::Corrupt { .. }));
}

#[test]
fn tombstone_heavy_read_and_high_degree_checkpoint_obey_the_budget() {
    const EDGE_COUNT: i64 = 360;
    const PAYLOAD_BYTES: usize = 3072;
    let directory = tempdir().unwrap();
    let path = directory.path().join("budget.devondb");
    build_heavy_graph(&path, EDGE_COUNT, PAYLOAD_BYTES);

    let mut database = Database::open_with(
        &path,
        Options {
            page_size: PAGE_SIZE,
            memory_limit: LOW_LIMIT,
        },
    )
    .unwrap();
    for id in std::iter::once(0).chain(2..=100) {
        command(
            &mut database,
            &format!("detach delete from Person where id = {id}"),
        );
    }

    let nodes = query(&mut database, "nodes(Person) as p | project p.id").rows;
    assert!(!nodes.iter().any(|row| row == &[Value::Int64(0)]));
    assert_budget_bounded(&database);
    let rel_result = execute_text(
        &mut database,
        "nodes(Person) as p | expand Heavy out as q | project p.id, q.id",
    );
    assert_success_or_budget(rel_result);
    assert_budget_bounded(&database);

    let checkpoint = database.checkpoint();
    assert!(
        checkpoint.is_ok() || matches!(checkpoint, Err(DevonError::BudgetExceeded { .. })),
        "unexpected checkpoint result: {checkpoint:?}"
    );
    assert_budget_bounded(&database);
    drop(database);

    let mut recovered = Database::open_with(
        &path,
        Options {
            page_size: PAGE_SIZE,
            memory_limit: HIGH_LIMIT,
        },
    )
    .unwrap();
    assert!(
        query(&mut recovered, "nodes(Person) as p | project p.id")
            .rows
            .iter()
            .all(|row| row != &[Value::Int64(0)])
    );
    assert!(
        query(
            &mut recovered,
            "nodes(Person) as p | expand Heavy out as q | project p.id, q.id",
        )
        .rows
        .is_empty()
    );
    recovered.checkpoint().unwrap();
}

#[cfg(unix)]
#[test]
fn real_process_follower_rebase_keeps_pinned_detach_snapshot_byte_stable() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("follower.devondb");
    let mut database = crash_graph(&path);
    drop(database);
    Database::activate_multiprocess(&path).unwrap();
    database = Database::open(&path).unwrap();

    let mut follower = spawn_follower(&path);
    assert_eq!(
        read_child_marker(&mut follower),
        "READY|1,2,3|1-2,2-1,2-2,3-1"
    );
    command(&mut database, "detach delete from Person where id = 2");
    database.checkpoint().unwrap();

    send_child(&mut follower, "PINNED");
    assert_eq!(
        read_child_marker(&mut follower),
        "PINNED|1,2,3|1-2,2-1,2-2,3-1"
    );
    send_child(&mut follower, "REFRESH");
    assert_eq!(read_child_marker(&mut follower), "FRESH|1,3|3-1");
    send_child(&mut follower, "EXIT");
    assert_eq!(read_child_marker(&mut follower), "EXIT");
    assert_child_success(follower);
}

#[test]
fn detach_follower_child_process() {
    if env::var_os(FOLLOWER_CHILD_ENV).is_none() {
        return;
    }
    let path = PathBuf::from(env::var_os(FOLLOWER_PATH_ENV).unwrap());
    follower_child(&path);
}

#[test]
fn follower_refresh_rechecks_feature_acceptance_before_wal_bytes() {
    const UNKNOWN_FEATURE: u64 = 1 << 11;
    let directory = tempdir().unwrap();
    let path = directory.path().join("feature-refresh.devondb");
    let database = crash_graph(&path);
    drop(database);
    Database::activate_multiprocess(&path).unwrap();
    let follower = Database::open_read_only(&path).unwrap();
    let pinned = follower.snapshot();

    let pager = Pager::open(&path).unwrap();
    let flags = pager.superblock().feature_flags | UNKNOWN_FEATURE;
    pager.commit_feature_flags(flags).unwrap();
    drop(pager);
    fs::write(wal_path(&path), b"not a WAL envelope").unwrap();

    for _ in 0..2 {
        let error = follower.refresh().unwrap_err();
        assert!(matches!(error, DevonError::VersionMismatch { .. }));
        assert!(error.to_string().contains("upgrade devondb"));
    }
    assert_eq!(
        crash_reads_with(|input| pinned.run(&query_plan(input)).unwrap()),
        old_crash_reads()
    );
}

fn build_consumer_graph(database: &mut Database) {
    for statement in [
        "create node table Person (id Int64 primary key, name String)",
        "create node table Company (id Int64 primary key, name String)",
        "create rel table Knows from Person to Person (kind String)",
        "create rel table WorksAt from Person to Company (role String)",
        "create rel table Sponsors from Company to Person",
        "insert into Person values (1, \"Ada\"), (2, \"Grace\"), (3, \"Linus\"), (4, \"Barbara\")",
        "insert into Company values (10, \"Analytical\"), (20, \"Navy\")",
        "insert rel into Knows values (1 -> 2, \"dup\"), (1 -> 2, \"dup\"), (2 -> 1, \"in\"), (2 -> 2, \"self\"), (2 -> 3, \"out\"), (3 -> 4, \"keep\"), (4 -> 1, \"keep\")",
        "insert rel into WorksAt values (2 -> 10, \"remove\"), (1 -> 10, \"keep\"), (3 -> 20, \"keep\")",
        "insert rel into Sponsors values (10 -> 2), (20 -> 3)",
    ] {
        command(database, statement);
    }
}

fn read_consumer(database: &mut Database) -> ConsumerReads {
    consumer_reads_with(|input| query(database, input))
}

fn consumer_reads_with(mut run: impl FnMut(&str) -> QueryResult) -> ConsumerReads {
    ConsumerReads {
        people: run(people_query()),
        knows: run(knows_query()),
        works_at: run(works_at_query()),
        sponsors: run(sponsors_query()),
    }
}

fn expected_consumer_reads() -> ConsumerReads {
    ConsumerReads {
        people: result(
            &["id", "name"],
            vec![
                vec![Value::Int64(1), Value::String("Ada".into())],
                vec![Value::Int64(3), Value::String("Linus".into())],
                vec![Value::Int64(4), Value::String("Barbara".into())],
            ],
        ),
        knows: pair_result(&[(3, 4), (4, 1)]),
        works_at: pair_result(&[(1, 10), (3, 20)]),
        sponsors: pair_result(&[(20, 3)]),
    }
}

fn result(columns: &[&str], rows: Vec<Vec<Value>>) -> QueryResult {
    QueryResult {
        columns: columns.iter().map(|column| (*column).to_owned()).collect(),
        rows,
    }
}

fn pair_result(pairs: &[(i64, i64)]) -> QueryResult {
    result(
        &["source", "target"],
        pairs
            .iter()
            .map(|(source, target)| vec![Value::Int64(*source), Value::Int64(*target)])
            .collect(),
    )
}

fn people_query() -> &'static str {
    "nodes(Person) as p | sort p.id | project p.id as id, p.name as name"
}

fn knows_query() -> &'static str {
    "nodes(Person) as p | expand Knows out as q | sort p.id, q.id | project p.id as source, q.id as target"
}

fn works_at_query() -> &'static str {
    "nodes(Person) as p | expand WorksAt out as c | sort p.id, c.id | project p.id as source, c.id as target"
}

fn sponsors_query() -> &'static str {
    "nodes(Company) as c | expand Sponsors out as p | sort c.id, p.id | project c.id as source, p.id as target"
}

#[cfg(unix)]
fn run_crash_phase(phase: &str) {
    let directory = tempdir().unwrap();
    let path = directory.path().join(format!("{phase}.devondb"));
    let database = crash_graph(&path);
    drop(database);
    let mut child = spawn_crash_child(&path, phase);
    read_until_ready(&mut child, phase);
    child.kill().unwrap();
    assert!(!child.wait().unwrap().success());
    assert_child_stderr_empty(&mut child);

    let mut recovered = Database::open(&path).unwrap();
    let observed = crash_reads(&mut recovered);
    assert!(
        observed == old_crash_reads() || observed == new_crash_reads(),
        "{phase} exposed a node/edge half: {observed:?}"
    );
    let expected = if matches!(phase, "bit-set" | "mid-group") {
        old_crash_reads()
    } else {
        new_crash_reads()
    };
    assert_eq!(observed, expected, "unexpected {phase} recovery side");
}

fn crash_graph(path: &Path) -> Database {
    let mut database = Database::create(path, PAGE_SIZE).unwrap();
    for statement in [
        "create node table Person (id Int64 primary key)",
        "create rel table Knows from Person to Person",
        "insert into Person values (1), (2), (3)",
        "insert rel into Knows values (1 -> 2), (2 -> 1), (2 -> 2), (3 -> 1)",
    ] {
        command(&mut database, statement);
    }
    database.checkpoint().unwrap();
    database
}

fn prepare_crash_phase(path: &Path, phase: &str) {
    match phase {
        "bit-set" => set_detach_flags(path),
        "mid-group" => prepare_incomplete_detach_group(path),
        "post-fsync" => commit_detach(path),
        "catalog-publication" => prepare_catalog_publication_image(path),
        "wal-truncate" => prepare_wal_truncate_image(path),
        "bit-clear" => checkpoint_detach(path),
        other => panic!("unknown crash phase {other}"),
    }
}

fn set_detach_flags(path: &Path) {
    let pager = Pager::open(path).unwrap();
    let flags = pager.superblock().feature_flags | DML_WAL_FLAG | REL_TOMBSTONE_WAL_FLAG;
    pager.commit_feature_flags(flags).unwrap();
}

fn prepare_incomplete_detach_group(path: &Path) {
    set_detach_flags(path);
    let pager = Pager::open(path).unwrap();
    let next_lsn = pager.superblock().checkpoint_lsn + 1;
    drop(pager);
    let payload = encode_payload(&WalPayload::RelDelete {
        rel: "Knows".into(),
        endpoint: RelEndpoint::From,
        offset: 1,
    })
    .unwrap();
    let mut wal = WalWriter::open(wal_path(path), next_lsn).unwrap();
    wal.append(&payload).unwrap();
    wal.sync().unwrap();
}

fn commit_detach(path: &Path) {
    let mut database = Database::open(path).unwrap();
    command(&mut database, "detach delete from Person where id = 2");
}

fn checkpoint_detach(path: &Path) {
    let mut database = Database::open(path).unwrap();
    command(&mut database, "detach delete from Person where id = 2");
    database.checkpoint().unwrap();
}

fn prepare_catalog_publication_image(path: &Path) {
    let mut database = Database::open(path).unwrap();
    command(&mut database, "detach delete from Person where id = 2");
    let pending_wal = fs::read(wal_path(path)).unwrap();
    database.checkpoint().unwrap();
    drop(database);
    fs::write(wal_path(path), pending_wal).unwrap();
    set_detach_flags(path);
}

fn prepare_wal_truncate_image(path: &Path) {
    checkpoint_detach(path);
    set_detach_flags(path);
    assert_eq!(fs::metadata(wal_path(path)).unwrap().len(), 0);
}

#[cfg(unix)]
fn spawn_crash_child(path: &Path, phase: &str) -> Child {
    Command::new(env::current_exe().unwrap())
        .args([
            "--exact",
            "detach_crash_child_process",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CRASH_CHILD_ENV, "1")
        .env(CRASH_PATH_ENV, path)
        .env(CRASH_PHASE_ENV, phase)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}

fn run_autocheckpoint_off_child(test_name: &str, child_env: &str) -> bool {
    if env::var_os(child_env).is_some() {
        return false;
    }
    let output = Command::new(env::current_exe().unwrap())
        .args(["--exact", test_name, "--nocapture", "--test-threads=1"])
        .env(child_env, "1")
        .env(AUTOCHECKPOINT_ENV, "off")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "auto-checkpoint-off child failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    true
}

#[cfg(unix)]
fn read_until_ready(child: &mut Child, phase: &str) {
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    loop {
        let mut line = String::new();
        assert_ne!(stdout.read_line(&mut line).unwrap(), 0);
        if line.contains(READY_MARKER) {
            assert!(line.contains(phase));
            return;
        }
    }
}

fn crash_reads(database: &mut Database) -> CrashReads {
    crash_reads_with(|input| query(database, input))
}

fn crash_reads_with(mut run: impl FnMut(&str) -> QueryResult) -> CrashReads {
    CrashReads {
        people: run("nodes(Person) as p | sort p.id | project p.id")
            .rows
            .iter()
            .map(|row| int_at(row, 0))
            .collect(),
        edges: run(
            "nodes(Person) as p | expand Knows out as q | sort p.id, q.id | project p.id, q.id",
        )
        .rows
        .iter()
        .map(|row| (int_at(row, 0), int_at(row, 1)))
        .collect(),
    }
}

fn old_crash_reads() -> CrashReads {
    CrashReads {
        people: vec![1, 2, 3],
        edges: vec![(1, 2), (2, 1), (2, 2), (3, 1)],
    }
}

fn new_crash_reads() -> CrashReads {
    CrashReads {
        people: vec![1, 3],
        edges: vec![(3, 1)],
    }
}

fn build_heavy_graph(path: &Path, edge_count: i64, payload_bytes: usize) {
    let mut database = Database::create_with(
        path,
        Options {
            page_size: PAGE_SIZE,
            memory_limit: HIGH_LIMIT,
        },
    )
    .unwrap();
    command(
        &mut database,
        "create node table Person (id Int64 primary key)",
    );
    command(
        &mut database,
        "create rel table Heavy from Person to Person (payload String)",
    );
    command(&mut database, &node_insert_text(edge_count));
    command(&mut database, &edge_insert_text(edge_count, payload_bytes));
    database.checkpoint().unwrap();
}

fn node_insert_text(edge_count: i64) -> String {
    let values = (0..edge_count + 2)
        .map(|id| format!("({id})"))
        .collect::<Vec<_>>()
        .join(", ");
    format!("insert into Person values {values}")
}

fn edge_insert_text(edge_count: i64, payload_bytes: usize) -> String {
    let payload = "x".repeat(payload_bytes);
    let values = (2..edge_count + 2)
        .map(|source| format!("({source} -> 0, \"{payload}\")"))
        .collect::<Vec<_>>()
        .join(", ");
    format!("insert rel into Heavy values {values}")
}

fn assert_success_or_budget(result: DevonResult<Option<QueryResult>>) {
    match result {
        Ok(Some(query)) => assert!(query.rows.is_empty()),
        Err(DevonError::BudgetExceeded { .. }) => {}
        other => panic!("expected an empty read or BudgetExceeded, got {other:?}"),
    }
}

fn assert_budget_bounded(database: &Database) {
    let budget = database.memory_budget();
    assert!(budget.charged() <= budget.limit());
}

#[cfg(unix)]
fn spawn_follower(path: &Path) -> Child {
    Command::new(env::current_exe().unwrap())
        .args([
            "--exact",
            "detach_follower_child_process",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(FOLLOWER_CHILD_ENV, "1")
        .env(FOLLOWER_PATH_ENV, path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}

fn follower_child(path: &Path) {
    let follower = Database::open_read_only(path).unwrap();
    let pinned = follower.snapshot();
    let pinned_reads = || crash_reads_with(|input| pinned.run(&query_plan(input)).unwrap());
    write_follower_report("READY", &pinned_reads());
    for line in std::io::stdin().lock().lines() {
        match line.unwrap().as_str() {
            "PINNED" => write_follower_report("PINNED", &pinned_reads()),
            "REFRESH" => {
                follower.refresh().unwrap();
                let fresh = follower.snapshot();
                write_follower_report(
                    "FRESH",
                    &crash_reads_with(|input| fresh.run(&query_plan(input)).unwrap()),
                );
            }
            "EXIT" => {
                println!("{FOLLOWER_MARKER} EXIT");
                std::io::stdout().flush().unwrap();
                return;
            }
            command => panic!("unknown follower command {command}"),
        }
    }
}

fn write_follower_report(label: &str, reads: &CrashReads) {
    let people = reads
        .people
        .iter()
        .map(i64::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let edges = reads
        .edges
        .iter()
        .map(|(from, to)| format!("{from}-{to}"))
        .collect::<Vec<_>>()
        .join(",");
    println!("{FOLLOWER_MARKER} {label}|{people}|{edges}");
    std::io::stdout().flush().unwrap();
}

#[cfg(unix)]
fn send_child(child: &mut Child, command: &str) {
    let stdin = child.stdin.as_mut().unwrap();
    writeln!(stdin, "{command}").unwrap();
    stdin.flush().unwrap();
}

#[cfg(unix)]
fn read_child_marker(child: &mut Child) -> String {
    let stdout = child.stdout.as_mut().unwrap();
    let mut line = Vec::new();
    loop {
        let mut byte = [0_u8; 1];
        stdout.read_exact(&mut byte).unwrap();
        if byte[0] != b'\n' {
            line.push(byte[0]);
            continue;
        }
        let text = String::from_utf8(std::mem::take(&mut line)).unwrap();
        if let Some((_, report)) = text.trim().split_once(FOLLOWER_MARKER) {
            return report.trim().to_owned();
        }
    }
}

#[cfg(unix)]
fn assert_child_success(mut child: Child) {
    let status = child.wait().unwrap();
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    assert!(status.success(), "follower failed: {status}; {stderr}");
    assert!(stderr.is_empty(), "follower stderr: {stderr}");
}

fn execute_text(database: &mut Database, input: &str) -> DevonResult<Option<QueryResult>> {
    match parse(input)? {
        Parsed::Statement(envelope) => {
            database.execute(&envelope.stmt)?;
            Ok(None)
        }
        Parsed::Query(plan) => database.run(&plan).map(Some),
    }
}

fn command(database: &mut Database, input: &str) {
    assert!(
        execute_text(database, input).unwrap().is_none(),
        "expected statement: {input}"
    );
}

fn query(database: &mut Database, input: &str) -> QueryResult {
    execute_text(database, input)
        .unwrap()
        .unwrap_or_else(|| panic!("expected query: {input}"))
}

fn query_plan(input: &str) -> Plan {
    let Parsed::Query(plan) = parse(input).unwrap() else {
        panic!("expected query: {input}");
    };
    plan
}

fn int_at(row: &[Value], index: usize) -> i64 {
    let Value::Int64(value) = row[index] else {
        panic!("expected Int64 at column {index}: {row:?}");
    };
    value
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
    with_suffix.into()
}
