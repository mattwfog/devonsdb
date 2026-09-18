use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

const C_PROGRAM: &str = r#"
#include <stdint.h>
#include <stdio.h>
#include <string.h>

#include "devondb.h"

#define CHECK(condition, code) \
    do { \
        if (!(condition)) { \
            fprintf(stderr, "check failed at line %d: %s\n", __LINE__, #condition); \
            return (code); \
        } \
    } while (0)

int main(int argc, char **argv) {
    devondb_database *db = NULL;
    devondb_status status;
    char *json = NULL;
    char *schema = NULL;
    const char *error;

    CHECK(argc == 2, 2);
    devondb_close(NULL);
    devondb_string_free(NULL);

    status = devondb_create(argv[1], UINT32_C(4096), &db);
    printf("create_status=%d\n", (int)status);
    CHECK(status == DEVONDB_OK && db != NULL, 3);

    status = devondb_execute(
        db,
        "create node table Person (id Int64 primary key, name String, "
        "active Bool, score Float64, embedding Vector(2), nickname String, "
        "location GeoPoint)"
    );
    printf("ddl_status=%d\n", (int)status);
    CHECK(status == DEVONDB_OK, 4);

    status = devondb_execute(
        db,
        "insert into Person values "
        "(1, \"Ada\", true, 1.5, [0.5, 0.25], null, geo(45.5, -122.625)), "
        "(2, \"Grace\", false, 2.25, [0.75, 1.0], \"compiler\", "
        "geo(90.0, 0.0))"
    );
    printf("insert_status=%d\n", (int)status);
    CHECK(status == DEVONDB_OK, 5);

    status = devondb_query_json(
        db,
        "nodes(Person) as p | sort p.id | project p.id as id, p.name as name, "
        "p.active as active, p.score as score, p.embedding as embedding, "
        "p.nickname as nickname, p.location as location",
        &json
    );
    printf("query_status=%d\n", (int)status);
    CHECK(status == DEVONDB_OK && json != NULL, 6);
    printf("query=%s\n", json);
    devondb_string_free(json);
    json = NULL;

    status = devondb_query_json(
        db,
        "nodes(Person) as p | sort p.id | project p.score / 0.0 as non_finite",
        &json
    );
    printf("nonfinite_status=%d\n", (int)status);
    CHECK(status == DEVONDB_OK && json != NULL, 7);
    printf("nonfinite=%s\n", json);
    devondb_string_free(json);
    json = NULL;

    status = devondb_execute(db, "nodes(Person) as p");
    printf("error_status=%d\n", (int)status);
    CHECK(status == DEVONDB_ERR, 8);
    error = devondb_last_error(db);
    CHECK(error != NULL && error[0] != '\0', 9);
    CHECK(strcmp(error, "expected a statement, found a query") == 0, 10);
    printf("error_nonempty=1\n");

    status = devondb_schema_json(db, &schema);
    printf("schema_status=%d\n", (int)status);
    CHECK(status == DEVONDB_OK && schema != NULL, 11);
    printf("schema=%s\n", schema);
    CHECK(devondb_last_error(db)[0] == '\0', 12);
    printf("error_cleared=1\n");
    devondb_string_free(schema);

    status = devondb_execute(
        db,
        "create node table ScalarValue (id Int64 primary key, "
        "happened Timestamp, payload Bytes, amount Decimal(38, 6), "
        "document Json)"
    );
    printf("scalar_ddl_status=%d\n", (int)status);
    CHECK(status == DEVONDB_OK, 15);

    status = devondb_execute(
        db,
        "insert into ScalarValue values "
        "(1, timestamp(\"2024-08-09T00:00:00.123456Z\"), bytes(\"00ff1a\"), "
        "decimal(\"12345678901234567890123456789012.345678\"), "
        "json(\"{\\\"count\\\":3,\\\"flags\\\":[true,null],\\\"name\\\":\\\"Ada\\\"}\"))"
    );
    printf("scalar_insert_status=%d\n", (int)status);
    CHECK(status == DEVONDB_OK, 16);

    status = devondb_query_json(
        db,
        "nodes(ScalarValue) as s | project s.happened as happened, "
        "s.payload as payload, s.amount as amount, s.document as document",
        &json
    );
    printf("scalar_query_status=%d\n", (int)status);
    CHECK(status == DEVONDB_OK && json != NULL, 17);
    printf("scalar=%s\n", json);
    devondb_string_free(json);
    json = NULL;

    status = devondb_execute(
        db,
        "create node table Setting (id Int64 primary key, value String)"
    );
    printf("upsert_ddl_status=%d\n", (int)status);
    CHECK(status == DEVONDB_OK, 18);

    status = devondb_execute(db, "upsert Setting values (1, \"first\")");
    printf("upsert_first_status=%d\n", (int)status);
    CHECK(status == DEVONDB_OK, 19);
    status = devondb_execute(db, "upsert Setting values (1, \"second\")");
    printf("upsert_second_status=%d\n", (int)status);
    CHECK(status == DEVONDB_OK, 20);

    status = devondb_query_json(
        db,
        "nodes(Setting) as s | project s.id as id, s.value as value",
        &json
    );
    printf("upsert_query_status=%d\n", (int)status);
    CHECK(status == DEVONDB_OK && json != NULL, 21);
    printf("upsert=%s\n", json);
    devondb_string_free(json);
    json = NULL;

    devondb_close(db);
    db = NULL;
    status = devondb_open(argv[1], &db);
    printf("open_status=%d\n", (int)status);
    CHECK(status == DEVONDB_OK && db != NULL, 13);

    status = devondb_query_json(
        db,
        "nodes(Person) as p | sort p.id | project p.id as id, p.name as name, "
        "p.active as active, p.score as score, p.embedding as embedding, "
        "p.nickname as nickname, p.location as location",
        &json
    );
    printf("reopen_query_status=%d\n", (int)status);
    CHECK(status == DEVONDB_OK && json != NULL, 14);
    printf("reopen_query=%s\n", json);
    devondb_string_free(json);
    devondb_close(db);
    return 0;
}
"#;

struct TestDir(PathBuf);

impl TestDir {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock is before Unix epoch")
            .as_nanos();
        let path = env::temp_dir().join(format!("devondb-c-smoke-{}-{nonce}", std::process::id()));
        fs::create_dir(&path).expect("create C smoke test temp directory");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _result = fs::remove_dir_all(&self.0);
    }
}

