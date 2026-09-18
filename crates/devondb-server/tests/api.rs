use std::env;
use std::fs;
use std::io::{Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use devondb::{Database, Plan};
use devondb_server::api::Api;
use devondb_server::http::{ApiHandler, Request};
use serde_json::{Value, json};

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

const DAY_SECONDS: i64 = 86_400;

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
            "devondb-server-api-{label}-{timestamp}-{sequence}-{}",
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

struct TestServer {
    address: SocketAddr,
    _directory: TestDirectory,
}

impl TestServer {
    fn new(label: &str) -> Self {
        let directory = TestDirectory::new(label);
        let database = Database::create(directory.db_path(), 4096).expect("create database");
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let address = listener.local_addr().expect("read listener address");
        thread::spawn(move || {
            devondb_server::api::serve(database, listener).expect("serve API");
        });
        Self {
            address,
            _directory: directory,
        }
    }

    fn get(&self, path: &str) -> HttpResponse {
        self.request("GET", path, None)
    }

    fn post(&self, path: &str, body: Value) -> HttpResponse {
        self.request("POST", path, Some(body.to_string().as_bytes()))
    }

    fn post_raw(&self, path: &str, body: &[u8]) -> HttpResponse {
        self.request("POST", path, Some(body))
    }

    fn request(&self, method: &str, path: &str, body: Option<&[u8]>) -> HttpResponse {
        let body = body.unwrap_or_default();
        let head = format!(
            "{method} {path} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
            body.len()
        );
        let mut bytes = head.into_bytes();
        bytes.extend_from_slice(body);
        exchange(self.address, &bytes)
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

    fn get(&self, path: &str) -> HttpResponse {
        self.request("GET", path, None)
    }

    fn post(&self, path: &str, body: Value) -> HttpResponse {
        self.request("POST", path, Some(body))
    }

    fn request(&self, method: &str, path: &str, body: Option<Value>) -> HttpResponse {
        let request = Request {
            method: method.to_owned(),
            path: path.to_owned(),
            body: body.map_or_else(Vec::new, |value| value.to_string().into_bytes()),
        };
        let response = self.api.handle(&request);
        HttpResponse {
            status: response.status,
            body: serde_json::from_slice(&response.body).expect("response body is JSON"),
        }
    }
}

struct HttpResponse {
    status: u16,
    body: Value,
}

fn exchange(address: SocketAddr, request: &[u8]) -> HttpResponse {
    let mut stream = TcpStream::connect(address).expect("connect to test server");
    stream.write_all(request).expect("write request");
    stream.shutdown(Shutdown::Write).expect("finish request");

    let mut response = String::new();
    stream.read_to_string(&mut response).expect("read response");
    let (headers, body) = response
        .split_once("\r\n\r\n")
        .expect("response has header terminator");
    let status = headers
        .split_whitespace()
        .nth(1)
        .expect("response has status")
        .parse()
        .expect("status is numeric");
    HttpResponse {
        status,
        body: serde_json::from_str(body).expect("response body is JSON"),
    }
}

fn statement(server: &TestServer, text: &str) {
    let response = server.post("/api/statement", json!({"text": text}));
    assert_eq!(response.status, 200, "statement failed: {}", response.body);
    assert_eq!(response.body, json!({"ok": true}));
}

fn seed_people(server: &TestServer) {
    statement(
        server,
        "create node table Person (id Int64 primary key, name String, age Int64)",
    );
    statement(
        server,
        "insert into Person values (1, \"Ada\", 36), (2, \"Grace\", 50), (3, \"Linus\", 30)",
    );
}

fn api_statement(api: &TestApi, text: &str) {
    let response = api.post("/api/statement", json!({"text": text}));
    assert_eq!(response.status, 200, "statement failed: {}", response.body);
    assert_eq!(response.body, json!({"ok": true}));
}

fn seed_people_api(api: &TestApi) {
    api_statement(
        api,
        "create node table Person (id Int64 primary key, name String, age Int64)",
    );
    api_statement(
        api,
        "insert into Person values (1, \"Ada\", 36), (2, \"Grace\", 50), (3, \"Linus\", 30)",
    );
}

#[test]
fn schema_reflects_created_tables_with_exact_json() {
    let server = TestServer::new("schema");
    statement(
        &server,
        "create node table Person (id Int64 primary key, name String)",
    );
    statement(
        &server,
        "create rel table Knows from Person to Person (since Int64)",
    );

    let response = server.get("/api/schema");

    assert_eq!(response.status, 200);
    // ONTOLOGY.md:39-42: plain schemas expose their synthesized node classes.
    assert_eq!(
        response.body,
        json!({
            "node_tables": [{
                "name": "Person",
                "columns": [
                    {"name": "id", "type": "Int64", "primary_key": true},
                    {"name": "name", "type": "String", "primary_key": false}
                ]
            }],
            "rel_tables": [{
                "name": "Knows",
                "from": "Person",
                "to": "Person",
                "columns": [
                    {"name": "since", "type": "Int64", "primary_key": false}
                ]
            }],
            "classes": {
                "interfaces": [],
                "node_classes": [{
                    "table": "Person",
                    "display": "Person",
                    "plural": "people",
                    "label": "name"
                }],
                "rel_classes": []
            }
        })
    );
}

#[test]
fn schema_pins_have_exact_natural_shape() {
    let api = TestApi::new("schema-pins");
    seed_people_api(&api);
    let text = "people under 40";
    let canonical = "nodes(Person) as p | filter p.age < 40 | project p.id as id, p.name as name";
    let explained = api.post("/api/explain", json!({"text": canonical}));
    assert_eq!(explained.status, 200, "explain failed: {}", explained.body);

    let pinned = api.post(
        "/api/pin",
        json!({
            "name": "young people",
            "plan": explained.body["plan"].clone(),
            "text": text,
        }),
    );
    assert_eq!(pinned.status, 200, "pin failed: {}", pinned.body);
    assert_eq!(pinned.body, json!({"ok": true}));

    assert_eq!(
        api.get("/api/schema").body["pins"],
        json!([{
            "name": "young people",
            "text": text,
            "canonical": canonical,
        }])
    );
}

#[test]
fn non_finite_float64_and_null_are_distinct_including_vector_elements() {
    // Value ingress rejects non-finite floats, so the only way
    // they reach the API is query-time arithmetic — float division by zero
    // yields ±inf and 0.0/0.0 yields NaN (the same pattern the C FFI's
    // `c_nonfinite_null_round_trip` smoke test uses).
    let api = TestApi::new("non-finite-floats");
    api_statement(
        &api,
        "create node table Metric (id Int64 primary key, score Float64)",
    );
    api_statement(
        &api,
        "insert into Metric values (1, 1.5), (2, null), (3, 0.0), (4, -3.0)",
    );

    let query = api.post(
        "/api/query",
        json!({"text": "nodes(Metric) as m | sort m.id | project m.score as score, m.score / 0.0 as over_zero"}),
    );
    assert_eq!(query.status, 200, "query failed: {}", query.body);
    assert_eq!(
        query.body,
        json!({
            "columns": ["score", "over_zero"],
            "rows": [
                [1.5, {"f64": "inf"}],
                [null, null],
                [0.0, {"f64": "NaN"}],
                [-3.0, {"f64": "-inf"}]
            ],
            "row_count": 4,
            "truncated": false
        })
    );
}

#[test]
fn geo_schema_and_within_query_return_exact_map_overlay_rows() {
    let server = TestServer::new("map-within");
    statement(
        &server,
        "create node table Place (id Int64 primary key, name String, location GeoPoint)",
    );
    statement(
        &server,
        "insert into Place values \
         (1, \"Burnside\", geo(45.5231, -122.6765)), \
         (2, \"Seattle\", geo(47.6062, -122.3321)), \
         (3, \"Unknown\", null)",
    );

    let schema = server.get("/api/schema");
    assert_eq!(schema.status, 200);
    assert_eq!(
        schema.body,
        json!({
            "node_tables": [{
                "name": "Place",
                "columns": [
                    {"name": "id", "type": "Int64", "primary_key": true},
                    {"name": "name", "type": "String", "primary_key": false},
                    {"name": "location", "type": "GeoPoint", "primary_key": false}
                ]
            }],
            "rel_tables": [],
            "classes": {
                "interfaces": [],
                "node_classes": [{
                    "table": "Place",
                    "display": "Place",
                    "plural": "places",
                    "label": "name"
                }],
                "rel_classes": []
            }
        })
    );

    let text = "within(Place.location, geo(45.5152, -122.6784), 3218.688)";
    let explained = server.post("/api/explain", json!({"text": text}));
    assert_eq!(explained.status, 200);
    assert_eq!(explained.body["canonical"], text);
    assert_eq!(explained.body["plan"]["plan"]["op"], "WithinScan");

    let result = server.post(
        "/api/query",
        json!({"plan": explained.body["plan"].clone()}),
    );
    assert_eq!(result.status, 200, "within query failed: {}", result.body);
    assert_eq!(
        result.body,
        json!({
            "columns": ["Place.id", "Place.name", "Place.location"],
            "rows": [[
                1,
                "Burnside",
                {"geo": {"lat_deg": 45.5231, "lng_deg": -122.6765}}
            ]],
            "row_count": 1,
            "truncated": false
        })
    );
}

#[test]
fn explain_is_canonical_and_never_executes_statements() {
    let server = TestServer::new("explain");
    let query = server.post(
        "/api/explain",
        json!({"text": "nodes(Person) as p | sort p.id asc"}),
    );

    assert_eq!(query.status, 200);
    assert_eq!(query.body["kind"], "query");
    assert_eq!(query.body["canonical"], "nodes(Person) as p | sort p.id");
    assert_eq!(
        query.body["plan"],
        json!({
            "v": 0,
            "plan": {
                "op": "Sort",
                "keys": [{"expr": {"col": "p.id"}, "order": "asc"}],
                "input": {"op": "ScanNodes", "table": "Person", "binding": "p"}
            }
        })
    );

    let statement = server.post(
        "/api/explain",
        json!({"text": "create node table Ghost (id Int64 primary key)"}),
    );
    assert_eq!(statement.status, 200);
    assert_eq!(statement.body["kind"], "statement");
    assert_eq!(
        server.get("/api/schema").body,
        json!({
            "node_tables": [],
            "rel_tables": []
        })
    );
}

#[test]
fn ask_success_matches_the_exact_explain_query_shape() {
    let api = TestApi::new("ask-success");
    api_statement(
        &api,
        "create node table Person (id Int64 primary key, name String)",
    );
    api_statement(&api, "create rel table Knows from Person to Person");
    // The value must exist: `/api/ask` resolves a value slot against committed
    // rows and refuses when nothing matches (`docs/NL.md` § entity value
    // slots). It never emits a filter known at compile time to match nothing.
    api_statement(&api, "insert into Person values (1, \"ada\")");
    let canonical = "nodes(Person) as person | filter person.name = \"ada\" | expand Knows out as other | project other.name";

    let response = api.post("/api/ask", json!({"text": "who does ada know"}));
    let explained = api.post("/api/explain", json!({"text": canonical}));

    assert_eq!(response.status, 200);
    assert_eq!(response.body, explained.body);
    assert_eq!(
        response.body,
        json!({
            "kind": "query",
            "canonical": canonical,
            "plan": {
                "v": 0,
                "plan": {
                    "op": "Project",
                    "exprs": [{
                        "expr": {"col": "other.name"},
                        "as": "other.name"
                    }],
                    "input": {
                        "op": "Expand",
                        "rel": "Knows",
                        "direction": "out",
                        "from_binding": "person",
                        "binding": "other",
                        "input": {
                            "op": "Filter",
                            "predicate": {"eq": [
                                {"col": "person.name"},
                                {"lit": "ada"}
                            ]},
                            "input": {
                                "op": "ScanNodes",
                                "table": "Person",
                                "binding": "person"
                            }
                        }
                    }
                }
            }
        })
    );
}

#[test]
fn ask_mutation_returns_the_explain_statement_shape_without_executing() {
    let api = TestApi::new("ask-statement");
    api_statement(
        &api,
        "create node table Person (name String primary key, age Int64)",
    );
    api_statement(&api, "insert into Person values (\"ada\", 36)");
    let canonical = "update Person set age = 39 where name = \"ada\"";

    let response = api.post("/api/ask", json!({"text": "set person ada age to 39"}));
    let explained = api.post("/api/explain", json!({"text": canonical}));

    assert_eq!(response.status, 200);
    assert_eq!(response.body, explained.body);
    assert_eq!(response.body["kind"], "statement");
    assert_eq!(response.body["canonical"], canonical);
    assert_eq!(
        response.body["plan"],
        json!({
            "v": 0,
            "stmt": {
                "stmt": "UpdateNode",
                "table": "Person",
                "set": [{"column": "age", "value": {"Int64": 39}}],
                "key_column": "name",
                "key": {"String": "ada"}
            }
        })
    );

    let query = api.post(
        "/api/query",
        json!({"text": "nodes(Person) as p | project p.age as age"}),
    );
    assert_eq!(query.status, 200, "query failed: {}", query.body);
    assert_eq!(query.body["rows"], json!([[36]]));
}

/// The browser's ask box must ground values against committed rows.
///
/// `seed_people_api` stores `Ada` capitalized, so asking in lowercase
/// discriminates: only row-backed grounding can turn the typed `ada` into the
/// stored `Ada`. If `/api/ask` ever reverts to the schema-only `compile`,
/// this fails with `= "ada"` — which is what the UI actually shipped,
/// compiling a filter that matched nothing.
#[test]
fn ask_grounds_a_typed_value_to_its_stored_spelling() {
    let api = TestApi::new("ask-grounding");
    seed_people_api(&api);

    let response = api.post("/api/ask", json!({"text": "show ada"}));

    assert_eq!(response.status, 200);
    assert_eq!(response.body["kind"], "query");
    let canonical = response.body["canonical"].as_str().unwrap();
    assert!(
        canonical.contains("person.name = \"Ada\""),
        "the canonical plan must carry the STORED spelling, got: {canonical}"
    );
}

/// `/api/ask` must supply the UTC reference date at its API boundary.
///
/// Without `compile_at`, `events occurred today` is refused. With it, the
/// route returns a query
/// explain that executes on the fixture row stored in today's UTC day.
#[test]
fn time_phrase_ask_compiles_with_the_server_utc_reference_date_and_executes_fixture_rows() {
    let api = TestApi::new("time-phrase-ask");
    api_statement(
        &api,
        "create node table Event (id Int64 primary key, title String, occurred Int64)",
    );

    let now_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after epoch")
        .as_secs() as i64;
    let reference_date = now_secs - now_secs.rem_euclid(DAY_SECONDS);
    let yesterday = reference_date - DAY_SECONDS;
    let noon_today = reference_date + 12 * 60 * 60;
    api_statement(
        &api,
        &format!(
            "insert into Event values (1, \"Yesterday\", {yesterday}), (2, \"Today\", {noon_today})"
        ),
    );

    let response = api.post("/api/ask", json!({"text": "events with occurred today"}));

    assert_eq!(response.status, 200);
    assert_eq!(response.body["kind"], "query");
    let canonical = format!(
        "nodes(Event) as event | filter event.occurred >= {reference_date} and event.occurred < {}",
        reference_date + DAY_SECONDS,
    );
    assert_eq!(response.body["canonical"], json!(canonical));

    let result = api.post("/api/query", json!({"plan": response.body["plan"]}));
    assert_eq!(result.status, 200, "time query failed: {}", result.body);
    assert_eq!(
        result.body,
        json!({
            "columns": ["event.id", "event.title", "event.occurred"],
            "rows": [[2, "Today", noon_today]],
            "row_count": 1,
            "truncated": false
        })
    );
}

#[test]
fn ask_noparse_returns_the_exact_structured_refusal() {
    let api = TestApi::new("ask-noparse");
    seed_people_api(&api);

    let response = api.post("/api/ask", json!({"text": "people older than 30"}));

    assert_eq!(response.status, 200);
    assert_eq!(
        response.body,
        json!({
            "noparse": {
                "recognized": [
                    {"token": "people", "target": "Person"},
                    {"token": "30", "target": "literal"}
                ],
                "unrecognized": [
                    {"token": "older", "suggestion": "over"},
                    {"token": "than", "suggestion": null}
                ],
                "nearest": [
                    {"example": "top 30 people with <column> over 30 by <sort-column>"},
                    {"example": "how many people have <column> over 30"},
                    {"example": "people with <column> over 30"}
                ]
            }
        })
    );
}

#[test]
fn ask_bulk_delete_keeps_question_refusal_and_appends_pk_hints() {
    let api = TestApi::new("ask-bulk-delete");
    seed_people_api(&api);

    let response = api.post("/api/ask", json!({"text": "delete all people over 40"}));

    assert_eq!(response.status, 200);
    assert_eq!(
        response.body,
        json!({
            "noparse": {
                "recognized": [
                    {"token": "delete", "target": "delete"},
                    {"token": "people", "target": "Person"},
                    {"token": "over", "target": "over"},
                    {"token": "40", "target": "literal"}
                ],
                "unrecognized": [],
                "nearest": [
                    {"example": "top 40 people with <column> over 40 by <sort-column>"},
                    {"example": "how many people have <column> over 40"},
                    {"example": "people with <column> over 40"},
                    {"example": "delete people 40"},
                    {"example": "remove people 40"},
                    {"example": "set people 40 <column> to <value>"}
                ]
            }
        })
    );

    let query = api.post(
        "/api/query",
        json!({"text": "nodes(Person) as p | project p.id as id | sort p.id"}),
    );
    assert_eq!(query.status, 200, "query failed: {}", query.body);
    assert_eq!(query.body["rows"], json!([[1], [2], [3]]));
}

#[test]
fn ask_route_rejects_wrong_methods_and_invalid_bodies() {
    let api = TestApi::new("ask-routing");

    let wrong_method = api.get("/api/ask");
    let invalid_body = api.post("/api/ask", json!({"question": "show people"}));

    assert_eq!(wrong_method.status, 405);
    assert_eq!(wrong_method.body, json!({"error": "method not allowed"}));
    assert_eq!(invalid_body.status, 400);
    assert!(invalid_body.body["error"].is_string());
}

#[test]
fn explain_plan_json_returns_canonical_text_and_plan() {
    let server = TestServer::new("explain-plan-json");
    let plan = json!({
        "v": 0,
        "plan": {
            "op": "Sort",
            "keys": [{"expr": {"col": "p.id"}, "order": "asc"}],
            "input": {"op": "ScanNodes", "table": "Person", "binding": "p"}
        }
    });

    let response = server.post("/api/explain", json!({"plan": plan.clone()}));

    assert_eq!(response.status, 200);
    assert_eq!(
        response.body,
        json!({
            "kind": "query",
            "canonical": "nodes(Person) as p | sort p.id",
            "plan": plan
        })
    );
}

#[test]
fn explain_plan_json_canonicalizes_explicit_exact_knn_mode() {
    let server = TestServer::new("explain-plan-canonicalizes");
    let response = server.post(
        "/api/explain",
        json!({
            "plan": {
                "v": 0,
                "plan": {
                    "op": "KnnScan",
                    "table": "Document",
                    "column": "embedding",
                    "query": [0.5],
                    "k": 2,
                    "metric": "l2",
                    "mode": "exact"
                }
            }
        }),
    );

    assert_eq!(response.status, 200);
    assert_eq!(
        response.body["canonical"],
        "knn(Document.embedding, [0.5], 2, l2)"
    );
    assert_eq!(
        response.body["plan"],
        json!({
            "v": 0,
            "plan": {
                "op": "KnnScan",
                "table": "Document",
                "column": "embedding",
                "query": [0.5],
                "k": 2,
                "metric": "l2"
            }
        })
    );
}

#[test]
fn explain_plan_json_rejects_unknown_operator_with_engine_message() {
    let server = TestServer::new("explain-plan-invalid");
    let plan = json!({"v": 0, "plan": {"op": "Teleport"}});
    let engine_message = Plan::from_json(&plan.to_string())
        .expect_err("unknown operator must be rejected")
        .to_string();

    let response = server.post("/api/explain", json!({"plan": plan}));

    assert_eq!(response.status, 422);
    assert_eq!(response.body, json!({"error": engine_message}));
}

#[test]
fn explain_plan_statement_json_is_canonical_and_never_executes() {
    let server = TestServer::new("explain-statement-json");
    let statement = json!({
        "v": 0,
        "stmt": {
            "stmt": "CreateNodeTable",
            "name": "Ghost",
            "columns": [{"name": "id", "ty": "Int64", "primary_key": true}]
        }
    });

    let response = server.post("/api/explain", json!({"statement": statement.clone()}));

    assert_eq!(response.status, 200);
    assert_eq!(
        response.body,
        json!({
            "kind": "statement",
            "canonical": "create node table Ghost (id Int64 primary key)",
            "plan": statement
        })
    );
    assert_eq!(
        server.get("/api/schema").body,
        json!({"node_tables": [], "rel_tables": []})
    );
}

#[test]
fn query_by_text_returns_exact_filtered_projection_rows() {
    let server = TestServer::new("query-text");
    seed_people(&server);

    let response = server.post(
        "/api/query",
        json!({
            "text": "nodes(Person) as p | filter p.age >= 36 | project p.name as name, p.age as age | sort p.age"
        }),
    );

    assert_eq!(response.status, 200);
    assert_eq!(
        response.body,
        json!({
            "columns": ["name", "age"],
            "rows": [["Ada", 36], ["Grace", 50]],
            "row_count": 2,
            "truncated": false
        })
    );
}

#[test]
fn query_by_explained_canonical_json_matches_text_query() {
    let server = TestServer::new("query-json");
    seed_people(&server);
    let text = "nodes(Person) as p | filter p.age < 40 | project p.id as id, p.name as name";
    let explained = server.post("/api/explain", json!({"text": text}));
    assert_eq!(explained.status, 200);

    let by_text = server.post("/api/query", json!({"text": text}));
    let by_plan = server.post(
        "/api/query",
        json!({"plan": explained.body["plan"].clone()}),
    );

    assert_eq!(by_plan.status, 200);
    assert_eq!(by_plan.body, by_text.body);
    assert_eq!(by_plan.body["rows"], json!([[1, "Ada"], [3, "Linus"]]));
}

#[test]
fn pin_round_trip_schema_run_unpin_and_unknown_name_suggestion() {
    let api = TestApi::new("pin-round-trip");
    seed_people_api(&api);
    let text = "people under 40";
    let canonical = "nodes(Person) as p | filter p.age < 40 | project p.id as id, p.name as name";
    let explained = api.post("/api/explain", json!({"text": canonical}));
    assert_eq!(explained.status, 200);

    let pinned = api.post(
        "/api/pin",
        json!({
            "name": "young people",
            "plan": explained.body["plan"].clone(),
            "text": text,
        }),
    );
    assert_eq!(pinned.status, 200);
    assert_eq!(pinned.body, json!({"ok": true}));
    assert_eq!(
        api.get("/api/schema").body["pins"],
        json!([{
            "name": "young people",
            "text": text,
            "canonical": canonical,
        }])
    );

    let run = api.post("/api/run-pin", json!({"name": "young people"}));
    assert_eq!(run.status, 200);
    assert_eq!(
        run.body,
        json!({
            "columns": ["id", "name"],
            "rows": [[1, "Ada"], [3, "Linus"]],
            "row_count": 2,
            "truncated": false,
        })
    );

    let suggested = api.post("/api/run-pin", json!({"name": "young peple"}));
    assert_eq!(suggested.status, 422);
    assert_eq!(
        suggested.body,
        json!({"error": "not found: pin `young peple` (did you mean `young people`?)"})
    );

    let unpinned = api.post("/api/unpin", json!({"name": "young people"}));
    assert_eq!(unpinned.status, 200);
    assert_eq!(unpinned.body, json!({"ok": true}));
    assert!(api.get("/api/schema").body.get("pins").is_none());

    let removed = api.post("/api/run-pin", json!({"name": "young people"}));
    assert_eq!(removed.status, 422);
    assert_eq!(
        removed.body,
        json!({"error": "not found: pin `young people`"})
    );
}

#[test]
fn pin_duplicate_name_returns_exact_engine_message() {
    let api = TestApi::new("pin-duplicate");
    seed_people_api(&api);
    let explained = api.post(
        "/api/explain",
        json!({"text": "nodes(Person) as p | project p.name"}),
    );
    let request = json!({
        "name": "people",
        "plan": explained.body["plan"].clone(),
        "text": "show people",
    });

    assert_eq!(api.post("/api/pin", request.clone()).status, 200);
    let duplicate = api.post("/api/pin", request);

    assert_eq!(duplicate.status, 422);
    assert_eq!(
        duplicate.body,
        json!({"error": "invalid argument: pin `people` is already in the catalog"})
    );
}

#[test]
fn explained_statement_round_trips_verbatim_and_insert_is_visible() {
    let server = TestServer::new("statement-round-trip");
    let create = server.post(
        "/api/explain",
        json!({"text": "create node table Account (id Int64 primary key, active Bool)"}),
    );
    assert_eq!(create.status, 200);
    assert_eq!(create.body["kind"], "statement");

    let executed = server.post(
        "/api/statement",
        json!({"statement": create.body["plan"].clone()}),
    );
    assert_eq!(executed.status, 200);
    assert_eq!(executed.body, json!({"ok": true}));

    let insert = server.post(
        "/api/explain",
        json!({"text": "insert into Account values (7, true)"}),
    );
    let inserted = server.post(
        "/api/statement",
        json!({"statement": insert.body["plan"].clone()}),
    );
    assert_eq!(inserted.body, json!({"ok": true}));

    let query = server.post(
        "/api/query",
        json!({"text": "nodes(Account) as a | project a.id as id, a.active as active"}),
    );
    assert_eq!(query.body["rows"], json!([[7, true]]));
}

#[test]
fn grid_shaped_statement_text_updates_and_deletes_rows() {
    let api = TestApi::new("grid-dml");
    seed_people_api(&api);
    let update = "update Person set name = \"Augusta\" where id = 2";
    let delete = "delete from Person where id = 3";

    let update_explanation = api.post("/api/explain", json!({"text": update}));
    assert_eq!(update_explanation.status, 200);
    assert_eq!(update_explanation.body["kind"], "statement");
    assert_eq!(update_explanation.body["canonical"], update);
    let updated = api.post("/api/statement", json!({"text": update}));
    assert_eq!(updated.status, 200, "update failed: {}", updated.body);
    assert_eq!(updated.body, json!({"ok": true}));

    let delete_explanation = api.post("/api/explain", json!({"text": delete}));
    assert_eq!(delete_explanation.status, 200);
    assert_eq!(delete_explanation.body["kind"], "statement");
    assert_eq!(delete_explanation.body["canonical"], delete);
    let deleted = api.post("/api/statement", json!({"text": delete}));
    assert_eq!(deleted.status, 200, "delete failed: {}", deleted.body);
    assert_eq!(deleted.body, json!({"ok": true}));

    let query = api.post(
        "/api/query",
        json!({"text": "nodes(Person) as p | project p.id as id, p.name as name, p.age as age | sort p.id"}),
    );
    assert_eq!(query.status, 200, "query failed: {}", query.body);
    assert_eq!(
        query.body["rows"],
        json!([[1, "Ada", 36], [2, "Augusta", 50]])
    );
}

#[test]
fn grid_shaped_vector_and_geo_statement_text_updates_rows() {
    let api = TestApi::new("grid-vector-geo-dml");
    api_statement(
        &api,
        "create node table Feature (id Int64 primary key, embedding Vector(2), location GeoPoint)",
    );
    api_statement(
        &api,
        "insert into Feature values (1, [1.0, 2.0], geo(45.5231, -122.6765))",
    );

    let vector_update = "update Feature set embedding = [0.25, -1.5] where id = 1";
    let vector_explanation = api.post("/api/explain", json!({"text": vector_update}));
    assert_eq!(vector_explanation.status, 200);
    assert_eq!(vector_explanation.body["kind"], "statement");
    assert_eq!(vector_explanation.body["canonical"], vector_update);
    let vector_updated = api.post("/api/statement", json!({"text": vector_update}));
    assert_eq!(
        vector_updated.status, 200,
        "vector update failed: {}",
        vector_updated.body
    );
    assert_eq!(vector_updated.body, json!({"ok": true}));

    let vector_query = api.post(
        "/api/query",
        json!({"text": "nodes(Feature) as f | project f.embedding as embedding"}),
    );
    assert_eq!(
        vector_query.status, 200,
        "query failed: {}",
        vector_query.body
    );
    assert_eq!(vector_query.body["rows"], json!([[[0.25, -1.5]]]));

    let geo_update = "update Feature set location = geo(40.7128, -74.006) where id = 1";
    let geo_explanation = api.post("/api/explain", json!({"text": geo_update}));
    assert_eq!(geo_explanation.status, 200);
    assert_eq!(geo_explanation.body["kind"], "statement");
    assert_eq!(geo_explanation.body["canonical"], geo_update);
    let geo_updated = api.post("/api/statement", json!({"text": geo_update}));
    assert_eq!(
        geo_updated.status, 200,
        "GeoPoint update failed: {}",
        geo_updated.body
    );
    assert_eq!(geo_updated.body, json!({"ok": true}));

    let geo_query = api.post(
        "/api/query",
        json!({"text": "nodes(Feature) as f | project f.location as location"}),
    );
    assert_eq!(geo_query.status, 200, "query failed: {}", geo_query.body);
    assert_eq!(
        geo_query.body["rows"],
        json!([[{"geo": {"lat_deg": 40.7128, "lng_deg": -74.006}}]])
    );
}

#[test]
fn malformed_vector_update_explain_returns_engine_error() {
    let api = TestApi::new("grid-malformed-vector");
    api_statement(
        &api,
        "create node table Feature (id Int64 primary key, embedding Vector(2))",
    );

    let response = api.post(
        "/api/explain",
        json!({"text": "update Feature set embedding = [1.0,] where id = 1"}),
    );

    assert_eq!(response.status, 422);
    let error = response.body["error"]
        .as_str()
        .expect("engine error is text");
    assert!(
        error.starts_with("invalid argument:"),
        "unexpected error: {error}"
    );
}

#[test]
fn grid_shaped_delete_returns_referenced_table_refusal_verbatim() {
    let api = TestApi::new("grid-delete-refusal");
    seed_people_api(&api);
    api_statement(&api, "create rel table Knows from Person to Person");

    let response = api.post(
        "/api/statement",
        json!({"text": "delete from Person where id = 3"}),
    );

    assert_eq!(response.status, 422);
    assert_eq!(
        response.body,
        json!({
            "error": "invalid argument: delete refused for node table `Person` because a relationship table names it as an endpoint; detach-delete is not supported"
        })
    );
}

#[test]
fn graph_returns_exact_nodes_and_edges_in_both_directions() {
    let server = TestServer::new("graph");
    statement(
        &server,
        "create node table Person (id Int64 primary key, name String)",
    );
    statement(&server, "create rel table Knows from Person to Person");
    statement(
        &server,
        "insert into Person values (1, \"Ada\"), (2, \"Grace\"), (3, \"Linus\")",
    );
    statement(&server, "insert rel into Knows values (1 -> 2), (3 -> 1)");

    let response = server.post("/api/graph", json!({"table": "Person", "key": 1}));

    assert_eq!(response.status, 200);
    assert_eq!(
        response.body,
        json!({
            "nodes": [
                {"table": "Person", "key": 1, "props": {"name": "Ada"}},
                {"table": "Person", "key": 2, "props": {"name": "Grace"}},
                {"table": "Person", "key": 3, "props": {"name": "Linus"}}
            ],
            "edges": [
                {"rel": "Knows",
                 "from": {"table": "Person", "key": 1},
                 "to": {"table": "Person", "key": 2}},
                {"rel": "Knows",
                 "from": {"table": "Person", "key": 3},
                 "to": {"table": "Person", "key": 1}}
            ],
            "truncated": false
        })
    );
}

#[test]
fn graph_case_insensitive_table_lookup() {
    let server = TestServer::new("graph-case-insensitive");
    statement(
        &server,
        "create node table Person (id Int64 primary key, name String)",
    );
    statement(&server, "create rel table Knows from Person to Person");
    statement(
        &server,
        "insert into Person values (1, \"Ada\"), (2, \"Grace\")",
    );
    statement(&server, "insert rel into Knows values (1 -> 2)");

    let exact = server.post("/api/graph", json!({"table": "Person", "key": 1}));
    let folded = server.post("/api/graph", json!({"table": "person", "key": 1}));

    assert_eq!(exact.status, 200);
    assert_eq!(folded.status, 200);
    assert_eq!(folded.body, exact.body);
    assert_eq!(
        folded.body,
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

#[test]
fn graph_case_unknown_table_suggests() {
    let server = TestServer::new("graph-case-suggests");
    statement(
        &server,
        "create node table Person (id Int64 primary key, name String)",
    );

    let response = server.post("/api/graph", json!({"table": "Persn", "key": 1}));

    assert_eq!(response.status, 422);
    assert_eq!(
        response.body,
        json!({"error": "not found: node table `Persn` (did you mean `Person`?)"})
    );
}

#[test]
fn graph_edges_qualify_endpoints_across_tables_with_colliding_keys() {
    let server = TestServer::new("graph-collide");
    statement(
        &server,
        "create node table Person (id Int64 primary key, name String)",
    );
    statement(
        &server,
        "create node table Company (id Int64 primary key, name String)",
    );
    statement(&server, "create rel table WorksAt from Person to Company");
    statement(&server, "insert into Person values (1, \"Ada\")");
    statement(&server, "insert into Company values (1, \"Devon Labs\")");
    statement(&server, "insert rel into WorksAt values (1 -> 1)");

    let response = server.post("/api/graph", json!({"table": "Person", "key": 1}));

    assert_eq!(response.status, 200);
    assert_eq!(
        response.body["edges"],
        json!([
            {"rel": "WorksAt",
             "from": {"table": "Person", "key": 1},
             "to": {"table": "Company", "key": 1}}
        ])
    );
    let nodes = response.body["nodes"].as_array().unwrap();
    assert_eq!(nodes.len(), 2);
}

#[test]
fn graph_honors_node_limit_and_reports_truncation() {
    let server = TestServer::new("graph-limit");
    statement(
        &server,
        "create node table Person (id Int64 primary key, name String)",
    );
    statement(&server, "create rel table Knows from Person to Person");
    statement(
        &server,
        "insert into Person values (1, \"Ada\"), (2, \"Grace\"), (3, \"Linus\")",
    );
    statement(&server, "insert rel into Knows values (1 -> 2), (1 -> 3)");

    let response = server.post(
        "/api/graph",
        json!({"table": "Person", "key": 1, "limit": 2}),
    );

    assert_eq!(response.status, 200);
    assert_eq!(response.body["nodes"].as_array().unwrap().len(), 2);
    assert_eq!(response.body["edges"].as_array().unwrap().len(), 1);
    assert_eq!(response.body["truncated"], true);
}

#[test]
fn malformed_json_and_unknown_api_path_return_expected_statuses() {
    let server = TestServer::new("routing-errors");
    let malformed = server.post_raw("/api/query", br#"{"text": "#);
    assert_eq!(malformed.status, 400);
    assert!(malformed.body["error"].is_string());

    let unknown = server.get("/api/does-not-exist");
    assert_eq!(unknown.status, 404);
    assert_eq!(unknown.body, json!({"error": "not found"}));
}

#[test]
fn wrong_method_on_known_route_returns_method_not_allowed() {
    let server = TestServer::new("wrong-method");
    let response = server.get("/api/query");

    assert_eq!(response.status, 405);
    assert_eq!(response.body, json!({"error": "method not allowed"}));
}

#[test]
fn engine_errors_return_unprocessable_error_body() {
    let server = TestServer::new("engine-error");
    let response = server.post(
        "/api/query",
        json!({"text": "nodes(Missing) as m | project m.id"}),
    );

    assert_eq!(response.status, 422);
    let message = response.body["error"].as_str().expect("error is text");
    assert!(message.contains("Missing"));
    assert!(message.contains("not found"));
}

#[test]
fn graph_rejects_unknown_tables_and_missing_keys() {
    let server = TestServer::new("graph-errors");
    statement(
        &server,
        "create node table Person (id Int64 primary key, name String)",
    );
    statement(&server, "insert into Person values (1, \"Ada\")");

    let unknown = server.post("/api/graph", json!({"table": "Ghost", "key": 1}));
    assert_eq!(unknown.status, 422);
    assert!(unknown.body["error"].as_str().unwrap().contains("Ghost"));

    let missing = server.post("/api/graph", json!({"table": "Person", "key": 999}));
    assert_eq!(missing.status, 422);
    assert!(missing.body["error"].as_str().unwrap().contains("999"));
}

#[test]
fn ask_fulltext_explains_a_data_only_query_without_execution() {
    let api = TestApi::new("fulltext");
    api_statement(
        &api,
        "create node table Document (id Int64 primary key, body String)",
    );
    api_statement(
        &api,
        "insert into Document values (9, \"rust\"), (1, \"rust\")",
    );
    let response = api.post(
        "/api/ask",
        json!({"text":"search Document body for \"rust\""}),
    );
    assert_eq!(response.status, 200, "{}", response.body);
    assert_eq!(
        response.body["canonical"],
        "textscan(Document.body, \"rust\", k=10) as document"
    );
    assert_eq!(response.body["plan"]["plan"]["query"], "rust");
    #[cfg(feature = "fts")]
    {
        let executed = api.post("/api/query", json!({"plan":response.body["plan"].clone()}));
        assert_eq!(executed.status, 200, "{}", executed.body);
        assert_eq!(executed.body["rows"], json!([[1, "rust"], [9, "rust"]]));
    }
}
