# devondb ontology — entity classes

devondb uses entity classes so the system can understand what stored things
are, rather than treating every table as an opaque collection of columns.

Read with `docs/ARCHITECTURE.md`, `docs/NL.md` (the compiler this feeds), and
`docs/UI_INTERFACE_VIEWS.md` (the views this drives). The ontology gives the
compiler, UI, and programmatic consumers a shared description of the graph.

## 1. What v1 is (and deliberately is not)

v1 is a **class layer in the catalog over the existing engine** — pure
metadata with real consumers. It does NOT change row storage, MVCC,
execution, or the plan IR. Tables stay tables; classes make them legible
to the NL compiler, the UI, and programmatic consumers. Interface-typed query
execution is defined in § 7.

## 2. The class registry (catalog addition, pre-freeze)

- **NodeClass** — one per node table: `display_name` (singular),
  `plural`, `label_column` (PINNED — replaces the UI's name/title/label
  guess, UI.md § 6), `summary_columns` (the columns a card/grid leads
  with), `color`, `description`.
- **RelClass** — one per rel table: `verb` ("knows"), `inverse_verb`
  ("is known by"), endpoint role names ("follower"/"followed").
- **Interface** — a named property set (`Nameable { name: String }`,
  `Locatable { lat: Float64, lon: Float64 }`) using v0 types only. A
  node class DECLARES the interfaces it implements; declaration is
  validated against the table's actual columns (fold-matched) at DDL
  time. Interfaces are how "different classes of entities" stay
  composable: NL and views can target "everything Nameable" without
  caring which table it is.
- Defaults are derived, never required: an unannotated table gets a
  NodeClass synthesized from its name (display from the identifier,
  plural via the NL layer's folding, label via the existing heuristic) —
  the ontology deepens a database; it never gates one.

Storage uses a catalog-page section beside tables and indexes, updated through
the existing copy-on-write catalog machinery. `docs/FORMAT.md` § Catalog
defines the byte layout, and `docs/PLAN_IR.md` defines the statement grammar.

## 3. Consumers

1. **NL grounding** (`devondb-nl/src/ground.rs`): class `display_name`/
   `plural` join the table-resolution candidates (replacing pure
   morphology where declared); RelClass `verb`/`inverse_verb` ground
   traversal phrases — "who follows ada" works because the ontology says
   `Follows.verb = "follows"`, not because of string luck. Interfaces
   ground "everything with a name".
2. **UI**: `label_column` replaces the graph-view guess; class `color`
   replaces the hash palette; grids and entity pages (§ 4) are BUILT
   FROM `summary_columns`; the tree builder's choice lists show class
   display names.
3. **`SchemaSummary`** grows a `classes` section — one introspection
   surface, every consumer (UI, bindings, /api/schema) inherits it.

## 4. Class-driven views

Class-driven views join Table/Graph in the results panel and the
explorer:

- **Grid view** (Airtable-shaped): one class per tab, all rows,
  `summary_columns` first, sortable/filterable in the chrome (compiles
  to the same DevonPlan underneath — the trust loop holds; inline edits
  use the PK-addressed DML surface).
- **Entity page** (Foundry-shaped): one entity — label as title,
  properties as a card, linked entities GROUPED BY RelClass verb
  ("knows (3)", "works at (1)"), each group expandable, the
  neighborhood graph docked beside it. Reached by clicking any node,
  grid row, or result cell.
- **Ontology browser**: the sidebar schema panel grows into it —
  classes with colors, interfaces, verbs; clicking seeds queries (the
  existing starter mechanism).

## 5. Programmatic use

Every programmatic surface uses the same contract: deterministic NL in,
canonical plans out, durable pins, C/Python bindings, and a JSON API. The
ontology lets clients reason about the graph (for example, finding every
Nameable entity linked to Ada) without hardcoding table names. The ontology
is readable through every binding.

## 6. Further extensions

Interface-typed query sources (`nodes(Nameable) as n` — union scan);
class inheritance; computed properties; per-class permissions when the
server grows beyond loopback; ontology-aware KNN defaults (a class
declares its embedding column).

## 7. The executable interface layer

The interface layer makes ontology declarations available to query execution,
natural-language compilation, bindings, and UI views.

**The derive-first law (governs all of v2.0):** every v2.0 capability
derives from the bit-1 catalog exactly as FORMAT.md § Catalog `ontology`
specifies today — ZERO format changes, zero new corruption rules.
Explicit per-class declarations (`embedding`, `time`) are future extensions
(§7.5), gated on an ambiguous derivation, and must claim a feature bit per the
FORMAT extension law (unknown
ontology fields are corruption — FORMAT.md § Catalog `ontology`, final
rule — so they cannot ship bit-free).

### 7.1 Interface-typed query sources (ScanInterface)

`nodes(Nameable) as n` scans every node table whose class `implements`
the interface.

- **IR**: a new source operator `ScanInterface { interface, binding }`,
  additive in canonical JSON beside `ScanNodes` (existing pins are
  untouched; the JSON key and text keyword follow PLAN_IR.md's
  operator-naming conventions).
- **Semantics**: concatenation of the implementing tables' scans in
  catalog declaration order, each row projected to the interface's
  declared columns (types were fold-validated at DDL time, ONTOLOGY §2).
  Zero implementers is legal and yields zero rows. Zone-map pruning
  applies per underlying table scan unchanged.
- **Typing**: the binding exposes EXACTLY the interface's columns.
- **Class discriminator: `classof(binding)`**. `_class` is a legal bare user
  identifier, so
  an implicit column cannot satisfy both "exactly the declared columns"
  and "zero collision with user columns" — the expression form is the
  only shape meeting both. `classof(binding)` types String, is legal
  only on an interface-scan binding, and follows the standard Expr
  canonical JSON/text conventions recorded in PLAN_IR.md.
- **Name-resolution law (no format change)**: `nodes(X)` resolves X
  against node tables first, then interfaces. DDL refuses NEW collisions
  in both directions (creating an interface that takes a table's folded
  name, and vice versa) as InvalidArgument. A pre-existing collision in
  a released-file stays readable — the table shadows the interface, and
  `nodes()` of a shadowed interface fails with a targeted error naming
  the collision. Adding a catalog corruption rule post-freeze is not an
  option and is not needed.

### 7.2 Example-anchored KNN (KnnScan grows a vector source)

"people similar to ada" — KNN whose query vector comes from the graph.

- **IR**: `KnnScan.query` becomes a vector source: the existing literal
  `[f32]` array (unchanged JSON, full pin compatibility) or a scalar
  subquery in PLAN_IR's `{"scalar":{…}}` encoding. The scalar must type as
  `Vector(dim)` matching the scanned column.
- **Execution**: the scalar evaluates ONCE at operator open, on the same
  snapshot as the outer query. A NULL result is an
  evaluation error naming the subquery ("knn query vector is null");
  zero-row and 2+-row behavior follow PLAN_IR's scalar-query rules.
- **Ontology derivation**: NL grounds "similar to <entity>" only when
  the entity's class has EXACTLY ONE Vector column (the derived
  embedding); two or more refuse with the candidates named. The explicit
  `embedding` declaration is the v2.1 escape hatch.

