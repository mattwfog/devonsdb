//! Minimal synchronous HTTP/1.1 server over [`std::net::TcpListener`].

use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const MAX_BODY_BYTES: usize = 1_048_576;
const MAX_HEADER_BYTES: usize = 64 * 1024;
const HEADER_ERROR_DRAIN_BYTES: usize = MAX_BODY_BYTES;
const CONNECTION_POOL_SIZE: usize = 4;
const READ_TIMEOUT: Duration = Duration::from_secs(3);
const WRITE_TIMEOUT: Duration = Duration::from_secs(3);

/// An HTTP request parsed from the wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    /// The request method, such as `GET` or `POST`.
    pub method: String,
    /// The request-target path with its query string removed.
    pub path: String,
    /// The request body bytes.
    pub body: Vec<u8>,
}

/// An HTTP response returned by the API or static-asset router.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    /// The HTTP status code.
    pub status: u16,
    /// The value of the `Content-Type` response header.
    pub content_type: &'static str,
    /// The response body bytes.
    pub body: Vec<u8>,
}

impl Response {
    /// Creates a response with an explicit status, content type, and body.
    pub fn new(status: u16, content_type: &'static str, body: impl Into<Vec<u8>>) -> Self {
        Self {
            status,
            content_type,
            body: body.into(),
        }
    }

    /// Creates a JSON response.
    pub fn json(status: u16, body: impl Into<Vec<u8>>) -> Self {
        Self::new(status, "application/json", body)
    }

    /// Creates a one-line JSON error response.
    pub fn error(status: u16, message: &str) -> Self {
        Self::json(status, json_error_body(message))
    }

    /// Creates a `404 Not Found` JSON response.
    pub fn not_found() -> Self {
        Self::error(404, "not found")
    }

    /// Creates a `405 Method Not Allowed` JSON response.
    pub fn method_not_allowed() -> Self {
        Self::error(405, "method not allowed")
    }
}

/// Handles requests routed under `/api/`.
pub trait ApiHandler: Send + Sync {
    /// Handles one parsed API request and returns its response.
    fn handle(&self, request: &Request) -> Response;
}

/// Serves requests synchronously on a small fixed thread pool.
///
/// Each connection gets `READ_TIMEOUT`/`WRITE_TIMEOUT` deadlines, so an idle
/// or stalled client cannot hold a pool slot forever; requests serialize at
/// the handler's own synchronization (UI.md §3), never at the accept loop.
/// A failed connection is reported to stderr without ending the accept loop.
/// Every response includes `Connection: close`.
pub fn serve(listener: TcpListener, handler: &dyn ApiHandler) -> io::Result<()> {
    let listener = Arc::new(Mutex::new(listener));

    thread::scope(|scope| {
        for _ in 0..CONNECTION_POOL_SIZE {
            let listener = Arc::clone(&listener);
            scope.spawn(move || accept_loop(listener, handler));
        }
    });

    Ok(())
}

fn accept_loop(listener: Arc<Mutex<TcpListener>>, handler: &dyn ApiHandler) {
    loop {
        let stream = {
            let listener = listener.lock().unwrap_or_else(|error| error.into_inner());
            listener.accept()
        };
        match stream {
            Ok((mut stream, _)) => {
                if stream.set_read_timeout(Some(READ_TIMEOUT)).is_err()
                    || stream.set_write_timeout(Some(WRITE_TIMEOUT)).is_err()
                {
                    continue;
                }
                if let Err(error) = handle_connection(&mut stream, handler) {
                    eprintln!("HTTP connection failed: {error}");
                }
            }
            Err(error) => eprintln!("HTTP accept failed: {error}"),
        }
    }
}

fn handle_connection(stream: &mut TcpStream, handler: &dyn ApiHandler) -> io::Result<()> {
    match read_request(stream) {
        Ok(request) => write_response(stream, &route(&request, handler)),
        Err(ReadRequestError::Client(error)) => write_client_error(stream, error),
        Err(ReadRequestError::Io(error)) => Err(error),
    }
}