fn library_filename() -> &'static str {
    if cfg!(target_os = "macos") {
        "libdevondb_c.dylib"
    } else if cfg!(target_os = "windows") {
        "devondb_c.dll"
    } else {
        "libdevondb_c.so"
    }
}

fn find_library(executable: &Path) -> Option<PathBuf> {
    executable
        .ancestors()
        .map(|directory| directory.join(library_filename()))
        .find(|candidate| candidate.is_file())
}

fn command_output(command: &mut Command, context: &str) -> Output {
    command
        .output()
        .unwrap_or_else(|error| panic!("{context}: {error}"))
}

fn build_and_find_library() -> PathBuf {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
    let mut build = Command::new(env::var_os("CARGO").unwrap_or_else(|| "cargo".into()));
    build.arg("build").arg("--manifest-path").arg(manifest);
    if !cfg!(debug_assertions) {
        build.arg("--release");
    }
    let output = command_output(&mut build, "build devondb C library");
    assert!(
        output.status.success(),
        "building the C library failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let executable = env::current_exe().expect("locate the C smoke test executable");
    find_library(&executable).unwrap_or_else(|| {
        panic!(
            "could not find {} above {}",
            library_filename(),
            executable.display()
        )
    })
}

#[test]
fn c_nonfinite_null_round_trip() {
    match Command::new("cc").arg("--version").output() {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            println!("skipping C non-finite test: cc is not on PATH");
            return;
        }
        Err(error) => panic!("failed to probe cc: {error}"),
        Ok(output) => assert!(output.status.success(), "cc --version failed"),
    }
    let library = build_and_find_library();
    let library_dir = library.parent().expect("devondb C library has a parent");
    let test_dir = TestDir::new();
    let database = test_dir.path().join("nonfinite.devondb");
    let source = test_dir.path().join("nonfinite.c");
    let binary = test_dir.path().join("nonfinite");
    let program = r#"
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include "devondb.h"

int main(int argc, char **argv) {
    devondb_options options = {0};
    devondb_database *db = NULL;
    char *json = NULL;
    if (argc != 2) { fprintf(stderr, "missing path\n"); return 1; }
    options.size_bytes = sizeof(options);
    if (devondb_create_with(argv[1], &options, &db)) { perror("create_with"); fprintf(stderr, "last=%s\n", devondb_last_error(NULL)); return 1; }
    if (devondb_execute(db, "create node table T (id Int64 primary key, v Float64)")) return 2;
    if (devondb_execute(db, "insert into T values (1, 1.5), (2, null), (3, 0.0), (4, 2.0), (5, -3.0)")) { fprintf(stderr, "insert failed: %s\n", devondb_last_error(db)); return 3; }
    if (devondb_query_json(db, "nodes(T) as t | sort t.id | project t.v as v, t.v / 0.0 as over_zero", &json)) return 4;
    printf("%s\n", json);
    devondb_string_free(json);
    devondb_close(db);
    return 0;
}
"#;
    fs::write(&source, program).expect("write non-finite C program");
    let include_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("include");
    let compile = command_output(
        Command::new("cc")
            .arg("-std=c99")
            .arg(&source)
            .arg("-I")
            .arg(&include_dir)
            .arg("-L")
            .arg(library_dir)
            .arg("-ldevondb_c")
            .arg(format!("-Wl,-rpath,{}", library_dir.display()))
            .arg("-o")
            .arg(&binary),
        "compile non-finite C program",
    );
    assert!(
        compile.status.success(),
        "C compilation failed:\n{}",
        String::from_utf8_lossy(&compile.stderr)
    );
    let run = command_output(
        Command::new(&binary).arg(&database),
        "run non-finite C program",
    );
    assert!(
        run.status.success(),
        "non-finite C program failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&run.stdout),
        String::from_utf8_lossy(&run.stderr)
    );
    let stdout = String::from_utf8(run.stdout).expect("C stdout is UTF-8");
    assert_eq!(
        stdout.trim(),
        r#"{"columns":["v","over_zero"],"rows":[[1.5,{"f64":"inf"}],[null,null],[0.0,{"f64":"NaN"}],[2.0,{"f64":"inf"}],[-3.0,{"f64":"-inf"}]]}"#
    );
}

