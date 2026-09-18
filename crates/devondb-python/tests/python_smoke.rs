use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

const PYTHON_SCRIPT: &str = r#"
import json
import math
import os
import datetime
import decimal
import devondb

path = os.environ["DEVONDB_SMOKE_PATH"]
with devondb.Database.create(path, page_size=4096, memory_limit=8 * 1024 * 1024) as db:
    db.execute("create node table Person (id Int64 primary key, name String, active Bool, score Float64, embedding Vector(2), note String)")
    db.execute('insert into Person values (1, "Ada", true, 1.5, [1.0, 2.0], null), (2, "Grace", false, 2.5, [3.0, 4.0], "pioneer")')
    db.execute("create node table ScalarValue (id Int64 primary key, happened Timestamp, payload Bytes, amount Decimal(38, 6), document Json)")
    db.execute(r'insert into ScalarValue values (1, timestamp("2024-08-09T00:00:00.123456Z"), bytes("00ff1a"), decimal("12345678901234567890123456789012.345678"), json("{\"count\":3,\"flags\":[true,null],\"name\":\"Ada\"}"))')
    scalar = db.query("nodes(ScalarValue) as s | project s.happened as happened, s.payload as payload, s.amount as amount, s.document as document").rows[0]
    assert type(scalar[0]) is datetime.datetime
    assert scalar[0] == datetime.datetime(2024, 8, 9, 0, 0, 0, 123456, tzinfo=datetime.timezone.utc)
    assert type(scalar[1]) is bytes and scalar[1] == b"\x00\xff\x1a"
    assert type(scalar[2]) is decimal.Decimal
    assert scalar[2] == decimal.Decimal("12345678901234567890123456789012.345678")
    assert type(scalar[3]) is dict
    assert scalar[3] == {"count": 3, "flags": [True, None], "name": "Ada"}
    db.execute("create node table Setting (id Int64 primary key, value String)")
    db.execute('upsert Setting values (1, "first")')
    db.execute('upsert Setting values (1, "second")')
    upserted = db.query("nodes(Setting) as s | project s.id as id, s.value as value").rows
    assert upserted == [[1, "second"]]
    result = db.query("nodes(Person) as p | project p.id as id, p.name as name, p.active as active, p.score as score, p.embedding as embedding, p.note as note | sort p.id")
    schema = db.schema()
    nonfinite = db.query("nodes(Person) as p | limit 1 | project 1.0 / 0.0 as pos_inf, -1.0 / 0.0 as neg_inf, 0.0 / 0.0 as nan").rows[0]
    assert math.isinf(nonfinite[0]) and nonfinite[0] > 0
    assert math.isinf(nonfinite[1]) and nonfinite[1] < 0
    assert math.isnan(nonfinite[2])
    try:
        db.execute("nodes(Person) as p")
    except ValueError as error:
        assert str(error) == "expected a statement, found a query"
    else:
        raise AssertionError("query text passed to execute did not raise ValueError")
    try:
        db.query("insert into Person values (3, \"Katherine\", true, 3.5, [5.0, 6.0], null)")
    except ValueError as error:
        assert str(error) == "expected a query, found a statement"
    else:
        raise AssertionError("statement text passed to query did not raise ValueError")
    db.checkpoint()
    try:
        db.execute("bogus")
    except RuntimeError as error:
        error_message = str(error)
    else:
        raise AssertionError("bad DevonPlan text did not raise RuntimeError")

try:
    db.query("nodes(Person) as p | project p.id as id")
except RuntimeError as error:
    assert str(error) == "database is closed"
else:
    raise AssertionError("a closed database did not raise RuntimeError")

try:
    db.execute("bogus")
except RuntimeError as error:
    assert str(error) == "database is closed"
except ValueError:
    raise AssertionError("closed database raised ValueError from parsing before the closed check")
else:
    raise AssertionError("invalid text on a closed database did not raise RuntimeError")

with devondb.Database.open(path, page_size=4096, memory_limit=8 * 1024 * 1024) as reopened:
    reopened_result = reopened.query("nodes(Person) as p | sort p.id | project p.name as name")

reopened.close()

payload = {
    "columns": result.columns,
    "rows": result.rows,
    "schema": schema,
    "error": error_message,
    "reopened": [reopened_result.columns, reopened_result.rows],
    "scalar": [scalar[0].isoformat(), scalar[1].hex(), str(scalar[2]), scalar[3]],
    "upserted": upserted,
}
print(json.dumps(payload, sort_keys=True, separators=(",", ":")))
"#;