fn read_request(stream: &mut TcpStream) -> Result<Request, ReadRequestError> {
    let header_lines = read_header_block(stream)?;
    let (request_line, headers) = header_lines
        .split_first()
        .ok_or_else(|| ClientError::bad_request("missing request line"))?;
    let (method, path) = parse_request_line(request_line)?;
    let headers = parse_headers(headers)?;
    if headers.expect_continue && headers.content_length.is_some() {
        write_continue(stream)?;
    }

    let content_length = headers.content_length.unwrap_or(0);
    let mut body = vec![0; content_length];
    if let Err(error) = stream.read_exact(&mut body) {
        return match error.kind() {
            io::ErrorKind::UnexpectedEof => {
                Err(ClientError::bad_request("incomplete request body").into())
            }
            _ => Err(error.into()),
        };
    }

    Ok(Request { method, path, body })
}

fn read_header_block<R: Read>(reader: &mut R) -> Result<Vec<Vec<u8>>, ReadRequestError> {
    let mut header_bytes = 0;
    let mut lines = Vec::new();
    loop {
        let line = read_header_line(reader, &mut header_bytes)?;
        if line.is_empty() {
            return Ok(lines);
        }
        lines.push(line);
    }
}

fn parse_headers(headers: &[Vec<u8>]) -> Result<ParsedHeaders, ClientError> {
    let mut parsed = ParsedHeaders::default();
    for line in headers {
        parse_header(line, &mut parsed)?;
    }
    Ok(parsed)
}

fn read_header_line<R: Read>(
    reader: &mut R,
    header_bytes: &mut usize,
) -> Result<Vec<u8>, ReadRequestError> {
    let mut line = Vec::new();
    loop {
        if *header_bytes == MAX_HEADER_BYTES {
            return Err(ClientError::bad_request("request headers too large").into());
        }
        let mut byte = [0_u8];
        match reader.read(&mut byte) {
            Ok(0) => {
                return Err(ClientError::bad_request("incomplete request headers").into());
            }
            Ok(_) => {
                *header_bytes += 1;
                line.push(byte[0]);
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error.into()),
        }
        if byte[0] == b'\n' {
            break;
        }
    }
    if !line.ends_with(b"\r\n") {
        return Err(ClientError::bad_request("headers must use CRLF line endings").into());
    }
    line.truncate(line.len() - 2);
    Ok(line)
}

fn parse_request_line(line: &[u8]) -> Result<(String, String), ClientError> {
    let line = std::str::from_utf8(line)
        .map_err(|_| ClientError::bad_request("request line is not valid UTF-8"))?;
    let mut parts = line.split(' ');
    let method = parts.next().unwrap_or_default();
    let target = parts.next().unwrap_or_default();
    let version = parts.next().unwrap_or_default();
    if method.is_empty()
        || !method.bytes().all(is_token_byte)
        || target.is_empty()
        || version != "HTTP/1.1"
        || parts.next().is_some()
    {
        return Err(ClientError::bad_request("malformed HTTP/1.1 request line"));
    }

    let path = target.split_once('?').map_or(target, |(path, _)| path);
    Ok((method.to_owned(), path.to_owned()))
}

fn parse_header(line: &[u8], parsed: &mut ParsedHeaders) -> Result<(), ClientError> {
    let Some(colon) = line.iter().position(|byte| *byte == b':') else {
        return Err(ClientError::bad_request("malformed header"));
    };
    let name = &line[..colon];
    let value = trim_optional_whitespace(&line[colon + 1..]);
    if name.is_empty() || !name.iter().copied().all(is_token_byte) || !valid_header_value(value) {
        return Err(ClientError::bad_request("malformed header"));
    }

    if name.eq_ignore_ascii_case(b"content-length") {
        if parsed.content_length.is_some() {
            return Err(ClientError::bad_request("duplicate Content-Length header"));
        }
        parsed.content_length = Some(parse_content_length(value)?);
    } else if name.eq_ignore_ascii_case(b"expect") && value.eq_ignore_ascii_case(b"100-continue") {
        parsed.expect_continue = true;
    }
    Ok(())
}

fn parse_content_length(value: &[u8]) -> Result<usize, ClientError> {
    if value.is_empty() || !value.iter().all(u8::is_ascii_digit) {
        return Err(ClientError::bad_request("invalid Content-Length"));
    }

    let mut length = 0_usize;
    for byte in value {
        let digit = usize::from(*byte - b'0');
        let Some(next) = length
            .checked_mul(10)
            .and_then(|value| value.checked_add(digit))
        else {
            return Err(ClientError::payload_too_large());
        };
        if next > MAX_BODY_BYTES {
            return Err(ClientError::payload_too_large());
        }
        length = next;
    }
    Ok(length)
}