### 7.3 Natural-language templates

Each is a separate template family with a golden corpus, refusals, and
an NL_VERSION bump, per NL.md's regime:

1. **Multi-hop traversal** — "friends of friends of ada"; hard cap 2
   hops in v2.0 (promoting the recorded v1 refusal, NL.md §3); 3+ hops
   keep refusing with the cap named.
2. **Time phrases** — "edited since last week / in the last 30 days /
   yesterday" over an Int64 epoch column the QUESTION names (grounded
   by the existing fold/morphology path). `current_date` is an input,
   never an expression, because a clock read
   contradicts pinned-plan determinism — the expression spec § Rejected
   shapes — and date arithmetic types over Timestamp, not Int64
   epochs. The **reference date is a
   compiler input** (additive `compile_at(question, schema,
   reference_date)`; NL.md's R1 determinism extends to (question,
   schema, reference_date) → identical plan), and the COMPILER performs
   the UTC day arithmetic host-side, emitting plain Int64 literal
   epoch-second bounds compared with existing `>=`/`<` — zero engine
   change, no new Expr, pins freeze their bounds at pin time (the pin
   law holds). Callers (CLI/REPL/server) supply the clock value at their
   boundary. Implicit per-class time columns remain a future extension.
3. **Similar-to** — §7.2's NL face: entity lookup → scalar projecting
   the derived embedding column → KnnScan, k defaulting to 10.

### 7.4 Consumers

- **Semantic-search clients** can combine similarity search with deterministic
  time filters through engine-native plans.
- **UI**: grid/entity views over interface scans
  (`docs/UI_INTERFACE_VIEWS.md`).
- **Bindings**: Python/C/JSON inherit ScanInterface through the existing
  plan surface with zero binding work.

### 7.5 Future extensions

Explicit class `embedding`/`time` declarations (feature-bit, FORMAT
extension law) · class inheritance · computed properties · per-class
permissions (server beyond loopback). Each requires a concrete use case.
