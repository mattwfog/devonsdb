# devondb UI — design contract

The built-in UI provides a natural-language trust loop, interactive query
tree, durable pins, grid, graph, and GeoPoint map.

Read with `docs/ARCHITECTURE.md` § UI and § Edge budget (both binding) and
`docs/PLAN_IR.md` (the language the trust loop renders).

## 1. Mandate and scope

`devondb ui` starts a local server from the single binary and serves an
embedded SPA: query editor, plan-confirmation view (the trust loop), table
results, and force-directed graph rendering (ARCHITECTURE.md:162-169).

The query surface accepts natural language or DevonPlan text
(PLAN_IR.md:124-135). Both paths show the *canonical* spelling and
operator tree of what the engine will run before execution. Plans can be
pinned durably; browser-local session history remains a separate surface.

**Out of v1 scope:** multi-user/remote access (the server binds 127.0.0.1
only) and relationship-property display in the graph view.

## 2. The trust loop (product shape)

One page, three panels:

```
┌────────────────────────────────────────────────────────┐
│ editor: text-form plan input          [Explain] [Run]  │
├────────────────────────────────────────────────────────┤
│ plan view: canonical text + operator tree              │
│   "this is what I will run" — confirm here            │
├────────────────────────────────────────────────────────┤
│ results: table view ⟷ graph view (per result shape)   │
└────────────────────────────────────────────────────────┘
      sidebar: schema browser + plan history
```

Flow rules (binding):

1. **Explain-first.** Run always resolves through `/api/ask`; a NoParse
   response falls back to `/api/explain` for DevonPlan text. The UI renders
   the engine-confirmed canonical text + tree before execution. A
   keyboard-driven user can chain both (Cmd+Enter = explain + run), but the
   trust loop is not skippable, only fast.
2. **Canonical text is the artifact.** What the plan view shows is
   `print_plan`/`print_statement` output (the engine's own canonical
   spelling, main.rs:115-128), never a UI-side re-rendering of the input.
   Sugared input visibly normalizes — that *is* the trust demonstration.
3. **Errors are first-class.** Parse errors render inline at their 1-based
   character position (PLAN_IR.md:282-288 mandates position + token);
   engine errors render in the results panel verbatim.
4. **Pins and history are distinct.** The Pin button stores a named plan in
   the engine-backed pin store. Executed inputs also append to a browser-
   local session history that makes its local scope explicit.
5. **Schema browser.** Sidebar lists node/rel tables and columns from
   `/api/schema`; clicking a node table inserts a `nodes(T) as t` starter;
   clicking a node row's key in results opens the graph view centered on
   it.

## 3. Architecture

```
devondb-cli ── ui subcommand ──▶ devondb-server (feature "ui")
                                   ├─ http.rs    minimal sync HTTP/1.1
                                   ├─ api.rs     JSON handlers
                                   ├─ assets.rs  embedded SPA (build.rs)
                                   └─ assets/    SPA source (no build step)
                                          ▼
                                devondb (public facade: run/execute/
                                         explain via text::*, schema_summary)
```

- **Crate `devondb-server`**, workspace member, reached only through
  the CLI's `ui` cargo feature — the library core never pays for it
  (ARCHITECTURE.md:88-97). Depends on `devondb`, `devondb-nl`, and the
  workspace serialization crates.
- **No tokio, no async.** A hand-rolled synchronous HTTP/1.1 server over
  `std::net::TcpListener` (Edge budget §4, ARCHITECTURE.md:94-97).
  Scope makes this safe: it serves exactly one local single-user SPA.
  `Connection: close` per request — no keep-alive state machine; browsers
  cope fine on loopback. **Zero new external dependencies** —
  devondb-server depends on `devondb` plus the workspace's existing
  `serde`/`serde_json` (already in-tree via devondb-plan); no HTTP crate,
  no async crate.
- **Asset embedding via build.rs**: walks `assets/`, generates a
  `path → &'static [u8]` table with `include_bytes!`. Adding an asset file
  requires no registration edit.
- **Concurrency v1: `Mutex<Database>`.** `Database` is Send+Sync with a
  snapshot/transaction API, but the plan-running facade methods take
  `&mut self`. A local single-user explorer serializing requests through one
  mutex is correct and simple.
  A future read path may use `snapshot()` with read-only plan execution,
  while writes continue through the transaction API.
- **Bind 127.0.0.1 only. Never 0.0.0.0.** No auth in v1 *because* of that
  bind; any future `--host` flag is a design-block change, not a flag.
- Request body cap 1 MiB; unknown routes 404; malformed JSON 400 with the
  serde error message; engine errors 422 with `DevonError`'s Display.

## 4. HTTP/JSON API v0

