use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, ChildStdout, Command, Output, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

const CREATE_PERSON: &str = r#"{"v":0,"stmt":{"stmt":"CreateNodeTable","name":"Person","columns":[{"name":"id","ty":"Int64","primary_key":true},{"name":"name","ty":"String","primary_key":false},{"name":"age","ty":"Int64","primary_key":false}]}}"#;
const INSERT_FOUR_PEOPLE: &str = r#"{"v":0,"stmt":{"stmt":"InsertNode","table":"Person","rows":[[{"Int64":1},{"String":"Ada"},{"Int64":36}],[{"Int64":2},{"String":"Grace"},{"Int64":50}],[{"Int64":3},{"String":"Linus"},{"Int64":30}],[{"Int64":4},{"String":"Barbara"},{"Int64":45}]]}}"#;
const INSERT_TWO_PEOPLE: &str = r#"{"v":0,"stmt":{"stmt":"InsertNode","table":"Person","rows":[[{"Int64":1},{"String":"Ada"},{"Int64":36}],[{"Int64":2},{"String":"Grace"},{"Int64":50}]]}}"#;
const INSERT_ADA: &str = r#"{"v":0,"stmt":{"stmt":"InsertNode","table":"Person","rows":[[{"Int64":1},{"String":"Ada"},{"Int64":36}]]}}"#;
const INSERT_GRACE: &str = r#"{"v":0,"stmt":{"stmt":"InsertNode","table":"Person","rows":[[{"Int64":2},{"String":"Grace"},{"Int64":50}]]}}"#;
const INSERT_LINUS: &str = r#"{"v":0,"stmt":{"stmt":"InsertNode","table":"Person","rows":[[{"Int64":3},{"String":"Linus"},{"Int64":30}]]}}"#;
const FILTER_PROJECT_PLAN: &str = r#"{"v":0,"plan":{"op":"Project","exprs":[{"expr":{"col":"p.name"},"as":"name"}],"input":{"op":"Filter","predicate":{"gt":[{"col":"p.age"},{"lit":30}]},"input":{"op":"ScanNodes","table":"Person","binding":"p"}}}}"#;
const SCAN_PLAN: &str = r#"{"v":0,"plan":{"op":"ScanNodes","table":"Person","binding":"p"}}"#;
const TEXT_CREATE_PERSON: &str =
    "create node table Person (id Int64 primary key, name String, age Int64)";
const TEXT_INSERT_FOUR_PEOPLE: &str = "insert into Person values (1, \"Ada\", 36), (2, \"Grace\", 50), (3, \"Linus\", 30), (4, \"Barbara\", 45)";
const TEXT_INSERT_TWO_PEOPLE: &str =
    "insert into Person values (1, \"Ada\", 36), (2, \"Grace\", 50)";
