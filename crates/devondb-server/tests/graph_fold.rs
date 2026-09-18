use std::env;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use devondb::Database;
use devondb_server::api::Api;
use devondb_server::http::{ApiHandler, Request};
use serde_json::{Value, json};

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
            "devondb-server-graph-fold-{label}-{timestamp}-{sequence}-{}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("create test directory");
        Self { path }
    }

    fn db_path(&self) -> PathBuf {
        self.path.join("db.devondb")
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

struct TestApi {
    api: Api,
    _directory: TestDirectory,
}

impl TestApi {
    fn new(label: &str) -> Self {
        let directory = TestDirectory::new(label);
        let database = Database::create(directory.db_path(), 4096).expect("create database");
        Self {
            api: Api::new(database),
            _directory: directory,
        }
    }

    fn post(&self, path: &str, body: Value) -> Value {
        let request = Request {
            method: "POST".to_owned(),
            path: path.to_owned(),
            body: body.to_string().into_bytes(),
        };
        let response = self.api.handle(&request);
        let parsed = serde_json::from_slice(&response.body).expect("response body is JSON");
        if response.status != 200 {
            panic!("request failed: {path}: {parsed}");
        }
        parsed
    }
}

fn statement(server: &TestApi, text: &str) {
    let response = server.post("/api/statement", json!({"text": text}));
    assert_eq!(response, json!({"ok": true}), "statement failed: {text}");
}

#[test]
fn graph_resolves_folded_relationship_endpoints_with_node_spelling() {
    let server = TestApi::new("folded-endpoints");
    statement(
        &server,
        "create node table Person (id Int64 primary key, name String)",
    );
    statement(
        &server,
        "create rel table Knows from person to person (since Int64)",
    );
    statement(
        &server,
        "insert into Person values (1, \"Ada\"), (2, \"Grace\")",
    );
    statement(&server, "insert rel into Knows values (1 -> 2, 0)");

    let response = server.post("/api/graph", json!({"table": "Person", "key": 1}));

    assert_eq!(
        response,
        json!({
            "nodes": [
                {"table": "Person", "key": 1, "props": {"name": "Ada"}},
                {"table": "Person", "key": 2, "props": {"name": "Grace"}}
            ],
            "edges": [{
                "rel": "Knows",
                "from": {"table": "Person", "key": 1},
                "to": {"table": "Person", "key": 2}
            }],
            "truncated": false
        })
    );
}
