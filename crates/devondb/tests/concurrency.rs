use std::{
    collections::BTreeMap,
    env, fs,
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdout, Command, Stdio},
    sync::{
        Arc, Barrier,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use devondb::{Database, DevonError, Plan, QueryResult, Statement};
use devondb_plan::ops::Operator;
use devondb_types::{logical_type::LogicalType, schema::Column, value::Value};

const PAGE_SIZE: u32 = 4096;
const T1_ROWS_PER_COMMIT: usize = 4;
const T2_WRITERS: usize = 4;
const T2_ROWS_PER_TXN: usize = 3;
const T7_READERS: usize = 4;
const T7_ROWS_PER_BATCH: usize = 4;
const T8_RUNS: usize = 10;
const T8_ROWS_PER_TXN: u64 = 8;
const T8_CHILD_ENV: &str = "DEVONDB_T8_CHILD";
const T8_PATH_ENV: &str = "DEVONDB_T8_PATH";

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct TestDirectory {
    path: PathBuf,
}

impl TestDirectory {
    fn new(label: &str) -> Self {
        let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = env::temp_dir().join(format!(
            "devondb-concurrency-{label}-{}-{timestamp}-{sequence}",
            std::process::id()
        ));
        fs::create_dir(&path).unwrap();
        Self { path }
    }

    fn database(&self) -> PathBuf {
        self.path.join("stress.devondb")
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

#[test]
fn t1_pinned_reader_is_row_identical_across_writer_commits() {
    let directory = TestDirectory::new("t1");
    let path = directory.database();
    let mut database = Database::create(&path, PAGE_SIZE).unwrap();
    create_node_table(&mut database, "Person");

    for iteration in 0..stress_iters() {
        let ready = Arc::new(Barrier::new(2));
        let committed = Arc::new(Barrier::new(2));
        let reader = spawn_t1_reader(database.clone(), Arc::clone(&ready), Arc::clone(&committed));
        let writer = spawn_t1_writer(
            database.clone(),
            Arc::clone(&ready),
            Arc::clone(&committed),
            iteration,
        );

        let (before, after) = reader.join().unwrap();
        writer.join().unwrap();
        assert_eq!(after, before, "T1 iteration {iteration}");
        assert_eq!(
            scan(&database, "Person").rows.len(),
            before.rows.len() + T1_ROWS_PER_COMMIT,
            "T1 iteration {iteration} fresh snapshot"
        );
    }
}

#[test]
fn t2_four_disjoint_writers_commit_every_transaction_once() {
    let directory = TestDirectory::new("t2");
    let path = directory.database();
    let mut database = Database::create(&path, PAGE_SIZE).unwrap();
    create_node_table(&mut database, "Person");
    let transactions = stress_iters();
    let start = Arc::new(Barrier::new(T2_WRITERS));

    let writers = (0..T2_WRITERS)
        .map(|writer| {
            let database = database.clone();
            let start = Arc::clone(&start);
            thread::spawn(move || t2_writer(database, start, writer, transactions))
        })
        .collect::<Vec<_>>();
    for writer in writers {
        writer.join().unwrap();
    }

    let expected = T2_WRITERS * transactions * T2_ROWS_PER_TXN;
    let mut actual = scan_ids(&database, "Person");
    actual.sort_unstable();
    assert_eq!(actual, (0..expected as i64).collect::<Vec<_>>());
}

#[test]
fn t3_same_primary_key_conflicts_in_both_commit_orders() {
    for iteration in 0..stress_iters() {
        for winner in 0..2 {
            run_t3_case(iteration, winner);
        }
    }
}

#[test]
fn t4_conflict_summary_survives_checkpoint_for_old_writer() {
    let directory = TestDirectory::new("t4");
    let path = directory.database();
    let mut database = Database::create(&path, PAGE_SIZE).unwrap();
    create_node_table(&mut database, "Person");
    let old_started = Arc::new(Barrier::new(2));
    let checkpointed = Arc::new(Barrier::new(2));

    let old_writer = spawn_t4_old_writer(
        database.clone(),
        Arc::clone(&old_started),
        Arc::clone(&checkpointed),
    );
    let winner = spawn_t4_winner(database.clone(), old_started, checkpointed);
    winner.join().unwrap().unwrap();
    let error = old_writer.join().unwrap().unwrap_err();

    assert_conflict(
        error,
        "transaction conflict: node table `Person` primary key 7 was inserted by a concurrent transaction (committed at LSN 4)",
    );
    assert_eq!(scan_ids(&database, "Person"), vec![7]);
}

#[test]
fn t5_concurrent_table_creation_has_one_winner_and_one_conflict() {
    let directory = TestDirectory::new("t5");
    let path = directory.database();
    let database = Database::create(&path, PAGE_SIZE).unwrap();
    let ready = Arc::new(Barrier::new(2));
    let creators = (0..2)
        .map(|_| {
            let database = database.clone();
            let ready = Arc::clone(&ready);
            thread::spawn(move || {
                let mut transaction = database.begin().unwrap();
                transaction.execute(&create_table("T")).unwrap();
                ready.wait();
                transaction.commit()
            })
        })
        .collect::<Vec<_>>();

    let mut successes = 0;
    let mut errors = Vec::new();
    for creator in creators {
        match creator.join().unwrap() {
            Ok(()) => successes += 1,
            Err(error) => errors.push(error),
        }
    }
    assert_eq!(successes, 1);
    assert_eq!(errors.len(), 1);
    assert_conflict(
        errors.pop().unwrap(),
        "transaction conflict: table `T` was created by a concurrent transaction (committed at LSN 2)",
    );
    assert!(scan(&database, "T").rows.is_empty());
}

#[test]
fn t6_pinned_reader_survives_commit_and_checkpoint_page_replacement() {
    let directory = TestDirectory::new("t6");
    let path = directory.database();
    let mut database = Database::create(&path, PAGE_SIZE).unwrap();
    create_node_table(&mut database, "Person");
    database.execute(&insert_ids("Person", 0..2)).unwrap();
    database.checkpoint().unwrap();
    let ready = Arc::new(Barrier::new(2));
    let checkpointed = Arc::new(Barrier::new(2));

    let reader = spawn_t6_reader(
        database.clone(),
        Arc::clone(&ready),
        Arc::clone(&checkpointed),
    );
    let writer = spawn_t6_writer(database.clone(), ready, checkpointed);
    let (before, after) = reader.join().unwrap();
    writer.join().unwrap().unwrap();

    assert_eq!(after, before);
    assert_eq!(result_ids(before), vec![0, 1]);
    assert_eq!(scan_ids(&database, "Person"), vec![0, 1, 2, 3, 4, 5]);
}

#[test]
fn t7_writer_checkpoint_and_readers_preserve_prefix_consistency() {
    let directory = TestDirectory::new("t7");
    let path = directory.database();
    let mut database = Database::create(&path, PAGE_SIZE).unwrap();
    create_node_table(&mut database, "Person");
    let stop = Arc::new(AtomicBool::new(false));
    let committed = Arc::new(AtomicUsize::new(0));
    let start = Arc::new(Barrier::new(T7_READERS + 2));

    let writer = spawn_t7_writer(
        database.clone(),
        Arc::clone(&start),
        Arc::clone(&stop),
        Arc::clone(&committed),
    );
    let checkpointer =
        spawn_t7_checkpointer(database.clone(), Arc::clone(&start), Arc::clone(&stop));
    let readers = (0..T7_READERS)
        .map(|_| spawn_t7_reader(database.clone(), Arc::clone(&start), Arc::clone(&stop)))
        .collect::<Vec<_>>();

    thread::sleep(Duration::from_secs(2));
    stop.store(true, Ordering::Release);
    let written = writer.join().unwrap();
    let checkpoints = checkpointer.join().unwrap();
    assert_eq!(written, committed.load(Ordering::Acquire));
    assert!(checkpoints > 0);
    for reader in readers {
        assert!(reader.join().unwrap() > 0);
    }
    assert_prefix(&scan(&database, "Person"), T7_ROWS_PER_BATCH);
    assert_eq!(
        scan_ids(&database, "Person").len(),
        written * T7_ROWS_PER_BATCH
    );
}

#[test]
fn t8_sigkill_recovers_acknowledged_transactions_without_partial_tail() {
    let mut random = random_seed();
    for run in 0..T8_RUNS {
        random = xorshift(random);
        run_t8_crash_case(run, random);
    }
}

#[test]
fn t8_commit_child_process() {
    if env::var_os(T8_CHILD_ENV).is_none() {
        return;
    }
    let path = PathBuf::from(env::var_os(T8_PATH_ENV).unwrap());
    let database = Database::open(path).unwrap();
    let stdout = std::io::stdout();
    let mut stdout = stdout.lock();
    writeln!(stdout, "T8_READY").unwrap();
    stdout.flush().unwrap();

    // Intentionally endless: this child inserts and ACKs until the harness kills it.
    let mut transaction_id = 0_u64;
    loop {
        let first = transaction_id.checked_mul(T8_ROWS_PER_TXN).unwrap();
        let mut transaction = database.begin().unwrap();
        transaction
            .execute(&insert_ids(
                "Person",
                (first..first + T8_ROWS_PER_TXN).map(|id| id as i64),
            ))
            .unwrap();
        transaction.commit().unwrap();
        writeln!(stdout, "T8_ACK {transaction_id}").unwrap();
        stdout.flush().unwrap();
        transaction_id += 1;
    }
}

fn spawn_t1_reader(
    database: Database,
    ready: Arc<Barrier>,
    committed: Arc<Barrier>,
) -> thread::JoinHandle<(QueryResult, QueryResult)> {
    thread::spawn(move || {
        let snapshot = database.snapshot();
        let before = snapshot.run(&scan_plan("Person", "p")).unwrap();
        ready.wait();
        committed.wait();
        let after = snapshot.run(&scan_plan("Person", "p")).unwrap();
        (before, after)
    })
}

fn spawn_t1_writer(
    database: Database,
    ready: Arc<Barrier>,
    committed: Arc<Barrier>,
    iteration: usize,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        ready.wait();
        let first = (iteration * T1_ROWS_PER_COMMIT) as i64;
        let mut transaction = database.begin().unwrap();
        transaction
            .execute(&insert_ids(
                "Person",
                first..first + T1_ROWS_PER_COMMIT as i64,
            ))
            .unwrap();
        transaction.commit().unwrap();
        committed.wait();
    })
}

fn t2_writer(database: Database, start: Arc<Barrier>, writer: usize, transactions: usize) {
    start.wait();
    for transaction in 0..transactions {
        let jitter = ((writer * 17 + transaction * 13) % 9) as u64;
        thread::sleep(Duration::from_micros(jitter * 40));
        let first = ((writer * transactions + transaction) * T2_ROWS_PER_TXN) as i64;
        let mut write = database.begin().unwrap();
        write
            .execute(&insert_ids("Person", first..first + T2_ROWS_PER_TXN as i64))
            .unwrap();
        write.commit().unwrap();
        thread::yield_now();
    }
}

fn run_t3_case(iteration: usize, winner: usize) {
    let directory = TestDirectory::new("t3");
    let path = directory.database();
    let mut database = Database::create(&path, PAGE_SIZE).unwrap();
    create_node_table(&mut database, "Person");
    let ready = Arc::new(Barrier::new(2));
    let winner_done = Arc::new(Barrier::new(2));
    let contenders = (0..2)
        .map(|index| {
            spawn_t3_contender(
                database.clone(),
                ready.clone(),
                winner_done.clone(),
                index == winner,
            )
        })
        .collect::<Vec<_>>();
    let results = contenders
        .into_iter()
        .map(|contender| contender.join().unwrap())
        .collect::<Vec<_>>();

    assert!(
        results[winner].is_ok(),
        "T3 iteration {iteration}, winner {winner}"
    );
    let loser = 1 - winner;
    let error = results.into_iter().nth(loser).unwrap().unwrap_err();
    assert_conflict(
        error,
        "transaction conflict: node table `Person` primary key 7 was inserted by a concurrent transaction (committed at LSN 4)",
    );
    assert_eq!(scan_ids(&database, "Person"), vec![7]);
}

fn spawn_t3_contender(
    database: Database,
    ready: Arc<Barrier>,
    winner_done: Arc<Barrier>,
    wins: bool,
) -> thread::JoinHandle<Result<(), DevonError>> {
    thread::spawn(move || {
        let mut transaction = database.begin().unwrap();
        transaction.execute(&insert_ids("Person", [7])).unwrap();
        ready.wait();
        if wins {
            let result = transaction.commit();
            winner_done.wait();
            result
        } else {
            winner_done.wait();
            transaction.commit()
        }
    })
}

fn spawn_t4_old_writer(
    database: Database,
    old_started: Arc<Barrier>,
    checkpointed: Arc<Barrier>,
) -> thread::JoinHandle<Result<(), DevonError>> {
    thread::spawn(move || {
        let mut transaction = database.begin().unwrap();
        old_started.wait();
        checkpointed.wait();
        transaction.execute(&insert_ids("Person", [7])).unwrap();
        transaction.commit()
    })
}

fn spawn_t4_winner(
    mut database: Database,
    old_started: Arc<Barrier>,
    checkpointed: Arc<Barrier>,
) -> thread::JoinHandle<Result<(), DevonError>> {
    thread::spawn(move || {
        old_started.wait();
        let result = (|| {
            let mut transaction = database.begin()?;
            transaction.execute(&insert_ids("Person", [7]))?;
            transaction.commit()?;
            database.checkpoint()
        })();
        checkpointed.wait();
        result
    })
}

fn spawn_t6_reader(
    database: Database,
    ready: Arc<Barrier>,
    checkpointed: Arc<Barrier>,
) -> thread::JoinHandle<(QueryResult, QueryResult)> {
    thread::spawn(move || {
        let snapshot = database.snapshot();
        let before = snapshot.run(&scan_plan("Person", "p")).unwrap();
        ready.wait();
        checkpointed.wait();
        let after = snapshot.run(&scan_plan("Person", "p")).unwrap();
        (before, after)
    })
}

fn spawn_t6_writer(
    mut database: Database,
    ready: Arc<Barrier>,
    checkpointed: Arc<Barrier>,
) -> thread::JoinHandle<Result<(), DevonError>> {
    thread::spawn(move || {
        ready.wait();
        let result = (|| {
            let mut transaction = database.begin()?;
            transaction.execute(&insert_ids("Person", 2..6))?;
            transaction.commit()?;
            database.checkpoint()
        })();
        checkpointed.wait();
        result
    })
}

fn spawn_t7_writer(
    database: Database,
    start: Arc<Barrier>,
    stop: Arc<AtomicBool>,
    committed: Arc<AtomicUsize>,
) -> thread::JoinHandle<usize> {
    thread::spawn(move || {
        start.wait();
        let mut batch = 0;
        while !stop.load(Ordering::Acquire) {
            let first = (batch * T7_ROWS_PER_BATCH) as i64;
            let mut transaction = database.begin().unwrap();
            transaction
                .execute(&insert_ids(
                    "Person",
                    first..first + T7_ROWS_PER_BATCH as i64,
                ))
                .unwrap();
            transaction.commit().unwrap();
            batch += 1;
            committed.store(batch, Ordering::Release);
            thread::sleep(Duration::from_millis(1));
        }
        batch
    })
}

fn spawn_t7_checkpointer(
    mut database: Database,
    start: Arc<Barrier>,
    stop: Arc<AtomicBool>,
) -> thread::JoinHandle<usize> {
    thread::spawn(move || {
        start.wait();
        let mut checkpoints = 0;
        while !stop.load(Ordering::Acquire) {
            database.checkpoint().unwrap();
            checkpoints += 1;
            thread::sleep(Duration::from_millis(1));
        }
        checkpoints
    })
}

fn spawn_t7_reader(
    database: Database,
    start: Arc<Barrier>,
    stop: Arc<AtomicBool>,
) -> thread::JoinHandle<usize> {
    thread::spawn(move || {
        start.wait();
        let mut scans = 0;
        while !stop.load(Ordering::Acquire) {
            assert_prefix(&scan(&database, "Person"), T7_ROWS_PER_BATCH);
            scans += 1;
            thread::yield_now();
        }
        scans
    })
}

fn run_t8_crash_case(run: usize, seed: u64) {
    let directory = TestDirectory::new("t8");
    let path = directory.database();
    let mut database = Database::create(&path, PAGE_SIZE).unwrap();
    create_node_table(&mut database, "Person");
    drop(database);

    let mut child = spawn_t8_child(&path);
    let mut output = BufReader::new(child.stdout.take().unwrap());
    let mut acknowledged = Vec::new();
    read_until(&mut output, &mut acknowledged, |line| {
        line.contains("T8_READY")
    });
    read_until(&mut output, &mut acknowledged, |line| {
        line.contains("T8_ACK ")
    });
    thread::sleep(Duration::from_millis(1 + seed % 25));
    child.kill().unwrap();
    let status = child.wait().unwrap();
    assert!(
        !status.success(),
        "T8 run {run}, seed {seed}: child survived"
    );
    drain_acks(&mut output, &mut acknowledged);
    assert_child_stderr_empty(&mut child, run, seed);

    assert_eq!(
        acknowledged,
        (0..acknowledged.len() as u64).collect::<Vec<_>>(),
        "T8 run {run}, seed {seed}: non-sequential acknowledgements"
    );
    let database = Database::open(&path).unwrap();
    assert_t8_recovery(scan_ids(&database, "Person"), &acknowledged, run, seed);
}

fn spawn_t8_child(path: &Path) -> Child {
    Command::new(env::current_exe().unwrap())
        .args([
            "--exact",
            "t8_commit_child_process",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(T8_CHILD_ENV, "1")
        .env(T8_PATH_ENV, path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}

fn read_until(
    output: &mut BufReader<ChildStdout>,
    acknowledged: &mut Vec<u64>,
    mut done: impl FnMut(&str) -> bool,
) {
    loop {
        let mut line = String::new();
        let bytes = output.read_line(&mut line).unwrap();
        assert_ne!(bytes, 0, "T8 child exited before its expected output");
        record_ack(&line, acknowledged);
        if done(&line) {
            return;
        }
    }
}

fn drain_acks(output: &mut BufReader<ChildStdout>, acknowledged: &mut Vec<u64>) {
    loop {
        let mut line = String::new();
        if output.read_line(&mut line).unwrap() == 0 {
            return;
        }
        record_ack(&line, acknowledged);
    }
}

fn record_ack(line: &str, acknowledged: &mut Vec<u64>) {
    if !line.ends_with('\n') {
        return;
    }
    let Some((_, suffix)) = line.split_once("T8_ACK ") else {
        return;
    };
    let digits = suffix
        .chars()
        .take_while(|character| character.is_ascii_digit())
        .collect::<String>();
    if !digits.is_empty() {
        acknowledged.push(digits.parse().unwrap());
    }
}

fn assert_child_stderr_empty(child: &mut Child, run: usize, seed: u64) {
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    assert!(
        stderr.is_empty(),
        "T8 run {run}, seed {seed}: child stderr: {stderr}"
    );
}

fn assert_t8_recovery(ids: Vec<i64>, acknowledged: &[u64], run: usize, seed: u64) {
    let mut groups = BTreeMap::<u64, Vec<u64>>::new();
    for id in ids {
        let id = u64::try_from(id).unwrap();
        groups.entry(id / T8_ROWS_PER_TXN).or_default().push(id);
    }
    let tail = acknowledged.len() as u64;
    for transaction_id in 0..tail {
        let expected = t8_transaction_ids(transaction_id);
        assert_eq!(
            groups.remove(&transaction_id).unwrap_or_default(),
            expected,
            "T8 run {run}, seed {seed}: acknowledged txn {transaction_id}"
        );
    }
    let tail_rows = groups.remove(&tail).unwrap_or_default();
    assert!(
        tail_rows.is_empty() || tail_rows == t8_transaction_ids(tail),
        "T8 run {run}, seed {seed}: partial tail txn {tail}: {tail_rows:?}"
    );
    assert!(
        groups.is_empty(),
        "T8 run {run}, seed {seed}: rows beyond tail: {groups:?}"
    );
}

fn t8_transaction_ids(transaction_id: u64) -> Vec<u64> {
    let first = transaction_id * T8_ROWS_PER_TXN;
    (first..first + T8_ROWS_PER_TXN).collect()
}

fn assert_prefix(result: &QueryResult, rows_per_batch: usize) {
    let ids = result
        .rows
        .iter()
        .map(|row| match row.as_slice() {
            [Value::Int64(id)] => *id,
            other => panic!("unexpected scan row: {other:?}"),
        })
        .collect::<Vec<_>>();
    assert_eq!(ids.len() % rows_per_batch, 0, "partial committed batch");
    assert_eq!(ids, (0..ids.len() as i64).collect::<Vec<_>>());
}

fn create_node_table(database: &mut Database, name: &str) {
    database.execute(&create_table(name)).unwrap();
}

fn create_table(name: &str) -> Statement {
    Statement::CreateNodeTable {
        name: name.to_owned(),
        columns: vec![Column {
            name: "id".to_owned(),
            ty: LogicalType::Int64,
            primary_key: true,
        }],
    }
}

fn insert_ids(table: &str, ids: impl IntoIterator<Item = i64>) -> Statement {
    Statement::InsertNode {
        table: table.to_owned(),
        rows: ids.into_iter().map(|id| vec![Value::Int64(id)]).collect(),
    }
}

fn scan(database: &Database, table: &str) -> QueryResult {
    database.snapshot().run(&scan_plan(table, "n")).unwrap()
}

fn scan_ids(database: &Database, table: &str) -> Vec<i64> {
    result_ids(scan(database, table))
}

fn result_ids(result: QueryResult) -> Vec<i64> {
    result
        .rows
        .into_iter()
        .map(|row| match row.as_slice() {
            [Value::Int64(id)] => *id,
            other => panic!("unexpected scan row: {other:?}"),
        })
        .collect()
}

fn scan_plan(table: &str, binding: &str) -> Plan {
    Plan {
        v: 0,
        plan: Operator::ScanNodes {
            table: table.to_owned(),
            binding: binding.to_owned(),
        },
    }
}

fn assert_conflict(error: DevonError, expected: &str) {
    assert!(matches!(error, DevonError::TransactionConflict { .. }));
    assert_eq!(error.to_string(), expected);
}

fn stress_iters() -> usize {
    let iterations = env::var("DEVONDB_STRESS_ITERS")
        .map(|value| value.parse().unwrap())
        .unwrap_or(25);
    assert!(iterations > 0, "DEVONDB_STRESS_ITERS must be positive");
    iterations
}

fn random_seed() -> u64 {
    let time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64;
    let seed = time ^ u64::from(std::process::id());
    if seed == 0 {
        0x9e37_79b9_7f4a_7c15
    } else {
        seed
    }
}

fn xorshift(mut value: u64) -> u64 {
    value ^= value << 13;
    value ^= value >> 7;
    value ^ (value << 17)
}
