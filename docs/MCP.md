# MCP — devondb as a tool server for Claude Desktop

Static project knowledge is retrieval-only: it does not execute structured
queries, and fragment retrieval cannot reliably perform arithmetic over the
whole dataset. The MCP surface runs queries while minimizing the amount of
data added to the model's context.

Read with `docs/NL.md` (the compiler), `docs/UI.md` § 2 (the trust loop),
`docs/ONTOLOGY.md` (the tree the schema tool walks).

## §1 Objective function

**Tokens per correct answer.** Every byte the server returns is paid for
in the model's context on every later turn of the conversation. The
server is judged by how little it says, not how much. Laws:

1. **Shape first, data on demand.** `schema` returns the tree — tables,
   columns, types, counts, interfaces, classes, pins — one line each. No
   JSON. A model that has seen the tree composes exact questions.
2. **Rows are bounded twice.** A row window (`limit` default 50, max 500;
   `offset`) AND a body byte cap (8 KiB). Whichever trips first ends the
   body; the footer says what was cut and how to narrow (filter,
   aggregate, `limit`/`offset`). The total row count is always stated —
   the model learns scale without paying for rows.
3. **Types ride the header, cells stay bare.** The header names each
   column with its plan-IR type once (`at:Timestamp`, `amount:Decimal` —
   the type is read off the first non-null value, so Decimal carries no
   declared precision; the scale is visible in the cell); cells are
   the bare value — no `decimal("…")` / `timestamp("…")` wrappers, no
   quotes around strings. Tab-separated; a cell containing a tab or
   newline is escaped `\t` / `\n`. `null` is spelled `null`.
4. **Vectors are never printed.** A `Vector(768)` cell renders as
   `<vector 768>`. Dumping embeddings is the canonical bloat.
5. **Cells are clipped** at 160 chars with `…(+N)`. Bytes render as
   `0x` + hex, clipped the same way. JSON renders raw, clipped.
6. **The plan line is always returned.** `ask` leads with `plan:` and the
   canonical engine-printed text — the trust loop (`docs/UI.md` § 2)
   over MCP: the model (and the human reading the transcript) sees the
   exact deterministic plan that produced the rows. Never the plan JSON
   tree; `explain` prints canonical text only.
7. **Refusals are compact and actionable.** A no-parse returns the
   unrecognized tokens with nearest-name suggestions, the recognized
   ones, and up to three nearest phrasings — the same report
   `devondb ask` prints, without JSON key overhead.
8. **Tool descriptions are terse.** Tool schemas are injected into every
   turn of every conversation; every word there is paid for forever.

## §2 Read-only law

The server opens with `mcp::open`: `Database::open_read_only_with` first —
a true follower (zero WAL footprint, no writer lease) when the file is
multiprocess-activated; a plain file refuses that open with
`InvalidArgument` (`options.rs:426-434` requires the coordination flag),
and then opens exclusively via `open_with`. Either way, read-only-ness is
the tool surface's law, not the open mode's. Mutations are refused at
every entry: a statement compiled from `ask` or parsed by `query` returns one line naming
the canonical statement and that the server is read-only. Pin creation is
likewise out. A writer surface, if ever wanted, is a separate flag and a
separate confirm design — the UI's confirm-land loop has no analog when
the confirmer is the model.

## §3 Tool surface (exactly four)

| Tool | Params | Returns |
|---|---|---|
| `schema` | `counts?: bool` (default true) | the tree (§4) |
| `ask` | `question: string`, `limit?`, `offset?` | `plan:` line, blank line, result table — or a refusal |
| `query` | `text?: string` (plan language) or `pin?: string`, `limit?`, `offset?` | result table |
| `explain` | `text: string` | canonical plan text, or the parser's positioned error |

New functionality is new template coverage in `devondb-nl`, not new tools.

## §4 The tree

```
nodes
  Person (id:Int64 pk, name:String, age:Int64)  1,203 rows
  Company (id:Int64 pk, name:String, revenue:Decimal(18,2))  87 rows
rels
  Knows: Person -> Person (since:Int64)  4,410 rows
  WorksAt: Person -> Company  1,190 rows
interfaces
  Party (name:String) <- Person, Company
classes
  Person "Person"/"people" label=name summary=(name, role) implements Party
  Knows verb "knows" inverse "is known by"
pins
  top_customers: nodes(Company) as c | … | limit 10
```