fn route(request: &Request, handler: &dyn ApiHandler) -> Response {
    if request.path.starts_with("/api/") {
        return call_handler(handler, request);
    }
    if request.method != "GET" {
        return Response::method_not_allowed();
    }
    match crate::assets::lookup(&request.path) {
        Some((content_type, body)) => Response::new(200, content_type, body),
        None => Response::not_found(),
    }
}

fn call_handler(handler: &dyn ApiHandler, request: &Request) -> Response {
    match catch_unwind(AssertUnwindSafe(|| handler.handle(request))) {
        Ok(response) => response,
        Err(_) => {
            eprintln!("HTTP API handler panicked");
            Response::error(500, "internal error")
        }
    }
}

fn write_continue(stream: &mut TcpStream) -> io::Result<()> {
    stream.write_all(b"HTTP/1.1 100 Continue\r\n\r\n")?;
    stream.flush()
}

fn write_client_error(stream: &mut TcpStream, error: ClientError) -> io::Result<()> {
    write_response(stream, &Response::error(error.status, error.message))?;
    drain_request(stream, error.drain_limit);
    stream.shutdown(Shutdown::Write)
}

fn drain_request(stream: &mut TcpStream, limit: usize) {
    let deadline = Instant::now() + READ_TIMEOUT;
    let mut remaining = limit;
    let mut buffer = [0_u8; 8 * 1024];
    while remaining > 0 {
        let timeout = deadline.saturating_duration_since(Instant::now());
        if timeout.is_zero() || stream.set_read_timeout(Some(timeout)).is_err() {
            break;
        }
        let read_limit = remaining.min(buffer.len());
        match stream.read(&mut buffer[..read_limit]) {
            Ok(0) => break,
            Ok(read) => remaining -= read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
}

fn write_response(stream: &mut TcpStream, response: &Response) -> io::Result<()> {
    let head = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        response.status,
        reason_phrase(response.status),
        response.content_type,
        response.body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(&response.body)?;
    stream.flush()
}

fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        413 => "Payload Too Large",
        422 => "Unprocessable Content",
        500 => "Internal Server Error",
        _ => "Unknown",
    }
}

fn trim_optional_whitespace(mut value: &[u8]) -> &[u8] {
    while value
        .first()
        .is_some_and(|byte| matches!(byte, b' ' | b'\t'))
    {
        value = &value[1..];
    }
    while value
        .last()
        .is_some_and(|byte| matches!(byte, b' ' | b'\t'))
    {
        value = &value[..value.len() - 1];
    }
    value
}

fn is_token_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte)
}

fn valid_header_value(value: &[u8]) -> bool {
    value
        .iter()
        .all(|byte| *byte == b'\t' || *byte >= b' ' && *byte != 127)
}

fn json_error_body(message: &str) -> Vec<u8> {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut body = String::from("{\"error\":\"");
    for character in message.chars() {
        match character {
            '"' => body.push_str("\\\""),
            '\\' => body.push_str("\\\\"),
            '\u{08}' => body.push_str("\\b"),
            '\u{0c}' => body.push_str("\\f"),
            '\n' => body.push_str("\\n"),
            '\r' => body.push_str("\\r"),
            '\t' => body.push_str("\\t"),
            control if control <= '\u{1f}' => {
                let byte = control as u8;
                body.push_str("\\u00");
                body.push(HEX[usize::from(byte >> 4)] as char);
                body.push(HEX[usize::from(byte & 0x0f)] as char);
            }
            other => body.push(other),
        }
    }
    body.push_str("\"}");
    body.into_bytes()
}

#[derive(Debug)]
enum ReadRequestError {
    Client(ClientError),
    Io(io::Error),
}

impl From<ClientError> for ReadRequestError {
    fn from(error: ClientError) -> Self {
        Self::Client(error)
    }
}

impl From<io::Error> for ReadRequestError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

#[derive(Debug, Clone, Copy)]
struct ClientError {
    status: u16,
    message: &'static str,
    drain_limit: usize,
}

impl ClientError {
    fn bad_request(message: &'static str) -> Self {
        Self {
            status: 400,
            message,
            drain_limit: HEADER_ERROR_DRAIN_BYTES,
        }
    }

    fn payload_too_large() -> Self {
        Self {
            status: 413,
            message: "request body exceeds 1 MiB limit",
            drain_limit: MAX_BODY_BYTES,
        }
    }
}

#[derive(Debug, Default)]
struct ParsedHeaders {
    content_length: Option<usize>,
    expect_continue: bool,
}
