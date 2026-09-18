//! `devondb mcp` end to end (`docs/MCP.md` §7): the real binary, MCP over
//! its stdio, rows asserted — the would-fail-if-severed test for the
//! Claude Desktop surface.

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
            "devondb-cli-mcp-{label}-{}-{timestamp}-{sequence}",
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

fn run_mcp(path: &Path, input: &str) -> Output {
    run_binary(&["mcp".as_ref(), path.as_os_str()], input)
}

#[test]
fn mcp_real_binary_answers_ask_over_stdio() {
    let directory = TestDirectory::new("ask");
    let path = directory.database();
    seed_graph(&path);

    let input = concat!(
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"test","version":"0"}}}"#,
        "\n",
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
        "\n",
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
        "\n",
        r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"ask","arguments":{"question":"who does ada know"}}}"#,
        "\n",
    );
    let output = run_mcp(&path, input);

    assert!(output.status.success(), "mcp failed: {output:?}");
    assert!(
        output.stderr.is_empty(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    let replies: Vec<Value> = stdout
        .lines()
        .map(|line| serde_json::from_str(line).unwrap_or_else(|e| panic!("{e}: {line}")))
        .collect();
    assert_eq!(
        replies.len(),
        3,
        "one reply per request, none for the notification: {stdout}"
    );
    assert_eq!(replies[0]["id"], 1);
    assert_eq!(replies[0]["result"]["serverInfo"]["name"], "devondb");
    assert_eq!(replies[1]["result"]["tools"].as_array().unwrap().len(), 4);
    assert_eq!(replies[2]["id"], 3);
    assert_eq!(replies[2]["result"]["isError"], false);
    assert_eq!(
        replies[2]["result"]["content"][0]["text"],
        format!("plan: {CANONICAL}\n\nother.name:String\nGrace\nLinus\n(2 rows)")
    );
}

#[test]
fn mcp_missing_database_fails_on_stderr() {
    let directory = TestDirectory::new("missing");
    let output = run_mcp(&directory.database(), "");
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.starts_with("error: "), "{stderr}");
}

#[test]
fn mcp_rejects_extra_positionals_with_usage() {
    let output = run_binary(&["mcp".as_ref(), "a".as_ref(), "b".as_ref()], "");
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("devondb mcp <path>"), "{stderr}");
}