All POST bodies and responses are JSON. Errors: `{"error": "<message>"}`
with the status codes above.

| Route | Body | Response |
|---|---|---|
| `GET /` and static paths | — | embedded SPA assets |
| `GET /api/schema` | — | `{"node_tables":[{"name","columns":[{"name","type","primary_key"}]}],"rel_tables":[{"name","from","to","columns":[…]}]}` + `pins: [{name, text, canonical}]` |
| `POST /api/explain` | `{"text": "…"}` | `{"kind":"query"\|"statement","canonical":"<text>","plan":<canonical JSON per PLAN_IR § JSON>}` — parse only, never executes |
| `POST /api/query` | `{"text":"…"}` or `{"plan":{…}}` | `{"columns":[…],"rows":[[…]],"row_count":n,"truncated":bool}` |
| `POST /api/statement` | `{"text":"…"}` or `{"statement":{…}}` | `{"ok":true}` |
| `POST /api/graph` | `{"table","key",<opt>"limit"}` | `{"nodes":[{"table","key","props":{…}}],"edges":[{"rel","from":{"table","key"},"to":{"table","key"}}],"truncated":bool}` — endpoints are table-qualified; bare keys are ambiguous across tables |

- `/api/query` caps returned rows at 10 000 (`"truncated": true` beyond) —
  the facade materializes `QueryResult` outside the engine's memory budget,
  so the server is the row-count backstop.
- `/api/graph` is implemented **server-side by composing DevonPlan trees**
  against the catalog: for each rel table incident to `table` (from
  `/api/schema`'s source), run
  `nodes(T) as n | filter n.<pk> = <key> | expand R out|in as m |
  project m.<pk>, m.<col>…` and assemble the neighborhood. No engine
  changes needed beyond schema introspection; caps at `limit` (default
  500) nodes. Edges carry rel-table name + endpoints only (v0 grammar
  limitation, § 1).
- Value JSON uses PLAN_IR's natural-JSON literal encoding. Non-finite
  `Float64` values and vector elements are emitted as
  `{"f64":"NaN"|"inf"|"-inf"}` so they never collide with JSON `null` = SQL
  NULL, matching the C FFI (PLAN_IR.md:84-98). This keeps one value language
  across every surface.

## 5. SPA toolchain (decision)

**No-build, framework-free, zero third-party code.** Hand-written
`index.html` + ES modules + CSS in `crates/devondb-server/assets/`. No
vendored libraries: the force layout is a hand-written simulation module
(`views/force.js` — velocity-Verlet integration, pairwise repulsion,
link springs, centering; O(n²) per tick is comfortably fine at the
500-node cap). No node toolchain, no package.json, no build step, no
CDN, **no network access at runtime** — an edge device's UI must work
fully offline. The 500-node cap does not require Barnes-Hut acceleration.

This keeps the whole SPA to tens of KB against the ≤ 10 MB binary budget
(ARCHITECTURE.md:88-89), and a local explorer's state fits comfortably in
plain modules. A framework is warranted only if the UI outgrows this model.

Structure:

```
assets/
  index.html         shell + panel layout
  style.css          hand-written; system font stack, dark/light via
                     prefers-color-scheme
  app.js             boot, state store, fetch wrappers, view registry
  views/plan.js      canonical text + operator tree renderer
  views/table.js     results table (virtualized past 1k rows)
  views/grid.js      schema-aware record grid
  views/schema.js    sidebar schema browser + history panel
  views/graph.js     force-directed neighborhood view
  views/force.js     hand-written force simulation (no vendored libs)
  views/map.js       offline GeoPoint map + within-query overlay
```

## 6. Graph view

- `views/graph.js` renders `/api/graph` neighborhoods: nodes labeled
  `table:key` (primary-key value), edges labeled with rel-table name,
  `views/force.js` simulation, drag to fix nodes, double-click a node to
  expand ITS neighborhood into the same canvas (incremental exploration).
- SVG rendering up to 500 nodes (the `/api/graph` cap); beyond that the
  server truncates and the UI says so. Canvas rendering remains a possible
  upgrade if measured graph sizes require it.
- Node color by table (stable hash → palette); props on hover card.
- Node display label: the first
  string-valued prop whose ASCII-folded name is `name`, then `title`, then
  `label` — else `table:key`. Identity stays discoverable (hover card and
  aria-label show both); node identity, dedupe, and simulation keys remain
  `table:key`-based.

## 7. Schema introspection

The UI reads catalog metadata through a compact, engine-free summary:

```rust
// devondb/src/introspect.rs — serializable, engine-free summary
pub struct SchemaSummary { pub node_tables: Vec<NodeTableSummary>,
                           pub rel_tables:  Vec<RelTableSummary> }
impl Database { pub fn schema_summary(&self) -> SchemaSummary }
```

The method reads the current committed catalog snapshot and is re-exported
through the public facade.

## 8. Interactive query tree

A DevonPlan query is an operator tree (PLAN_IR.md:76-90). The PLAN panel
renders that tree as an interactive, schema-driven builder:

- Every tree node offers ONLY its legal continuations, derived from
  `SchemaSummary`: filter lists the in-scope bindings' columns with
  comparators matched to column types; expand offers only rels incident
  to the binding's table (direction inferred from the matching endpoint,
  both offered on self-rels); project/sort offer in-scope columns;
  aggregate offers type-legal functions. Invalid trees are
  unrepresentable in the builder; the validator stays the final
  authority behind it.
