use std::{
    collections::HashMap,
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, ExitStatus, Stdio},
    sync::mpsc::{self, Receiver, RecvTimeoutError},
    thread,
    time::{Duration, Instant},
};

#[cfg(unix)]
use std::fs;

use devondb::{Database, Statement};
#[cfg(unix)]
use devondb_storage::lock::{LockPaths, PublicationGate};
use devondb_types::{logical_type::LogicalType, schema::Column};
use tempfile::{TempDir, tempdir};

#[path = "helpers/mp_worker.rs"]
mod mp_worker;

use mp_worker::DataRow;

const PAGE_SIZE: u32 = 4096;
const READ_DEADLINE: Duration = Duration::from_secs(30);
const SCHEDULER_TOLERANCE: Duration = Duration::from_millis(500);
const READ_LATENCY_BOUND: Duration = Duration::from_secs(1);
const LONG_PAYLOAD_BYTES: u32 = 12 * 1024 * 1024;
const CHECKPOINT_PAYLOAD_BYTES: u32 = 8 * 1024 * 1024;
const KILL_ITERATIONS: u32 = 10;
const WORKER_TEST: &str = "multiprocess_kill_matrix_worker_entry";

struct Fixture {
    _directory: TempDir,
    path: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let directory = tempdir().unwrap();
        let path = directory.path().join("database.devondb");
        let mut database = Database::create(&path, PAGE_SIZE).unwrap();
        database.execute(&create_batch_table()).unwrap();
        database.execute(&create_payload_table()).unwrap();
        drop(database);
        Database::activate_multiprocess(&path).unwrap();
        Self {
            _directory: directory,
            path,
        }
    }
}

#[derive(Debug)]
struct Report {
    pid: u32,
    role: String,
    checkpoint_lsn: u64,
    commit_lsn: u64,
    phase: String,
    rows: Vec<DataRow>,
    note: String,
}

struct Worker {
    child: Child,
    stdin: ChildStdin,
    lines: Receiver<Result<String, String>>,
    role: &'static str,
}

impl Worker {
    fn spawn(role: &'static str, path: &Path) -> Self {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", WORKER_TEST, "--nocapture", "--test-threads=1"])
            .env(mp_worker::ROLE_ENV, role)
            .env(mp_worker::PATH_ENV, path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (sender, lines) = mpsc::channel();
        thread::Builder::new()
            .name(format!("devondb-kill-matrix-{role}-stdout"))
            .spawn(move || {
                for line in BufReader::new(stdout).lines() {
                    let event = line.map_err(|error| error.to_string());
                    if sender.send(event).is_err() {
                        return;
                    }
                }
            })
            .unwrap();
        Self {
            child,
            stdin,
            lines,
            role,
        }
    }

    fn send(&mut self, command: &str) {
        writeln!(self.stdin, "{command}").unwrap();
        self.stdin.flush().unwrap();
    }

    fn command(&mut self, command: &str, expected_phase: &str) -> Report {
        self.send(command);
        self.read_phase(expected_phase)
    }

    fn read_phase(&mut self, expected_phase: &str) -> Report {
        let deadline = Instant::now() + READ_DEADLINE;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let line = match self.lines.recv_timeout(remaining) {
                Ok(Ok(line)) => line,
                Ok(Err(error)) => panic!(
                    "{} worker pipe failed while awaiting phase `{expected_phase}`: {error}",
                    self.role
                ),
                Err(RecvTimeoutError::Timeout) => self.panic_timeout(expected_phase),
                Err(RecvTimeoutError::Disconnected) => self.panic_disconnected(expected_phase),
            };
            let Some(report) = parse_report(&line) else {
                continue;
            };
            assert_eq!(report.role, self.role);
            assert_eq!(
                report.phase, expected_phase,
                "{} worker reported an unexpected phase: {report:?}",
                self.role
            );
            return report;
        }
    }

    fn exit(mut self) {
        let report = self.command("EXIT", "exiting");
        assert_ne!(report.pid, 0);
        let status = self.child.wait().unwrap();
        self.assert_clean_exit(status);
    }

    fn finish(mut self) {
        let status = self.child.wait().unwrap();
        self.assert_clean_exit(status);
    }

    #[cfg(unix)]
    fn kill(mut self) {
        self.child.kill().unwrap();
        let status = self.child.wait().unwrap();
        assert!(
            !status.success(),
            "killed {} worker exited cleanly",
            self.role
        );
    }

