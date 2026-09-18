# DevonPlan IR — spec v0 (development)

DevonPlan is devondb's real query language: a typed, versioned logical plan.
The natural-language surface compiles to it; the executor executes it;
bindings emit it; golden tests pin it. **Status: v0 = development, may change
freely until the first tagged release; versioned and stable after that.**

Design rules:

- **Deterministic.** A DevonPlan has exactly one meaning. All nondeterminism
  (NL interpretation) lives above this layer.
- **Closed vocabulary, versioned.** Consumers reject plans whose `v` is newer
  than they understand — clean error, never a guess.
- **Two canonical serializations**: JSON (machine interchange, pinning) and a
  compact pipeline text form (REPL, docs, debugging). Both round-trip.

## Type system (v0)

Non-null logical column types are
`Bool | Int64 | Float64 | String | Vector(dim: u32) | GeoPoint |
Timestamp | Bytes | Decimal(precision: u8, scale: u8) | Json`. `Null` is
a value, not a separate column type: every logical column type admits
`Null`.

`GeoPoint` (`docs/GEO.md` §5) exists
everywhere in the IR (literals, projection, results), but DDL rejects
`GeoPoint` columns until the geo storage codec lands, and `GeoPoint` is
not scalar-comparable — comparisons, ordering, and sort keys reject it
until the geo predicate operators arrive. Its text spellings are
type `GeoPoint` and literal `geo(<lat>, <lng>)` (components in degrees;
the parser normalizes the two canonical foldings — `+180` longitude and
pole longitudes — and rejects anything else out of range).

The scalar-v2 types follow the GeoPoint pattern: the values exist everywhere
in the IR, but DDL rejects the column types until the scalar-v2 storage codec
lands (`docs/FORMAT.md` § Feature flag registry, `SCALAR_TYPES_V2`). Their
canonical spellings are:

| Type | DDL name | Text literal | Tagged JSON literal |
|------|----------|--------------|---------------------|
| `Timestamp` | `Timestamp` | `timestamp("<ISO-8601 UTC>")` — printer emits `YYYY-MM-DDThh:mm:ssZ` for years 0001–9999 and ISO-8601 expanded `+YYYYYY-MM-DDThh:mm:ssZ` / `-YYYYYY-MM-DDThh:mm:ssZ` outside that range (sign mandatory, exactly six year digits), with `.ffffff` iff nonzero; value is epoch-microseconds UTC | `{"ts": <i64 epoch-micros>}` |
| `Bytes` | `Bytes` | `bytes("<lowercase hex>")` | `{"bytes": "<lowercase hex>"}` |
| `Decimal(p, s)` | `Decimal(p, s)`, 1 ≤ p ≤ 38, s ≤ p | `decimal("<sign?digits[.digits]>")` — exact string, never a float | `{"decimal": "<same string>"}` |
| `Json` | `Json` | `json("<canonical text>")` through the standard string escape | `{"json": "<canonical text as JSON string>"}` |

Canonical-form law for `Json`: the carried text is the document's
canonical serde_json serialization (minimal spacing, preserved key
order); the parse boundaries that construct `Json` values enforce it —
a `json(…)` literal whose text is not valid JSON is a parse error.
Evaluation semantics: `Timestamp`
and `Decimal` are ordered scalars — comparisons, sort keys, `min`/`max`
work, with `Decimal` comparisons legal only at equal scales; `Bytes` and
`Json` admit equality only; none of the four joins arithmetic in v0.

(Planned, not in v0: Date, List, Struct.)

### Binding expression typing rules

Validation derives expression types from the input schema before execution.
`Null` is admitted anywhere an operand value is admitted and propagates at
runtime; it does not by itself select a non-null type. When a wholly
unconstrained null expression must become a projection, group, `min`, or
`max` output column, it uses `Bool` as the deterministic nullable carrier.
`sum(null)` uses an `Int64` carrier. These carriers admit the resulting
`Null`; they do not assert that the expression can produce a non-null value.

| Expression | Operand requirement | Result type |
|------------|---------------------|-------------|
| `+ - * /` | each operand is `Int64`, `Float64`, or `Null` | `Float64` if either non-null operand is `Float64`; otherwise `Int64` when a non-null operand fixes that type |
| `+ -` over `Decimal(p1,s)` / `Decimal(p2,s)` | equal scales (unequal scales refuse); `Null` admitted | `Decimal(min(max(p1,p2)+1, 38), s)` — exact, never rounded |
| `*` over `Decimal(p1,s1)` / `Decimal(p2,s2)` | `s1+s2 ≤ 38` else refused | `Decimal(min(p1+p2, 38), s1+s2)` — exact |
| `+ - *` over `Decimal(p,s)` / `Int64` | the Int64 promotes exactly to `Decimal(19,0)`; for `+ -` it is then scaled by `10^s` (overflow-checked at runtime) | as the Decimal rows above with `Decimal(19,0)` (or `Decimal(19+s, s)`) as the Int64's type |
| `/` with any `Decimal` operand · any `Float64` ∘ `Decimal` mix | — | refused at typing: use `round_div(numerator, denominator, places)`; a float operand would round |
| `= != < <= > >=` | same scalar type (`Bool`, `String`, or a numeric type), with `Int64`/`Float64` cross-comparison allowed | `Bool` |
| `and or not` | `Bool` or `Null` | `Bool` |
| `distance(a, b, metric)` | equal-dimension vectors; either operand may be `Null` | `Float64` |
| `scoreof(binding)` | `binding` originates in `TextScan` | `Float64` (integer Q32.32 conversion after ranking) |
| `classof(binding)` | `binding` is introduced by `ScanInterface` | `String` |