const TEXT_FILTER_PROJECT: &str = "nodes(Person) as p | filter p.age > 30 | project p.name as name";

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
            "devondb-cli-repl-test-{}-{timestamp}-{sequence}",
            std::process::id()
        ));
        fs::create_dir(&path).unwrap();
        Self { path }
    }

    fn database(&self) -> PathBuf {
        self.path.join("test.devondb")
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

#[test]
fn version_flag_prints_the_release_identity_without_opening_a_database() {
    let output = Command::new(env!("CARGO_BIN_EXE_devondb"))
        .arg("--version")
        .output()
        .unwrap();

    assert_success(&output);
    assert_eq!(
        output.stdout,
        format!("devondb {}\n", env!("CARGO_PKG_VERSION")).as_bytes()
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn full_m1_loop_formats_filtered_projection_exactly() {
    let directory = TestDirectory::new();
    let input = input(&[
        CREATE_PERSON,
        INSERT_FOUR_PEOPLE,
        FILTER_PROJECT_PLAN,
        ".exit",
    ]);

    let output = run_session(&directory.database(), &input);

    assert_success(&output);
    assert_eq!(
        output.stdout,
        b"ok\nok\nname\n----\n\"Ada\"\n\"Grace\"\n\"Barbara\"\n(3 rows)\n"
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn text_statements_and_query_format_filtered_projection_exactly() {
    let directory = TestDirectory::new();
    let input = input(&[
        TEXT_CREATE_PERSON,
        TEXT_INSERT_FOUR_PEOPLE,
        TEXT_FILTER_PROJECT,
        ".exit",
    ]);

    let output = run_session(&directory.database(), &input);

    assert_success(&output);
    assert_eq!(
        output.stdout,
        b"ok\nok\nname\n----\n\"Ada\"\n\"Grace\"\n\"Barbara\"\n(3 rows)\n"
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn sugared_text_project_uses_the_expression_as_its_alias() {
    let directory = TestDirectory::new();
    let output = run_session(
        &directory.database(),
        &input(&[
            TEXT_CREATE_PERSON,
            TEXT_INSERT_TWO_PEOPLE,
            "nodes(Person) as p | project p.name",
            ".exit",
        ]),
    );

    assert_success(&output);
    assert_eq!(
        output.stdout,
        b"ok\nok\np.name\n------\n\"Ada\"\n\"Grace\"\n(2 rows)\n"
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn explain_query_prints_canonical_defaults_without_execution() {
    let directory = TestDirectory::new();
    let output = run_session(
        &directory.database(),
        &input(&[
            "explain nodes(Person) as p | sort p.age asc",
            TEXT_CREATE_PERSON,
            ".exit",
        ]),
    );

    assert_success(&output);
    assert_eq!(output.stdout, b"nodes(Person) as p | sort p.age\nok\n");
    assert!(output.stderr.is_empty());
}

#[test]
fn explain_statement_round_trips_canonically_without_execution() {
    let directory = TestDirectory::new();
    let statement = "create node table Person (id Int64 primary key, name String)";
    let output = run_session(
        &directory.database(),
        &input(&[
            "explain create node table Person (id Int64 primary key,name String)",
            statement,
            ".exit",
        ]),
    );

    assert_success(&output);
    assert_eq!(
        output.stdout,
        b"create node table Person (id Int64 primary key, name String)\nok\n"
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn text_parse_error_reports_position_and_the_repl_continues() {
    let directory = TestDirectory::new();
    let output = run_session(
        &directory.database(),
        &input(&[
            "nodes(Person) as p | Filter p.age > 30",
            TEXT_CREATE_PERSON,
            ".exit",
        ]),
    );

    assert_success(&output);
    assert_eq!(output.stdout, b"ok\n");
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        "error: invalid argument: DevonPlan text parse error at position 22: keyword `Filter` must be lowercase; did you mean `filter`?; offending token \"Filter\"\n"
    );
}

#[test]
fn checkpointed_rows_survive_restart() {
    let directory = TestDirectory::new();
    let path = directory.database();
    let first = run_session(
        &path,
        &input(&[CREATE_PERSON, INSERT_TWO_PEOPLE, ".checkpoint", ".exit"]),
    );
    assert_success(&first);
    assert_eq!(first.stdout, b"ok\nok\nok\n");
    assert!(first.stderr.is_empty());

    let second = run_session(&path, &input(&[SCAN_PLAN, ".exit"]));

    assert_success(&second);
    assert_eq!(
        second.stdout,
        b"p.id | p.name | p.age\n---------------------\n1 | \"Ada\" | 36\n2 | \"Grace\" | 50\n(2 rows)\n"
    );
    assert!(second.stderr.is_empty());
}

#[test]
fn sigkill_after_acknowledged_inserts_recovers_all_rows_from_wal() {
    let directory = TestDirectory::new();
    let path = directory.database();
    let mut child = spawn_database(&path);
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());

    for statement in [CREATE_PERSON, INSERT_ADA, INSERT_GRACE, INSERT_LINUS] {
        send_and_expect_ok(&mut stdin, &mut stdout, statement);
    }

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

    let recovered = run_session(&path, &input(&[SCAN_PLAN, ".exit"]));

    assert_success(&recovered);
    assert_eq!(
        recovered.stdout,
        b"p.id | p.name | p.age\n---------------------\n1 | \"Ada\" | 36\n2 | \"Grace\" | 50\n3 | \"Linus\" | 30\n(3 rows)\n"
    );
    assert!(recovered.stderr.is_empty());
}

#[test]
fn bad_json_reports_an_error_and_the_repl_continues() {
    let directory = TestDirectory::new();
    let output = run_session(
        &directory.database(),
        &input(&[CREATE_PERSON, "{not-json}", SCAN_PLAN, ".exit"]),
    );

    assert_success(&output);
    assert_eq!(
        output.stdout,
        b"ok\np.id | p.name | p.age\n---------------------\n(0 rows)\n"
    );
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("error:"), "unexpected stderr: {stderr}");
}

#[test]
fn unknown_command_reports_an_error_and_the_repl_continues() {
    let directory = TestDirectory::new();
    let output = run_session(
        &directory.database(),
        &input(&[".unknown", CREATE_PERSON, SCAN_PLAN, ".exit"]),
    );

    assert_success(&output);
    assert_eq!(
        output.stdout,
        b"ok\np.id | p.name | p.age\n---------------------\n(0 rows)\n"
    );
    assert_eq!(output.stderr, b"error: unknown command `.unknown`\n");
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
