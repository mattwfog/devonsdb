use std::{
    env, fs,
    io::{BufRead, BufReader, Read, Write},
    net::{Shutdown, TcpStream},
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
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
        let path = env::temp_dir().join(format!(
            "devondb-cli-ui-{label}-{}-{timestamp}-{sequence}",
            std::process::id()
        ));
        fs::create_dir(&path).unwrap();
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

struct UiServer {
    child: Child,
    port: u16,
    announced_url: String,
    _directory: TestDirectory,
}

impl UiServer {
    fn new(label: &str) -> Self {
        let directory = TestDirectory::new(label);
        create_database(&directory.database());
        let child = Command::new(env!("CARGO_BIN_EXE_devondb"))
            .arg("ui")
            .arg(directory.database())
            .args(["--port", "0"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut server = Self {
            child,
            port: 0,
            announced_url: String::new(),
            _directory: directory,
        };
        (server.port, server.announced_url) = read_announcement(&mut server.child);
        server
    }

    fn with_opener(label: &str, opener: &Path, record: &Path) -> Self {
        let directory = TestDirectory::new(label);
        create_database(&directory.database());
        let child = Command::new(env!("CARGO_BIN_EXE_devondb"))
            .arg("ui")
            .arg(directory.database())
            .args(["--port", "0", "--open"])
            .env("DEVONDB_UI_OPENER", opener)
            .env("DEVONDB_UI_OPENER_RECORD", record)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut server = Self {
            child,
            port: 0,
            announced_url: String::new(),
            _directory: directory,
        };
        (server.port, server.announced_url) = read_announcement(&mut server.child);
        server
    }

    fn get(&self, path: &str) -> HttpResponse {
        self.request("GET", path, "")
    }

    fn post(&self, path: &str, body: &str) -> HttpResponse {
        self.request("POST", path, body)
    }

    fn request(&self, method: &str, path: &str, body: &str) -> HttpResponse {
        let request = format!(
            "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        exchange(self.port, request.as_bytes())
    }
}

impl Drop for UiServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// `ui` opens an existing database only, so the server fixtures create the
/// file first through the REPL's create path.
fn create_database(path: &Path) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_devondb"))
        .arg(path)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(b".exit\n").unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "seed failed: {output:?}");
}

#[derive(Debug)]
struct HttpResponse {
    status: u16,
    body: String,
}

#[test]
fn root_serves_the_embedded_spa_shell() {
    let server = UiServer::new("spa");

    let response = server.get("/");

    assert_eq!(response.status, 200, "unexpected response: {response:?}");
    assert!(response.body.contains(r#"id="app""#));
}

#[test]
fn trust_loop_executes_statements_explains_and_queries_exact_rows() {
    let server = UiServer::new("trust-loop");
    seed_graph(&server);
    let sugared = "nodes(Person) as p | filter p.age >= 36 | project p.name as name, p.age as age | sort p.age asc";

    let explained = server.post("/api/explain", &text_request(sugared));
    assert_eq!(explained.status, 200, "explain failed: {explained:?}");
    let canonical = "nodes(Person) as p | filter p.age >= 36 | project p.name as name, p.age as age | sort p.age";
    assert!(
        explained
            .body
            .contains(&format!(r#""canonical":"{canonical}""#)),
        "unexpected explain response: {explained:?}"
    );

    let queried = server.post("/api/query", &text_request(canonical));
    assert_eq!(queried.status, 200, "query failed: {queried:?}");
    assert_eq!(
        queried.body,
        r#"{"columns":["name","age"],"row_count":2,"rows":[["Ada",36],["Grace",50]],"truncated":false}"#
    );
}

#[test]
fn grid_dml_lands_through_the_statement_route_and_requeries() {
    // The §12.6 severed-proof: an update composed exactly as the grid
    // composes it, landed over real HTTP, visible to the next query.
    let server = UiServer::new("grid-dml");
    seed_graph(&server);

    let update = r#"update Person set age = 37 where id = 1"#;
    let explained = server.post("/api/explain", &text_request(update));
    assert_eq!(explained.status, 200, "explain failed: {explained:?}");
    assert!(
        explained
            .body
            .contains(r#""canonical":"update Person set age = 37 where id = 1""#),
        "unexpected explain response: {explained:?}"
    );

    statement(&server, update);

    let queried = server.post(
        "/api/query",
        &text_request(
            "nodes(Person) as p | filter p.id = 1 | project p.name as name, p.age as age",
        ),
    );
    assert_eq!(queried.status, 200, "query failed: {queried:?}");
    assert_eq!(
        queried.body,
        r#"{"columns":["name","age"],"row_count":1,"rows":[["Ada",37]],"truncated":false}"#
    );

    let geo_update = r#"update Person set location = geo(42.3601, -71.0589) where id = 1"#;
    let geo_explained = server.post("/api/explain", &text_request(geo_update));
    assert_eq!(
        geo_explained.status, 200,
        "explain failed: {geo_explained:?}"
    );
    assert!(
        geo_explained.body.contains(
            r#""canonical":"update Person set location = geo(42.3601, -71.0589) where id = 1""#
        ),
        "unexpected explain response: {geo_explained:?}"
    );

    statement(&server, geo_update);

    let geo_queried = server.post(
        "/api/query",
        &text_request("nodes(Person) as p | filter p.id = 1 | project p.location as location"),
    );
    assert_eq!(geo_queried.status, 200, "query failed: {geo_queried:?}");
    assert_eq!(
        geo_queried.body,
        r#"{"columns":["location"],"row_count":1,"rows":[[{"geo":{"lat_deg":42.3601,"lng_deg":-71.0589}}]],"truncated":false}"#
    );
}

#[test]
fn graph_returns_the_exact_bidirectional_neighborhood() {
    let server = UiServer::new("graph");
    seed_graph(&server);

    let response = server.post("/api/graph", r#"{"table":"Person","key":1}"#);

    assert_eq!(response.status, 200, "graph failed: {response:?}");
    assert_eq!(
        response.body,
        r#"{"edges":[{"from":{"key":1,"table":"Person"},"rel":"Knows","to":{"key":2,"table":"Person"}},{"from":{"key":3,"table":"Person"},"rel":"Knows","to":{"key":1,"table":"Person"}}],"nodes":[{"key":1,"props":{"age":36,"location":{"geo":{"lat_deg":40.7128,"lng_deg":-74.006}},"name":"Ada"},"table":"Person"},{"key":2,"props":{"age":50,"location":{"geo":{"lat_deg":41.8781,"lng_deg":-87.6298}},"name":"Grace"},"table":"Person"},{"key":3,"props":{"age":30,"location":{"geo":{"lat_deg":45.5152,"lng_deg":-122.6784}},"name":"Linus"},"table":"Person"}],"truncated":false}"#
    );
}

#[test]
fn opening_a_directory_reports_one_clean_error_and_fails() {
    let directory = TestDirectory::new("open-error");

    let output = run_ui_to_completion(&directory.path);

    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.starts_with("error: "), "unexpected stderr: {stderr}");
    assert!(stderr.ends_with('\n'), "unexpected stderr: {stderr}");
    assert_eq!(stderr.lines().count(), 1, "unexpected stderr: {stderr}");
}

/// `ui` opens an existing database only: a typo path errors and creates
/// nothing.
#[test]
fn missing_database_errors_without_creating_it() {
    let directory = TestDirectory::new("missing-ui");
    let path = directory.database();

    let output = run_ui_to_completion(&path);

    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains(&format!("database file not found: {}", path.display())),
    );
    assert!(!path.exists());
}

#[cfg(unix)]
#[test]
fn open_uses_the_configured_opener_with_the_announced_url() {
    use std::os::unix::fs::PermissionsExt;

    let opener_directory = TestDirectory::new("opener");
    let opener = opener_directory.path.join("record-opener");
    let record = opener_directory.path.join("url");
    fs::write(
        &opener,
        "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$DEVONDB_UI_OPENER_RECORD\"\n",
    )
    .unwrap();
    let mut permissions = fs::metadata(&opener).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&opener, permissions).unwrap();

    let server = UiServer::with_opener("open", &opener, &record);

    assert_eq!(read_record(&record), server.announced_url);
}

#[test]
fn missing_opener_does_not_stop_the_server() {
    let opener_directory = TestDirectory::new("missing-opener");
    let opener = opener_directory.path.join("does-not-exist");
    let record = opener_directory.path.join("unused");
    let server = UiServer::with_opener("missing-open", &opener, &record);

    let response = server.get("/");

    assert_eq!(response.status, 200, "unexpected response: {response:?}");
}

fn read_announcement(child: &mut Child) -> (u16, String) {
    let stdout = child.stdout.take().unwrap();
    let mut stdout = BufReader::new(stdout);
    let mut line = String::new();
    if stdout.read_line(&mut line).unwrap() == 0 {
        let mut stderr = String::new();
        child
            .stderr
            .take()
            .unwrap()
            .read_to_string(&mut stderr)
            .unwrap();
        let status = child.wait().unwrap();
        panic!("UI exited before announcing its URL ({status}): {stderr}");
    }
    let url = line
        .strip_prefix("devondb ui: ")
        .and_then(|url| url.strip_suffix('\n'))
        .unwrap_or_else(|| panic!("unexpected UI announcement: {line:?}"));
    let port = url
        .strip_prefix("http://127.0.0.1:")
        .and_then(|port| port.strip_suffix('/'))
        .and_then(|port| port.parse().ok())
        .unwrap_or_else(|| panic!("unexpected UI announcement: {line:?}"));
    (port, url.to_owned())
}

#[cfg(unix)]
fn read_record(path: &Path) -> String {
    for _ in 0..100 {
        match fs::read_to_string(path) {
            Ok(record) => return record.trim_end().to_owned(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(error) => panic!("failed to read opener record: {error}"),
        }
    }
    panic!("opener did not record its arguments")
}

fn exchange(port: u16, request: &[u8]) -> HttpResponse {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream.write_all(request).unwrap();
    stream.shutdown(Shutdown::Write).unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    let (headers, body) = response.split_once("\r\n\r\n").unwrap();
    let status = headers.split_whitespace().nth(1).unwrap().parse().unwrap();
    HttpResponse {
        status,
        body: body.to_owned(),
    }
}

fn seed_graph(server: &UiServer) {
    statement(
        server,
        "create node table Person (id Int64 primary key, name String, age Int64, location GeoPoint)",
    );
    statement(server, "create rel table Knows from Person to Person");
    statement(
        server,
        "insert into Person values \
         (1, \"Ada\", 36, geo(40.7128, -74.006)), \
         (2, \"Grace\", 50, geo(41.8781, -87.6298)), \
         (3, \"Linus\", 30, geo(45.5152, -122.6784))",
    );
    statement(server, "insert rel into Knows values (1 -> 2), (3 -> 1)");
}

fn statement(server: &UiServer, text: &str) {
    let response = server.post("/api/statement", &text_request(text));
    assert_eq!(response.status, 200, "statement failed: {response:?}");
    assert_eq!(response.body, r#"{"ok":true}"#);
}

fn text_request(text: &str) -> String {
    let escaped = text.replace('\\', "\\\\").replace('"', "\\\"");
    format!(r#"{{"text":"{escaped}"}}"#)
}

fn run_ui_to_completion(path: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_devondb"))
        .arg("ui")
        .arg(path)
        .args(["--port", "0"])
        .output()
        .unwrap()
}
