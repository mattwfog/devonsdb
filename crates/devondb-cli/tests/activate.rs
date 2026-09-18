use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

const MULTIPROCESS_COORDINATION_FLAG: u64 = 1 << 8;
const FEATURE_FLAGS_OFFSET: usize = 16;
const PAGE_SIZE_OFFSET: usize = 24;

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new(label: &str) -> Self {
        let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "devondb-cli-activate-{label}-{}-{nanos}-{sequence}",
            std::process::id()
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn database(&self) -> PathBuf {
        self.0.join("test.devondb")
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn activate_sets_both_slots_and_reports_idempotence() {
    let directory = TestDirectory::new("durable");
    let path = directory.database();
    seed(&path, ".exit\n");

    let first = run(&["activate".as_ref(), path.as_os_str()], "");
    assert_success(&first);
    assert_eq!(
        first.stdout,
        format!("activated {}\n", path.display()).as_bytes()
    );
    assert!(first.stderr.is_empty());
    assert!(
        slot_feature_flags(&path)
            .iter()
            .all(|flags| flags & MULTIPROCESS_COORDINATION_FLAG != 0)
    );

    let main_before = fs::read(&path).unwrap();
    let wal_before = fs::read(wal_path(&path)).unwrap();
    let second = run(&["activate".as_ref(), path.as_os_str()], "");
    assert_success(&second);
    assert_eq!(
        second.stdout,
        format!("already active {}\n", path.display()).as_bytes()
    );
    assert!(second.stderr.is_empty());
    assert_eq!(fs::read(&path).unwrap(), main_before);
    assert_eq!(fs::read(wal_path(&path)).unwrap(), wal_before);
}

#[test]
fn activate_reports_busy_after_a_real_repl_is_ready() {
    let directory = TestDirectory::new("busy");
    let path = directory.database();
    seed(
        &path,
        "create node table Person (id Int64 primary key)\n.exit\n",
    );
    assert_success(&run(&["activate".as_ref(), path.as_os_str()], ""));

    let mut holder = spawn_repl(&path);
    let mut holder_stdin = holder.stdin.take().unwrap();
    let mut holder_stdout = BufReader::new(holder.stdout.take().unwrap());
    writeln!(
        holder_stdin,
        "nodes(Person) as person | aggregate count(person.id) as total"
    )
    .unwrap();
    holder_stdin.flush().unwrap();
    let mut readiness = String::new();
    for _ in 0..4 {
        let mut line = String::new();
        assert_ne!(holder_stdout.read_line(&mut line).unwrap(), 0);
        readiness.push_str(&line);
    }
    assert!(readiness.ends_with("0\n(1 rows)\n"), "{readiness}");

    let activation = run(&["activate".as_ref(), path.as_os_str()], "");
    assert_eq!(activation.status.code(), Some(1), "{activation:?}");
    assert!(activation.stdout.is_empty());
    let stderr = String::from_utf8(activation.stderr).unwrap();
    assert!(stderr.starts_with("error: busy: "), "{stderr}");

    writeln!(holder_stdin, ".exit").unwrap();
    drop(holder_stdin);
    drop(holder_stdout);
    let status = holder.wait().unwrap();
    assert!(status.success(), "holder exited with {status}");
    let mut holder_stderr = String::new();
    holder
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut holder_stderr)
        .unwrap();
    assert!(holder_stderr.is_empty(), "{holder_stderr}");
}

#[test]
fn activate_refuses_a_pack_with_the_engine_error() {
    let directory = TestDirectory::new("pack");
    let path = directory.database();
    let pack = directory.0.join("test.pack");
    seed(&path, ".exit\n");
    let packed = run(&["pack".as_ref(), path.as_os_str(), pack.as_os_str()], "");
    assert_success(&packed);

    let activation = run(&["activate".as_ref(), pack.as_os_str()], "");
    assert_eq!(activation.status.code(), Some(1), "{activation:?}");
    assert!(activation.stdout.is_empty());
    let stderr = String::from_utf8(activation.stderr).unwrap();
    assert!(stderr.starts_with("error: read-only: "), "{stderr}");
    assert!(stderr.contains("DEVONPACK"), "{stderr}");
}

fn seed(path: &Path, input: &str) {
    let output = run(&[path.as_os_str()], input);
    assert_success(&output);
    assert!(output.stderr.is_empty(), "{output:?}");
}

fn spawn_repl(path: &Path) -> Child {
    Command::new(env!("CARGO_BIN_EXE_devondb"))
        .arg(path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}

fn run(arguments: &[&std::ffi::OsStr], input: &str) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_devondb"))
        .args(arguments)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "child failed with stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn slot_feature_flags(path: &Path) -> [u64; 2] {
    let bytes = fs::read(path).unwrap();
    let page_size = u32::from_le_bytes(
        bytes[PAGE_SIZE_OFFSET..PAGE_SIZE_OFFSET + size_of::<u32>()]
            .try_into()
            .unwrap(),
    ) as usize;
    std::array::from_fn(|slot| {
        let offset = slot * page_size + FEATURE_FLAGS_OFFSET;
        u64::from_le_bytes(bytes[offset..offset + size_of::<u64>()].try_into().unwrap())
    })
}

fn wal_path(path: &Path) -> PathBuf {
    PathBuf::from(format!("{}-wal", path.display()))
}
