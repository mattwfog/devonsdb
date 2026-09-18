//! Pin-store coverage from text statement through durable catalog publication,
//! reopen, exact re-execution, introspection, removal, and SIGKILL recovery.

use std::{
    env, fs,
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use devondb::{
    Database, DevonError, Plan, Statement,
    text::parser::{Parsed, parse},
};
use devondb_storage::{catalog::Catalog, pager::Pager};
use devondb_types::value::Value;

const PAGE_SIZE: u32 = 4096;
const CHILD_ENV: &str = "DEVONDB_PIN_KILL_CHILD";
const CHILD_PATH_ENV: &str = "DEVONDB_PIN_KILL_PATH";
const PIN_NAME: &str = "ada friends";
const QUERY: &str = concat!(
    "nodes(Person) as person | filter person.name = \"Ada\" | ",
    "expand Knows out as friend | sort friend.name | project friend.name as name"
);
static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct TestDirectory {
    path: PathBuf,
}

impl TestDirectory {
    fn new(label: &str) -> Self {
        let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        let path = env::temp_dir().join(format!(
            "devondb-pins-{label}-{}-{timestamp}-{sequence}",
            std::process::id()
        ));
        fs::create_dir(&path).expect("create pin test directory");
        Self { path }
    }

    fn database(&self) -> PathBuf {
        self.path.join("db.devondb")
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn statement(input: &str) -> Statement {
    let Parsed::Statement(envelope) = parse(input).expect("statement parses") else {
        panic!("expected statement: {input}");
    };
    envelope.stmt
}

fn query_plan() -> Plan {
    let Parsed::Query(plan) = parse(QUERY).expect("query parses") else {
        panic!("expected query");
    };
    plan
}

fn execute_text(database: &mut Database, input: &str) {
    database
        .execute(&statement(input))
        .unwrap_or_else(|error| panic!("execute {input:?}: {error}"));
}

fn create_seeded_graph(path: &Path) -> Database {
    let mut database = Database::create(path, PAGE_SIZE).expect("create database");
    execute_text(
        &mut database,
        "create node table Person (id Int64 primary key, name String)",
    );
    execute_text(
        &mut database,
        "create rel table Knows from Person to Person",
    );
    execute_text(
        &mut database,
        "insert into Person values (1, \"Ada\"), (2, \"Linus\"), (3, \"Grace\")",
    );
    execute_text(
        &mut database,
        "insert rel into Knows values (1 -> 2), (1 -> 3)",
    );
    database
}

fn expected_rows() -> Vec<Vec<Value>> {
    vec![
        vec![Value::String("Grace".to_owned())],
        vec![Value::String("Linus".to_owned())],
    ]
}

fn load_catalog(path: &Path) -> Catalog {
    let pager = Pager::open(path).expect("open pager");
    Catalog::load(&pager).expect("load catalog")
}

fn stored_plan_json(path: &Path, name: &str) -> Vec<u8> {
    let catalog = load_catalog(path);
    let pin = catalog
        .pins()
        .iter()
        .find(|pin| pin.name == name)
        .expect("stored pin");
    serde_json::to_vec(&pin.plan).expect("stored plan serializes")
}

#[test]
fn pin_text_reopens_runs_identically_and_unpins() {
    let directory = TestDirectory::new("e2e");
    let path = directory.database();
    let mut database = create_seeded_graph(&path);
    let pin_text = format!("pin \"{PIN_NAME}\" as {QUERY}");
    let pin_statement = statement(&pin_text);
    let Statement::PinPlan { plan, text, .. } = &pin_statement else {
        panic!("expected parsed pin statement");
    };
    // `text` is the original statement text (provenance), not the canonical
    // query print.
    assert_eq!(text, &pin_text);
    let pinned_value: serde_json::Value =
        serde_json::from_str(&plan.to_json().expect("canonical plan JSON"))
            .expect("canonical plan value");
    let pinned_bytes = serde_json::to_vec(&pinned_value).expect("canonical value bytes");

    database.execute(&pin_statement).expect("commit text pin");
    let before = database.run_pin(PIN_NAME).expect("run committed pin");
    assert_eq!(before.columns, vec!["name"]);
    assert_eq!(before.rows, expected_rows());
    let summary = database.schema_summary();
    assert_eq!(summary.pins.len(), 1);
    assert_eq!(summary.pins[0].name, PIN_NAME);
    assert_eq!(summary.pins[0].text, pin_text);
    assert_eq!(summary.pins[0].canonical, QUERY);
    drop(database);

    let first_stored = stored_plan_json(&path, PIN_NAME);
    assert_eq!(first_stored, pinned_bytes);
    let mut reopened = Database::open(&path).expect("reopen pinned database");
    let after = reopened.run_pin(PIN_NAME).expect("run reopened pin");
    assert_eq!(after, before);
    let reopened_stored = stored_plan_json(&path, PIN_NAME);
    assert_eq!(reopened_stored, first_stored);

    let suggestion = reopened
        .unpin("ada frends")
        .expect_err("unknown pin must suggest");
    assert_eq!(
        suggestion.to_string(),
        "not found: pin `ada frends` (did you mean `ada friends`?)"
    );
    reopened.unpin(PIN_NAME).expect("remove pin");
    assert!(reopened.schema_summary().pins.is_empty());
    assert!(matches!(
        reopened.run_pin(PIN_NAME),
        Err(DevonError::NotFound { .. })
    ));
    drop(reopened);
    assert!(load_catalog(&path).pins().is_empty());

    println!(
        "M5 pin E2E: rows={:?}; reopened_rows={:?}; stored_plan={}; unpinned=true",
        before.rows,
        after.rows,
        String::from_utf8(first_stored).expect("plan JSON is UTF-8")
    );
}

#[test]
fn database_that_never_pinned_has_no_catalog_or_summary_key() {
    let directory = TestDirectory::new("absent");
    let path = directory.database();
    let mut database = create_seeded_graph(&path);
    database.checkpoint().expect("publish seed catalog");
    let summary = serde_json::to_value(database.schema_summary()).expect("summary JSON");
    assert_eq!(summary.get("pins"), None);
    drop(database);

    let catalog = serde_json::to_value(load_catalog(&path)).expect("catalog JSON");
    assert_eq!(catalog.get("pins"), None);
}

#[test]
fn pin_validates_the_full_plan_before_publication() {
    let directory = TestDirectory::new("validation");
    let path = directory.database();
    let mut database = create_seeded_graph(&path);
    let mut future = query_plan();
    future.v += 1;
    let version = database
        .pin("future", "future plan", &future)
        .expect_err("future plan version must fail");
    assert!(version.to_string().contains("version 1"));

    let unknown = match parse("nodes(Ghost) as ghost").expect("unknown-table query parses") {
        Parsed::Query(plan) => plan,
        Parsed::Statement(_) => panic!("expected query"),
    };
    let table = database
        .pin("ghosts", "show ghosts", &unknown)
        .expect_err("unknown table must fail at pin time");
    assert!(matches!(table, DevonError::NotFound { .. }));
    assert!(database.schema_summary().pins.is_empty());
}

#[test]
fn kill_after_ack_recovers_the_published_pin() {
    let directory = TestDirectory::new("kill");
    let path = directory.database();
    drop(create_seeded_graph(&path));

    let mut child = spawn_pin_child(&path);
    let mut stdout = BufReader::new(child.stdout.take().expect("child stdout"));
    loop {
        let mut line = String::new();
        assert_ne!(
            stdout.read_line(&mut line).expect("read child output"),
            0,
            "pin child exited before acknowledgement"
        );
        if line.contains("PIN_ACK") {
            break;
        }
    }
    child.kill().expect("SIGKILL pin child");
    let status = child.wait().expect("wait for killed child");
    assert!(!status.success(), "pin child unexpectedly survived");
    assert_child_stderr_empty(&mut child);

    let mut recovered = Database::open(&path).expect("recover killed pin database");
    let result = recovered.run_pin(PIN_NAME).expect("run kill-recovered pin");
    assert_eq!(result.rows, expected_rows());
    assert_eq!(load_catalog(&path).pins().len(), 1);
    println!(
        "M5 kill-9 recovery: pin={PIN_NAME:?}; rows={:?}",
        result.rows
    );
}

#[test]
fn pin_commit_child_process() {
    if env::var_os(CHILD_ENV).is_none() {
        return;
    }
    let path = PathBuf::from(env::var_os(CHILD_PATH_ENV).expect("child database path"));
    let mut database = Database::open(path).expect("child opens database");
    database
        .pin(PIN_NAME, "who does Ada know", &query_plan())
        .expect("child commits pin");
    let stdout = std::io::stdout();
    let mut stdout = stdout.lock();
    writeln!(stdout, "PIN_ACK").expect("write pin acknowledgement");
    stdout.flush().expect("flush pin acknowledgement");
    loop {
        std::thread::park();
    }
}

fn spawn_pin_child(path: &Path) -> Child {
    Command::new(env::current_exe().expect("current test executable"))
        .args([
            "--exact",
            "pin_commit_child_process",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD_ENV, "1")
        .env(CHILD_PATH_ENV, path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn pin child")
}

fn assert_child_stderr_empty(child: &mut Child) {
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .expect("child stderr")
        .read_to_string(&mut stderr)
        .expect("read child stderr");
    assert!(stderr.is_empty(), "pin child stderr: {stderr}");
}
