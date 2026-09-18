use std::{
    env, fs,
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use devondb::{Database, DevonError, Options, Plan, QueryResult, Statement};
use devondb_plan::text::parser::{Parsed, parse};
use devondb_storage::{bulk::geo_atom_sort_key, node_group::NODE_GROUP_CAPACITY};
use devondb_types::{GeoPoint, logical_type::LogicalType, schema::Column, value::Value};

const PAGE_SIZE: u32 = 4096;
const COPY_CHILD_ENV: &str = "DEVONDB_COPY_KILL_CHILD";
const COPY_CHILD_PATH_ENV: &str = "DEVONDB_COPY_KILL_PATH";
const COPY_CHILD_CSV_ENV: &str = "DEVONDB_COPY_KILL_CSV";
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
        let path = env::temp_dir().join(format!(
            "devondb-copy-test-{}-{timestamp}-{sequence}",
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

fn column(name: &str, ty: LogicalType, primary_key: bool) -> Column {
    Column {
        name: name.to_owned(),
        ty,
        primary_key,
    }
}

fn create_people(database: &mut Database) {
    database
        .execute(&Statement::CreateNodeTable {
            name: "Person".to_owned(),
            columns: vec![
                column("id", LogicalType::Int64, true),
                column("name", LogicalType::String, false),
            ],
        })
        .unwrap();
}

fn parsed_statement(text: &str) -> Statement {
    match parse(text).unwrap() {
        Parsed::Statement(statement) => statement.stmt,
        Parsed::Query(_) => panic!("expected statement: {text}"),
    }
}

fn parsed_plan(text: &str) -> Plan {
    match parse(text).unwrap() {
        Parsed::Query(plan) => plan,
        Parsed::Statement(_) => panic!("expected query: {text}"),
    }
}

fn quoted_path(path: &Path) -> String {
    path.to_string_lossy()
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
}

fn copy_statement(table: &str, path: &Path, sort_by: Option<&str>) -> Statement {
    let sort = sort_by.map_or_else(String::new, |column| format!(" sort by {column}"));
    parsed_statement(&format!(
        "copy {table} from \"{}\"{sort}",
        quoted_path(path)
    ))
}

fn scan(database: &mut Database, table: &str) -> QueryResult {
    database
        .run(&parsed_plan(&format!("nodes({table}) as t")))
        .unwrap()
}

#[test]
fn parsed_copy_round_trips_every_csv_type_and_null() {
    let directory = TestDirectory::new();
    let csv = directory.path("all-types.csv");
    fs::write(
        &csv,
        concat!(
            "name,id,score,active,embedding,place\n",
            "Ada,1,1.5,true,\"[0.1, 0.2]\",\"geo(45.5, -122.625)\"\n",
            ",2,,,\"[3, 4]\",\n",
            "\"\",3,-0.0,false,,\"geo(0, 0)\"\n"
        ),
    )
    .unwrap();
    let mut database = Database::create(directory.path("all-types.devondb"), PAGE_SIZE).unwrap();
    database
        .execute(&Statement::CreateNodeTable {
            name: "Reading".to_owned(),
            columns: vec![
                column("id", LogicalType::Int64, true),
                column("score", LogicalType::Float64, false),
                column("name", LogicalType::String, false),
                column("active", LogicalType::Bool, false),
                column("embedding", LogicalType::Vector { dim: 2 }, false),
                column("place", LogicalType::GeoPoint, false),
            ],
        })
        .unwrap();

    database
        .execute(&copy_statement("Reading", &csv, None))
        .unwrap();

    assert_eq!(
        scan(&mut database, "Reading").rows,
        vec![
            vec![
                Value::Int64(1),
                Value::Float64(1.5),
                Value::String("Ada".to_owned()),
                Value::Bool(true),
                Value::Vector(vec![0.1, 0.2]),
                Value::GeoPoint(GeoPoint::new(45.5, -122.625).unwrap()),
            ],
            vec![
                Value::Int64(2),
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Vector(vec![3.0, 4.0]),
                Value::Null,
            ],
            vec![
                Value::Int64(3),
                Value::Float64(-0.0),
                Value::String(String::new()),
                Value::Bool(false),
                Value::Null,
                Value::GeoPoint(GeoPoint::new(0.0, 0.0).unwrap()),
            ],
        ]
    );
}

#[test]
fn sorted_copy_proves_zone_map_multiplier_two_sidedly() {
    let directory = TestDirectory::new();
    let csv = directory.path("shuffled.csv");
    write_shuffled_csv(&csv, 4 * NODE_GROUP_CAPACITY);
    let mut sorted = range_database(&directory.path("sorted.devondb"));
    let mut unsorted = range_database(&directory.path("unsorted.devondb"));
    sorted
        .execute(&copy_statement("Reading", &csv, Some("id")))
        .unwrap();
    unsorted
        .execute(&copy_statement("Reading", &csv, None))
        .unwrap();

    let selective = parsed_plan("nodes(Reading) as r | filter r.id < 20 | project r.id as id");
    sorted.reset_page_read_count();
    let sorted_result = sorted.run(&selective).unwrap();
    let sorted_selective_reads = sorted.page_read_count();
    unsorted.reset_page_read_count();
    let unsorted_result = unsorted.run(&selective).unwrap();
    let unsorted_selective_reads = unsorted.page_read_count();
    assert_same_ids(sorted_result.rows, unsorted_result.rows);

    sorted.reset_page_read_count();
    let full = sorted.run(&parsed_plan("nodes(Reading) as r")).unwrap();
    let sorted_full_reads = sorted.page_read_count();
    assert_eq!(full.rows.len(), 4 * NODE_GROUP_CAPACITY);
    assert!(
        sorted_selective_reads < unsorted_selective_reads,
        "sorted selective read {sorted_selective_reads} pages; unsorted read {unsorted_selective_reads}"
    );
    assert!(
        sorted_full_reads >= sorted_selective_reads,
        "sorted full scan read {sorted_full_reads} pages; selective read {sorted_selective_reads}"
    );
}

fn write_shuffled_csv(path: &Path, rows: usize) {
    let mut file = fs::File::create(path).unwrap();
    writeln!(file, "id,value").unwrap();
    let group_count = 4;
    for lane in 0..group_count {
        for offset in 0..(rows / group_count) {
            let id = offset * group_count + lane;
            writeln!(file, "{id},{}", id * 3).unwrap();
        }
    }
}

fn range_database(path: &Path) -> Database {
    let mut database = Database::create(path, PAGE_SIZE).unwrap();
    database
        .execute(&Statement::CreateNodeTable {
            name: "Reading".to_owned(),
            columns: vec![
                column("id", LogicalType::Int64, true),
                column("value", LogicalType::Int64, false),
            ],
        })
        .unwrap();
    database
}

fn assert_same_ids(left: Vec<Vec<Value>>, right: Vec<Vec<Value>>) {
    let mut left = left
        .into_iter()
        .map(|row| row[0].clone())
        .collect::<Vec<_>>();
    let mut right = right
        .into_iter()
        .map(|row| row[0].clone())
        .collect::<Vec<_>>();
    left.sort_by_key(|value| match value {
        Value::Int64(value) => *value,
        other => panic!("unexpected id {other:?}"),
    });
    right.sort_by_key(|value| match value {
        Value::Int64(value) => *value,
        other => panic!("unexpected id {other:?}"),
    });
    assert_eq!(left, (0..20).map(Value::Int64).collect::<Vec<_>>());
    assert_eq!(right, left);
}

#[test]
fn primary_key_failures_match_insert_and_leave_rows_unchanged() {
    let directory = TestDirectory::new();
    let mut database = Database::create(directory.path("pk.devondb"), PAGE_SIZE).unwrap();
    create_people(&mut database);
    database
        .execute(&Statement::InsertNode {
            table: "Person".to_owned(),
            rows: vec![vec![Value::Int64(1), Value::String("existing".to_owned())]],
        })
        .unwrap();
    let expected = scan(&mut database, "Person").rows;
    let cases = [
        (
            "null.csv",
            "id,name\n,nullish\n",
            "invalid argument: primary key column `id` in node table `Person` cannot be null",
        ),
        (
            "file-duplicate.csv",
            "id,name\n2,Ada\n2,Grace\n",
            "invalid argument: duplicate primary key `2` in node table `Person`",
        ),
        (
            "existing-duplicate.csv",
            "id,name\n1,replacement\n",
            "invalid argument: duplicate primary key `1` in node table `Person`",
        ),
    ];
    for (name, contents, expected_error) in cases {
        let csv = directory.path(name);
        fs::write(&csv, contents).unwrap();
        let error = database
            .execute(&copy_statement("Person", &csv, None))
            .unwrap_err();
        assert_eq!(error.to_string(), expected_error);
        assert_eq!(scan(&mut database, "Person").rows, expected);
    }
}

#[test]
fn open_write_transaction_refuses_copy_then_commit_unblocks_it() {
    let directory = TestDirectory::new();
    let csv = directory.path("one.csv");
    fs::write(&csv, "id,name\n1,Ada\n").unwrap();
    let mut database = Database::create(directory.path("quiescence.devondb"), PAGE_SIZE).unwrap();
    create_people(&mut database);
    let transaction = database.begin().unwrap();

    let error = database
        .execute(&copy_statement("Person", &csv, None))
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "invalid argument: copy requires no open write transactions"
    );
    transaction.commit().unwrap();
    database
        .execute(&copy_statement("Person", &csv, None))
        .unwrap();
    assert_eq!(scan(&mut database, "Person").rows.len(), 1);
}

#[test]
fn copy_refuses_rel_unknown_missing_and_string_sort() {
    let directory = TestDirectory::new();
    let people_csv = directory.path("people.csv");
    fs::write(&people_csv, "id,name\n1,Ada\n").unwrap();
    let document_csv = directory.path("documents.csv");
    fs::write(&document_csv, "id,embedding\n1,\"[1, 0]\"\n").unwrap();
    let mut database = Database::create(directory.path("refusals.devondb"), PAGE_SIZE).unwrap();
    create_people(&mut database);
    database
        .execute(&parsed_statement(
            "create rel table Knows from Person to Person",
        ))
        .unwrap();
    database
        .execute(&parsed_statement(
            "create node table Document (id Int64 primary key, embedding Vector(2))",
        ))
        .unwrap();
    database
        .execute(&parsed_statement(
            "create hnsw index document_ann on Document.embedding metric cosine",
        ))
        .unwrap();

    // Relationship-table COPY takes the relationship path, so this node-shaped
    // CSV is rejected by its header.
    let rel = database
        .execute(&copy_statement("Knows", &people_csv, None))
        .unwrap_err()
        .to_string();
    assert!(rel.contains("unknown header column `id`"), "{rel}");
    // Indexed node tables are legal COPY targets because replacement roots
    // build inside the fence. The end-to-end behavior lives in
    // copy_hnsw.rs — here only the old refusal's absence is pinned.
    database
        .execute(&copy_statement("Document", &document_csv, None))
        .expect("COPY into an HNSW-indexed table is legal");
    let unknown = database
        .execute(&copy_statement("Persn", &people_csv, None))
        .unwrap_err()
        .to_string();
    assert!(unknown.contains("node table `Persn`"), "{unknown}");
    assert!(unknown.contains("did you mean `Person`"), "{unknown}");
    let missing = database
        .execute(&copy_statement(
            "Person",
            &directory.path("does-not-exist.csv"),
            None,
        ))
        .unwrap_err();
    assert!(matches!(
        missing,
        DevonError::Io(error) if error.kind() == std::io::ErrorKind::NotFound
    ));
    let string_sort = database
        .execute(&copy_statement("Person", &people_csv, Some("name")))
        .unwrap_err()
        .to_string();
    assert!(
        string_sort.contains("unsortable type String"),
        "{string_sort}"
    );
    assert!(scan(&mut database, "Person").rows.is_empty());
}

#[test]
fn float_and_geo_sorts_place_nulls_last() {
    let directory = TestDirectory::new();
    let csv = directory.path("sort-types.csv");
    fs::write(
        &csv,
        concat!(
            "id,score,place\n",
            "1,2.5,\"geo(45, -122)\"\n",
            "2,-1.0,\"geo(0, 0)\"\n",
            "3,,\n"
        ),
    )
    .unwrap();
    let mut database = Database::create(directory.path("sort-types.devondb"), PAGE_SIZE).unwrap();
    for table in ["ByFloat", "ByGeo"] {
        database
            .execute(&Statement::CreateNodeTable {
                name: table.to_owned(),
                columns: vec![
                    column("id", LogicalType::Int64, true),
                    column("score", LogicalType::Float64, false),
                    column("place", LogicalType::GeoPoint, false),
                ],
            })
            .unwrap();
    }
    database
        .execute(&copy_statement("ByFloat", &csv, Some("score")))
        .unwrap();
    database
        .execute(&copy_statement("ByGeo", &csv, Some("place")))
        .unwrap();

    assert_eq!(ids(scan(&mut database, "ByFloat").rows), vec![2, 1, 3]);
    let geo_rows = scan(&mut database, "ByGeo").rows;
    assert_eq!(geo_rows.last().unwrap()[2], Value::Null);
    let keys = geo_rows[..2]
        .iter()
        .map(|row| match row[2] {
            Value::GeoPoint(point) => geo_atom_sort_key(point).unwrap(),
            ref other => panic!("unexpected geo sort value: {other:?}"),
        })
        .collect::<Vec<_>>();
    assert!(keys[0] <= keys[1]);
}

fn ids(rows: Vec<Vec<Value>>) -> Vec<i64> {
    rows.into_iter()
        .map(|row| match row[0] {
            Value::Int64(value) => value,
            ref other => panic!("unexpected id: {other:?}"),
        })
        .collect()
}

#[test]
fn sorted_copy_budget_error_is_atomic_and_database_stays_usable() {
    let directory = TestDirectory::new();
    let csv = directory.path("too-large.csv");
    let mut file = fs::File::create(&csv).unwrap();
    writeln!(file, "id,name").unwrap();
    let payload = "x".repeat(128);
    for id in 0..20_000 {
        writeln!(file, "{id},{payload}").unwrap();
    }
    drop(file);
    let mut database = Database::create_with(
        directory.path("budget.devondb"),
        Options {
            page_size: PAGE_SIZE,
            memory_limit: 1024 * 1024,
        },
    )
    .unwrap();
    create_people(&mut database);

    let error = database
        .execute(&copy_statement("Person", &csv, Some("id")))
        .unwrap_err();
    assert!(
        matches!(error, DevonError::BudgetExceeded { .. }),
        "{error}"
    );
    assert!(scan(&mut database, "Person").rows.is_empty());
    database
        .execute(&Statement::InsertNode {
            table: "Person".to_owned(),
            rows: vec![vec![Value::Int64(-1), Value::String("usable".to_owned())]],
        })
        .unwrap();
    assert_eq!(scan(&mut database, "Person").rows.len(), 1);
}

#[test]
fn kill_mid_copy_keeps_original_rows_and_copy_can_retry() {
    let directory = TestDirectory::new();
    let path = directory.path("kill.devondb");
    let csv = directory.path("kill.csv");
    let mut database = Database::create(&path, PAGE_SIZE).unwrap();
    create_people(&mut database);
    database
        .execute(&Statement::InsertNode {
            table: "Person".to_owned(),
            rows: vec![vec![Value::Int64(0), Value::String("original".to_owned())]],
        })
        .unwrap();
    database.checkpoint().unwrap();
    drop(database);
    write_kill_csv(&csv, 200_000);
    let baseline_len = fs::metadata(&path).unwrap().len();

    let mut child = Command::new(env::current_exe().unwrap())
        .args([
            "--exact",
            "copy_kill_child_process",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(COPY_CHILD_ENV, "1")
        .env(COPY_CHILD_PATH_ENV, &path)
        .env(COPY_CHILD_CSV_ENV, &csv)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let ready = read_until_ready(&mut stdout);
    assert!(ready.contains("COPY_READY"), "child output: {ready:?}");
    wait_for_bulk_page(&mut child, &path, baseline_len);
    child.kill().unwrap();
    let status = child.wait().unwrap();
    assert!(!status.success(), "COPY child completed before SIGKILL");

    let mut recovered = Database::open(&path).unwrap();
    assert_eq!(
        scan(&mut recovered, "Person").rows,
        vec![vec![Value::Int64(0), Value::String("original".to_owned())]]
    );
    recovered
        .execute(&copy_statement("Person", &csv, None))
        .unwrap();
    let count = recovered
        .run(&parsed_plan(
            "nodes(Person) as p | aggregate count(p.id) as total",
        ))
        .unwrap();
    assert_eq!(count.rows, vec![vec![Value::Int64(200_001)]]);
}

fn write_kill_csv(path: &Path, rows: usize) {
    let mut file = fs::File::create(path).unwrap();
    writeln!(file, "id,name").unwrap();
    for id in 1..=rows {
        writeln!(file, "{id},row-{id}").unwrap();
    }
}

fn read_until_ready(stdout: &mut BufReader<std::process::ChildStdout>) -> String {
    let mut output = String::new();
    loop {
        let mut line = String::new();
        let bytes = stdout.read_line(&mut line).unwrap();
        assert_ne!(bytes, 0, "COPY child exited before announcing readiness");
        output.push_str(&line);
        if line.contains("COPY_READY") {
            return output;
        }
    }
}

fn wait_for_bulk_page(child: &mut std::process::Child, path: &Path, baseline_len: u64) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if fs::metadata(path).unwrap().len() > baseline_len {
            return;
        }
        assert!(
            child.try_wait().unwrap().is_none(),
            "COPY child exited before writing an unpublished bulk page"
        );
        assert!(Instant::now() < deadline, "COPY child wrote no bulk page");
        thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn copy_kill_child_process() {
    if env::var_os(COPY_CHILD_ENV).is_none() {
        return;
    }
    let path = PathBuf::from(env::var_os(COPY_CHILD_PATH_ENV).unwrap());
    let csv = PathBuf::from(env::var_os(COPY_CHILD_CSV_ENV).unwrap());
    let mut database = Database::open(path).unwrap();
    println!("COPY_READY");
    std::io::stdout().flush().unwrap();
    database
        .execute(&copy_statement("Person", &csv, None))
        .unwrap();
}