    fn panic_timeout(&mut self, phase: &str) -> ! {
        let _ = self.child.kill();
        let status = self.child.wait().ok();
        let stderr = take_stderr(&mut self.child);
        panic!(
            "30s deadline expired awaiting {} phase `{phase}`; status={status:?}; stderr={stderr}",
            self.role
        );
    }

    fn panic_disconnected(&mut self, phase: &str) -> ! {
        let status = self.child.wait().ok();
        let stderr = take_stderr(&mut self.child);
        panic!(
            "{} pipe closed before phase `{phase}`; status={status:?}; stderr={stderr}",
            self.role
        );
    }

    fn assert_clean_exit(&mut self, status: ExitStatus) {
        let stderr = take_stderr(&mut self.child);
        assert!(
            status.success(),
            "{} worker failed with {status}; stderr={stderr}",
            self.role
        );
        assert!(
            stderr.is_empty(),
            "{} worker wrote stderr: {stderr}",
            self.role
        );
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

#[test]
fn multiprocess_kill_matrix_worker_entry() {
    mp_worker::run_if_configured();
}

#[cfg(unix)]
#[test]
fn sigkill_after_writer_lease_acquisition_recovers_empty_database() {
    let fixture = Fixture::new();
    let mut writer = Worker::spawn("writer", &fixture.path);
    let ready = writer.read_phase("ready");
    assert!(ready.rows.is_empty());
    writer.kill();

    recover_and_verify(&fixture.path, 0, false, &[], None);
}

#[cfg(unix)]
#[test]
fn sigkill_between_two_commits_preserves_the_acked_prefix() {
    let fixture = Fixture::new();
    let mut writer = Worker::spawn("writer", &fixture.path);
    writer.read_phase("ready");
    let mid_batch = writer.command("MULTI_COMMIT 0 1", "mid-batch");
    assert_eq!(mid_batch.rows, expected_batches(1));
    writer.kill();

    recover_and_verify(&fixture.path, 1, true, &[], None);
}

#[cfg(unix)]
#[test]
fn sigkill_after_commit_ack_recovers_the_commit_exactly_once() {
    let fixture = Fixture::new();
    let mut writer = Worker::spawn("writer", &fixture.path);
    writer.read_phase("ready");
    let ack = writer.command("COMMIT 0", "committed");
    assert_eq!(ack.rows, expected_batches(1));
    writer.kill();

    recover_and_verify(&fixture.path, 1, false, &[], None);
}

#[cfg(unix)]
#[test]
fn sigkill_during_long_payload_append_is_transaction_atomic() {
    for iteration in 0..KILL_ITERATIONS {
        let fixture = Fixture::new();
        let mut writer = Worker::spawn("writer", &fixture.path);
        writer.read_phase("ready");
        let started = writer.command(
            &format!("LONG_COMMIT {iteration} {LONG_PAYLOAD_BYTES}"),
            "long-commit-started",
        );
        assert!(started.rows.is_empty());
        writer.kill();

        recover_and_verify(
            &fixture.path,
            0,
            false,
            &[],
            Some((iteration, LONG_PAYLOAD_BYTES)),
        );
    }
}

#[cfg(unix)]
#[test]
fn sigkill_during_checkpoint_preserves_every_acked_transaction() {
    for iteration in 0..KILL_ITERATIONS {
        let fixture = Fixture::new();
        let mut writer = Worker::spawn("writer", &fixture.path);
        writer.read_phase("ready");
        writer.command(
            &format!("PAYLOAD_COMMIT {iteration} {CHECKPOINT_PAYLOAD_BYTES}"),
            "payload-committed",
        );
        writer.command("CHECKPOINT_IN_FLIGHT", "checkpoint-started");
        writer.kill();

        recover_and_verify(
            &fixture.path,
            0,
            false,
            &[(iteration, CHECKPOINT_PAYLOAD_BYTES)],
            None,
        );
    }
}

#[cfg(unix)]
#[test]
fn sigkill_reader_mid_refresh_loop_changes_no_durable_state() {
    let fixture = Fixture::new();
    let mut writer = Worker::spawn("writer", &fixture.path);
    writer.read_phase("ready");
    let mut reader = Worker::spawn("reader", &fixture.path);
    reader.read_phase("ready");
    writer.command("COMMIT 0", "committed");

    let paths = LockPaths::for_main(&fixture.path).unwrap();
    let gate = PublicationGate::open(&paths).unwrap();
    let guard = gate.try_exclusive().unwrap();
    let refreshing = reader.command("REFRESH_LOOP 0", "refresh-loop");
    assert!(refreshing.rows.is_empty());
    reader.kill();
    drop(guard);
    writer.exit();

    recover_and_verify(&fixture.path, 1, false, &[], None);
}

#[cfg(unix)]
#[test]
fn sigkill_reader_holding_pinned_snapshot_across_checkpoint_is_safe() {
    let fixture = Fixture::new();
    let mut writer = Worker::spawn("writer", &fixture.path);
    writer.read_phase("ready");
    writer.command("COMMIT 0", "committed");
    let mut reader = Worker::spawn("reader", &fixture.path);
    reader.read_phase("ready");
    let pinned = reader.command("PIN_HOLD", "pin-holding");
    assert_eq!(pinned.rows, expected_batches(1));

    writer.command("COMMIT 1", "committed");
    writer.command("CHECKPOINT", "checkpointed");
    let stable = reader.command("CHECK_PIN", "pin-checked");
    assert_eq!(stable.note, "stable=true");
    assert_eq!(stable.rows, expected_batches(1));
    reader.kill();
    writer.exit();

    recover_and_verify(&fixture.path, 2, false, &[], None);
}

#[cfg(unix)]
#[test]
fn replacing_live_writer_sidecar_proves_split_brain_negative() {
    use std::os::unix::fs::MetadataExt;

    let fixture = Fixture::new();
    let paths = LockPaths::for_main(&fixture.path).unwrap();
    let original_inode = fs::metadata(&paths.writer).unwrap().ino();
    let mut original = Worker::spawn("writer", &fixture.path);
    original.read_phase("ready");

    fs::remove_file(&paths.writer).unwrap();
    let mut replacement_owner = Worker::spawn("writer", &fixture.path);
    replacement_owner.read_phase("ready");
    let replacement_inode = fs::metadata(&paths.writer).unwrap().ino();
    assert_ne!(replacement_inode, original_inode);
    let still_alive = original.command("SCAN", "scanned");
    assert_eq!(still_alive.role, "writer");

    replacement_owner.exit();
    original.exit();
}

#[cfg(unix)]
#[test]
fn persistent_writer_sidecar_stays_busy_for_the_holders_lifetime() {
    let fixture = Fixture::new();
    let paths = LockPaths::for_main(&fixture.path).unwrap();
    let original_inode = lock_inode(&paths.writer);
    let mut holder = Worker::spawn("writer", &fixture.path);
    holder.read_phase("ready");

    for _ in 0..2 {
        let mut contender = Worker::spawn("contender", &fixture.path);
        let busy = contender.read_phase("busy");
        assert!(busy.note.contains("kind=Busy"));
        assert!(busy.note.contains("writer lease"));
        contender.finish();
        assert_eq!(lock_inode(&paths.writer), original_inode);
        holder.command("SCAN", "scanned");
    }

    holder.exit();
    let mut successor = Worker::spawn("writer", &fixture.path);
    successor.read_phase("ready");
    assert_eq!(lock_inode(&paths.writer), original_inode);
    successor.exit();
}

#[test]
fn polling_readers_observe_ack_within_poll_bound_plus_tolerance() {
    let fixture = Fixture::new();
    let mut writer = Worker::spawn("writer", &fixture.path);
    writer.read_phase("ready");
    let mut readers = (0..3)
        .map(|_| Worker::spawn("reader", &fixture.path))
        .collect::<Vec<_>>();
    for reader in &mut readers {
        reader.read_phase("ready");
        reader.send("POLL_BATCH 0");
    }
    for reader in &mut readers {
        let polling = reader.read_phase("polling");
        assert!(polling.rows.is_empty());
    }

    let ack = writer.command("COMMIT 0", "committed");
    let acknowledged_at = Instant::now();
    assert_eq!(ack.rows, expected_batches(1));
    for reader in &mut readers {
        let observed = reader.read_phase("observed");
        let latency = acknowledged_at.elapsed();
        assert!(
            latency <= mp_worker::POLL_BOUND + SCHEDULER_TOLERANCE,
            "reader {} observed ack after {latency:?}, beyond {:?}",
            observed.pid,
            mp_worker::POLL_BOUND + SCHEDULER_TOLERANCE
        );
        assert_eq!(observed.rows, expected_batches(1));
        assert_eq!(observed.commit_lsn, ack.commit_lsn);
    }

    for reader in readers {
        reader.exit();
    }
    writer.exit();
}

#[test]
fn query_start_refresh_observes_an_available_commit_immediately() {
    let fixture = Fixture::new();
    let mut writer = Worker::spawn("writer", &fixture.path);
    writer.read_phase("ready");
    let mut reader = Worker::spawn("reader", &fixture.path);
    let ready = reader.read_phase("ready");
    writer.command("COMMIT 0", "committed");

    let started = Instant::now();
    let scanned = reader.command("SCAN", "scanned");
    let latency = started.elapsed();
    assert!(
        latency < READ_LATENCY_BOUND,
        "query-start refresh took {latency:?}"
    );
    assert_eq!(scanned.rows, expected_batches(1));
    assert!(scanned.commit_lsn > ready.commit_lsn);

    reader.exit();
    writer.exit();
}

#[cfg(unix)]
#[test]
fn exclusive_gate_keeps_queries_fast_and_old_then_first_refresh_catches_up() {
    let fixture = Fixture::new();
    let mut writer = Worker::spawn("writer", &fixture.path);
    writer.read_phase("ready");
    let mut reader = Worker::spawn("reader", &fixture.path);
    reader.read_phase("ready");
    writer.command("COMMIT 0", "committed");

    let paths = LockPaths::for_main(&fixture.path).unwrap();
    let gate = PublicationGate::open(&paths).unwrap();
    let guard = gate.try_exclusive().unwrap();
    let started = Instant::now();
    let stale = reader.command("SCAN", "scanned");
    let latency = started.elapsed();
    assert!(
        latency < READ_LATENCY_BOUND,
        "gate-contended query took {latency:?}"
    );
    assert!(stale.rows.is_empty());

    drop(guard);
    let caught_up = reader.command("REFRESH", "refreshed");
    assert_eq!(caught_up.rows, expected_batches(1));
    assert!(caught_up.note.contains("advanced=true"));

    reader.exit();
    writer.exit();
}

#[cfg(unix)]
fn lock_inode(path: &Path) -> u64 {
    use std::os::unix::fs::MetadataExt;

    fs::metadata(path).unwrap().ino()
}

#[cfg(unix)]
fn recover_and_verify(
    path: &Path,
    acknowledged_batches: u32,
    optional_next_batch: bool,
    acknowledged_payloads: &[(u32, u32)],
    optional_payload: Option<(u32, u32)>,
) {
    let mut successor = Worker::spawn("writer", path);
    let recovered = successor.read_phase("ready");
    assert_recovered_batches(&recovered.rows, acknowledged_batches, optional_next_batch);
    let verified = successor.command("VERIFY", "verified");
    assert_recovered_batches(&verified.rows, acknowledged_batches, optional_next_batch);
    assert_payload_verification(&verified.note, acknowledged_payloads, optional_payload);
    assert!(verified.commit_lsn >= verified.checkpoint_lsn);
    successor.exit();
}

fn assert_recovered_batches(rows: &[DataRow], acknowledged: u32, optional_next: bool) {
    let acknowledged_rows = expected_batches(acknowledged);
    if optional_next {
        let with_optional = expected_batches(acknowledged + 1);
        assert!(
            rows == acknowledged_rows || rows == with_optional,
            "recovery exposed neither the acked prefix nor one whole optional transaction: {rows:?}"
        );
    } else {
        assert_eq!(rows, acknowledged_rows);
    }
}

fn assert_payload_verification(
    note: &str,
    acknowledged: &[(u32, u32)],
    optional: Option<(u32, u32)>,
) {
    let fields = note_fields(note);
    assert_eq!(required_field(&fields, "tables"), "Batch,Payload");
    let actual_ids = parse_payload_ids(required_field(&fields, "payload_ids"));
    let actual_count: usize = parse_note_field(&fields, "payload_count");
    let actual_bytes: u64 = parse_note_field(&fields, "payload_bytes");
    assert_eq!(actual_count, actual_ids.len());

    let acknowledged_ids = acknowledged.iter().map(|(id, _)| *id).collect::<Vec<_>>();
    let acknowledged_bytes = payload_bytes(acknowledged);
    if let Some((optional_id, optional_bytes)) = optional {
        let mut with_optional = acknowledged_ids.clone();
        with_optional.push(optional_id);
        assert!(actual_ids == acknowledged_ids || actual_ids == with_optional);
        assert!(
            actual_bytes == acknowledged_bytes
                || actual_bytes == acknowledged_bytes + u64::from(optional_bytes)
        );
        assert_eq!(
            actual_ids == with_optional,
            actual_bytes == acknowledged_bytes + u64::from(optional_bytes)
        );
    } else {
        assert_eq!(actual_ids, acknowledged_ids);
        assert_eq!(actual_bytes, acknowledged_bytes);
    }
}

fn payload_bytes(payloads: &[(u32, u32)]) -> u64 {
    payloads
        .iter()
        .map(|(_, byte_len)| u64::from(*byte_len))
        .sum()
}

fn parse_payload_ids(encoded: &str) -> Vec<u32> {
    if encoded.is_empty() {
        Vec::new()
    } else {
        encoded.split(',').map(|id| id.parse().unwrap()).collect()
    }
}

fn expected_batches(batch_count: u32) -> Vec<DataRow> {
    (0..batch_count)
        .flat_map(|batch| {
            (0..mp_worker::BATCH_ROWS).map(move |ordinal| DataRow {
                id: i64::from(batch * mp_worker::BATCH_ROWS + ordinal),
                batch: i64::from(batch),
                ordinal: i64::from(ordinal),
                name: format!("batch-{batch}-row-{ordinal}"),
            })
        })
        .collect()
}

fn parse_report(line: &str) -> Option<Report> {
    let start = line.find(mp_worker::REPORT_PREFIX)?;
    let fields = line[start + mp_worker::REPORT_PREFIX.len()..]
        .split('\t')
        .filter_map(|field| field.split_once('='))
        .collect::<HashMap<_, _>>();
    let rows = mp_worker::decode_rows(required_field(&fields, "rows")).unwrap();
    let count: usize = parse_field(&fields, "count");
    assert_eq!(count, rows.len(), "worker report row count disagreed");
    Some(Report {
        pid: parse_field(&fields, "pid"),
        role: required_field(&fields, "role").to_owned(),
        checkpoint_lsn: parse_field(&fields, "checkpoint"),
        commit_lsn: parse_field(&fields, "commit"),
        phase: required_field(&fields, "phase").to_owned(),
        rows,
        note: required_field(&fields, "note").to_owned(),
    })
}

fn note_fields(note: &str) -> HashMap<&str, &str> {
    note.split(';')
        .filter_map(|field| field.split_once('='))
        .collect()
}

fn required_field<'a>(fields: &'a HashMap<&str, &str>, name: &str) -> &'a str {
    fields
        .get(name)
        .copied()
        .unwrap_or_else(|| panic!("worker report omitted `{name}`: {fields:?}"))
}

