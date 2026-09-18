use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::thread;
use std::time::{Duration, Instant};

use devondb_server::http::{self, ApiHandler, Request, Response};
use serde_json::{Value, json};

struct EchoHandler;

impl ApiHandler for EchoHandler {
    fn handle(&self, request: &Request) -> Response {
        let body = json!({
            "method": request.method,
            "path": request.path,
            "body": String::from_utf8_lossy(&request.body),
        });
        Response::json(200, serde_json::to_vec(&body).unwrap())
    }
}

struct PanicHandler;

impl ApiHandler for PanicHandler {
    fn handle(&self, request: &Request) -> Response {
        if request.path == "/api/boom" {
            panic!("test handler panic");
        }
        EchoHandler.handle(request)
    }
}

#[test]
fn get_root_serves_embedded_html() {
    let response = exchange(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n");
    let (headers, body) = split_response(&response);

    assert!(headers.starts_with("HTTP/1.1 200 OK\r\n"));
    assert!(headers.contains("Content-Type: text/html; charset=utf-8\r\n"));
    assert!(body.contains("devondb"));
    assert!(body.contains("/favicon.svg"));
}

#[test]
fn favicon_is_served_as_svg() {
    let response = exchange(b"GET /favicon.svg HTTP/1.1\r\nHost: localhost\r\n\r\n");
    let (headers, body) = split_response(&response);

    assert!(headers.starts_with("HTTP/1.1 200 OK\r\n"), "{headers}");
    assert!(headers.contains("Content-Type: image/svg+xml"), "{headers}");
    assert!(body.contains("<svg"), "{body}");
}

#[test]
fn map_module_is_served_with_projection_and_overlay_logic() {
    let response = exchange(b"GET /views/map.js HTTP/1.1\r\nHost: localhost\r\n\r\n");
    let (headers, body) = split_response(&response);

    assert!(headers.starts_with("HTTP/1.1 200 OK\r\n"), "{headers}");
    assert!(
        headers.contains("Content-Type: text/javascript; charset=utf-8"),
        "{headers}"
    );
    assert!(body.contains("export function registerMapView"), "{body}");
    assert!(body.contains("column?.type === \"GeoPoint\""), "{body}");
    assert!(body.contains("devonGridDisplayCell"), "{body}");
    assert!(body.contains("expandMaxZoomClusters"), "{body}");
    assert!(body.contains("Small-circle approximation"), "{body}");
    assert!(body.contains("WithinScan"), "{body}");
    assert!(
        !body.contains("https://"),
        "map module must not fetch external tiles"
    );
}

#[test]
fn app_module_carries_conditional_map_tab_wiring() {
    let response = exchange(b"GET /app.js HTTP/1.1\r\nHost: localhost\r\n\r\n");
    let (headers, body) = split_response(&response);

    assert!(headers.starts_with("HTTP/1.1 200 OK\r\n"), "{headers}");
    assert!(body.contains("from \"./views/map.js\""), "{body}");
    assert!(body.contains("registerMapView(registerView)"), "{body}");
    assert!(body.contains("installMapTab()"), "{body}");
    assert!(
        body.contains("dom.mapTab.hidden = mapContext === null"),
        "{body}"
    );
    assert!(body.contains("body: { plan: request.plan }"), "{body}");

    let ask_first = body
        .find("const response = await ask(text);")
        .expect("resolveInput should ask before parsing DevonPlan");
    let explain_second = body
        .find("explanation: await explain(text)")
        .expect("resolveInput should still parse DevonPlan fallback input");
    assert!(ask_first < explain_second, "{body}");
}

#[test]
fn graph_node_labels_are_pointer_targets() {
    let response = exchange(b"GET /views/graph.js HTTP/1.1\r\nHost: localhost\r\n\r\n");
    let (headers, body) = split_response(&response);

    assert!(headers.starts_with("HTTP/1.1 200 OK\r\n"), "{headers}");
    assert!(body.contains("function nodeGroup(state, node)"), "{body}");
    assert!(
        !body.contains("label.style.pointerEvents = \"none\""),
        "node labels must bubble pointer events to their group"
    );
}

#[test]
fn missing_static_asset_returns_not_found() {
    let response = exchange(b"GET /missing.js HTTP/1.1\r\nHost: localhost\r\n\r\n");
    let (headers, _) = split_response(&response);

    assert!(headers.starts_with("HTTP/1.1 404 Not Found\r\n"));
}

#[test]
fn non_get_static_request_returns_method_not_allowed() {
    let response = exchange(b"POST /missing HTTP/1.1\r\nHost: localhost\r\n\r\n");
    let (headers, _) = split_response(&response);

    assert!(headers.starts_with("HTTP/1.1 405 Method Not Allowed\r\n"));
}

#[test]
fn post_api_request_reaches_handler_and_closes_connection() {
    let request = b"POST /api/echo HTTP/1.1\r\nHost: localhost\r\nContent-Length: 17\r\n\r\n{\"hello\":\"world\"}";
    let response = exchange(request);
    let (headers, body) = split_response(&response);

    assert!(headers.starts_with("HTTP/1.1 200 OK\r\n"));
    assert!(headers.ends_with("\r\nConnection: close"));
    assert_eq!(
        serde_json::from_str::<Value>(body).unwrap(),
        json!({
            "method": "POST",
            "path": "/api/echo",
            "body": "{\"hello\":\"world\"}",
        })
    );
}

#[test]
fn idle_connection_does_not_block_another_client() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    thread::spawn(move || http::serve(listener, &EchoHandler).unwrap());

    let mut idle = TcpStream::connect(address).unwrap();
    let mut active = TcpStream::connect(address).unwrap();
    active
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    active
        .write_all(b"GET /api/echo HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .unwrap();

    // The idle connection holds one pool slot; the active client's request
    // must still be answered well inside the server's read timeout.
    let start = Instant::now();
    let mut response = String::new();
    active.read_to_string(&mut response).unwrap();
    assert!(
        start.elapsed() < Duration::from_secs(2),
        "active client was blocked behind the idle connection"
    );
    assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
    drop(active);

    // The idle client never sent a request: the server closes the connection
    // once the read timeout (3 seconds) expires.
    idle.set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let start = Instant::now();
    let mut closed = String::new();
    idle.read_to_string(&mut closed).unwrap();
    assert!(closed.is_empty(), "{closed}");
    assert!(
        start.elapsed() >= Duration::from_secs(2),
        "idle connection closed before the read timeout"
    );
}

#[test]
fn oversized_content_length_returns_payload_too_large_without_body() {
    let response = exchange(b"POST /api/echo HTTP/1.1\r\nContent-Length: 2000000\r\n\r\n");
    let (headers, body) = split_response(&response);

    assert!(headers.starts_with("HTTP/1.1 413 Payload Too Large\r\n"));
    assert!(body.contains("1 MiB"));
}

#[test]
fn oversized_body_is_drained_before_the_error_connection_closes() {
    let address = start_server(EchoHandler);
    let mut request = b"POST /api/echo HTTP/1.1\r\nContent-Length: 2000000\r\n\r\n".to_vec();
    request.resize(request.len() + 256 * 1024, b'x');

    // Closing a TCP socket with unread receive data can send an RST on macOS
    // and Linux, erasing the already-queued JSON error. Write the burst fully
    // before reading so this test gates that kernel behavior deterministically.
    let mut stream = TcpStream::connect(address).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let mut writer = stream.try_clone().unwrap();
    let write = thread::spawn(move || {
        if let Err(error) = writer.write_all(&request) {
            assert!(
                matches!(
                    error.kind(),
                    io::ErrorKind::BrokenPipe
                        | io::ErrorKind::ConnectionAborted
                        | io::ErrorKind::ConnectionReset
                ),
                "unexpected request write failure: {error}"
            );
        }
        let _ = writer.shutdown(Shutdown::Write);
    });
    write.join().unwrap();

    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    let (headers, body) = split_response(&response);
    assert!(headers.starts_with("HTTP/1.1 413 Payload Too Large\r\n"));
    assert_eq!(
        serde_json::from_str::<Value>(body).unwrap(),
        json!({"error": "request body exceeds 1 MiB limit"})
    );
}

#[test]
fn panicking_handler_returns_internal_error_and_pool_keeps_serving() {
    let address = start_server(PanicHandler);

    let response = exchange_at(
        address,
        b"GET /api/boom HTTP/1.1\r\nHost: localhost\r\n\r\n",
    );
    let (headers, body) = split_response(&response);
    assert!(headers.starts_with("HTTP/1.1 500 Internal Server Error\r\n"));
    assert_eq!(
        serde_json::from_str::<Value>(body).unwrap(),
        json!({"error": "internal error"})
    );

    let response = exchange_at(
        address,
        b"GET /api/echo HTTP/1.1\r\nHost: localhost\r\n\r\n",
    );
    let (headers, _) = split_response(&response);
    assert!(headers.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
}

#[test]
fn expect_continue_is_sent_before_reading_a_valid_body() {
    const CONTINUE: &[u8] = b"HTTP/1.1 100 Continue\r\n\r\n";

    let address = start_server(EchoHandler);
    let mut stream = TcpStream::connect(address).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream
        .write_all(
            b"POST /api/echo HTTP/1.1\r\nHost: localhost\r\nContent-Length: 4\r\nExpect: 100-continue\r\n\r\n",
        )
        .unwrap();

    let mut interim = vec![0; CONTINUE.len()];
    stream.read_exact(&mut interim).unwrap();
    assert_eq!(interim, CONTINUE);

    stream.write_all(b"ping").unwrap();
    stream.shutdown(Shutdown::Write).unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    let (headers, body) = split_response(&response);
    assert!(headers.starts_with("HTTP/1.1 200 OK\r\n"));
    assert_eq!(serde_json::from_str::<Value>(body).unwrap()["body"], "ping");
}

#[test]
fn expect_continue_is_not_sent_for_an_oversized_body() {
    let response = exchange(
        b"POST /api/echo HTTP/1.1\r\nContent-Length: 2000000\r\nExpect: 100-continue\r\n\r\n",
    );
    let (headers, body) = split_response(&response);

    assert!(headers.starts_with("HTTP/1.1 413 Payload Too Large\r\n"));
    assert!(!response.contains("100 Continue"), "{response}");
    assert_eq!(
        serde_json::from_str::<Value>(body).unwrap(),
        json!({"error": "request body exceeds 1 MiB limit"})
    );
}

#[test]
fn garbage_request_line_returns_bad_request_json() {
    let response = exchange(b"NOT-HTTP\r\n\r\n");
    let (headers, body) = split_response(&response);

    assert!(headers.starts_with("HTTP/1.1 400 Bad Request\r\n"));
    assert_eq!(
        headers
            .matches("Content-Type: application/json\r\n")
            .count(),
        1
    );
    assert!(serde_json::from_str::<Value>(body).unwrap()["error"].is_string());
    assert!(!body.contains('\n'));
}

#[test]
fn query_string_is_discarded_before_api_routing() {
    let response = exchange(b"GET /api/echo?ignored=yes HTTP/1.1\r\nHost: localhost\r\n\r\n");
    let (_, body) = split_response(&response);
    let echoed: Value = serde_json::from_str(body).unwrap();

    assert_eq!(echoed["path"], "/api/echo");
    assert_eq!(echoed["body"], "");
}

#[test]
fn content_length_name_is_case_insensitive() {
    let response =
        exchange(b"POST /api/echo HTTP/1.1\r\nHost: localhost\r\ncOnTeNt-LeNgTh: 4\r\n\r\nping");
    let (_, body) = split_response(&response);
    let echoed: Value = serde_json::from_str(body).unwrap();

    assert_eq!(echoed["body"], "ping");
}

#[test]
fn malformed_header_returns_bad_request_json() {
    let response = exchange(b"GET / HTTP/1.1\r\nNot-A-Header\r\n\r\n");
    let (headers, body) = split_response(&response);

    assert!(headers.starts_with("HTTP/1.1 400 Bad Request\r\n"));
    assert_eq!(
        serde_json::from_str::<Value>(body).unwrap(),
        json!({"error": "malformed header"})
    );
}

fn exchange(request: &[u8]) -> String {
    exchange_at(start_server(EchoHandler), request)
}

fn start_server(handler: impl ApiHandler + 'static) -> std::net::SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    thread::spawn(move || {
        http::serve(listener, &handler).unwrap();
    });
    address
}

fn exchange_at(address: std::net::SocketAddr, request: &[u8]) -> String {
    let mut stream = TcpStream::connect(address).unwrap();
    stream.write_all(request).unwrap();
    stream.shutdown(Shutdown::Write).unwrap();

    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    response
}

fn split_response(response: &str) -> (&str, &str) {
    response.split_once("\r\n\r\n").unwrap()
}
