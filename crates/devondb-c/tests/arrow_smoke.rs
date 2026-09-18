//! Smoke tests for Arrow C Data Interface export: `devondb_query_arrow` is
//! exercised end-to-end through a compiled C program against
//! `include/devondb.h` (the same pattern as `c_smoke.rs`). The Rust
//! struct-walk suite moved to the builder's home
//! (`devondb-types/tests/arrow_export.rs`) when the exporter was unified in
//! `devondb-types::arrow`; the buffer bytes and format strings this C
//! program asserts are unchanged by that move.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

struct TestDir(PathBuf);

impl TestDir {
    fn new(tag: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock is before Unix epoch")
            .as_nanos();
        let path = env::temp_dir().join(format!(
            "devondb-arrow-{tag}-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir(&path).expect("create arrow smoke test temp directory");
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
    struct ArrowSchema schema;
    struct ArrowArray array;
    int64_t *ids;
    int32_t *offsets;
    const char *names;

    CHECK(argc == 2, 2);

    status = devondb_create(argv[1], UINT32_C(4096), &db);
    CHECK(status == DEVONDB_OK && db != NULL, 3);

    status = devondb_execute(
        db,
        "create node table Person (id Int64 primary key, name String, "
        "amount Decimal(10, 2))"
    );
    CHECK(status == DEVONDB_OK, 4);

    status = devondb_execute(
        db,
        "insert into Person values (1, \"Ada\", decimal(\"12345678.90\")), "
        "(2, \"Grace\", null)"
    );
    CHECK(status == DEVONDB_OK, 5);

    status = devondb_query_arrow(
        db,
        "nodes(Person) as p | sort p.id | project p.id as id, p.name as name, "
        "p.amount as amount",
        &schema,
        &array
    );
    printf("arrow_status=%d\n", (int)status);
    CHECK(status == DEVONDB_OK, 6);
    CHECK(schema.release != NULL && array.release != NULL, 7);
    CHECK(strcmp(schema.format, "+s") == 0 && schema.n_children == 3, 8);
    CHECK(array.length == 2 && array.n_children == 3, 9);
    CHECK(strcmp(schema.children[0]->format, "l") == 0, 10);
    CHECK(strcmp(schema.children[1]->format, "u") == 0, 11);
    CHECK(strcmp(schema.children[2]->format, "d:10,2") == 0, 12);
    CHECK(strcmp(schema.children[1]->name, "name") == 0, 13);

    ids = (int64_t *)array.children[0]->buffers[1];
    CHECK(ids[0] == 1 && ids[1] == 2, 14);
    offsets = (int32_t *)array.children[1]->buffers[1];
    CHECK(offsets[0] == 0 && offsets[1] == 3 && offsets[2] == 8, 15);
    names = (const char *)array.children[1]->buffers[2];
    CHECK(memcmp(names, "AdaGrace", 8) == 0, 16);
    CHECK(array.children[2]->null_count == 1, 17);

    schema.release(&schema);
    array.release(&array);
    CHECK(schema.release == NULL && array.release == NULL, 18);
    printf("release_ok=1\n");

    status = devondb_query_arrow(db, "insert into Person values (3, \"x\", null)",
                                 &schema, &array);
    printf("error_status=%d\n", (int)status);
    CHECK(status == DEVONDB_ERR, 19);
    CHECK(schema.release == NULL && array.release == NULL, 20);
    CHECK(devondb_last_error(db)[0] != '\0', 21);
    printf("error_nonempty=1\n");

    devondb_close(db);
    return 0;
}
"#;

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

    let executable = env::current_exe().expect("locate the arrow smoke test executable");
    find_library(&executable).unwrap_or_else(|| {
        panic!(
            "could not find {} above {}",
            library_filename(),
            executable.display()
        )
    })
}

#[test]
fn c_header_arrow_export_runs() {
    match Command::new("cc").arg("--version").output() {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            println!("skipping C arrow smoke test: cc is not on PATH");
            return;
        }
        Err(error) => panic!("failed to probe cc: {error}"),
        Ok(output) => assert!(output.status.success(), "cc --version failed"),
    }

    let library = build_and_find_library();
    let library_dir = library.parent().expect("devondb C library has a parent");
    let test_dir = TestDir::new("ffi");
    let source = test_dir.path().join("arrow.c");
    let binary = test_dir.path().join("arrow");
    let database = test_dir.path().join("arrow.devondb");
    fs::write(&source, C_PROGRAM).expect("write C arrow smoke program");

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
        "compile C arrow smoke program",
    );
    assert!(
        compile.status.success(),
        "C compilation failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&compile.stdout),
        String::from_utf8_lossy(&compile.stderr)
    );

    let run = command_output(
        Command::new(&binary).arg(&database),
        "run C arrow smoke program",
    );
    assert!(
        run.status.success(),
        "C arrow smoke program failed with {}:\nstdout:\n{}\nstderr:\n{}",
        run.status,
        String::from_utf8_lossy(&run.stdout),
        String::from_utf8_lossy(&run.stderr)
    );
    let stdout = String::from_utf8(run.stdout).expect("C stdout is UTF-8");
    print!("{stdout}");
    assert_eq!(
        stdout,
        "arrow_status=0\nrelease_ok=1\nerror_status=1\nerror_nonempty=1\n"
    );
    assert!(run.stderr.is_empty(), "unexpected C stderr output");
}