- Three doors, one tree: typed DevonPlan parses into it; natural language
  compiles into it; clicks build it directly. Edits round-trip through
  the ENGINE printer — flow rule 2 (canonical text is the artifact)
  extends to the builder: the UI never re-renders its own spelling.
- `/api/explain` accepts `{"plan": …}` alongside `{"text": …}`
  (the untagged-request pattern `/api/query` already uses, api.rs) so
  every tree edit gets its canonical text + validation from the engine.
- The editor is an NL-first single box: input tries DevonPlan parse, then
  compiles through `IntentCompiler`; both land in the same tree. Grammar errors stop
  being the front door; NoParse hints render as clickable phrasings.
- Sidebar stays the schema overview; the tree contextualizes it — the
  builder's choice lists ARE the "what am I working with" surface, shown
  at the point of use.

## 9. Testing

- Server tests use a real `TcpStream` against a spawned
  server on an ephemeral port — no HTTP client dependency, hand-written
  request bytes, asserting exact status lines and JSON bodies.
- Static assets are covered by end-to-end asset assertions. JavaScript unit
  tests are warranted when client-side logic cannot be covered through the
  existing no-toolchain test surface.
- The CLI end-to-end test drives the binary over real HTTP, explains a plan,
  executes a query, retrieves a graph neighborhood, and loads the SPA shell.

## 10. Map view

A class whose summary carries a GeoPoint column gets a Map tab beside
grid/graph. Self-contained rendering per the edge promise — an SVG
equirectangular projection of stored points, no external tile server,
no network fetch; points cluster at low zoom by DevonGrid cell (the
congruent hierarchy is the clustering structure, free). A `within` query
result overlays center pin + great-circle radius disc + matched points;
click-through to the entity page.

## 11. Grid inline-edit → DML

Grid inline editing uses the engine's PK-addressed update and delete statement
surface. The grid emits only statements the engine can validate and execute.

### 11.1 Statement surface (PK-addressed, v1)

```text
update <Table> set <col> = <literal>[, <col> = <literal>…] where <pk-col> = <literal>
delete from <Table> where <pk-col> = <literal>
```

`Statement::UpdateNode { table, set: Vec<(String, Value)>, key_column,
key }` · `Statement::DeleteNode { table, key_column, key }`. The `where`
clause is EXACTLY the primary-key column, `=`, one literal — validation
rejects anything else. Predicate-driven bulk DML requires the executor in the
write path and remains a separate extension. Updating the PK itself is
rejected in v1 (it is delete+insert; the grid greys PK
cells). Set values are literals, not expressions — same law as insert
rows (statement.rs:62-71).

### 11.2 MVCC semantics

Row versions live in the committed overlay (MVCC.md §46, overlay.rs:
213-237). DML extends `CommitSummary` with PK-keyed entries: updates
carry replacement rows, deletes carry tombstones.

- **Scan merge**: persisted-group rows and older overlay rows are
  dropped when a newer summary in the view's chain shadows their PK
  (update) or tombstones them (delete); replacement rows are emitted
  from the overlay. Merge points are the existing `node_rows` sites
  (view.rs:388-418, checkpoint.rs:57, hnsw.rs:683-706).
- **Zone-map pruning stays sound unchanged**: a pruned group's rows are
  either absent from the result anyway or shadowed/tombstoned by PK —
  shadowing never needs the group's bytes, and replacement rows arrive
  from the overlay scan. No pruning change; a regression test asserts
  it (update a row inside a pruned-away group; result identical
  pruned vs unpruned).
- **Conflicts**: write-write on the same PK (update/update,
  update/delete, delete/delete, and either against a concurrent
  insert) is a commit-time conflict — the same PK law and message
  shape as commit.rs:775. First committer wins; the loser's commit
  errors whole.

### 11.3 Durability

