use std::{
    env, fs,
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use devondb::{
    Database, Plan, Statement,
    text::parser::{Parsed, parse},
};
use devondb_storage::node_group::NODE_GROUP_CAPACITY;
use devondb_types::value::Value;

const PAGE_SIZE: u32 = 4096;
const KILL_CHILD_ENV: &str = "DEVONDB_REL_COPY_KILL_CHILD";
const KILL_PATH_ENV: &str = "DEVONDB_REL_COPY_KILL_PATH";
const KILL_CSV_ENV: &str = "DEVONDB_REL_COPY_KILL_CSV";
const KILL_NODE_COUNT: usize = 3 * NODE_GROUP_CAPACITY;
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
            "devondb-copy-rel-{label}-{}-{timestamp}-{sequence}",
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

fn statement(input: &str) -> Statement {
    let Parsed::Statement(envelope) = parse(input).unwrap() else {
        panic!("expected statement: {input}");
    };
    envelope.stmt
}

fn plan(input: &str) -> Plan {
    let Parsed::Query(plan) = parse(input).unwrap() else {
        panic!("expected query: {input}");
    };
    plan
}

fn execute_text(database: &mut Database, input: &str) {
    database
        .execute(&statement(input))
        .unwrap_or_else(|error| panic!("execute {input:?}: {error}"));
}

fn copy_text(table: &str, path: &Path) -> String {
    let path = path
        .to_string_lossy()
        .replace('\\', "\\\\")
        .replace('"', "\\\"");
    format!("copy {table} from \"{path}\"")
}

fn create_graph(database: &mut Database) {
    execute_text(
        database,
        "create node table Person (id Int64 primary key, name String)",
    );
    execute_text(
        database,
        "create rel table Knows from Person to Person (since Int64)",
    );
}

fn friend_query(direction: &str) -> Plan {
    plan(&format!(
        "nodes(Person) as p | filter p.name = \"Ada\" | expand Knows {direction} as friend | sort friend.name | project friend.name as name"
    ))
}

#[test]
fn text_copy_builds_csr_and_expand_returns_exact_rows() {
    let directory = TestDirectory::new("e2e");
    let nodes = directory.path("people.csv");
    let rels = directory.path("knows.csv");
    fs::write(&nodes, "name,id\nAda,1\nLinus,2\nGrace,3\n").unwrap();
    fs::write(&rels, "since,to,from\n1843,2,1\n1952,1,3\n1991,3,1\n").unwrap();
    let mut database = Database::create(directory.path("graph.devondb"), PAGE_SIZE).unwrap();
    create_graph(&mut database);

    execute_text(&mut database, &copy_text("Person", &nodes));
    execute_text(&mut database, &copy_text("Knows", &rels));

    assert_eq!(
        database.run(&friend_query("out")).unwrap().rows,
        vec![
            vec![Value::String("Grace".to_owned())],
            vec![Value::String("Linus".to_owned())],
        ]
    );
    assert_eq!(
        database.run(&friend_query("in")).unwrap().rows,
        vec![vec![Value::String("Grace".to_owned())]]
    );
}

#[test]
fn copy_and_insert_are_query_equivalent_and_survive_reopen() {
    let directory = TestDirectory::new("equivalence");
    let copied_path = directory.path("copied.devondb");
    let inserted_path = directory.path("inserted.devondb");
    let rels = directory.path("edges.csv");
    fs::write(&rels, "from,to,since\n1,2,1843\n1,3,1991\n3,2,1952\n").unwrap();
    let mut copied = seeded_graph(&copied_path);
    let mut inserted = seeded_graph(&inserted_path);

    execute_text(&mut copied, &copy_text("Knows", &rels));
    execute_text(
        &mut inserted,
        "insert rel into Knows values (1 -> 2, 1843), (1 -> 3, 1991), (3 -> 2, 1952)",
    );
    inserted.checkpoint().unwrap();
    let query = plan(
        "nodes(Person) as p | expand Knows out as friend | project p.id, friend.id, friend.name",
    );
    let expected = inserted.run(&query).unwrap();
    assert_eq!(
        expected.rows,
        vec![
            vec![
                Value::Int64(1),
                Value::Int64(2),
                Value::String("Linus".to_owned()),
            ],
            vec![
                Value::Int64(1),
                Value::Int64(3),
                Value::String("Grace".to_owned()),
            ],
            vec![
                Value::Int64(3),
                Value::Int64(2),
                Value::String("Linus".to_owned()),
            ],
        ]
    );
    assert_eq!(copied.run(&query).unwrap(), expected);

    drop(copied);
    drop(inserted);
    let mut copied = Database::open(&copied_path).unwrap();
    let mut inserted = Database::open(&inserted_path).unwrap();
    assert_eq!(copied.run(&query).unwrap(), expected);
    assert_eq!(inserted.run(&query).unwrap(), expected);
}

fn seeded_graph(path: &Path) -> Database {
    let mut database = Database::create(path, PAGE_SIZE).unwrap();
    create_graph(&mut database);
    execute_text(
        &mut database,
        "insert into Person values (1, \"Ada\"), (2, \"Linus\"), (3, \"Grace\")",
    );
    database
}

#[test]
fn endpoint_header_sort_and_open_transaction_refusals_are_atomic() {
    let directory = TestDirectory::new("refusals");
    let path = directory.path("refusals.devondb");
    let missing = directory.path("missing.csv");
    let drift = directory.path("drift.csv");
    let valid = directory.path("valid.csv");
    let multiline_missing = directory.path("multiline-missing.csv");
    fs::write(&missing, "from,to,since\n1,2,1843\n2,99,1952\n").unwrap();
    fs::write(&drift, "from,target,since\n1,2,1843\n").unwrap();
    fs::write(&valid, "from,to,since\n2,3,1952\n").unwrap();
    fs::write(
        &multiline_missing,
        "from,to,note\n1,2,\"first\nedge\"\n2,99,missing\n",
    )
    .unwrap();
    let mut database = seeded_graph(&path);
    execute_text(
        &mut database,
        "create rel table Describes from Person to Person (note String)",
    );
    execute_text(&mut database, "insert rel into Knows values (1 -> 2, 1815)");
    let query = plan("nodes(Person) as p | expand Knows out as friend | project p.id, friend.id");
    let expected = database.run(&query).unwrap();

    let error = database
        .execute(&statement(&copy_text("Knows", &missing)))
        .unwrap_err()
        .to_string();
    let insert_error = database
        .execute(&statement("insert rel into Knows values (2 -> 99, 1952)"))
        .unwrap_err();
    assert_eq!(error, format!("{insert_error} (CSV line 3)"));
    assert_eq!(database.run(&query).unwrap(), expected);

    let error = database
        .execute(&statement(&copy_text("Describes", &multiline_missing)))
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("node table `Person` primary key 99"),
        "{error}"
    );
    assert!(error.contains("CSV line 4"), "{error}");
    assert_eq!(database.run(&query).unwrap(), expected);

    let error = database
        .execute(&statement(&copy_text("Knows", &drift)))
        .unwrap_err()
        .to_string();
    assert!(error.contains("CSV line 1, column 2"), "{error}");
    assert!(error.contains("unknown header column `target`"), "{error}");
    assert_eq!(database.run(&query).unwrap(), expected);

    let sorted = format!("{} sort by since", copy_text("Knows", &valid));
    let error = database
        .execute(&statement(&sorted))
        .unwrap_err()
        .to_string();
    assert!(error.contains("relationship table `Knows`"), "{error}");
    assert!(error.contains("edges load in CSR order"), "{error}");

    let transaction = database.begin().unwrap();
    let error = database
        .execute(&statement(&copy_text("Knows", &valid)))
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "invalid argument: copy requires no open write transactions"
    );
    transaction.commit().unwrap();
    assert_eq!(database.run(&query).unwrap(), expected);
}

