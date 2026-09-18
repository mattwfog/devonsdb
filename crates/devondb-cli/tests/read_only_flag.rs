use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new() -> Self {
        let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "devondb-cli-read-only-{}-{nanos}-{sequence}",
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
fn read_only_shell_queries_but_refuses_mutations_without_changing_bytes() {
    let directory = TestDirectory::new();
    let path = directory.database();
    seed_and_activate(&path);
    let main_before = fs::read(&path).unwrap();
    let wal_before = fs::read(wal_path(&path)).unwrap();

    let input = "nodes(Person) as person | project person.name\n\
insert into Person values (3, \"Linus\")\n\
create node table Other (id Int64 primary key)\n\
.checkpoint\n\
.exit\n";
    let output = run(&[path.as_os_str(), "--read-only".as_ref()], Some(input));

    assert_success(&output);
    assert_eq!(
        output.stdout,
        b"person.name\n-----------\n\"ada\"\n\"Grace\"\n(2 rows)\n"
    );
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert_eq!(stderr.lines().count(), 3, "{stderr}");
    assert!(
        stderr
            .lines()
            .all(|line| line.starts_with("error: read-only: ")),
        "{stderr}"
    );
    assert_eq!(fs::read(&path).unwrap(), main_before);
    assert_eq!(fs::read(wal_path(&path)).unwrap(), wal_before);
}

#[test]
fn read_only_ask_queries_without_changing_database_or_wal() {
    let directory = TestDirectory::new();
    let path = directory.database();
    seed_and_activate(&path);
    let main_before = fs::read(&path).unwrap();
    let wal_before = fs::read(wal_path(&path)).unwrap();

    let output = run(
        &[
            "ask".as_ref(),
            path.as_os_str(),
            "who does ada know".as_ref(),
            "--yes".as_ref(),
            "--read-only".as_ref(),
        ],
        None,
    );

    assert_success(&output);
    assert!(output.stderr.is_empty(), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("other.name"), "{stdout}");
    assert!(stdout.contains("\"Grace\""), "{stdout}");
    assert!(stdout.ends_with("(1 rows)\n"), "{stdout}");
    assert_eq!(fs::read(&path).unwrap(), main_before);
    assert_eq!(fs::read(wal_path(&path)).unwrap(), wal_before);
}

#[test]
fn read_only_ask_reports_the_engine_error_for_a_mutation() {
    let directory = TestDirectory::new();
    let path = directory.database();
    seed_and_activate(&path);
    let main_before = fs::read(&path).unwrap();
    let wal_before = fs::read(wal_path(&path)).unwrap();

    let output = run(
        &[
            "ask".as_ref(),
            path.as_os_str(),
            "set person 1 name to Linus".as_ref(),
            "--yes".as_ref(),
            "--read-only".as_ref(),
        ],
        None,
    );

    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stdout).starts_with("update Person set name = \"Linus\""),
        "{output:?}"
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).starts_with("error: read-only: "),
        "{output:?}"
    );
    assert_eq!(fs::read(&path).unwrap(), main_before);
    assert_eq!(fs::read(wal_path(&path)).unwrap(), wal_before);
}

fn seed_and_activate(path: &Path) {
    let input = "create node table Person (id Int64 primary key, name String)\n\
create rel table Knows from Person to Person\n\
insert into Person values (1, \"ada\"), (2, \"Grace\")\n\
insert rel into Knows values (1 -> 2)\n\
.checkpoint\n\
.exit\n";
    let seeded = run(&[path.as_os_str()], Some(input));
    assert_success(&seeded);
    assert!(seeded.stderr.is_empty(), "{seeded:?}");
    let activated = run(&["activate".as_ref(), path.as_os_str()], None);
    assert_success(&activated);
    assert!(activated.stderr.is_empty(), "{activated:?}");
}

fn run(arguments: &[&std::ffi::OsStr], input: Option<&str>) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_devondb"));
    command
        .args(arguments)
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().unwrap();
    if let Some(input) = input {
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
    }
    child.wait_with_output().unwrap()
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "child failed with stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn wal_path(path: &Path) -> PathBuf {
    PathBuf::from(format!("{}-wal", path.display()))
}
