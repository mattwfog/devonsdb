use std::{
    collections::HashSet,
    ffi::OsString,
    fs,
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, ExitStatus, Stdio},
    sync::mpsc::{self, Receiver, RecvTimeoutError},
    thread,
    time::{Duration, Instant},
};

use devondb::{Database, Statement};
use devondb_types::{logical_type::LogicalType, schema::Column};
use tempfile::{TempDir, tempdir};

#[cfg(unix)]
use devondb_storage::lock::LockPaths;

#[path = "helpers/mp_worker.rs"]
mod mp_worker;

use mp_worker::DataRow;

const PAGE_SIZE: u32 = 4096;
const READ_DEADLINE: Duration = Duration::from_secs(30);
const WORKER_TEST: &str = "multiprocess_core_topology_is_transaction_prefix_safe";

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
    fn spawn(role: &'static str, path: &Path, contender_batch: Option<u32>) -> Self {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", WORKER_TEST, "--nocapture", "--test-threads=1"])
            .env(mp_worker::ROLE_ENV, role)
            .env(mp_worker::PATH_ENV, path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(batch) = contender_batch {
            command.env(mp_worker::CONTENDER_BATCH_ENV, batch.to_string());
        }
        let mut child = command.spawn().unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (sender, lines) = mpsc::channel();
        thread::Builder::new()
            .name(format!("devondb-mp-{role}-stdout"))
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
fn multiprocess_core_topology_is_transaction_prefix_safe() {
    if mp_worker::run_if_configured() {
        return;
    }
    let fixture = Fixture::new();
    let mut writer = Worker::spawn("writer", &fixture.path, None);
    let writer_ready = writer.read_phase("ready");
    assert!(writer_ready.rows.is_empty());
    let mut readers = (0..3)
        .map(|_| Worker::spawn("reader", &fixture.path, None))
        .collect::<Vec<_>>();
    let mut pids = HashSet::from([writer_ready.pid]);
    let mut last_lsns = Vec::new();
    for reader in &mut readers {
        let ready = reader.read_phase("ready");
        assert!(ready.rows.is_empty());
        assert!(pids.insert(ready.pid), "worker PIDs must be distinct");
        assert_ne!(ready.pid, std::process::id());
        last_lsns.push(ready.commit_lsn);
    }
    assert_ne!(writer_ready.pid, std::process::id());

    let first = writer.command("COMMIT 0", "committed");
    assert_eq!(first.rows, expected_batches(1));
    refresh_readers(&mut readers, &mut last_lsns, 1);
    let pinned = readers[0].command("PIN", "pinned");
    assert_eq!(pinned.rows, expected_batches(1));

    writer.command("COMMIT 1", "committed");
    writer.command("CHECKPOINT", "checkpointed");
    refresh_readers(&mut readers, &mut last_lsns, 2);
    let pin_check = readers[0].command("CHECK_PIN", "pin-checked");
    assert_eq!(pin_check.note, "stable=true");
    assert_eq!(pin_check.rows, expected_batches(1));

    writer.command("COMMIT 2", "committed");
    refresh_readers(&mut readers, &mut last_lsns, 3);
    let final_commit = writer.command("COMMIT 3", "committed");
    writer.command("CHECKPOINT", "checkpointed");
    refresh_readers(&mut readers, &mut last_lsns, 4);
    assert!(last_lsns.iter().all(|lsn| *lsn == final_commit.commit_lsn));

    for reader in readers {
        reader.exit();
    }
    writer.exit();
}

#[test]
fn multiprocess_second_writer_is_busy_then_cli_takes_over() {
    let fixture = Fixture::new();
    let mut writer = Worker::spawn("writer", &fixture.path, None);
    writer.read_phase("ready");
    writer.command("COMMIT 0", "committed");

    let mut busy = Worker::spawn("contender", &fixture.path, None);
    let busy_report = busy.read_phase("busy");
    assert!(busy_report.note.contains("kind=Busy"));
    assert!(busy_report.note.contains("writer lease"));
    busy.finish();

    writer.exit();
    let mut successor = Worker::spawn("contender", &fixture.path, Some(1));
    let committed = successor.read_phase("committed");
    assert_eq!(committed.rows, expected_batches(2));
    successor.finish();

    let mut reader = Worker::spawn("reader", &fixture.path, None);
    reader.read_phase("ready");
    let scanned = reader.command("SCAN", "scanned");
    assert_eq!(scanned.rows, expected_batches(2));
    assert_eq!(scanned.commit_lsn, committed.commit_lsn);
    reader.exit();
}

#[cfg(unix)]
#[test]
fn multiprocess_sigkill_writer_allows_recovery_and_takeover() {
    let fixture = Fixture::new();
    let mut writer = Worker::spawn("writer", &fixture.path, None);
    writer.read_phase("ready");
    writer.command("COMMIT 0", "committed");
    writer.command("CHECKPOINT", "checkpointed");
    let acked = writer.command("COMMIT 1", "committed");
    assert_eq!(acked.rows, expected_batches(2));
    writer.kill();

    let mut successor = Worker::spawn("writer", &fixture.path, None);
    let recovered = successor.read_phase("ready");
    assert_eq!(recovered.rows, expected_batches(2));
    assert_eq!(recovered.commit_lsn, acked.commit_lsn);
    let full_scan = successor.command("SCAN", "scanned");
    assert_eq!(full_scan.rows, expected_batches(2));
    let new_commit = successor.command("COMMIT 2", "committed");
    assert_eq!(new_commit.rows, expected_batches(3));
    assert!(new_commit.commit_lsn > acked.commit_lsn);
    successor.exit();
}

#[cfg(unix)]
#[test]
fn multiprocess_sigkill_preserves_stale_lock_inodes_for_writer_and_reader() {
    let fixture = Fixture::new();
    let original = lock_inodes(&fixture.path);
    let mut writer = Worker::spawn("writer", &fixture.path, None);
    writer.read_phase("ready");
    writer.command("COMMIT 0", "committed");
    writer.kill();
    assert_eq!(lock_inodes(&fixture.path), original);

    let mut successor = Worker::spawn("writer", &fixture.path, None);
    successor.read_phase("ready");
    assert_eq!(lock_inodes(&fixture.path), original);
    successor.exit();

    let mut active_writer = Worker::spawn("writer", &fixture.path, None);
    active_writer.read_phase("ready");
    let mut reader = Worker::spawn("reader", &fixture.path, None);
    reader.read_phase("ready");
    reader.command("HOLD", "holding");
    reader.kill();
    assert_eq!(lock_inodes(&fixture.path), original);
    let committed = active_writer.command("COMMIT 1", "committed");
    assert_eq!(committed.rows, expected_batches(2));
    assert_eq!(lock_inodes(&fixture.path), original);
    active_writer.exit();
}

#[cfg(unix)]
#[test]
fn multiprocess_reader_crash_changes_no_database_bytes() {
    let fixture = Fixture::new();
    let mut writer = Worker::spawn("writer", &fixture.path, None);
    writer.read_phase("ready");
    writer.command("COMMIT 0", "committed");
    let before_reader = database_bytes(&fixture.path);

    let mut reader = Worker::spawn("reader", &fixture.path, None);
    reader.read_phase("ready");
    reader.command("HOLD", "holding");
    assert_eq!(database_bytes(&fixture.path), before_reader);
    reader.kill();
    assert_eq!(database_bytes(&fixture.path), before_reader);

    let committed = writer.command("COMMIT 1", "committed");
    assert_ne!(database_bytes(&fixture.path), before_reader);
    let mut fresh = Worker::spawn("reader", &fixture.path, None);
    fresh.read_phase("ready");
    let scanned = fresh.command("SCAN", "scanned");
    assert_eq!(scanned.rows, expected_batches(2));
    assert_eq!(scanned.rows, committed.rows);
    assert_eq!(scanned.commit_lsn, committed.commit_lsn);
    fresh.exit();
    writer.exit();
}

fn refresh_readers(readers: &mut [Worker], last_lsns: &mut [u64], batch_count: u32) {
    for (reader, last_lsn) in readers.iter_mut().zip(last_lsns) {
        let report = reader.command("REFRESH", "refreshed");
        assert_transaction_prefix(&report.rows, batch_count);
        assert_eq!(report.rows, expected_batches(batch_count));
        assert!(report.commit_lsn >= *last_lsn);
        assert!(report.commit_lsn >= report.checkpoint_lsn);
        *last_lsn = report.commit_lsn;
    }
}

fn assert_transaction_prefix(rows: &[DataRow], acknowledged_batches: u32) {
    assert_eq!(rows.len() % mp_worker::BATCH_ROWS as usize, 0);
    assert!(
        (0..=acknowledged_batches).any(|count| rows == expected_batches(count)),
        "reader exposed a non-transaction-prefix row set: {rows:?}"
    );
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
        .collect::<std::collections::HashMap<_, _>>();
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

fn required_field<'a>(fields: &'a std::collections::HashMap<&str, &str>, name: &str) -> &'a str {
    fields
        .get(name)
        .copied()
        .unwrap_or_else(|| panic!("worker report omitted `{name}`: {fields:?}"))
}

fn parse_field<T>(fields: &std::collections::HashMap<&str, &str>, name: &str) -> T
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    required_field(fields, name)
        .parse()
        .unwrap_or_else(|error| panic!("worker report field `{name}` is invalid: {error}"))
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

fn database_bytes(path: &Path) -> (Vec<u8>, Vec<u8>) {
    (fs::read(path).unwrap(), fs::read(wal_path(path)).unwrap())
}

fn wal_path(path: &Path) -> PathBuf {
    let mut suffixed = OsString::from(path.as_os_str());
    suffixed.push("-wal");
    suffixed.into()
}

#[cfg(unix)]
fn lock_inodes(path: &Path) -> (u64, u64) {
    use std::os::unix::fs::MetadataExt;

    let paths = LockPaths::for_main(path).unwrap();
    (
        fs::metadata(paths.writer).unwrap().ino(),
        fs::metadata(paths.publish).unwrap().ino(),
    )
}
