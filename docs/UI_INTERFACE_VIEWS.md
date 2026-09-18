# UI interface views — grid/entity over interface scans

Read with `docs/UI.md` (the binding UI contract), `docs/ONTOLOGY.md` § 7
(the binding executable-interface contract), `docs/PLAN_IR.md` (the
`ScanInterface`/`classof` spellings), and `docs/ARCHITECTURE.md` § Edge
budget (the binding memory law).

This document defines the built-in UI's views over ontology interfaces:
a grid over the heterogeneous rows a `ScanInterface` emits, an entity
view that expands one interface row into its concrete class's full
columns and neighborhood, and inline DML routed to that concrete row.
It composes existing engine surfaces without introducing new storage or plan
semantics.

## 0. Existing surfaces

The views consume the executable interface layer:

- `docs/ONTOLOGY.md:138-169` binds `ScanInterface`: concatenation in
  catalog declaration order, projection to EXACTLY the interface's
  declared columns, zero implementers legal, and the decided
  discriminator expression `classof(binding)` (types `String`, legal
  only on an interface-scan binding — the only shape satisfying both
  "exactly the declared columns" and "zero collision with user
  columns").
- `crates/devondb-plan/src/ops.rs:196-202` defines the additive
  `ScanInterface { interface, binding }` operator beside `ScanNodes`;
  `crates/devondb-plan/src/expr.rs:374` and `:549-573` define the
  canonical `{"classof":"<binding>"}` JSON form; `crates/devondb-plan/
  src/text/parser.rs:754-788` and `crates/devondb-plan/src/text/
  printer.rs:627-629` define `classof(b)` text. `docs/PLAN_IR.md:75`,
  `:166-178`, and `:403-416` record the binding spellings, including
  the no-implicit-discriminator law and folded-name shadowing.
- `crates/devondb/src/database/view.rs:393-448` defines the interface
  scan pipeline (per-table scans in catalog order, interface-column
  projections, deferred per-class sources); `view.rs:1123-1151` shows
  the executor layout: interface columns first, one trailing classof
  slot (`width: interface.columns.len() + 1`). The exact row shape —
  interface columns then the class string — is pinned by
  `crates/devondb/tests/scan_interface_e2e.rs:98-114` (rows like
  `["Ada", "Person"]` for `project entity.name, classof(entity) as
  class`) and `:164-165`.
- `docs/UI.md:107-133` is the API v0 surface this view extends:
  `/api/schema`, `/api/explain`, `/api/query`, `/api/statement`,
  `/api/graph`, the 10 000-row server backstop, and the graph
  neighborhood's table-qualified endpoint law.
- `crates/devondb/src/introspect.rs:53-70` and `:136-149` define
  `InterfaceSummary` (name + typed columns) inside
  `SchemaSummary.classes`; `introspect.rs:72-98` defines
  `NodeClassSummary` (display, label, color, `implements`). The SPA
  therefore already receives everything needed to enumerate interfaces
  and resolve a class string back to its table.
- The grid mechanics this view extends are in
  `crates/devondb-server/assets/views/grid.js:1-77` (500-row pages,
  plan composition), `:79-213` (class tabs, typed columns, result
  validation), `:390-417` (PK-addressed row context),
  `:419-437` (paging), and `:770-909` (the `docs/UI.md` §11 confirm loop:
  `/api/explain` the composed statement, show the engine's canonical
  text, land through `/api/statement`, then refetch).
- The entity panel is defined in
  `crates/devondb-server/assets/views/entity.js:1-35` (panel factory),
  `:37-82` (class card + ordered properties),
  `:84-109` (linked-entities section with truncation notice);
  `crates/devondb-server/assets/views/graph.js:302-335` is the
  `/api/graph` fetch that feeds it.

## 1. API surface: extend `/api/query` and `/api/graph`; no new routes

`docs/UI.md:186-202` names schema introspection the facade seam, and
the existing `SchemaSummary.classes` (`crates/devondb/src/introspect.rs:
136-149`) is that seam's interface face. The interface views need
nothing more from the server than `docs/UI.md` §7's introspection promise: the SPA
composes DevonPlan and the existing endpoints execute it. New routes
would duplicate `/api/query`'s plan path (`crates/devondb-server/src/
api.rs:108-115`), `/api/statement`'s DML path (`api.rs:117-126`), and
`/api/graph`'s neighborhood path (`api.rs:147-151`, `api.rs:443-456`)
without adding a capability.

### 1.1 Interface enumeration

`GET /api/schema` already returns, when any ontology declaration
exists:

```json
{"classes": {
  "interfaces": [{"name": "Nameable", "columns": [
    {"name": "name", "type": "String"}
  ]}],
  "node_classes": [{"table": "Person", "implements": ["Nameable"], …}]
}}
```

(exact shapes at `introspect.rs:53-98`). The SPA derives each
interface's implementer list by fold-matching `node_classes[i].
implements` against `interfaces[j].name` — the same fold discipline
the grid already uses for tables (`grid.js:25-49`) and the server uses
for graph endpoints (`api.rs:458-475`).

### 1.2 Grid request/response over an interface

Request: the SPA composes the plan and sends it through the untagged
`POST /api/query` body (`{"plan": …}`, `api.rs:108-112`). For interface
`Nameable` with binding `entity`, the canonical grid plan is:

```json
{"v": 0, "plan": {
  "op": "Project", "input": {"op": "ScanInterface",
    "interface": "Nameable", "binding": "entity"},
  "exprs": [
    {"expr": {"col": "entity.name"}, "as": "name"},
    {"expr": {"classof": "entity"}, "as": "class"}
  ]}}
```

`classof` is the required discriminator: it is the only legal
spelling (`docs/PLAN_IR.md:414-416` — the binding exposes NO implicit
column), it types `String` (`docs/PLAN_IR.md:75`), and the e2e pins
its output position and value shape (`scan_interface_e2e.rs:49-66`,
`:98-114`). The projection is mandatory, not cosmetic: it fixes the
wire column order (`name`, then `class`) so the SPA can validate the
response exactly as the table grid validates its columns
(`grid.js:196-207`), and it gives the class column a stable alias.

Response is exactly §4's query shape (`docs/UI.md:117`; serializer at
`api.rs:108-115` and `:780-789`):

```json
{"columns": ["name", "class"],
 "rows": [["Ada", "Person"], ["Atlas", "Project"]],
 "row_count": 2, "truncated": false}
```

Per-class column typing is derived client-side, not invented on the
wire: for row's `class`, the SPA looks up the `node_classes` entry and
the interface's declared column types from `/api/schema`. A class
string with no fold-matching `node_classes` entry is a malformed
result rendered verbatim as an error (flow rule 3, `docs/UI.md:43-64`)
— the SPA never guesses.

### 1.3 Entity expansion request/response

One interface row expands in two calls, both existing:

1. Full concrete columns: a `ScanNodes` + PK filter + full projection
   plan through `/api/query`, composed exactly like the server's own
   center lookup (`api.rs:596-618`), using the concrete class's
   `node_tables` entry for PK name, PK type, and column order. This
   is the same plan shape `/api/graph` already uses for its center
   node (`api.rs:447-448`).
2. Incident relationships: `POST /api/graph` with the CONCRETE
   `table` and PK `key` (`docs/UI.md:119` — endpoints are
   table-qualified; bare keys are ambiguous). The response shape is
   unchanged (`docs/UI.md:119`; builder `api.rs:443-456`, node/edge
   structs `api.rs:656-688`). The entity panel then renders with its
   existing card/link code (`entity.js:30-109`), fed by the class's
   `NodeClassSummary` display/label metadata (`introspect.rs:72-98`).

No interface-specific graph variant is added: the graph endpoint's
table-qualified law is the anti-ambiguity contract, and an
interface-qualified variant would need its own endpoint-resolution
rules for zero benefit — the row already knows its class.

## 2. Grid over an interface: column union, not per-class tabs

**Decision: one heterogeneous grid showing the union of the
interface's declared columns plus a trailing `class` column.**

- The union is exactly the interface's declared columns — the engine
  already projects every implementing table to that shape
  (`docs/ONTOLOGY.md:147-152`; `view.rs:416-429`), so there is no
  client-side coalescing to get wrong. "Union" here means the shared
  interface columns plus the discriminator; it is NOT the union of
  every class's extra columns.
- The `class` column is the heterogeneity surface: it is sortable and
  filterable like any `String` column (composing `Filter` over
  `{"classof": "entity"}`, legal per `docs/PLAN_IR.md:403-416` and
  validator enforcement at `crates/devondb-plan/src/validate.rs:763-
  772`), which gives the user per-class slicing inside one view.

**Rejected alternative: per-class tabs.** The grid already renders one
tab per node table (`grid.js:110-180`); adding one tab per interface
would (a) duplicate the table tabs for every implementer, (b) hide the
ontology's actual value — seeing `Person` and `Project` rows side by
side as `Nameable` things — behind navigation, and (c) degenerate to
the existing table grid with an extra click, adding no capability.
The interface grid is a DIFFERENT question ("all Nameable things"),
and the UI should show that answer as one result set. Per-class focus
remains available by filtering the `class` column, and full per-class
editing remains the existing table grid's job.

Concretely, `views/grid.js` gains an interface mode beside the table
mode: the picker lists interfaces from `schema.classes.interfaces`
(`introspect.rs:113-123`) after node tables; `createGridRequest`
(`grid.js:25-49`) accepts an interface target and composes §1.2's
plan with the same filter/sort/limit wrappers (`grid.js:51-77`);
`renderGridResult` validates `columns` against
`[interface columns…, "class"]` instead of a table's column list.

## 3. Entity view: one row → concrete class

The interface grid's rows gain an "open" affordance (row click or a
dedicated action cell beside the existing delete cell, `grid.js:390-
417`). Opening a row:

1. Reads `class` from the row and resolves it to the concrete
   `NodeTableSummary` + `NodeClassSummary` by fold match
   (`introspect.rs:72-98`).
2. Fetches full columns via §1.3's `ScanNodes` plan — the interface
   row carries only the interface's columns, so the entity view MUST
   re-query; it never fabricates absent columns.
3. Fetches the neighborhood via `/api/graph` with the concrete table
   and PK (`docs/UI.md:119`).
4. Renders with `createEntityPanel` (`entity.js:1-35`): the class
   card, the concrete class's ordered properties
   (`entity.js:186-215`), and the linked-entities groups
   (`entity.js:84-109`), including the existing truncation notice for
   the 500-node cap (`docs/UI.md:129-131`).

The entity view is reachable from the interface grid, the table grid,
and the graph view alike; the graph/map panels already share this
panel (`graph.js:54`, `map.js:77`), so the interface grid joins an
existing pattern rather than founding a new one.

## 4. Inline edit on interface rows: route to the concrete PK

The confirmation loop is the contract (`docs/UI.md` §11.6; implementation
`grid.js:770-909`): compose a PK-addressed statement, `/api/explain`
it, show the engine's canonical spelling, land it via
`/api/statement`, then refetch the page. Interface rows reuse it with
one addition — the row context must carry the CONCRETE class:

- `rowContext` (`grid.js:390-407`) currently derives `keyColumn` from
  the table's schema. The interface rowContext derives it from the
  row's `class` → concrete `NodeTableSummary`: `composeUpdate`
  (`grid.js:901-904`) emits `update <ConcreteTable> set <col> =
  <literal> where <pk-col> = <literal>` and `composeDelete`
  (`grid.js:906-909`) emits the matching delete, exactly §11.1's
  surface (`docs/UI.md` §11.1; statement variants
  `crates/devondb-plan/src/statement.rs:108-132`).
- Only interface-declared columns are editable from the interface
  grid: the concrete class's extra columns are not on the wire, so an
  edit affordance for them would silently compose against a value the
  row never carried. Editing those columns is the concrete class's
  table grid or entity view — the interface
  grid edits what it shows.
- PK cells stay read-only (§11.1's update-the-PK refusal,
  `docs/UI.md:335-337`); the class column is likewise read-only — it
  is a discriminator expression, not a stored column, and no DML
  surface can write it.
- Indexed updates use exact KNN until checkpoint rebuild. Plain delete still
  refuses relationship endpoints and names the explicit detach-delete form;
  errors render through the existing confirm loop (`docs/UI.md` §11.5).
- The trust loop is not bypassed: every interface-row edit passes
  through the same `previewWrite`/`landWrite` machinery
  (`grid.js:839-899`), showing `update Person set name = "Grace" where
  id = 2` — the concrete spelling — before Enter lands it.

## 5. Pagination, limits, and the budget law

The interface grid inherits the table grid's exact paging shape:
`Limit` with `count: 500` and offset (`grid.js:1`, `:51-77`,
`:419-437`), one page per request, refetch after every landed write
(`grid.js:890-891`) — no client-side row cache that could lie after a
DML commit.

The budget law is the server backstop, unchanged: `/api/query` caps
returned rows at 10 000 and reports `truncated` (`docs/UI.md:121-123`)
because the facade materializes `QueryResult` OUTSIDE the engine's
memory budget (`docs/UI.md:121-123`, citing the facade's
materialization). The interface grid's 500-row page sits far below
that cap; the cap exists for hand-typed plans, and an interface scan
over many implementers is precisely the kind of unbounded result the
backstop exists to bound. No interface-specific limit is added: the
engine's `Limit` pushdown already bounds what an interface page reads,
and `crates/devondb/tests/scan_interface_e2e.rs:231-266` pins that a
limited interface scan avoids reading later implementers' pages — the
paging behavior is provably budget-friendly today.

Entity expansion adds two bounded reads per open row (one PK-addressed
plan, one `/api/graph` capped at its default 500 nodes,
`docs/UI.md:129-131`), both far inside the backstop. No view ever
issues an unpaginated full-interface fetch.

## 6. The trust loop: the plan text the UI ran, pinnable

`docs/UI.md:43-64` binds explain-first and canonical-text-as-artifact.
The interface views obey it mechanically because they compose PLANS
and send them through `/api/explain` before execution
(`grid.js:1343-1358` does exactly this for the table grid):

1. Every interface grid load runs `POST /api/explain {"plan": …}` on
   the composed `ScanInterface` tree; the plan panel shows the
   ENGINE's canonical text and operator tree (flow rule 2), then
   `/api/query` executes that confirmed plan.
2. The canonical interface plan is pinnable through the existing
   `POST /api/pin` flow (`api.rs:128-133`) — `Plan::from_json` accepts
   `ScanInterface`
   and `classof` because they are ordinary v0 spellings
   (`docs/PLAN_IR.md:403-416`), and
   `crates/devondb/tests/scan_interface_e2e.rs:180-182` already pins
   an interface plan end-to-end. A pinned interface grid view
   therefore re-executes identically forever, including its catalog-
   order row layout (`scan_interface_e2e.rs:117-196`).
3. Every inline edit shows the concrete-class DML statement's
   canonical text before landing (§4), the same double-trust §11.6
   binds for table grids (`docs/UI.md:422-431`).

The SPA never re-renders a plan of its own invention: what the user
confirms is `print_plan`/`print_statement` output (`docs/UI.md:50-53`),
and what it pins is that same canonical artifact.

## 7. Tests (UI.md § 9 regime)

### Server E2E — `crates/devondb-server/tests/api.rs`

In the real-TcpStream pattern the file already establishes
(`tests/api.rs:47-153`), with the interface fixture shape from
`scan_interface_e2e.rs:75-96` (two tables implementing one interface,
rows in both):

1. `schema_reflects_interfaces_with_exact_json`: create the fixture
   through `/api/statement`, assert `GET /api/schema` carries
   `classes.interfaces` and `node_classes[].implements` exactly.
2. `interface_grid_plan_round_trips_with_discriminator`: POST the
   §1.2 plan to `/api/explain`, assert canonical text contains
   `nodes(Nameable)` and `classof(entity)`; POST to `/api/query`,
   assert exact `columns` and rows (interface columns then class).
3. `interface_grid_filters_by_class`: the §1.2 plan wrapped in
   `Filter eq(classof(entity), "Person")` returns only Person rows —
   proving per-class slicing composes on the wire.
4. `interface_entity_expansion_uses_concrete_table`: for one row,
   POST the concrete `ScanNodes` plan (assert full concrete columns
   including non-interface columns) and POST `/api/graph` with the
   concrete table/key (assert the known neighborhood, table-qualified
   endpoints).
5. `interface_row_edit_lands_concrete_dml`: compose
   `update Person set name = … where id = …` from an interface row,
   explain it (assert `kind == "statement"` and the concrete
   canonical text), land it, refetch the interface grid, assert the
   new value AND unchanged class; repeat delete with the §11.5
   rel-reference refusal asserted verbatim.

### SPA unit tests

- Asset assertions in the server end-to-end test verify that `GET /` serves a
  shell whose script imports the interface-grid module; grep-level assertions
  verify that
  `createGridRequest` handles an interface target and that the class
  resolver fold-matches `node_classes`.
- Pure plan-composition, class-resolution, and row-context functions can be
  exercised from a Node-free, browser-free assertion script invoked by the
  Rust end-to-end fixture. This preserves the no-build, no-network toolchain
  law in `docs/UI.md` §5.

On the seeded two-class fixture, the end-to-end test explains the interface
plan, runs it, filters by class, expands one row to concrete columns and its
neighborhood, and lands one concrete DML edit. This covers `ScanInterface` execution, schema
introspection, and the DML path together.
All five live in `crates/devondb-server/tests/api.rs`, the crate whose
handlers (`crates/devondb-server/src/api.rs`) they exercise.

## Rejected shapes

- **New `/api/interface-grid`, `/api/interface-entity` routes.** They
  would duplicate `/api/query` + `/api/graph` (`api.rs:108-115`,
  `:443-456`) and fork the trust loop; `docs/UI.md` §7 exists so
  the SPA composes plans against introspection instead.
- **An implicit `class`/`_class` column on interface rows.** The IR excludes
  it (`docs/ONTOLOGY.md` §7.1): user columns
  can legally collide with it, and the binding exposes EXACTLY the
  declared columns. The UI uses `classof` and gets the same
  information without a wire-format exception.
- **Per-class tabs for the interface grid.** §2's decision; they
  duplicate table tabs, fragment the interface's core answer, and
  reduce to the existing table grid.
- **Interface-qualified `/api/graph` requests.** Endpoints are
  table-qualified because bare keys are ambiguous (`docs/UI.md:119`);
  an interface variant would need its own resolution rules while the
  row already carries the concrete class.
- **Editing non-interface columns from the interface grid.** The row
  does not carry them; composing an update against a client-fabricated
  value would be a lie. The table grid and entity view own those
  columns.
- **A separate interface row-limit.** The 500-row page + 10 000-row
  server backstop already bound memory (`docs/UI.md:121-123`), and
  the limited-scan e2e proves interface paging avoids unread
  implementers (`scan_interface_e2e.rs:231-266`). A second constant
  would drift from the first.

## Presentation defaults

1. **Entity view.** The graph and map views use `createEntityPanel` as an aside
   (`graph.js:54`, `map.js:77`). The interface grid reuses the same panel.
2. **Class-column display metadata.** `NodeClassSummary` carries
   `display`/`color` (`introspect.rs:72-98`); the grid renders
   class chips with them (as the table tabs already do,
   `grid.js:127-158`).
3. **Interface grid default sort.** Unsorted interface scans emit
   catalog-declaration order per implementer (`docs/ONTOLOGY.md:
   147-152`). The grid adds no implicit sort; paging is stable within a
   snapshot, and user-chosen sorts compose `Sort` as usual.