fn parse_field<T>(fields: &HashMap<&str, &str>, name: &str) -> T
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    required_field(fields, name)
        .parse()
        .unwrap_or_else(|error| panic!("worker report field `{name}` is invalid: {error}"))
}

fn parse_note_field<T>(fields: &HashMap<&str, &str>, name: &str) -> T
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    required_field(fields, name)
        .parse()
        .unwrap_or_else(|error| panic!("worker note field `{name}` is invalid: {error}"))
}

fn create_batch_table() -> Statement {
    Statement::CreateNodeTable {
        name: "Batch".to_owned(),
        columns: vec![
            column("id", LogicalType::Int64, true),
            column("batch", LogicalType::Int64, false),
            column("ordinal", LogicalType::Int64, false),
            column("name", LogicalType::String, false),
        ],
    }
}

fn create_payload_table() -> Statement {
    Statement::CreateNodeTable {
        name: "Payload".to_owned(),
        columns: vec![
            column("id", LogicalType::Int64, true),
            column("byte_len", LogicalType::Int64, false),
            column("body", LogicalType::String, false),
        ],
    }
}

fn column(name: &str, ty: LogicalType, primary_key: bool) -> Column {
    Column {
        name: name.to_owned(),
        ty,
        primary_key,
    }
}

fn take_stderr(child: &mut Child) -> String {
    let mut stderr = String::new();
    if let Some(mut pipe) = child.stderr.take() {
        pipe.read_to_string(&mut stderr).unwrap();
    }
    stderr
}