#[test]
fn c_memory_limit_enforced() {
    match Command::new("cc").arg("--version").output() {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            println!("skipping C memory-limit test: cc is not on PATH");
            return;
        }
        Err(error) => panic!("failed to probe cc: {error}"),
        Ok(output) => assert!(output.status.success(), "cc --version failed"),
    }
    let library = build_and_find_library();
    let library_dir = library.parent().expect("devondb C library has a parent");
    let test_dir = TestDir::new();
    let database = test_dir.path().join("budget.devondb");
    let source = test_dir.path().join("budget.c");
    let binary = test_dir.path().join("budget");
    let program = r#"
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include "devondb.h"

int main(int argc, char **argv) {
    (void)argc;
    devondb_options options = {0};
    options.size_bytes = sizeof(options);
    options.memory_limit = 1024 * 1024;
    devondb_database *db = NULL;
    if (!argv[1]) return 2;
    if (devondb_create_with(argv[1], &options, &db) || db == NULL) return 3;
    if (devondb_execute(db, "create node table Big (id Int64 primary key, payload String)")) return 4;
    /* One row whose payload alone (3 MiB) cannot fit the 1 MiB budget. */
    size_t n = 3 * 1024 * 1024;
    char *statement = malloc(n + 128);
    if (!statement) return 5;
    strcpy(statement, "insert into Big values (1, \"");
    memset(statement + strlen(statement), 'a', n);
    statement[strlen(statement)] = '\0';
    strcat(statement, "\")");
    devondb_status inserted = devondb_execute(db, statement);
    free(statement);
    if (inserted == DEVONDB_ERR) {
        const char *error = devondb_last_error(db);
        if (!error || !strstr(error, "memory budget exceeded")) return 6;
        printf("budget_error=%s\n", error);
        devondb_close(db);
        return 0;
    }
    devondb_close(db);
    return 7;
}
"#;
    fs::write(&source, program).expect("write budget C program");
    let include_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("include");
    let compile = command_output(
        Command::new("cc")
            .arg("-std=c99")
            .arg(&source)
            .arg("-I")
            .arg(&include_dir)
            .arg("-L")
            .arg(library_dir)
            .arg("-ldevondb_c")
            .arg(format!("-Wl,-rpath,{}", library_dir.display()))
            .arg("-o")
            .arg(&binary),
        "compile budget C program",
    );
    assert!(
        compile.status.success(),
        "C compilation failed:\n{}",
        String::from_utf8_lossy(&compile.stderr)
    );
    let run = command_output(Command::new(&binary).arg(&database), "run budget C program");
    assert!(
        run.status.success(),
        "budget C program failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&run.stdout),
        String::from_utf8_lossy(&run.stderr)
    );
    let stdout = String::from_utf8(run.stdout).expect("C stdout is UTF-8");
    assert!(stdout.contains("memory budget exceeded"), "{stdout}");
}

