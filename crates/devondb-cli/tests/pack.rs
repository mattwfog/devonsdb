//! End-to-end coverage for `devondb pack` (`docs/SCALE.md` §7): the binary
//! packs a database, then `mcp` and the REPL open the container by magic.
//! Reads succeed and mutations hit the read-only fence.

use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use serde_json::Value;

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

const CANONICAL: &str = "nodes(Person) as person | filter person.name = \"ada\" | expand Knows out as other | project other.name";

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
            "devondb-cli-pack-{label}-{}-{timestamp}-{sequence}",
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

fn run_binary(arguments: &[&std::ffi::OsStr], input: &str) -> Output {
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

fn seed_graph(path: &Path) {
    let input = "create node table Person (id Int64 primary key, name String)\n\
create rel table Knows from Person to Person\n\
insert into Person values (1, \"ada\"), (2, \"Grace\"), (3, \"Linus\")\n\
insert rel into Knows values (1 -> 2), (1 -> 3)\n\
.exit\n";
    let output = run_binary(&[path.as_os_str()], input);
    assert!(output.status.success(), "seed failed: {output:?}");
    assert_eq!(output.stdout, b"ok\nok\nok\nok\n");
}

/// Packs `input` to `output`, asserting success.
fn pack(input: &Path, output: &Path, extra: &[&std::ffi::OsStr]) {
    let mut arguments: Vec<&std::ffi::OsStr> =
        vec!["pack".as_ref(), input.as_os_str(), output.as_os_str()];
    arguments.extend_from_slice(extra);
    let run = run_binary(&arguments, "");
    assert!(
        run.status.success(),
        "pack failed: {}",
        String::from_utf8_lossy(&run.stderr)
    );
}

#[test]
fn packed_file_answers_mcp_and_refuses_repl_writes() {
    let directory = TestDirectory::new("roundtrip");
    let source = directory.file("source.devondb");
    seed_graph(&source);
    let packed = directory.file("packed.devondb");
    pack(&source, &packed, &[]);

    let bytes = fs::read(&packed).unwrap();
    assert_eq!(&bytes[..9], b"DEVONPACK", "container magic at byte 0");

    // `devondb mcp <packed>` opens by magic and answers over stdio.
    let input = concat!(
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"test","version":"0"}}}"#,
        "\n",
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
        "\n",
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"ask","arguments":{"question":"who does ada know"}}}"#,
        "\n",
    );
    let output = run_binary(&["mcp".as_ref(), packed.as_os_str()], input);
    assert!(output.status.success(), "mcp failed: {output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    let replies: Vec<Value> = stdout
        .lines()
        .map(|line| serde_json::from_str(line).unwrap_or_else(|e| panic!("{e}: {line}")))
        .collect();
    assert_eq!(replies.len(), 2, "one reply per request: {stdout}");
    assert_eq!(replies[1]["result"]["isError"], false);
    assert_eq!(
        replies[1]["result"]["content"][0]["text"],
        format!("plan: {CANONICAL}\n\nother.name:String\nGrace\nLinus\n(2 rows)")
    );

    // The REPL opens the packed file by magic too: reads answer, and an
    // insert hits the read-only fence with its error text on stderr.
    let output = run_binary(
        &[packed.as_os_str()],
        "nodes(Person) as person\ninsert into Person values (4, \"Katherine\")\n.exit\n",
    );
    assert!(
        output.status.success(),
        "REPL over a pack must stay alive: {output:?}"
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.contains("person.id | person.name") && stdout.contains("(3 rows)"),
        "scan over the pack: {stdout}"
    );
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("error: read-only:") && stderr.contains("DEVONPACK"),
        "insert must hit the read-only fence: {stderr}"
    );
}

#[test]
fn pack_usage_and_missing_input_errors() {
    let directory = TestDirectory::new("errors");

    // Missing <out> positional: usage on stderr, exit code 2.
    let output = run_binary(&["pack".as_ref(), "only-one.devondb".as_ref()], "");
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("devondb pack <in> <out>"), "{stderr}");

    // A third positional is rejected the same way.
    let output = run_binary(
        &[
            "pack".as_ref(),
            "a.devondb".as_ref(),
            "b.devondb".as_ref(),
            "c.devondb".as_ref(),
        ],
        "",
    );
    assert_eq!(output.status.code(), Some(2));

    // A missing input database exits non-zero with the `error: ` prefix.
    let missing = directory.file("missing.devondb");
    let out = directory.file("out.devondb");
    let output = run_binary(&["pack".as_ref(), missing.as_os_str(), out.as_os_str()], "");
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.starts_with("error: "), "{stderr}");
    assert!(!out.exists(), "a failed pack must not leave a container");
}

#[test]
fn pack_frame_pages_flag_changes_the_directory() {
    let directory = TestDirectory::new("frames");
    let source = directory.file("source.devondb");
    seed_graph(&source);
    let packed = directory.file("packed.devondb");
    pack(&source, &packed, &["--frame-pages".as_ref(), "2".as_ref()]);

    let bytes = fs::read(&packed).unwrap();
    // Header: magic 9 · version 1 · codec 2 · page_size 4 · page_count 8 ·
    // frame_pages 4 — little-endian at offset 24 (docs/SCALE.md §7.1).
    assert_eq!(&bytes[24..28], &2_u32.to_le_bytes(), "frame_pages field");
    assert!(u64::from_le_bytes(bytes[28..36].try_into().unwrap()) > 1);
}