Sections with nothing to say are omitted. Classes print only what was
declared or derived beyond the table name. Counts are streaming
ungrouped aggregates (`aggregate count(pk)`), one per table, bounded by
the memory budget; `counts:false` skips them.

## §5 Transport

Newline-delimited JSON-RPC 2.0 over stdin/stdout, synchronous (Edge
budget § 4 — no tokio; a synchronous loop without the runtime).
`initialize` echoes the client's protocol version; `tools/list`,
`tools/call`, `ping`; `resources/list` and `prompts/list` answer empty;
notifications get no reply. Tool failures are tool-level results
(`isError: true`) carrying the engine's message, never protocol errors —
the model reads the message and adapts. Nothing in the process writes to
stdout except the serve loop; diagnostics go to stderr.

Registration (Claude Desktop `claude_desktop_config.json`):

```json
{"mcpServers": {"acme": {"command": "devondb", "args": ["mcp", "/path/to/acme.devondb"]}}}
```

One server process per file; `--memory-limit` applies as everywhere. A
`devondb pack` container (`docs/SCALE.md` §7) opens the same way — the
read-only distribution form is the natural thing to hand a team.

## §6 Placement

`crates/devondb-server/src/mcp/` — `mod.rs` (loop + dispatch), `tools.rs`
(definitions + handlers), `render.rs` (tree, table, refusal, budgets).
The server crate is the surfaces crate (SPA + JSON API) with exactly the
dependencies MCP needs (`devondb`, `devondb-nl`, `serde_json`). CLI
subcommand `devondb mcp <path>` behind cargo feature `mcp` (on by
default, like `ui`; the library core never depends on it).

## §7 Tests (UI.md § 9 regime)

- `crates/devondb-server/tests/mcp.rs`: in-process, drives `Mcp::handle_line`
  with hand-written JSON-RPC lines over a seeded database; asserts exact
  rendered text — the tree with counts, the `plan:` line and rows for
  "who does ada know", the refusal shape, `limit`/`offset` windows and
  the truncation footer, the byte cap, `<vector N>` in place of numbers
  (covering law 4), the read-only refusal for a
  statement, unknown-tool as a tool-level error, notifications silent.
- `crates/devondb-cli/tests/mcp.rs`: spawn the binary with
  `devondb mcp <path>`, write `initialize` + `tools/call ask` to stdin,
  read the JSON-RPC reply from stdout, and assert the rows.

## §8 Out of scope (v1)

Writes over MCP; pin creation; resources/prompts capabilities; streaming
results.

## §9 The native bundle

The current workspace version remains `0.1.1`; artifacts produced from this
unreleased checkout are development builds, not the published `v0.1.1`
artifacts. A new versioned release must follow `docs/RELEASING.md`.

`tools/mcpb/build.sh` produces `devondb-<version>-<Rust target>.mcpb`, a
plain zip with `manifest.json` (manifest_version 0.3), `LICENSE`, and the
geo crate's `THIRD_PARTY_LICENSES.md` at the root, a binary server at `server/<Rust target>/devondb`, and
`user_config.database` as a required file picker. The manifest declares the
matching Linux or macOS platform and shows the architecture in its display
name. The target-specific path is also its command and entry point.

`sh tools/mcpb/smoke.sh` builds and proves the PACKED native binary answers
`ask` over MCP stdio through the manifest's own command/args, discovers the
four tools, and refuses a write. Paths with spaces are exercised. Native
release jobs cover Linux GNU and macOS, each on x86_64 and aarch64; execution
on another platform is verified only when its CI runner passes. See
`tools/mcpb/README.md` for artifact selection, prerequisites and limitations.

On macOS, open the matching `.mcpb` with a compatible Claude Desktop client
to install and pick the `.devondb` file. Linux bundles require an MCPB-capable
Linux client or manual stdio registration. The install dialog and file picker
are not exercised by the script. MCPB 0.3 declares platforms but has no CPU
architecture constraint, so the user must select the matching target.