For arithmetic, `Int64`/`Int64` stays `Int64`; mixing either operand with
`Float64` promotes the operation and result to `Float64`. In particular,
`Int64 / Int64` is checked integer division truncated toward zero. Division
by zero and the `Int64::MIN / -1` overflow are errors. Division with a
`Float64` operand uses IEEE-754 division and returns `Float64`.
Decimal arithmetic is exact by construction (refuse-never-round): a runtime
result past 38 digits is an error naming the operator
and the declared `Decimal(38, s)` — never a wrap, never a rounding.

`count(expr)` accepts every value type and returns `Int64`. `sum(expr)` and
`avg(expr)` require numeric operands; `sum` preserves the operand's numeric
type and `avg` returns `Float64`. `min` and `max` preserve their operand type.
A `Filter` predicate must be `Bool` or `Null`.

A `Project` exposes each output under its alias; downstream expressions
reference an aliased output by that alias (when it has the required
`binding.column` shape). A directly projected column also retains its original
`binding.column` reference for downstream compatibility; other unprojected
input columns leave scope. `Aggregate` likewise exposes group keys under their
canonical expression text and aggregate results under their aliases. Duplicate
projection aliases, ambiguous projected alias/source references, duplicate
aggregate aliases, duplicate canonical group names, or any collision among
aggregate output names are validation errors.

## Statements

DDL and DML are statements; queries are operator trees.

| Statement | Fields |
|-----------|--------|
| `CreateNodeTable` | `name`, `columns: [{name, type, primary_key?}]` |
| `CreateRelTable` | `name`, `from: node_table`, `to: node_table`, `columns` |
| `InsertNode` | `table`, `rows: [[value…]]` |
| `InsertRel` | `table`, `rows: [{from_key, to_key, values}]` |
| `UpsertNode` | `table`, `rows: [[value…]]` — insert-or-replace by primary key: an absent key inserts, while a present key replaces the row wholesale like `UpdateNode`; lowered to insert + update WAL records at execute time, so it claims no WAL discriminator and no feature bit. Text form `upsert <Table> values (<lit>, …)[, …]`. |
| `UpdateNode` | `table`, `set: [{column, value}]`, `key_column`, `key` — PK-addressed (`docs/UI.md` §12.1) |
| `DeleteNode` | `table`, `key_column`, `key` — PK-addressed tombstone |
| `DetachDeleteNode` | `table`, `key_column`, `key` — PK-addressed node tombstone plus every visible incident relationship edge (`docs/DETACH_DELETE.md`). Exact text form: `detach delete from <Table> where <pk-column> = <literal>`. Exact tagged JSON: `{"v":0,"stmt":{"stmt":"DetachDeleteNode","table":"Person","key_column":"id","key":{"Int64":7}}}` |
| `CopyNode` | `table`, `path`, `sort_by?` — WAL-bypassing bulk CSV load (`docs/SCALE.md` §5) |
| `CreateInterface` | `name`, `columns: [{name, type}]` — ontology (docs/ONTOLOGY.md; catalog effect in FORMAT.md § ontology) |
| `CreateClass` | `table`, then optional clauses: `display`, `plural`, `label: column`, `summary: [column…]`, `color`, `description`, `verb`, `inverse`, `implements: [interface…]` — node-table targets admit the node clauses, rel-table targets admit `verb`/`inverse`; the validator enforces the split |

Ontology text form (clauses optional, fixed order as listed; strings are
ordinary string literals, column/interface references are identifiers):

```
create interface Nameable (name String)
create class for Person (display "Person", plural "people", label name,
  summary (name, role), color "#7aa2ff", description "a human",
  implements (Nameable))
create class for Knows (verb "knows", inverse "is known by")
```

| `PinPlan` | `name`, `text` (original input, provenance), `plan` (canonical plan JSON) — persists per FORMAT.md § Catalog `pins` |
| `UnpinPlan` | `name` |

Pin text form (`pin`/`unpin` are contextual words in statement-head
position, not reserved):

```
pin "ada friends" as nodes(Person) as person | filter person.name = "ada"
unpin "ada friends"
```