#[test]
fn python_module_imports_and_queries() {
    if !python3_available() {
        println!("SKIP: python3 is not on PATH; devondb Python smoke test skipped");
        return;
    }

    let directory = TestDirectory::new().expect("temporary directory should be created");
    let module = directory.path().join("devondb.so");
    fs::copy(cdylib_path(), &module).expect("devondb extension should be copied");

    let output = Command::new("python3")
        .arg("-c")
        .arg(PYTHON_SCRIPT)
        .env("PYTHONPATH", directory.path())
        .env("DEVONDB_SMOKE_PATH", directory.path().join("smoke.devondb"))
        .output()
        .expect("python3 smoke test should start");

    assert!(
        output.status.success(),
        "python3 smoke test failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).expect("python stdout should be UTF-8");
    let actual = stdout.trim();
    println!("python3 smoke output: {actual}");
    assert_eq!(actual, expected_output());
}

fn python3_available() -> bool {
    match Command::new("python3").arg("--version").output() {
        Ok(output) => output.status.success(),
        Err(error) if error.kind() == io::ErrorKind::NotFound => false,
        Err(error) => panic!("failed to check python3 availability: {error}"),
    }
}

fn cdylib_path() -> PathBuf {
    let executable = std::env::current_exe().expect("test executable path should resolve");
    let deps = executable
        .parent()
        .expect("test executable should be in target/<profile>/deps");
    let profile = deps
        .parent()
        .expect("deps should be inside target/<profile>");
    let extension = if cfg!(target_os = "macos") {
        "dylib"
    } else {
        "so"
    };
    let artifact = profile.join(format!("libdevondb_python.{extension}"));
    build_cdylib(profile);
    assert!(
        artifact.is_file(),
        "cdylib not found at {}",
        artifact.display()
    );
    artifact
}

fn build_cdylib(profile: &Path) {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("binding crate should be inside the workspace");
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let mut command = Command::new(cargo);
    command.args(["rustc", "-p", "devondb-python", "--lib"]);
    if cfg!(target_os = "macos") {
        command.args([
            "--",
            "-C",
            "link-arg=-undefined",
            "-C",
            "link-arg=dynamic_lookup",
        ]);
    }
    let output = command
        .current_dir(workspace)
        .env(
            "CARGO_TARGET_DIR",
            profile.parent().expect("profile has target root"),
        )
        .output()
        .expect("cargo build for the cdylib should start");
    assert!(
        output.status.success(),
        "cargo build for the cdylib failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn expected_output() -> &'static str {
    r#"{"columns":["id","name","active","score","embedding","note"],"error":"invalid argument: DevonPlan text parse error at position 1: expected query source `nodes`/`knn` or statement `create`/`insert`/`upsert`/`copy`/`update`/`delete`; offending token \"bogus\"","reopened":[["name"],[["Ada"],["Grace"]]],"rows":[[1,"Ada",true,1.5,[1.0,2.0],null],[2,"Grace",false,2.5,[3.0,4.0],"pioneer"]],"scalar":["2024-08-09T00:00:00.123456+00:00","00ff1a","12345678901234567890123456789012.345678",{"count":3,"flags":[true,null],"name":"Ada"}],"schema":{"classes":{"interfaces":[],"node_classes":[{"display":"Person","label":"name","plural":"people","table":"Person"},{"display":"ScalarValue","plural":"scalarvalues","table":"ScalarValue"},{"display":"Setting","plural":"settings","table":"Setting"}],"rel_classes":[]},"node_tables":[{"columns":[{"name":"id","primary_key":true,"type":"Int64"},{"name":"name","primary_key":false,"type":"String"},{"name":"active","primary_key":false,"type":"Bool"},{"name":"score","primary_key":false,"type":"Float64"},{"name":"embedding","primary_key":false,"type":"Vector(2)"},{"name":"note","primary_key":false,"type":"String"}],"name":"Person"},{"columns":[{"name":"id","primary_key":true,"type":"Int64"},{"name":"happened","primary_key":false,"type":"Timestamp"},{"name":"payload","primary_key":false,"type":"Bytes"},{"name":"amount","primary_key":false,"type":"Decimal(38, 6)"},{"name":"document","primary_key":false,"type":"Json"}],"name":"ScalarValue"},{"columns":[{"name":"id","primary_key":true,"type":"Int64"},{"name":"value","primary_key":false,"type":"String"}],"name":"Setting"}],"rel_tables":[]},"upserted":[[1,"second"]]}"#
}

struct TestDirectory {
    path: PathBuf,
}

impl TestDirectory {
    fn new() -> io::Result<Self> {
        let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should follow the Unix epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "devondb-python-smoke-{}-{timestamp}-{sequence}",
            std::process::id()
        ));
        fs::create_dir(&path)?;
        Ok(Self { path })
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}
