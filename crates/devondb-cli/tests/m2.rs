use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, ChildStdout, Command, Output, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

const CREATE_PERSON: &str =
    "create node table Person (id Int64 primary key, name String, age Int64)";
const CREATE_KNOWS: &str = "create rel table Knows from Person to Person";
const INSERT_PEOPLE: &str = "insert into Person values (1, \"Ada\", 36), (2, \"Grace\", 50), (3, \"Linus\", 30), (4, \"Barbara\", 45), (5, \"Alan\", 41), (6, \"Edsger\", 28)";
const INSERT_KNOWS: &str = "insert rel into Knows values (1 -> 2), (1 -> 3), (2 -> 4), (2 -> 5), (3 -> 4), (4 -> 1), (4 -> 5), (5 -> 2)";
const INSERT_RECOVERY_EDGES: &str = "insert rel into Knows values (1 -> 5), (3 -> 1), (5 -> 4)";
const M2_EXPAND: &str =
    "nodes(Person) as p | expand Knows out as f | filter f.age > 30 | project p.name, f.name";
const ALL_EDGES: &str = "nodes(Person) as p | expand Knows out as f | project p.name, f.name";
const FRIEND_COUNTS: &str = "nodes(Person) as p | expand Knows out as f | aggregate count(f.id) as friend_count by p.name | sort p.name desc";

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
            "devondb-cli-m2-test-{}-{timestamp}-{sequence}",
            std::process::id()
        ));
        fs::create_dir(&path).unwrap();
        Self { path }
    }

    fn database(&self) -> PathBuf {
        self.path.join("social.devondb")
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

#[test]
fn checkpointed_social_graph_runs_the_exact_m2_pipeline_after_restart() {
    let directory = TestDirectory::new();
    let path = directory.database();
    seed_social_graph(&path);

    let output = run_session(&path, &input(&[M2_EXPAND, ".exit"]));

    assert_success(&output);
    assert_eq!(
        output.stdout,
        b"p.name | f.name\n---------------\n\"Ada\" | \"Grace\"\n\"Grace\" | \"Barbara\"\n\"Grace\" | \"Alan\"\n\"Linus\" | \"Barbara\"\n\"Barbara\" | \"Ada\"\n\"Barbara\" | \"Alan\"\n\"Alan\" | \"Grace\"\n(7 rows)\n"
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn sigkill_recovers_acknowledged_relationship_inserts_from_the_wal() {
    let directory = TestDirectory::new();
    let path = directory.database();
    seed_social_graph(&path);
    let mut child = spawn_database(&path);
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    send_and_expect_ok(&mut stdin, &mut stdout, INSERT_RECOVERY_EDGES);

    child.kill().unwrap();
    drop(stdin);
    drop(stdout);
    let status = child.wait().unwrap();
    assert!(!status.success());
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    assert!(stderr.is_empty(), "unexpected child stderr: {stderr}");

    let recovered = run_session(&path, &input(&[ALL_EDGES, ".exit"]));

    assert_success(&recovered);
    assert_eq!(
        recovered.stdout,
        b"p.name | f.name\n---------------\n\"Ada\" | \"Grace\"\n\"Ada\" | \"Linus\"\n\"Ada\" | \"Alan\"\n\"Grace\" | \"Barbara\"\n\"Grace\" | \"Alan\"\n\"Linus\" | \"Barbara\"\n\"Linus\" | \"Ada\"\n\"Barbara\" | \"Ada\"\n\"Barbara\" | \"Alan\"\n\"Alan\" | \"Grace\"\n\"Alan\" | \"Barbara\"\n(11 rows)\n"
    );
    assert!(recovered.stderr.is_empty());
}

#[test]
fn sort_and_aggregate_return_exact_friend_counts() {
    let directory = TestDirectory::new();
    let path = directory.database();
    seed_social_graph(&path);

    let output = run_session(&path, &input(&[FRIEND_COUNTS, ".exit"]));

    assert_success(&output);
    assert_eq!(
        output.stdout,
        b"p.name | friend_count\n---------------------\n\"Linus\" | 1\n\"Grace\" | 2\n\"Barbara\" | 2\n\"Alan\" | 1\n\"Ada\" | 2\n(5 rows)\n"
    );
    assert!(output.stderr.is_empty());
}

fn seed_social_graph(path: &Path) {
    let output = run_session(
        path,
        &input(&[
            CREATE_PERSON,
            CREATE_KNOWS,
            INSERT_PEOPLE,
            INSERT_KNOWS,
            ".checkpoint",
            ".exit",
        ]),
    );
    assert_success(&output);
    assert_eq!(output.stdout, b"ok\nok\nok\nok\nok\n");
    assert!(output.stderr.is_empty());
}

fn input(lines: &[&str]) -> String {
    format!("{}\n", lines.join("\n"))
}

fn run_session(path: &Path, input: &str) -> Output {
    let mut child = spawn_database(path);
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

fn spawn_database(path: &Path) -> Child {
    Command::new(env!("CARGO_BIN_EXE_devondb"))
        .arg(path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}

fn send_and_expect_ok(
    stdin: &mut ChildStdin,
    stdout: &mut BufReader<ChildStdout>,
    statement: &str,
) {
    writeln!(stdin, "{statement}").unwrap();
    stdin.flush().unwrap();
    let mut response = String::new();
    let bytes = stdout.read_line(&mut response).unwrap();
    assert_ne!(bytes, 0, "child exited before acknowledging `{statement}`");
    assert_eq!(response, "ok\n");
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "child failed with stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