`pin <string> as <query>` captures the query's canonical plan; when a
pin is created from natural language (facade/API surfaces), `text`
carries the NL question and the compiled plan is stored — the statement
form shown above is what the printer emits with the canonical query
text.

`detach` is likewise a lowercase contextual word only in statement-head
position, not a reserved word. A table, column, or binding named `detach`
therefore remains a legal bare identifier. `print_statement` emits exactly
`detach delete from`, the canonically quoted table identifier, ` where `, the
canonically quoted key-column identifier, ` = `, and the canonical existing
`Value` literal. Text, printer, and tagged-JSON round trips follow the same
statement-envelope law as `UpdateNode` and `DeleteNode`.

(Planned: predicate-driven bulk DML, DropTable, DropClass.)

## Query operators (v0 tree)

Each operator consumes zero, one, or (for `HashJoin`) two children and
produces a stream of typed rows.

| Operator | Fields | Meaning |
|----------|--------|---------|
| `TextScan` | `table`, `column`, `query: String`, `k: u64`, `binding` | BM25 top-k over one String column; score descending, PK ascending |
| `ScanNodes` | `table`, `binding` | all nodes of a table, bound to a name |
| `ScanInterface` | `interface`, `binding` | all nodes whose classes implement an ontology interface, projected to exactly its declared columns |
| `Expand` | `rel`, `direction: out\|in\|both`, `from_binding`, `binding` | follow relationships from bound nodes |
| `ExpandRel` | `rel`, `direction`, `from_binding`, `binding`, `rel_binding` | follow relationships and bind their property values |
| `Filter` | `predicate: Expr` | keep rows where predicate is true |
| `Project` | `exprs: [{expr, as}]` | compute output columns |
| `Sort` | `keys: [{expr, order: asc\|desc}]` | total order |
| `Limit` | `count`, `offset?` | truncate |
| `Aggregate` | `group_by: [Expr]`, `aggs: [{fn, fraction?, expr, as}]` | `count`, `sum`, `min`, `max`, `avg`, and exact Decimal `percentile_cont` |
| `KnnScan` | `table`, `column`, `query: [f32] \| {"scalar":{"plan":<operator>}}`, `k`, `metric: cosine\|l2`, `mode?: exact\|approximate` | k nearest nodes by vector distance; a scalar query source must produce one `Vector(dim)` matching the scanned column. The result set and its order are deterministic, but runtime SIMD dispatch may produce last-ulp distance differences across CPU lane widths; construction and geo kernels remain bit-exact. |
| `WithinScan` | `table`, `column`, `center: GeoPoint`, `meters` | nodes whose GeoPoint column lies within `meters` of `center`; execution law in `docs/GEO.md` §7 |
| `HashJoin` | `join?: inner\|left`, `on: [{left: Expr, right: Expr}]`, `left`, `right` | equality join of two input pipelines; see § join semantics |

For `Expand`, `both` on a relationship whose endpoint tables differ is
exactly `out` when the source binding is the from-table and exactly `in` when
it is the to-table. When the endpoint tables coincide, `both` is the ordered
union of out-edges followed by in-edges, with a self-loop emitted once on the
out side.

`ExpandRel` follows exactly the direction, multiplicity and order rules of
`Expand`, pairing each neighbor with the properties of that same physical edge
occurrence. Its output is the input columns, neighbor-node columns under
`binding`, then relationship columns under `rel_binding`, each in catalog
order. Parallel edges remain separate rows; multiple self-loops remain
separate occurrences, each emitted once by `both`. Relationship bindings
expose only declared properties, with ordinary expression and NULL semantics;
they have no node identity and cannot be expanded from or used with `classof`.
Both introduced names must be fold-distinct from each other and existing
local/outer bindings, even for relationships without properties.

Canonical JSON adds the operator `ExpandRel` with fields `rel`, `direction`,
`from_binding`, `binding`, `rel_binding`, `input` in that order. Canonical text
is `expand_rel R out as n via e`, with optional `from b` immediately before
`as` following the existing Expand default/omission law. The nearest binding
becomes the neighbor node `n`. `expand_rel` and `via` are contextual words only
in their stage positions; old identifier and Expand spellings are unchanged.
Queries use the captured snapshot plus transaction-local writes, including
endpoint tombstones and physical offset holes. Properties survive checkpoint,
recovery and follower refresh under the existing storage laws. Resident query
working sets charge the shared budget and may refuse when they cannot fit.
See `RELATIONSHIP_QUERIES.md` for implementation and validation details.

## Expressions

Column refs (`binding.column`), interface class discriminators
(`classof(binding)`), literals, comparisons
(`= != < <= > >=`), boolean (`and or not`), arithmetic (`+ - * /`), and
`distance(vec_expr, vec_expr, metric)`, plus the conditional, UTC-day,
exact-Decimal, and scalar-query primary forms below.

Every expression in canonical JSON is an object with exactly one key. The
complete expression-key vocabulary is:

