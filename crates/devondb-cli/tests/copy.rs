use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct TestDirectory {
    path: PathBuf,
}

impl TestDirectory {
    fn new() -> Self {
        let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "devondb-cli-copy-test-{}-{timestamp}-{sequence}",
            std::process::id()
        ));
        fs::create_dir(&path).unwrap();
        Self { path }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

#[test]
fn real_shell_executes_copy_and_queries_loaded_rows() {
    let directory = TestDirectory::new();
    let csv = directory.path("people.csv");
    fs::write(&csv, "id,name\n1,Ada\n2,Grace\n").unwrap();
    let copy = format!(
        "copy Person from \"{}\"",
        csv.to_string_lossy()
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
    );
    let input = format!(
        "create node table Person (id Int64 primary key, name String)\n{copy}\n\
         nodes(Person) as p\n.exit\n"
    );

    let output = run_session(&directory.path("copy.devondb"), &input);

    assert_success(&output);
    assert_eq!(
        output.stdout,
        b"ok\nok\np.id | p.name\n-------------\n1 | \"Ada\"\n2 | \"Grace\"\n(2 rows)\n"
    );
    assert!(output.stderr.is_empty());
}

fn run_session(path: &Path, input: &str) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_devondb"))
        .arg(path)
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