#[test]
fn c_concurrent_handle_use_is_serialized() {
    match Command::new("cc").arg("--version").output() {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            println!("skipping C concurrency test: cc is not on PATH");
            return;
        }
        Err(error) => panic!("failed to probe cc: {error}"),
        Ok(output) => assert!(output.status.success(), "cc --version failed"),
    }
    let library = build_and_find_library();
    let library_dir = library.parent().expect("devondb C library has a parent");
    let test_dir = TestDir::new();
    let database = test_dir.path().join("threads.devondb");
    let source = test_dir.path().join("threads.c");
    let binary = test_dir.path().join("threads");
    let program = r#"
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include "devondb.h"

static devondb_database *shared_db;

/* last_error is shared handle state: the other thread's call may clear or
 * replace it between this thread's execute and last_error. What must never
 * happen is a torn string, so any non-empty error observed here must be the
 * exact message the failing statement produces. */
static void *worker(void *arg) {
    long id = (long)arg;
    const char *expected = "expected a statement, found a query";
    for (int i = 0; i < 200; ++i) {
        if ((id & 1) == 0) {
            if (devondb_execute(shared_db, "nodes(Person) as p") != DEVONDB_ERR) return (void *)1L;
        } else {
            if (devondb_execute(shared_db, "upsert Setting values (1, \"value\")") != DEVONDB_OK) return (void *)2L;
        }
        const char *error = devondb_last_error(shared_db);
        if (error[0] != '\0' && strcmp(error, expected) != 0) return (void *)3L;
    }
    return NULL;
}

int main(int argc, char **argv) {
    (void)argc;
    pthread_t threads[2];
    if (!argv[1]) return 2;
    if (devondb_create(argv[1], UINT32_C(4096), &shared_db)) return 3;
    if (devondb_execute(shared_db, "create node table Person (id Int64 primary key, name String)")) return 4;
    if (devondb_execute(shared_db, "create node table Setting (id Int64 primary key, value String)")) return 5;
    pthread_create(&threads[0], NULL, worker, (void *)0L);
    pthread_create(&threads[1], NULL, worker, (void *)1L);
    void *result_a = NULL;
    void *result_b = NULL;
    pthread_join(threads[0], &result_a);
    pthread_join(threads[1], &result_b);
    devondb_close(shared_db);
    return result_a || result_b ? 6 : 0;
}
"#;
    fs::write(&source, program).expect("write concurrency C program");
    let include_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("include");
    let compile = command_output(
        Command::new("cc")
            .arg("-std=c99")
            .arg(&source)
            .arg("-I")
            .arg(&include_dir)
            .arg("-L")
            .arg(library_dir)
            .arg("-ldevondb_c")
            .arg(format!("-Wl,-rpath,{}", library_dir.display()))
            .arg("-o")
            .arg(&binary),
        "compile concurrency C program",
    );
    assert!(
        compile.status.success(),
        "C compilation failed:\n{}",
        String::from_utf8_lossy(&compile.stderr)
    );
    let run = command_output(
        Command::new(&binary).arg(&database),
        "run concurrency C program",
    );
    assert!(
        run.status.success(),
        "concurrency C program failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&run.stdout),
        String::from_utf8_lossy(&run.stderr)
    );
}