| Expression | JSON key | JSON value |
|------------|----------|------------|
| Column reference | `col` | A `binding.column` string |
| Interface class discriminator | `classof` | The interface-scan binding name as a string |
| Literal | `lit` | A natural JSON literal, as defined below |
| Equality | `eq` | `[left, right]` |
| Inequality | `ne` | `[left, right]` |
| Less than | `lt` | `[left, right]` |
| Less than or equal | `le` | `[left, right]` |
| Greater than | `gt` | `[left, right]` |
| Greater than or equal | `ge` | `[left, right]` |
| Boolean conjunction | `and` | `[left, right]` |
| Boolean disjunction | `or` | `[left, right]` |
| Addition | `add` | `[left, right]` |
| Subtraction | `sub` | `[left, right]` |
| Multiplication | `mul` | `[left, right]` |
| Division | `div` | `[left, right]` |
| Boolean negation | `not` | An expression |
| Vector distance | `distance` | A distance object, as defined below |
| Conditional `if(cond, then_expr, else_expr)` | `if` | `{"cond":<expr>,"then":<expr>,"else":<expr>}`; evaluates only `then_expr` when `cond` is true and only `else_expr` when false or NULL |
| First non-NULL `coalesce(expr, expr[, ...])` | `coalesce` | `[<expr>,<expr>,...]`; evaluates left-to-right and returns the first non-NULL value, or NULL when all are NULL |
| Minimum `least(expr, expr[, ...])` | `least` | `[<expr>,<expr>,...]`; evaluates every operand left-to-right, skips NULLs, and returns the leftmost minimum or NULL when all are NULL |
| Maximum `greatest(expr, expr[, ...])` | `greatest` | `[<expr>,<expr>,...]`; evaluates every operand left-to-right, skips NULLs, and returns the leftmost maximum or NULL when all are NULL |
| UTC day truncation `date_trunc("day", timestamp_expr)` | `date_trunc` | `{"unit":"day","value":<expr>}`; returns the UTC-midnight Timestamp bucket containing the operand, with NULL propagated |
| Checked UTC-day offset `date_add("day", timestamp_expr, day_count_expr)` | `date_add` | `{"unit":"day","value":<expr>,"amount":<expr>}`; adds checked exact UTC days and propagates NULL |
| Exact Decimal rounding `round(decimal_expr, places)` | `round` | `{"value":<expr>,"places":<u8>}`; rounds midpoint ties away from zero while retaining the operand's declared Decimal type |
| Exact rounded Decimal division `round_div(numerator, denominator, places)` | `round_div` | `{"numerator":<expr>,"denominator":<expr>,"places":<u8>}`; computes the exact rational quotient and rounds once, with no Float64 intermediate |
| Scalar subquery `scalar(query)` | `scalar` | `{"plan":<operator>}`; the embedded operator shares the containing envelope's version, produces NULL for zero rows, its sole value for one row, and an error for two or more rows |

Every binary expression value is an array of exactly two expressions. Unknown
keys, objects with zero or multiple keys, and binary arrays with any other
arity are invalid.

Literal values use natural JSON rather than tagged `Value` objects:

| DevonPlan value | Natural JSON |
|-----------------|--------------|
| `Null` | `null` |
| `Bool` | `true` or `false` |
| `Int64` | An integer JSON number |
| `Float64` | A JSON number with a fractional or exponent form |
| `String` | A JSON string |
| `Vector` | An array of JSON numbers, each decoded as `f32` |
| `GeoPoint` | The tagged object `{"geo":{"lat_deg":<f64>,"lng_deg":<f64>}}` |

On decoding, a JSON number in `lit` with integer syntax becomes `Int64`; any
other JSON number becomes `Float64`. Thus `30` is `Int64(30)`, while `30.0`,
`30.5`, and `3e1` are `Float64` values. Integer literals outside the `Int64`
range are invalid. JSON arrays are vectors in v0. The only admitted object
literal is the tagged `geo` form above — its payload must be canonical
(`docs/GEO.md` §5: finite, lat in [-90, 90], lng in [-180, 180), lng 0 at
the poles; unknown fields rejected); every other object literal is invalid.

Distance uses the object form
`{"distance":{"left":<expr>,"right":<expr>,"metric":"cosine"|"l2"}}`.
The distance object contains exactly the `left`, `right`, and `metric` fields.

## Canonical JSON form

Top level: `{"v": 0, "plan": <operator>}`. Operators are
`{"op": "<Name>", ...fields, "input": <operator>?}`. Example:

```json
{
  "v": 0,
  "plan": {
    "op": "Project",
    "exprs": [{"expr": {"col": "p.name"}, "as": "name"}],
    "input": {
      "op": "Filter",
      "predicate": {"gt": [{"col": "p.age"}, {"lit": 30}]},
      "input": {"op": "ScanNodes", "table": "Person", "binding": "p"}
    }
  }
}
```

## Text form (complete v0 grammar)

