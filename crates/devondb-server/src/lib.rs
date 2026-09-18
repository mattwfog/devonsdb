//! devondb-server — the surfaces crate: the built-in UI (embedded SPA +
//! local JSON API) and the MCP tool server.
//!
//! Design: `docs/UI.md` (binding). Reached from the CLI's `ui` subcommand
//! behind the `ui` cargo feature. The server is synchronous (no async
//! runtime, Edge budget § 4) and binds 127.0.0.1 only.

pub mod api;
pub mod assets;
pub mod http;
pub mod mcp;