#[test]
fn c_header_compiles_links_and_runs() {
    match Command::new("cc").arg("--version").output() {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            println!("skipping C smoke test: cc is not on PATH");
            return;
        }
        Err(error) => panic!("failed to probe cc: {error}"),
        Ok(output) => assert!(output.status.success(), "cc --version failed"),
    }

    let library = build_and_find_library();
    let library_dir = library.parent().expect("devondb C library has a parent");
    let test_dir = TestDir::new();
    let source = test_dir.path().join("smoke.c");
    let binary = test_dir.path().join("smoke");
    let database = test_dir.path().join("smoke.devondb");
    fs::write(&source, C_PROGRAM).expect("write C smoke program");

    let include_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("include");
    let compile = command_output(
        Command::new("cc")
            .arg("-std=c99")
            .arg(&source)
            .arg("-I")
            .arg(&include_dir)
            .arg("-L")
            .arg(library_dir)
            .arg("-ldevondb_c")
            .arg(format!("-Wl,-rpath,{}", library_dir.display()))
            .arg("-o")
            .arg(&binary),
        "compile C smoke program",
    );
    assert!(
        compile.status.success(),
        "C compilation failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&compile.stdout),
        String::from_utf8_lossy(&compile.stderr)
    );

    let run = command_output(Command::new(&binary).arg(&database), "run C smoke program");
    assert!(
        run.status.success(),
        "C smoke program failed with {}:\nstdout:\n{}\nstderr:\n{}",
        run.status,
        String::from_utf8_lossy(&run.stdout),
        String::from_utf8_lossy(&run.stderr)
    );

    let query = "{\"columns\":[\"id\",\"name\",\"active\",\"score\",\"embedding\",\"nickname\",\"location\"],\"rows\":[[1,\"Ada\",true,1.5,[0.5,0.25],null,{\"geo\":{\"lat_deg\":45.5,\"lng_deg\":-122.625}}],[2,\"Grace\",false,2.25,[0.75,1.0],\"compiler\",{\"geo\":{\"lat_deg\":90.0,\"lng_deg\":0.0}}]]}";
    let nonfinite =
        "{\"columns\":[\"non_finite\"],\"rows\":[[{\"f64\":\"inf\"}],[{\"f64\":\"inf\"}]]}";
    // Every schema summary carries derived node classes.
    let schema = "{\"node_tables\":[{\"name\":\"Person\",\"columns\":[{\"name\":\"id\",\"type\":\"Int64\",\"primary_key\":true},{\"name\":\"name\",\"type\":\"String\",\"primary_key\":false},{\"name\":\"active\",\"type\":\"Bool\",\"primary_key\":false},{\"name\":\"score\",\"type\":\"Float64\",\"primary_key\":false},{\"name\":\"embedding\",\"type\":\"Vector(2)\",\"primary_key\":false},{\"name\":\"nickname\",\"type\":\"String\",\"primary_key\":false},{\"name\":\"location\",\"type\":\"GeoPoint\",\"primary_key\":false}]}],\"rel_tables\":[],\"classes\":{\"interfaces\":[],\"node_classes\":[{\"table\":\"Person\",\"display\":\"Person\",\"plural\":\"people\",\"label\":\"name\"}],\"rel_classes\":[]}}";
    let scalar = r#"{"columns":["happened","payload","amount","document"],"rows":[[{"ts":1723161600123456},{"bytes":"00ff1a"},{"decimal":"12345678901234567890123456789012.345678"},{"json":"{\"count\":3,\"flags\":[true,null],\"name\":\"Ada\"}"}]]}"#;
    let upsert = "{\"columns\":[\"id\",\"value\"],\"rows\":[[1,\"second\"]]}";
    let expected = format!(
        "create_status=0\nddl_status=0\ninsert_status=0\nquery_status=0\nquery={query}\nnonfinite_status=0\nnonfinite={nonfinite}\nerror_status=1\nerror_nonempty=1\nschema_status=0\nschema={schema}\nerror_cleared=1\nscalar_ddl_status=0\nscalar_insert_status=0\nscalar_query_status=0\nscalar={scalar}\nupsert_ddl_status=0\nupsert_first_status=0\nupsert_second_status=0\nupsert_query_status=0\nupsert={upsert}\nopen_status=0\nreopen_query_status=0\nreopen_query={query}\n"
    );
    let stdout = String::from_utf8(run.stdout).expect("C stdout is UTF-8");
    print!("{stdout}");
    assert_eq!(stdout, expected);
    assert!(run.stderr.is_empty(), "unexpected C stderr output");
}
