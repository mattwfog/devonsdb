use std::{
    ffi::OsStr,
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
    fn new(label: &str) -> Self {
        let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "devondb-cli-dump-{label}-{}-{timestamp}-{sequence}",
            std::process::id()
        ));
        fs::create_dir(&path).unwrap();
        Self { path }
    }

    fn file(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

#[test]
fn every_value_type_and_deleted_graph_round_trip_byte_identically() {
    let directory = TestDirectory::new("values");
    let original = directory.file("original.devondb");
    let rebuilt = directory.file("rebuilt.devondb");
    let seed = concat!(
        "create node table AllValues (id Int64 primary key, flag Bool, whole Int64, ratio Float64, name String, embedding Vector(2), point GeoPoint, happened Timestamp, payload Bytes, amount Decimal(10, 2), document Json, optional String)\n",
        "create rel table Links from AllValues to AllValues (note String, weight Float64, path Vector(2), active Bool)\n",
        "create interface Named (name String)\n",
        "create class for AllValues (display \"Value row\", plural \"value rows\", label name, summary (name, amount), color \"#123456\", description \"all scalar values\", implements (Named))\n",
        "create class for Links (verb \"links\", inverse \"is linked by\")\n",
        "insert into AllValues values (2, false, -9, 2.5, \"two\", [2.0, 0.0], geo(40.0, -74.0), timestamp(\"2024-08-09T00:00:00.123456Z\"), bytes(\"00ff\"), decimal(\"-19.99\"), json(\"{\\\"b\\\":2,\\\"a\\\":[true,null]}\"), null)\n",
        "insert into AllValues values (1, true, 7, -0.5, \"one\", [1.0, 1.0], geo(0.0, 0.0), timestamp(\"1970-01-01T00:00:00Z\"), bytes(\"\"), decimal(\"0.01\"), json(\"{}\"), \"present\")\n",
        "insert into AllValues values (3, null, null, null, \"deleted\", null, null, null, null, null, null, null)\n",
        "insert rel into Links values (1 -> 2, \"kept\", 0.75, [0.5, 1.5], true), (3 -> 1, null, null, null, null)\n",
        "detach delete from AllValues where id = 3\n",
        ".exit\n",
    );
    assert_success(&run(&[original.as_os_str()], seed), "seed every-value DB");

    let first = dump(&original);
    let first_text = text(&first);
    assert!(
        first_text
            .contains("insert rel into Links values (1 -> 2, \"kept\", 0.75, [0.5, 1.5], true)"),
        "relationship properties are missing from dump: {first_text}"
    );
    assert!(
        !first_text.contains("deleted"),
        "deleted row leaked: {first_text}"
    );
    assert_success(&run(&[rebuilt.as_os_str()], first_text), "restore dump");
    let second = dump(&rebuilt);
    assert_eq!(first.stdout, second.stdout, "dump is not a fixed point");

    let battery = concat!(
        "nodes(AllValues) as n | sort n.id | project n.id, n.flag, n.whole, n.ratio, n.name, n.embedding, n.point, n.happened, n.payload, n.amount, n.document, n.optional\n",
        "nodes(AllValues) as n | filter n.amount >= decimal(\"0.00\") | sort n.id | project n.id, n.amount\n",
        "nodes(AllValues) as n | expand Links out as other | project n.id, other.id, other.note, other.weight, other.path, other.active\n",
        "nodes(AllValues) as n | expand Links out as other | aggregate count(other.id) as count\n",
        ".exit\n",
    );
    let original_queries = run(&[original.as_os_str()], battery);
    let rebuilt_queries = run(&[rebuilt.as_os_str()], battery);
    assert_success(&original_queries, "query original");
    assert_success(&rebuilt_queries, "query rebuilt");
    assert_eq!(original_queries.stdout, rebuilt_queries.stdout);
}

#[test]
fn committed_golden_database_and_pack_restore_with_equal_counts() {
    let directory = TestDirectory::new("golden");
    let golden = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/golden/m2-social.devondb");
    let golden_wal = PathBuf::from(format!("{}-wal", golden.display()));
    assert!(!golden_wal.exists(), "golden corpus contains a WAL");
    let restored_db = directory.file("golden-restored.devondb");
    let golden_dump = dump(&golden);
    assert_success(
        &run(&[restored_db.as_os_str()], text(&golden_dump)),
        "restore golden database",
    );
    let query_source = directory.file("golden-query-source.devondb");
    fs::copy(&golden, &query_source).unwrap();
    assert_equal_counts(&query_source, &restored_db);
    assert!(
        !golden_wal.exists(),
        "dumping or querying the golden corpus created a WAL"
    );

    let pack_source = directory.file("pack-source.devondb");
    fs::copy(&golden, &pack_source).unwrap();
    let pack = directory.file("golden.pack");
    assert_success(
        &run(
            &["pack".as_ref(), pack_source.as_os_str(), pack.as_os_str()],
            "",
        ),
        "pack golden database",
    );
    let restored_pack = directory.file("pack-restored.devondb");
    let pack_dump = dump(&pack);
    assert_success(
        &run(&[restored_pack.as_os_str()], text(&pack_dump)),
        "restore golden pack",
    );
    assert_equal_counts(&pack, &restored_pack);
}

fn assert_equal_counts(left: &Path, right: &Path) {
    let query = "nodes(Person) as p | aggregate count(p.id) as count\n.exit\n";
    let left = run(&[left.as_os_str()], query);
    let right = run(&[right.as_os_str()], query);
    assert_success(&left, "count source");
    assert_success(&right, "count restored");
    assert_eq!(left.stdout, right.stdout);
}

fn dump(path: &Path) -> Output {
    let output = run(&["dump".as_ref(), path.as_os_str()], "");
    assert_success(&output, "dump database");
    output
}

fn run(arguments: &[&OsStr], input: &str) -> Output {
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

fn assert_success(output: &Output, context: &str) {
    assert!(
        output.status.success(),
        "{context} failed: status={} stdout={} stderr={}",
        output.status,
        text(output),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn text(output: &Output) -> &str {
    std::str::from_utf8(&output.stdout).unwrap()
}