Two new WAL payload record kinds beside the insert shapes
(FORMAT.md § WAL sidecar, :363-400): `{"update":{…}}`, `{"delete":{…}}`,
replayed into the same overlay semantics. Because a pre-DML binary
cannot correctly recover a WAL containing them, DML rides a
**NOT-read-safe feature bit with checkpoint-scoped lifetime**: set in
the superblock by the first update/delete commit since the last
checkpoint, cleared by the checkpoint that truncates the WAL (the
superblock already rewrites on both paths). Data at rest is plain
groups — a cleanly checkpointed file stays readable by any v1 binary;
only a crashed-with-pending-DML file refuses downlevel recovery, which
is exactly the §8.1 mask-union law applied to the WAL. FORMAT.md gains
the two record kinds + the bit in the SAME commit as the writer
(specs-are-law); the freeze permits this — feature bits and WAL record
kinds are the two extension mechanisms the frozen format reserved.

### 11.4 Checkpoint and the offset-stability law

**Node offsets are load-bearing**: rel edges address their endpoints by
stable global offset — checkpointed position + overlay position
(overlay.rs:475-487, `node_offset`; MVCC.md §3 rule 3) — and CSR groups
persist those offsets. Two consequences are LAW:

- **Updates preserve position.** Checkpoint rewrites an affected group
  with the replacement row in the SAME slot — the exact tail-rewrite
  mechanism (node_table.rs:160-201) generalized to interior groups,
  swapped by id in the catalog storage list. Offsets never move; no
  format change; superseded pages leak as today; zone maps recompute
  free at group write. Updates are therefore fully general in v1.
- **Deletes compact — and compaction shifts every later offset in the
  table.** Any CSR reference into the table (to ANY row, not just the
  deleted one) would dangle. Plain `delete from` therefore remains scoped to
  tables no rel table references (§11.5). The explicit relationship-aware
  form is `detach delete from <Table> where <pk-column> = <literal>`:
  it atomically tombstones the node and every incident edge, and its
  checkpoint remaps both physical CSR directions with the node offsets
  (`docs/DETACH_DELETE.md`). Persistent deletion vectors remain a separate
  not-read-safe format-extension option, not an implicit plain-delete mode.

Wear note: a one-row edit rewrites one group (~2048 rows); acceptable
v1.

### 11.5 Refusals

- HNSW-indexed tables allow scalar/vector updates and deletes. Visible mutation
  history routes KNN to exact scan until checkpoint rebuilds affected indexes
  over the materialized rows (`docs/HNSW.md` §5.7).
- **Delete refuses when any rel table names the node table as an
  endpoint** (offset-stability law, §11.4) — even if the addressed row
  has no edges. The refusal text stands and names the available explicit
  form: `detach delete from <Table> where <pk-column> = <literal>`. Update
  carries no such restriction; plain delete is never silently upgraded.
- `detach delete` also supports HNSW-indexed targets through the same exact
  fallback and checkpoint rebuild. Updating the PK remains rejected (§11.1).
- Rel tables still have no general predicate or single-edge DML in v1. The
  shipped detach operation is the deliberately narrower whole-incident-edge CSR
  rewrite; it does not create edge identity or a public `delete rel` surface.

### 11.6 API and grid UX

No new routes: the grid composes statement TEXT and sends it through
`POST /api/statement` (§4). The trust loop applies to writes doubly:
committing an edit shows the engine-canonical statement
(`print_statement`) in the confirm affordance — the user sees `update
Person set name = "Ada" where id = 7` before Enter lands it. Cell
double-click opens a type-aware input (Bool select, number field,
quoted-string field, explicit NULL control for nullable columns);
Esc cancels; row hover exposes Delete with the same canonical-text
confirm. Errors render verbatim per flow rule 3. After a landed write
the grid refetches its page — no client-side cache to lie.

The row-hover Delete affordance continues to compose plain `delete from` and
therefore preserves §11.5's schema-level refusal text. When that refusal names
an endpoint constraint, the manual statement form is
available through the same statement/confirm surface:
`detach delete from <Table> where <pk-column> = <literal>`. The UI does not
silently turn an ordinary Delete click into the potentially high-amplification
detach operation.

**Vector/GeoPoint cell editors.**
Double-click opens a literal TEXT editor pre-filled with the current
value's canonical spelling — `[0.25, -1.5]` for vectors (the text
language's `[` … `]` grammar, parser.rs `parse_vector`), `geo(lat, lng)`
for GeoPoint (docs/GEO.md §5 canonical form). The editor composes the
literal from the /api/query JSON value (vectors arrive as arrays,
GeoPoint in tagged natural form) and does NO client-side validation
beyond non-empty: the explain-confirm loop is the validator — a bad
literal's verbatim engine error IS the UX (flow rules 2 + 3). The
explicit NULL control applies as on scalar cells (all non-PK cells
offer NULL; schema has no nullable flag). §11.5 refusals are unchanged:
HNSW-indexed updates use exact KNN until checkpoint repair.