The text form is the human surface: what the REPL reads and what docs and
`EXPLAIN` print. It is an exact alternative spelling of the same IR — never
a different language. JSON remains the interchange and pinning form. A text
plan carries no version marker; parsing yields `v` equal to the parser's
`PLAN_VERSION`.

The same plan as above:

```
nodes(Person) as p | filter p.age > 30 | project p.name as name
```

### Lexical structure

Tokens are separated by any run of ASCII space, tab, CR, or LF. There are
no comments in v0. The token set:

- Punctuation: `|` `(` `)` `[` `]` `,` `.` `->`
- Operators: `=` `!=` `<` `<=` `>` `>=` `+` `-` `*` `/`
- Reserved words (always lowercase; other casings are ordinary
  identifiers): `and` `or` `not` `true` `false` `null` `as` `by` `asc`
  `desc` `out` `in` `both` `from` `to` `nodes` `expand` `filter` `project`
  `sort` `limit` `offset` `aggregate` `knn` `distance` `if` `coalesce`
  `least` `greatest` `date_trunc` `date_add` `round` `round_div` `scalar`
  `cosine` `l2` `count` `sum` `min` `max` `avg` `percentile_cont` `create`
  `insert` `into` `values` `node` `rel` `table` `primary` `key` `update`
  `set` `delete` `where` `upsert` `copy` `let` `join` `on` — 60 words.
  The lexer's `Keyword` table is the authority (`crates/devondb-plan/src/
  text/lexer.rs`).
- Identifiers, numbers, strings, defined below.

Type names (`Bool`, `Int64`, `Float64`, `String`, `Vector`) are NOT
reserved: they are contextual words matched exactly (case-sensitive) in
type position only.

`classof` is contextual rather than reserved: immediately followed by `(` in
expression-primary position it starts the discriminator expression; elsewhere
it remains an ordinary identifier.

**Identifiers.** Bare: `[A-Za-z_][A-Za-z0-9_]*`, excluding reserved words.
Any other non-empty name — spaces, dots, reserved words, any Unicode — is
written backtick-quoted: `` `my table` ``. Inside backticks the escapes are
`` \` `` `\\` `\n` `\r` `\t` and `\u{H}` (1–6 hex digits naming a Unicode
scalar value); all other characters stand for themselves. Empty identifiers
(` `` `) are invalid.

Identifiers retain the spelling written in the plan, while schema and binding
resolution compare the ASCII-lowercase fold of both names. Only ASCII `A`
through `Z` fold; non-ASCII bytes compare exactly. This applies to table,
relationship, column, index, and binding names, including both halves of every
`binding.column` reference and the table/column fields of `KnnScan` and index
DDL. Names that are fold-equal collide wherever that namespace requires
uniqueness. Catalog-backed result labels use catalog column spelling; plan
printing and validation errors preserve the spelling supplied by the plan.
String values are never folded. Reserved words and contextual type words keep
their lexical rules above, so wrongly-cased keywords remain errors rather than
case-insensitive keyword matches.

**Integers.** `[0-9]+`, optionally preceded by a folded minus (below). Must
fit `Int64`, else a parse error. Counts (`limit`, `offset`, `k`,
`Vector(dim)`) reject negative values.

**Floats.** `[0-9]+ ('.' [0-9]+)? ([eE] [+-]? [0-9]+)?` with at least one
of the fractional or exponent parts present. A number token is `Float64`
exactly when it contains `.`, `e`, or `E`; otherwise it is `Int64` —
identical to the JSON rule. There is no text spelling for non-finite
floats (JSON has none either).

**Minus folding.** `-` is always lexed as its own token. In operand
position (the start of an expression or after an operator), a `-`
immediately followed by a number token folds into a negative numeric
literal; the IR has no unary-minus expression. In binary position it is
subtraction: `a - 42` is `sub`, `a - -42` is `sub` with literal `-42`.
A `-` in operand position not followed by a number is a parse error.

**Strings.** Double-quoted. Any Unicode except unescaped `"`, `\`, and
control characters U+0000–U+001F. Escapes: `\"` `\\` `\n` `\r` `\t`
`\u{H}` (as in identifiers).

**Vectors.** `[` elements `]`, comma-separated, each an integer or float
number token (minus folding applies), decoded as `f32`. `[]` is the empty
vector.

### Expressions

Precedence, loosest to tightest; binary operators associate left except
comparisons, which do not associate at all (`a < b < c` is a parse error:
parenthesize):

| Level | Operators | IR |
|---|---|---|
| 1 | `or` | `or` |
| 2 | `and` | `and` |
| 3 | `not` (prefix) | `not` |
| 4 | `=` `!=` `<` `<=` `>` `>=` (non-assoc) | `eq` `ne` `lt` `le` `gt` `ge` |
| 5 | `+` `-` | `add` `sub` |
| 6 | `*` `/` | `mul` `div` |
| 7 | primary | — |

Primary: a literal (`null`, `true`, `false`, number, string, vector), a
column reference, `classof(binding)`, `distance(expr, expr, metric)` with metric `cosine` or
`l2`, `if(cond, then_expr, else_expr)`, `coalesce(expr, expr[, ...])`,
`least(expr, expr[, ...])`, `greatest(expr, expr[, ...])`,
`date_trunc("day", timestamp_expr)`, `date_add("day", timestamp_expr,
day_count_expr)`, `round(decimal_expr, places)`, `round_div(numerator,
denominator, places)`, `scalar(query)`, or a parenthesized expression. The
first `date_trunc` and `date_add` arguments are grammar-level syntax and accept
exactly the lowercase string literal `"day"`. Each `places` position is a
nonnegative integer literal in `0..=38`, not an expression. A `scalar`
argument is one complete query, including canonical `let` declarations; it
rejects statements, and its closing parenthesis is the one matching the
`scalar(` opener.

**Column references.** `binding.column`, each part an identifier (bare or
quoted). The IR `col` string is the two unescaped parts joined with `.`;
the printer splits an IR `col` string on its FIRST `.` (the engine's own
convention) and quotes each part as needed. An IR `col` string containing
no `.` cannot be printed (such a plan fails validation anyway).

### Queries (pipelines)

```
query    := ('let' name '=' pipeline ';')* pipeline
pipeline := source ( '|' stage )*
source   := 'nodes' '(' table_or_interface ')' 'as' binding
          | 'knn' '(' table '.' column ',' vector_source ',' int ',' metric (',' 'approximate')? ')'
          | 'within' '(' table '.' column ',' geo ',' number ')'
vector_source := vector | 'scalar' '(' query ')'
stage    := 'expand' rel direction ('from' binding)? 'as' binding
          | 'expand_rel' rel direction ('from' binding)? 'as' binding 'via' rel_binding
          | 'filter' expr
          | 'project' item (',' item)*            item := expr ('as' name)?
          | 'sort' key (',' key)*                 key  := expr ('asc'|'desc')?
          | 'limit' int ('offset' int)?
          | 'aggregate' agg (',' agg)* ('by' expr (',' expr)*)?
          | ('left')? 'join' name 'on' expr
agg      := ('count'|'sum'|'min'|'max'|'avg') '(' expr ')' 'as' name
          | 'percentile_cont' '(' 'decimal' '(' '"0.5"' ')' ',' expr ')' 'as' name
```

`knn(...)` arguments are positional: vector column as `table.column`, the
query vector source, `k`, the metric. A vector source is either the historical
literal array or `scalar(query)`, using the same spelling and complete
embedded-query grammar as scalar expressions. Sources start a pipeline;
stages require one. `expand` may appear anywhere after a source; `aggregate`
without `by` has an empty `group_by`.

**interface-scan semantics (ontology v2).** In a schema-aware parse,
`nodes(X) as b` resolves `X` against node tables first and then ontology
interfaces under the catalog's ASCII-folded name rule. A table therefore
retains the existing `ScanNodes` spelling; an unshadowed interface lowers to
`ScanInterface { interface: X, binding: b }`. The schema-free parser keeps
the historical table interpretation, so consumers parsing interface plans
must supply schema input.

`ScanInterface` concatenates the implementing node tables' scans in catalog
declaration order and projects every row to exactly the interface's declared
columns, in declaration order. An interface with zero implementers is legal
and emits zero rows. The binding exposes no implicit discriminator column:
`classof(b)` returns the implementing node-class name as `String` and is legal
only when `b` was introduced by `ScanInterface`.

If a released catalog contains a node table and interface with fold-equal
names, the table wins text resolution. An explicit `ScanInterface` naming the
shadowed interface is invalid and reports both catalog objects. Unknown
interfaces use the standard folded-name suggestion path.

`percentile_cont` accepts exactly the grammar-level fraction
`decimal("0.5")`; it is not an arbitrary expression. Its canonical aggregate
JSON item is exactly `{"fn":"percentile_cont","fraction":"0.5","expr":<expr>,"as":"alias"}`
in that field order. `fraction` is required for `percentile_cont` and forbidden
for the five existing aggregate functions. The input and result are exact
Decimal values, NULL inputs are skipped, and interpolation that cannot fit the
declared Decimal type is an evaluation error.

**knn semantics (v0).** `knn(T.c, …)` binds `T`'s columns under the
binding `T`, so downstream stages reference them as `T.column` exactly as
after `nodes(T) as T`. Its result rows additionally carry one trailing
OUTPUT-ONLY column named `distance` (`Float64`, ascending): it appears in
results but is not a referenceable binding — `distance` contains no `.`,
so no column reference can name it. To filter or sort on distance,
compute `distance(T.c, <query>, <metric>)` explicitly; a `project` stage
drops the output-only column like any unprojected column.

The `query` field is a two-variant vector source. A literal retains the exact
historical bare `[f32]` JSON array, preserving every existing plan and pin
byte-for-byte. A scalar source uses the exact
`{"scalar":{"plan":<operator>}}` encoding (no nested version envelope) and
prints as `scalar(<query>)`. Its complete inner plan validates under the
scalar rules and must expose exactly one output column. For this v1 source the
subquery is uncorrelated: no enclosing binding is visible inside it. The sole
output must have type `Vector(dim)` exactly matching the scanned column's
logical `Vector(dim)`; a different dimension or any non-Vector output is a
validation error naming both types. Cardinality and NULL behavior at execution
remain the scalar-query rules plus ONTOLOGY.md §8.2's KNN NULL refusal.

**knn `mode`.**
`KnnScan` carries an optional `mode`. `exact` is the default AND the
absent-field spelling in canonical JSON — an exact plan never serializes
the field, so every plan pinned before this field existed keeps exact
semantics byte-for-byte. `approximate` permits a matching installed index
to serve the scan; rows beyond the index's coverage are always scanned
exactly, and without a matching index an approximate scan runs the exact
operator. The text form appends `, approximate` after the metric;
exact mode prints nothing:

```
knn(Document.embedding, [0.5], 2, cosine, approximate)
```

**within semantics (v0).** `within(T.c, <geo>, <meters>)`
is a source: like `knn`, it binds `T`'s columns under the binding `T`.
`T.c` must be a `GeoPoint` column (any other type is a validation error
naming the column and its actual type); `center` is a canonical GeoPoint
(in JSON, GeoPoint's canonical object form directly — the field's type is
fixed, so the tagged `{"geo":…}` spelling remains an EXPRESSION-literal
disambiguator only; in text, the `geo(lat,lng)` literal — validating
serde folds and rejects exactly as for stored values); `meters` is a
finite JSON number > 0 (canonical JSON prints it like any other number;
non-finite or non-positive is a validation error).

Result law (binding): `within(T.c, g, m)` emits exactly the rows that
`nodes(T) as T` followed by a filter keeping rows where the great-circle
distance from `T.c` to `g` is ≤ `m` meters would emit, in `ScanNodes` row
order. The comparison is inclusive. Rows whose `T.c` is NULL never match.
The normative distance is `great_circle_meters` in `docs/GEO.md` §4's
deterministic kernels — identical bits on every platform, forever, so a
pinned `within` plan is reproducible like every other plan. There is no
output-only distance column: `within` is membership, not ranking. An explicit
distance expression is outside v0. How the executor accelerates this with DevonGrid
disc coverings — and the equivalence fence that keeps the accelerated
path honest — is `docs/GEO.md` §7.

**join semantics.** Multi-source pipelines
use `let` subplans and the `join` stage:

```
let recent = nodes(Commit) as c | filter c.pushed = false;
nodes(Repo) as r | join recent on r.pk = c.repo_pk | project r.name, c.hash
```

- **`let` is text-form sugar.** `let name = pipeline ;` binds a complete
  pipeline to a query-scoped name; a `join` stage's `name` argument
  references one. Subplans inline into the IR tree — they have NO IR
  representation, and a name is usable any number of times (each use
  inlines a copy). Referencing an unknown name is a parse error
  listing the defined names; defining a name twice, or a name that
  collides with a binding, is a parse error. `let` names follow
  identifier rules and the same case-folding as bindings.
- **IR.** `join name on expr` lowers to `HashJoin { join, on, left,
  right }`: `left` = the pipeline so far, `right` = the named subplan's
  tree. `left join name on expr` sets `join: "left"`; plain `join` is
  inner, and inner is the absent-field spelling in canonical JSON
  (KnnScan `mode` precedent).
- **Equality keys only.** The `on` expression must be one equality or a
  conjunction (`and`) of equalities. The parser decomposes it into the
  `on` key list; each pair's sides are ordered so `left` references
  only left-input bindings and `right` only right-input bindings
  (either textual order is accepted and normalized). Any other `on`
  shape is rejected with: non-equi predicates belong in a following
  `filter` stage. Key types must be exactly equal (no implicit casts);
  Vector and GeoPoint keys are a validation error naming the column.
- **NULL keys never match** — a row whose key is NULL joins nothing
  (matching the engine's comparison semantics elsewhere), though a
  `left` join still emits the unmatched left row.
- **Bindings.** Output rows carry the union of both inputs' bindings:
  left's columns then right's, in each input's own column order. The
  two inputs' binding names must be disjoint (validation error naming
  the duplicate). For a `left` join's unmatched left rows, every
  right-input column is NULL.
- **Deterministic order law.** Output order is left-input row order;
  for each left row, its matches appear in right-input row order. This
  is normative — a pinned join plan is reproducible forever, like every
  plan.
- **Canonical printing.** The printer emits each join's right subtree
  as a `let` binding with canonical generated names `j1`, `j2`, … in
  depth-first pre-order of the joins, then the main pipeline. User
  `let` names are not preserved (they inline away); the round-trip law
  holds on the IR: `parse(print(P)) == P`, and canonical text is a
  fixpoint of `print ∘ parse`.

### Statements

```
create node table Person (id Int64 primary key, name String, bio Vector(768))
create rel table Knows from Person to Person (since Int64)
create rel table Likes from Person to Person
insert into Person values (1, "Ada", [0.1, 0.2]), (2, "Grace", [0.3, 0.4])
insert rel into Knows values (1 -> 2, 1843), (2 -> 1, 1957)
detach delete from Person where id = 7
```

- `column-def := name type ('primary' 'key')?`; a parenthesized column
  list requires at least one column — a rel table with no property columns
  omits the parens entirely.
- Node-insert rows are `(` value literals `)` in schema-column order.
- Rel-insert rows are `(` from_key `->` to_key, then property values `)`;
  a rel with no properties is `(1 -> 2)`.
- The grammar is schema-free: name resolution, arity, and typing are the
  validator's job, never the parser's.

### Defaults (sugar) and canonical printing

Canonical text is the SHORTEST spelling: the printer omits everything the
parser defaults, and the defaults are defined so omission round-trips.

| Construct | Omitted form | Default |
|---|---|---|
| `expand … from b` | no `from` | the binding introduced by the nearest preceding `nodes`/`expand` stage; error if none |
| `project e as n` | no `as` | alias = the canonical printed text of `e` |
| `sort e asc` | no order | `asc` |
| `limit n offset 0` | — | `offset` omitted iff IR offset is absent (`None`); `offset 0` is preserved |
| rel-table `()` | no parens | empty column list |

The printer emits: ` | ` between stages, `, ` in lists, spaces around
binary operators and `->`, no space around `.` or inside `()`/`[]`, and
the minimal parentheses precedence requires (a right operand at equal
precedence keeps its parens: `a - (b - c)`). Aliases equal to their
expression's canonical text are omitted; other defaults likewise.

### Round-trip law

For every valid plan `P` and canonical text `T`:
`parse(print(P)) == P` and `print(parse(T)) == T`. Because `nodes(name)` is
intentionally shared by table and interface scans, the first law uses the
schema-aware parser for plans containing `ScanInterface`. Sugared input
normalizes: `print(parse(input))` is the canonical spelling of what the
parser accepted. Both properties are tested for every operator, statement,
expression form, and default.

### Errors

Parse errors are `InvalidArgument` and carry the 1-based character
position and the offending token text. Minimum content: an unknown stage
word names itself and the v0 stages; a wrongly-cased keyword (`Filter`)
hints the lowercase spelling; comparison chaining says "parenthesize";
a count position rejecting a negative number says which argument.
Expression nesting is limited to 128 levels; the token that would enter level
129 is rejected with `expression nesting exceeds 128 levels`.
A `scalar(query)` contributes one expression level outside every expression
in its embedded operator tree, so nested scalar queries obey the same 128/129
fence.

## Versioning

`v` bumps when an operator or field changes meaning. Adding a new operator or
optional field keeps `v` but consumers reject unknown operators by name —
additive growth is safe because the vocabulary is closed per version. Pinned
plans store their `v` forever; the engine keeps executing every released `v`.

## TextScan and score provenance

`textscan(Document.body, "rust graph", k=10) as d` is an additive source.
Its exact JSON shape is `{"op":"TextScan","table":"Document","column":"body",
"query":"rust graph","k":10,"binding":"d"}`. `table` resolves to a node table,
`column` resolves to String, and `k` is in `1..=i64::MAX`. Query text is data
and is tokenized by FULLTEXT.md; it never introduces plan syntax. Ranking
and fixed corpus statistics are governed by that document. Zero recognized
query tokens produce zero rows. Execution requires the `fts` feature.

`scoreof(d)` has canonical expression JSON `{"scoreof":"d"}` and Float64
output. Its value converts the selected integer score as
`score as f64 / 4294967296.0`; integer ordering and primary-key ties determine
top-k before this conversion. The name `scoreof` is contextual, so ordinary
identifiers bearing that name keep their old meaning outside a function call.

The score belongs to a TextScan binding, not a schema column. Validation
tracks that provenance through filter, project (even literal-only project),
sort, limit, expand, joins and correlated scalar scopes. Aggregate operands
may use it; aggregate output drops source score provenance. An unmatched
right side of a left join has NULL score. `scoreof` on ScanNodes, an unknown
binding, or a binding dropped by aggregation is invalid. Downstream filters
apply after TextScan's own top-k; they do not refill rejected ranked rows.

Plans containing TextScan anywhere reserve aliases/binding names beginning
with `\0devondb-scoreof\0` under ASCII folding, including aliases in sibling
join branches and nested scalar plans. This prevents collisions with private
score metadata while preserving prior arbitrary aliases in all plans without
TextScan. User properties named `score` and `_score` remain ordinary columns.
No persisted storage bytes, old canonical forms, or plan version change.
