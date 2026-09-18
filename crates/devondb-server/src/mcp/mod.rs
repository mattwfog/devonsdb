//! MCP stdio server — devondb as a read-only tool server for Claude
//! Desktop (`docs/MCP.md`, binding).
//!
//! Transport is newline-delimited JSON-RPC 2.0 over stdin/stdout,
//! synchronous (Edge budget § 4: no async runtime). NOTHING in this
//! process may write to stdout except [`serve`] — a stray `println!`
//! corrupts the protocol stream; diagnostics go to stderr.

mod render;
mod tools;

use std::io::{self, BufRead, Write};
use std::sync::Mutex;

use std::path::Path;

use devondb::{Database, DevonError, DevonResult, Options};
use serde_json::{Value as JsonValue, json};

pub use render::{BODY_BYTES, CELL_CHARS, DEFAULT_LIMIT, MAX_LIMIT};

/// Protocol version answered when the client names none.
const DEFAULT_PROTOCOL_VERSION: &str = "2025-06-18";
const PARSE_ERROR: i64 = -32700;
const METHOD_NOT_FOUND: i64 = -32601;

/// The MCP tool server over one serialized database handle.
pub struct Mcp {
    database: Mutex<Database>,
}

impl Mcp {
    /// Creates a server over `database`; open it read-only (`docs/MCP.md` §2).
    #[must_use]
    pub fn new(database: Database) -> Self {
        Self {
            database: Mutex::new(database),
        }
    }

    /// Handles one JSON-RPC line. `None` means nothing goes on the wire:
    /// blank lines and notifications never get a reply.
    #[must_use]
    pub fn handle_line(&self, line: &str) -> Option<String> {
        let line = line.trim();
        if line.is_empty() {
            return None;
        }
        let message: JsonValue = match serde_json::from_str(line) {
            Ok(message) => message,
            Err(error) => {
                return Some(
                    rpc_error(
                        JsonValue::Null,
                        PARSE_ERROR,
                        &format!("parse error: {error}"),
                    )
                    .to_string(),
                );
            }
        };
        self.dispatch(&message).map(|reply| reply.to_string())
    }

    fn dispatch(&self, message: &JsonValue) -> Option<JsonValue> {
        let method = message.get("method").and_then(JsonValue::as_str)?;
        let id = message.get("id")?.clone();
        if method.starts_with("notifications/") {
            return None;
        }
        let params = message.get("params").cloned().unwrap_or_else(|| json!({}));
        Some(match method {
            "initialize" => rpc_result(id, initialize_result(&params)),
            "ping" => rpc_result(id, json!({})),
            "tools/list" => rpc_result(id, json!({"tools": tools::definitions()})),
            "tools/call" => self.tool_call(id, &params),
            "resources/list" => rpc_result(id, json!({"resources": []})),
            "prompts/list" => rpc_result(id, json!({"prompts": []})),
            other => rpc_error(id, METHOD_NOT_FOUND, &format!("method not found: {other}")),
        })
    }

    fn tool_call(&self, id: JsonValue, params: &JsonValue) -> JsonValue {
        let name = params.get("name").and_then(JsonValue::as_str).unwrap_or("");
        let arguments = params
            .get("arguments")
            .cloned()
            .unwrap_or_else(|| json!({}));
        let outcome = tools::call(&self.database, name, &arguments);
        rpc_result(
            id,
            json!({
                "content": [{"type": "text", "text": outcome.text}],
                "isError": outcome.is_error,
            }),
        )
    }
}

fn initialize_result(params: &JsonValue) -> JsonValue {
    let protocol_version = params
        .get("protocolVersion")
        .and_then(JsonValue::as_str)
        .unwrap_or(DEFAULT_PROTOCOL_VERSION);
    json!({
        "protocolVersion": protocol_version,
        "capabilities": {"tools": {}},
        "serverInfo": {"name": "devondb", "version": env!("CARGO_PKG_VERSION")},
    })
}

/// Opens `path` for the read-only server (`docs/MCP.md` §2).
///
/// A multiprocess-activated file opens as a true follower — zero WAL
/// footprint, coexisting with a live writer. A plain file (the common
/// hand-off: someone built it, copied it over) refuses the follower open
/// with `InvalidArgument`; it then opens exclusively, and read-only-ness is
/// the tool surface's law: no tool has a mutation path.
pub fn open(path: &Path, options: Options) -> DevonResult<Database> {
    match Database::open_read_only_with(path, options) {
        Err(DevonError::InvalidArgument { .. }) => Database::open_with(path, options),
        opened => opened,
    }
}

/// Serves `input` until it ends, writing one reply line per request.
pub fn serve(database: Database, input: impl BufRead, mut output: impl Write) -> io::Result<()> {
    let mcp = Mcp::new(database);
    for line in input.lines() {
        let line = line?;
        if let Some(reply) = mcp.handle_line(&line) {
            output.write_all(reply.as_bytes())?;
            output.write_all(b"\n")?;
            output.flush()?;
        }
    }
    Ok(())
}

fn rpc_result(id: JsonValue, result: JsonValue) -> JsonValue {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn rpc_error(id: JsonValue, code: i64, message: &str) -> JsonValue {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}