#[test]
fn kill_before_rel_publish_keeps_prior_csr_readable() {
    let directory = TestDirectory::new("kill");
    let path = directory.path("kill.devondb");
    let nodes = directory.path("nodes.csv");
    let rels = directory.path("rels.csv");
    write_kill_nodes(&nodes);
    write_kill_rels(&rels);
    let mut database = Database::create(&path, PAGE_SIZE).unwrap();
    create_graph(&mut database);
    execute_text(&mut database, &copy_text("Person", &nodes));
    execute_text(&mut database, "insert rel into Knows values (0 -> 1, 1815)");
    database.checkpoint().unwrap();
    drop(database);
    let baseline_len = fs::metadata(&path).unwrap().len();

    let mut child = Command::new(env::current_exe().unwrap())
        .args([
            "--exact",
            "rel_copy_kill_child_process",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(KILL_CHILD_ENV, "1")
        .env(KILL_PATH_ENV, &path)
        .env(KILL_CSV_ENV, &rels)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    read_until_ready(&mut stdout);
    wait_for_unpublished_page(&mut child, &path, baseline_len);
    child.kill().unwrap();
    assert!(!child.wait().unwrap().success());

    let mut recovered = Database::open(&path).unwrap();
    let query = plan(
        "nodes(Person) as p | filter p.id = 0 | expand Knows out as friend | project friend.id",
    );
    assert_eq!(
        recovered.run(&query).unwrap().rows,
        vec![vec![Value::Int64(1)]]
    );
}

fn write_kill_nodes(path: &Path) {
    let mut file = fs::File::create(path).unwrap();
    writeln!(file, "id,name").unwrap();
    for id in 0..KILL_NODE_COUNT {
        writeln!(file, "{id},person-{id}").unwrap();
    }
}

fn write_kill_rels(path: &Path) {
    let mut file = fs::File::create(path).unwrap();
    writeln!(file, "from,to,since").unwrap();
    for id in 0..KILL_NODE_COUNT {
        for since in 0..16 {
            writeln!(file, "{id},{id},{since}").unwrap();
        }
    }
}

fn read_until_ready(stdout: &mut BufReader<std::process::ChildStdout>) {
    loop {
        let mut line = String::new();
        let bytes = stdout.read_line(&mut line).unwrap();
        assert_ne!(bytes, 0, "relationship COPY child exited before ready");
        if line.contains("REL_COPY_READY") {
            return;
        }
    }
}

fn wait_for_unpublished_page(child: &mut std::process::Child, path: &Path, baseline_len: u64) {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if fs::metadata(path).unwrap().len() > baseline_len {
            return;
        }
        assert!(
            child.try_wait().unwrap().is_none(),
            "relationship COPY child published before it could be killed"
        );
        assert!(Instant::now() < deadline, "relationship COPY wrote no page");
        thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn rel_copy_kill_child_process() {
    if env::var_os(KILL_CHILD_ENV).is_none() {
        return;
    }
    let path = PathBuf::from(env::var_os(KILL_PATH_ENV).unwrap());
    let csv = PathBuf::from(env::var_os(KILL_CSV_ENV).unwrap());
    let mut database = Database::open(path).unwrap();
    println!("REL_COPY_READY");
    std::io::stdout().flush().unwrap();
    execute_text(&mut database, &copy_text("Knows", &csv));
}
