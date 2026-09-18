# devondb NL — the deterministic intent compiler

The deterministic compiler is the default natural-language surface. No
language model runs in the core experience, either as a fallback or as a
tiebreaker. An optional, feature-gated LLM compiler may be configured
explicitly; the deterministic path works with networking disabled and no LLM
present.

Read with `docs/PLAN_IR.md` (the compile target — text form and grammar at
PLAN_IR.md:164-354) and `docs/UI.md` § 2 (the trust loop this slots into).

## 1. The determinism law (R1)

```
compile(question, schema_summary, NL_VERSION) → identical Plan bytes, forever
```

Same question + same catalog + same vocabulary version = byte-identical
canonical plan JSON. No randomness, no scores, no confidence — a
deterministic compiler either produces exactly one plan or refuses with a
structured report. "Confidence" is LLM-era thinking; here the trust loop
(explain-first, UI.md § 2) carries the trust, and the pin store
freezes plans, not questions — a pinned plan re-executes identically
forever without the compiler even loaded (PLAN_IR.md:10-13 puts all NL
nondeterminism above the IR; this design removes it from the compiler too).

For template families that need a calendar anchor, §15 extends the input to
`compile_at(question, schema_summary, reference_date, NL_VERSION)`. The same
question, schema, UTC reference date, and version produce identical plan bytes.
The reference date is caller data; the compiler never obtains it from a clock.

## 2. Grounding law: every content word grounds, or the compile refuses (R2)

After normalization (§ 4), every remaining token must be one of:

1. a **vocabulary word** (closed categorized tables, § 5),
2. a **catalog name** (table / column / rel, resolved per § 6), or
3. a **literal** (number, quoted string, or a bare capitalized word in a
   value slot).

One ungrounded content word → no plan. The compiler never drops a word it
doesn't understand and never guesses a meaning. This is the line between a
deterministic compiler and a parlor trick: `"people older than 30"` refuses
(nothing grounds `older` — no synonym dictionary in v1) and the refusal
teaches the working spelling: `people with age over 30`; `over` is vocabulary
(§ 5 comparators), while `older` is not. The refusal report names
the unrecognized word and shows the nearest template instantiated with
what DID ground (§ 7).

Ambiguity is an error, never a choice (R3): if `note` grounds fold-equal to
two tables (or a table and a column in the same slot), the compiler refuses
and lists every candidate. Determinism includes refusing identically.

## 3. v1 question shapes (the scope bar)

Six shapes compile in v1. Each is a template family (§ 5) that emits one
canonical pipeline:

| # | Shape | Example | Canonical plan |
|---|-------|---------|----------------|
| Q1 | list a table | `show all people` | `nodes(Person) as person` |
| Q2 | property filter | `people with age over 30` | `nodes(Person) as person \| filter person.age > 30` |
| Q3 | entity lookup | `who is Ada` / `show ada` | `nodes(Person) as person \| filter person.name = "Ada"` |
| Q4 | one-hop traversal | `who does ada know` | `nodes(Person) as person \| filter person.name = "Ada" \| expand Knows out as other \| project other.name` |
| Q5 | count | `how many people have age over 30` | `… \| aggregate count(person.id) as count` |
| Q6 | top-N by column | `top 5 people by age` | `nodes(Person) as person \| sort person.age desc \| limit 5` |

Compositions of these (filter + count, traversal + filter on the far end,
top-N of a filtered set) compile when the template grammar composes them
(§ 5); v1 caps traversal at ONE hop.

**Explicitly out of v1** (each refuses with its nearest template, and each
is a recorded v2 intent): ~~multi-hop chains ("friends of friends of ada")~~
— promoted in NL_VERSION 5 (§ 14) ·
~~KNN by example ("people similar to ada" — needs an entity-vector subquery;
the IR has no subplan operator, PLAN_IR.md:76-90)~~ — promoted in
NL_VERSION 8 (§ 17) · adjective synonym
grounding ("oldest", "richest" — no synonym dictionary; `by <column>`
phrasing is the v1 contract) · number words ("five oldest" — digits only) ·
statements (DDL/DML stay text-form; NL compiles queries only in v1,
matching the `show`/`find`/`list`/`who` verb scope).

## 4. Normalization pipeline

Deterministic, order-fixed, allocation-light:

1. Lowercase-fold ASCII (the engine's own `fold`, schema.rs:17-23 — never
   full-Unicode lowercasing, so NL matching agrees with identifier
   resolution).
2. Tokenize on whitespace and boundary punctuation (`?`, `,`, `.`, `!`,
   `;`, `:`, `(`, `)` dropped; quoted strings survive as single literal
   tokens; digits/floats lexed with PLAN_IR's number rule,
   PLAN_IR.md:216-224); a dot-led digit run stays whole and ungrounded, so
   `.5` refuses rather than silently becoming `5`.
   ASCII `'` stays inside a word for possessive stripping (a curly apostrophe
   stays literal), `_` stays inside identifier-like names, and `-` stays
   inside hyphenated names and values.
3. Strip possessives (`ada's` → `ada`).
4. Drop filler words (closed list: `the a an of please me all my out`) —
   filler is the ONLY droppable category; every other unmatched word
   triggers R2.
5. Record each original spelling alongside its folded form — value slots
   use the ORIGINAL spelling (string values are never folded,
   PLAN_IR.md:212), and refusal reports quote the user's spelling.

Plural folding happens at GROUNDING time, not normalization (§ 6), because
`people` must try `people` → `person` against the catalog while a quoted
`"people"` value must stay untouched.

## 5. The vocabulary and template grammar

The vocabulary is DATA: const tables in `devondb-nl`, versioned as
`NL_VERSION` (starts at 1, bumps on any vocabulary/template change so a
refusal or compile is reproducible per version). Categories:

- **LIST verbs**: `show list find get display give` (+ `who what which` as
  question heads)
- **COUNT heads**: `how many` · `count` · `number` (after filler-drop,
  `number of people` → `number people`)
- **COMPARATORS** (map to PLAN_IR comparison keys, PLAN_IR.md:101-110):
  `over above more greater after` → `gt` · `under below less fewer before`
  → `lt` · `at least` → `ge` · `at most` → `le` · `is equals equal exactly`
  → `eq` · `not is not` → `ne`
- **CONNECTORS**: `with whose where having has have` (introduce a filter
  clause) · `and` (conjoin filter clauses → nested `and`) · `by` (sort
  key) · `top first` / `bottom last` (sort direction + limit)
- **TRAVERSAL frame**: `does <entity> <rel-phrase>` (out) /
  `<rel-phrase>s <entity>` (in) — direction comes from the template's
  subject/object positions (R4), never from guessing.
- **SIMILAR-TO junction** (§ 17): `similar to` between the table and the
  entity. `like` is deliberately NOT vocabulary — it is not a synonym, and
  grounding refuses it by name.

Templates are ordered most-specific-first over category sequences with
typed slots (`<table> <column> <value> <rel> <n>`); first full match wins;
the order is part of `NL_VERSION`. A template must consume EVERY token
(R2) — partial matches refuse.

Rel-phrase grounding: a rel table `Knows` grounds `know`, `knows` (fold +
plural/`-s` folding both directions). Multi-word rel names (`LIVES_IN`)
ground their underscore-split word sequence (`lives in`, and `live in` by
the same folding).

## 6. Catalog grounding

Slot grounding resolves a folded token (or token run) against
`SchemaSummary` (introspect.rs:50-55) in this order per slot type:

1. exact fold-match (resolution compares folds,
   PLAN_IR.md:204-211);
2. plural candidates: strip `-s` / `-es` / `-ies`→`-y`, plus a small
   irregulars table (`people`→`person`, `children`→`child`, `men`→`man`,
   `women`→`woman`, `feet`→`foot`, `teeth`→`tooth`, `geese`→`goose`,
   `mice`→`mouse`) — applied to BOTH the token and the catalog name so
   `person` matches a table named `People` too;
3. no match → the slot fails; the report runs `did_you_mean` (schema.rs)
   over that slot's namespace for the suggestion.

Two matches in one slot → ambiguity refusal (R3), listing every candidate.

**Entity value slots (Q3/Q4)**: `ada` in a value position filters on the
table's LABEL COLUMN — the first string-valued column whose folded name is
`name`, then `title`, then `label` (THE SAME heuristic the UI graph view
ships, UI.md § 6 — one heuristic across the product, R5). No label column →
the template refuses and says which table lacks one.

**The literal is GROUNDED to the stored spelling, at compile time.** String
comparison in the IR is case-sensitive and stays that way (string values are
never folded, PLAN_IR.md:212) — so the compiler resolves the value the same
way it resolves every other slot: against live data, before emitting. `ada`
grounds to the stored `Ada` and the plan reads `person.name = "Ada"`.

This is the § 6 grounding law applied to values rather than an exception to
it, and it is the same mechanism § 12's `within` template uses to
resolve a place name to a geo literal at compile time. Its rules are the
grounding rules, unchanged:

- Exactly one stored value folds equal → emit that value's EXACT stored
  spelling. This is resolution, not a semantic the IR lacks: the emitted plan
  is an ordinary case-sensitive `=` that a pinned plan re-executes forever.
- Zero matches → refuse, listing near misses via `did_you_mean`. Never emit a
  filter that is known at compile time to match nothing.
- Two or more distinct stored spellings fold equal (`Ada` and `ADA`) → refuse
  as ambiguous and list every candidate (R3). Never guess which one was meant.
- No database available (compile-only, e.g. plan preview without an open
  file) → emit the ORIGINAL spelling as typed, since grounding is impossible;
  the trust loop still shows exactly what will run.

R8 is preserved: the compiler fakes no semantics the IR lacks. A
fold-insensitive string OPERATOR remains a possible future engine expression;
it is not required for this, and adding one would not change these rules.

## 7. Refusal: the NoParse report

```rust
pub struct NoParse {
    pub recognized: Vec<Grounded>,     // token → what it grounded to
    pub unrecognized: Vec<Ungrounded>, // token + did_you_mean suggestion
    pub nearest: Vec<TemplateHint>,    // ≤ 3, ranked by grounded-slot count
}
```

A `TemplateHint` is the template's example phrasing instantiated with the
slots that DID ground (`top <n> people by <column>` with `people` filled).
Ranking is deterministic: grounded-slot count desc, then template order.
The CLI prints it as "closest working phrasings"; the UI renders hints as
clickable candidates. Never a guess, never an execution.

## 8. Architecture and seams

New crate `devondb-nl`, depending ONLY on `devondb` (for `Plan`,
`SchemaSummary`, `DevonError`, and the re-exported `fold`/`did_you_mean`
— lib.rs:17-19). Zero external dependencies. The FACADE never depends on
devondb-nl (the edge budget core stays NL-free; ARCHITECTURE.md § Edge
budget); consumers compose:

```
devondb-cli  ──ask──▶ devondb-nl::DeterministicCompiler ──Plan──▶ devondb
devondb-server /api/ask ──▶ same trait object            ──Plan──▶ devondb
```

```rust
pub trait IntentCompiler {
    fn compile(&self, question: &str, schema: &SchemaSummary) -> Compiled;
    fn compile_at(
        &self,
        question: &str,
        schema: &SchemaSummary,
        reference_date: i64,
    ) -> Compiled {
        self.compile(question, schema)
    }
}
pub enum Compiled { Plan(Plan), NoParse(NoParse) }
```

`DeterministicCompiler` is the only implementation v1 ships and the only
one ever constructed by default. `LlmCompiler` implements the same
trait behind the `nl-llm` cargo feature (BYO endpoint config; compile
prompt carries schema + the PLAN_IR grammar; output = JSON plan, validated
by the same validator, rendered through the same trust loop) — wired only
when explicitly configured. The deterministic path runs with the feature
absent (R6).

Surfaces: CLI `devondb ask <path> "question"` → print
canonical text + plan tree → confirm → execute (the REPL gains an
`ask …` form too); server `POST /api/ask {"text"}` → `{"kind":"query",
"canonical",…}` on success (the SPA's existing explain-render path,
app.js:261-270) or `{"noparse": <report>}`.

## 9. Pin store

Pins persist PLANS, not questions: `{name, canonical plan JSON, nl_text,
created_lsn}` in the catalog page through the existing copy-on-write catalog
machinery. The format is documented in FORMAT.md § catalog. Facade API:
`pin(name, &Plan, source_text)`, `pins()`,
`run_pin(name)`, `unpin(name)` — zero devondb-nl dependency, so a pinned
plan outlives the compiler entirely (the product promise: pinned plans
never need the language model again, README). Text-form statement
spellings (`pin`, `unpin`) are additive grammar. The UI Pin button uses this
store while browser-local history remains separate (UI.md § 2 rule 4).

## 10. Testing regime

- **Golden question corpus**: (question, catalog DDL,
  expected canonical text) triples, table-driven — including every § 3
  example and every composition. The corpus drives
  `compile` and asserts exact canonical text, then executes against a
  seeded database and asserts exact rows.
- **Refusal corpus**: questions that MUST refuse (every out-of-scope § 3
  item), asserting the unrecognized words and the nearest-template names.
- **Determinism property**: every corpus question compiled twice
  byte-identically; the corpus file IS the NL_VERSION regression gate.
- **Validator round-trip**: every compiled plan passes `validate` against
  the same summary it was compiled from (R8, asserted in the compiler's
  own tests).

## 11. Offline acceptance

With networking disabled and no LLM configured, `devondb ask people.devondb
"who does ada know"` shows the canonical plan, executes on confirmation, and
returns the expected rows. A pinned form of the same question re-executes
identically in a fresh process with the NL compiler omitted from the binary.

## 12. Within-clause template (NL_VERSION 2)

`<shape> within <number> <unit> of <place>` — a filter clause composable
with the §3 shapes whose subject binds a class with exactly one GeoPoint
column ("Locatable" is that property, derived, never declared). Compiles
to `WithinScan` (PLAN_IR § Query operators) when it is the pipeline
source, or is refused with the nearest-template hint otherwise (v1 keeps
`within` source-position only, matching the operator).

- Units, by pinned exact constants: `mile|miles|mi` = 1609.344 m ·
  `kilometer|kilometers|km` = 1000 m · `meter|meters|m` = 1 ·
  `foot|feet|ft` = 0.3048 m. Any other unit word refuses (R2 — no
  guessing).
- `<place>` grounds per the grounding law to exactly one entity whose
  class is Locatable; zero or multiple candidates refuse with the
  candidate list. The entity's stored location is resolved AT COMPILE
  TIME to a GeoPoint literal in the emitted plan (`docs/GEO.md` §6):
  the confirmed/pinned plan carries coordinates, so entity moves never
  silently retarget a pin. A bare coordinate pair also grounds:
  "within 2 miles of geo(45.5152, -122.6784)".

## 13. Statement templates (NL_VERSION 4)

NL_VERSION 4 adds a statement-only compilation seam without changing the
existing query outcome or its consumers:

```rust
pub enum CompiledStatement {
    Statement(devondb::Statement),
    NoParse(NoParse),
}

pub trait IntentCompiler {
    // Existing compile and compile_with_database methods are unchanged.
    fn compile_statement(
        &self,
        input: &str,
        schema: &SchemaSummary,
    ) -> CompiledStatement;
}
```

The provided `compile_statement` method returns an all-ungrounded `NoParse`;
implementations opt in explicitly. `DeterministicCompiler` recognizes these
ordered, most-specific-first, all-token-consuming templates:

```text
set <table> <pk-value> <column> to <value>
change <table> <pk-value> <column> to <value>
delete <table> <pk-value>
remove <table> <pk-value>
```

`<table>` and `<column>` use §6 catalog folding and retain catalog display
spelling in the emitted statement. `<pk-value>` and `<value>` use the existing
DevonPlan literal grammar and must match, respectively, the table primary-key
type and target-column type. Type mismatch, an unknown or ambiguous catalog
name, an attempt to update the primary key, or any unconsumed token refuses
with the structured §7 `NoParse` and PK-addressed `TemplateHint`s. The new
closed vocabulary words are `delete`, `remove`, `set`, `change`, and `to`;
their category and the template order above are part of NL_VERSION 4.

The compile target is exactly the existing `Statement::UpdateNode` or
`Statement::DeleteNode` from `docs/UI.md` §11.1. There is no new engine
mutation surface and no predicate-driven or bulk DML. In particular, a phrase
such as `delete all people over 40` MUST refuse and show the PK-addressed forms
rather than compiling a scan or silently dropping its predicate. Possessive or
implicit-table forms such as `set ada's age to 39` likewise refuse: selecting a
table from an entity value would violate R2.

## 14. Two-hop traversal templates (NL_VERSION 5)

NL_VERSION 5 promotes § 3's recorded multi-hop refusal with a hard two-hop
limit. The ordered, all-token-consuming family accepts these surface shapes:

```text
<rel-phrase> of <rel-phrase> of <entity>
who do the <table> <entity> <rel-phrase> <rel-phrase>
```

Thus `friends of friends of ada` and `who do the people ada knows know`
compile as two traversals. The `of` and `the` words follow § 4's existing
filler rule; they delimit the surface phrasing but carry no independent plan
semantics. In the clause form, `<table>` must ground to the node class reached
by the first hop. Every other content word remains subject to R2.

Each relationship phrase independently uses § 6's existing one-hop grounding:
a declared RelClass `verb` or `inverse` is tried first, followed by the
relationship-table fold and morphology fallback. Endpoint types must compose:
the far table of hop one is the center table of hop two. The phrase order in
`X of Y of Ada` is inside-out (`Y` then `X`); the `who do …` clause is already
in execution order. Subject-owned forms traverse out, while grounding an
inverse RelClass phrase reverses that individual hop exactly as in Q4.

The emitted canonical pipeline is fixed:

```text
nodes(Person) as person | filter person.name = "Ada"
  | expand Knows out as hop1 | expand Knows out as other
  | project other.name
```

The start entity uses the first hop's center class and label-column law. The
final projection uses the second hop's far class label. `hop1` is the fixed
intermediate binding; `other` preserves Q4's final-result convention. With a
database available, the entity literal is resolved to its stored spelling
before emission, unchanged from one-hop traversal.

Three or more composable relationship phrases never truncate or partially
compile. They return the existing § 7 `NoParse` shape: the first blocked outer
hop has suggestion `multi-hop traversal has a hard 2-hop cap`, and the first
nearest hint is the same question shortened to its inner two-hop template.
This is a structured refusal, not a new outcome type. The golden corpus pins
both accepted phrasings to canonical text and executed rows, and the refusal
corpus pins the three-hop diagnostic; their tests are named `multi_hop*`.

## 15. Reference-date time phrases (NL_VERSION 6)

NL_VERSION 6 adds deterministic calendar phrases over an explicitly named
`Int64` epoch-second column. The additive trait method is
`compile_at(question, schema, reference_date)`. Its `reference_date` is an
`i64` Unix epoch-second value at UTC midnight, so it must be an exact multiple
of 86,400. This representation is a calendar date, not a current timestamp.
The method has a default implementation that calls `compile`, preserving
existing `IntentCompiler` implementations. `DeterministicCompiler::compile`
passes no date; a recognized time phrase through that path returns `NoParse`
whose diagnostic names the missing reference date and `compile_at`.

The ordered, all-token-consuming family is:

```text
<entity-set> [with] <column> since last week
<entity-set> [with] <column> in the last <N> days
<entity-set> [with] <column> yesterday
<entity-set> [with] <column> today
<entity-set> [with] <column> since <N> days ago
```

`<entity-set>` is a Q1 list source or Q2 property-filter source. A Q2 source
may compose its time predicate with `and`, for example `events with priority
over 1 and occurred yesterday`; emission keeps all predicates in one filter.
The optional `with` is syntax only. `<column>` is mandatory and resolves by
the catalog's exact ASCII fold rule against that table's columns; plural
morphology does not apply to this slot. The resolved column must have type
exactly `Int64`. An omitted, unknown, ambiguous, or non-Int64 column refuses.
`<N>` is a base-10 positive `Int64`; zero, negative, fractional, or out-of-
range counts refuse.

Let `R` be the supplied UTC-midnight epoch second and `D = 86,400`. Bounds are
UTC calendar-day bounds and ranges are half-open:

| Phrase | Emitted bounds |
|---|---|
| `since last week` | `column >= R - 7D` (no upper bound) |
| `in the last N days` | `column >= R - ND and column < R` |
| `yesterday` | `column >= R - D and column < R` |
| `today` | `column >= R and column < R + D` |
| `since N days ago` | `column >= R - ND` (no upper bound) |

Thus “last N days” means the N completed UTC days immediately before the
reference date and excludes the reference-date day; “today” names that day
separately. A `since` phrase is deliberately one-sided, so any stored future
epoch also satisfies it. Every multiplication, subtraction, and addition is
checked in `i64`; an invalid midnight or an unrepresentable boundary returns
`NoParse` rather than wrapping or changing the range.

The compiler performs all arithmetic before constructing DevonPlan and emits
only ordinary `>=`, `<`, `and`, and `Int64` literals. There is no date
expression and no clock read in `devondb-nl`. Canonical plans therefore freeze
their numeric bounds at compile/pin time and replay with the existing pinned-
plan determinism law unchanged. The golden corpus uses one fixed reference
date and fixture epochs on, just inside, and just outside every boundary; it
asserts canonical text and executed rows.

## 16. Aggregate question shapes (NL_VERSION 7)

NL_VERSION 7 adds the aggregate family — the analytics questions a consultant
types (`total revenue by region`). The ordered, most-specific-first,
all-token-consuming (R2) shapes, with plural folding on `<table>` as always,
`<column>`/`<column2>` resolved by exact fold against the source table (no
plural morphology, as § 15), and `<n>` digits only:

```text
total <column> [of|for] <entity-set> [by <column2>]
sum of <column> [of|for] <entity-set> [by <column2>]
<entity-set> total <column> [by <column2>]
average <column> [of|for] <entity-set> [by <column2>]      (also: mean)
<entity-set> average <column> [by <column2>]
highest <column> [of|for] <entity-set> [by <column2>]      (also: maximum, largest, max)
lowest <column> [of|for] <entity-set> [by <column2>]       (also: minimum, smallest, min)
how many <entity-set> by <column2>                          (count per group; extends Q5)
```

`<entity-set>` is a Q1 list source or a Q2 property-filter source (with `and`
conjunction) exactly as § 15 defines it, so `total revenue of sales with year
over 2023 by region` composes: one `filter`, then the aggregate. `of`/`for`
are syntax only (`of` is § 4 filler; `for` is consumed by the template).

Emission appends one aggregate stage to the entity-set pipeline. Output names
are fixed words per function — `sum`→`total`, `avg`→`average`, `max`→
`highest`, `min`→`lowest`, `count`→`` `count` `` (a reserved word in the text
language, backticked exactly as the printer does). The group column keeps its
own name; grouped rows carry the group columns first, then the aggregate:

```text
nodes(Sale) as sale | aggregate sum(sale.revenue) as total
nodes(Sale) as sale | aggregate sum(sale.revenue) as total by sale.region
nodes(Sale) as sale | filter sale.year > 2023 | aggregate avg(sale.revenue) as average by sale.region
nodes(Sale) as sale | aggregate max(sale.revenue) as highest
nodes(Sale) as sale | aggregate min(sale.revenue) as lowest by sale.region
nodes(Person) as person | aggregate count(person.id) as `count` by person.city
```

Rules:

- `by` after an aggregate head is GROUP; `by` in `top <n> … by <column>`
  stays Q6 sort. The heads are disjoint vocabulary, so template order alone
  resolves it; `top 5 sales by revenue` compiles to sort+limit, and
  `total revenue of sales by region` compiles to a grouped aggregate.
- Column typing is the ENGINE's law, not the compiler's: the compiler emits
  the plan and typing refuses at run time (`sum`/`avg` over String refuses
  with the engine's "requires a numeric operand" message; `avg` over Decimal
  refuses with the exact refuse-never-round message from `typing.rs`).
  The compiler itself refuses only on grounding (R2): unknown table/column,
  ambiguous column, missing `<column>` after a head, or a group column equal
  to the aggregated column.
- Determinism (R1) is unchanged: identical input compiles to identical
  canonical text; the corpus property runs the family across 1,000 shuffled
  compile orders.
- Refusals use the § 7 instantiated-example mechanism with one addition: when
  a failed column slot's `did_you_mean` suggestion is exactly a catalog
  column, the hint fills the slot with the suggestion — `total revenu of
  sales` reports `revenu (did you mean: revenue)` and offers `total revenue
  of sales`.

Aggregates do not compose with § 12 `within` or § 15 time phrases as the
entity-set's outer shape; such questions refuse with the nearest template.

## 17. Similar-to templates (NL_VERSION 8)

NL_VERSION 8 promotes § 3's recorded KNN-by-example refusal, now that
`KnnScan` carries a scalar vector source (ONTOLOGY.md § 7.2). The ordered,
all-token-consuming (R2) family accepts these surface shapes, with plural
folding on `<table>` as always and `<n>` digits only (number words keep the
v1 refusal):

```text
<table> similar to <entity>
show|find|list <table> similar to <entity>
<n> <table> similar to <entity>
top <n> <table> similar to <entity>
```

`<entity>` grounds through the SAME lookup the traversal templates use (§
6): the unique node class with a label column, the stored-spelling
resolution when a database is present, and the same ambiguity and unknown
refusals. The compiler never writes a second lookup.

The embedding is DERIVED, never named (ONTOLOGY.md § 7.2 third bullet): the
entity's class must have EXACTLY ONE `Vector(d)` column, with
`VectorEncoded(d, …)` counting by its value-type spelling. Zero columns
refuse with `` `<table>` has no vector column to compare by ``; two or more
refuse with every candidate named in schema order (`ambiguous embedding on
`<table>`: `a`, `b` — v2.1's explicit `embedding` declaration is the escape
hatch`).

The emitted canonical plan is fixed — entity lookup, scalar embedding
projection, then the KNN scan:

```text
knn(Person.embedding, scalar(nodes(Person) as anchor | filter anchor.name = "Ada" | project anchor.embedding), 10, cosine)
```

`anchor` is a canonical, fixed binding — pins depend on it. `k` defaults to
10; the counted forms set `k = <n>`. The metric is `cosine` and the mode is
the exact default, so the canonical text round-trips byte-identically
through the text parser. The result rows are the target table's rows plus
the facade's distance column, nearest first — the same output shape a list
of the table would give, ranked.

Refusals follow § 7 exactly. Matching the similar-to family is not a
preference tier: the recognized `similar to` marker grounds the family's
marker slot and counts as ONE grounded slot (§ 18), so every nearest hint is
ranked only by grounded-slot count and template declaration order:

- `<table> like <entity>` is NOT a synonym (R2: no synonym dictionary). The
  shape matches so the refusal can name it: `like` is reported with
  `` `like` is not a synonym for `similar to` ``, and the nearest hints
  teach the working spelling.
- Number words refuse as digits-only, exactly as `top five people by age`
  does: the count token is reported unrecognized, the nearest hints show
  `top <n> <table> similar to <entity>`.
- The anchor must live in the TARGET table in v2.0: cross-table forms
  ("documents similar to ada", where ada is a Person) refuse with
  `cross-table similar-to is v2.1; the anchor must be in `<table>``.

The v2.1 boundary is explicit: cross-table similar-to and the declared
`embedding` escape hatch (ONTOLOGY.md § 7.5) both wait. Determinism (R1) is
unchanged: identical question, schema, and reference inputs compile to
identical canonical text, and the compiled plan round-trips through the
canonical printer and parser to an identical operator.

## 18. Negation, ranges, and canonical output (NL_VERSION 9)

NL_VERSION 9 adds ordered, all-token-consuming negation and
range shapes: `<table> with no <relationship>` for edge absence, `<table>
without <column>` for NULL properties, `<table> not in <column> <value>` for
non-NULL inequality, closed numeric `between` ranges, and half-open `from
<month> to <month> <year>` calendar ranges over an explicitly named or
uniquely derived conventional Int64 time column. Well-formed questions that
match no rows remain successful plans (empty-result phrasing is never a
`NoParse`), while every ambiguous table, column, relationship, value, time
column, or endpoint direction refuses and lists the candidates rather than
selecting one.

The same vocabulary version includes canonical-output corrections. Identifier
emission folds the catalog spelling
before the reserved-word check, so fold-reserved names such as `Count` print
as `` `Count` `` and reparse as identifiers. String emission matches the
DevonPlan printer: `\"`, `\\`, `\n`, `\r`, and `\t` use their short escapes,
and every other C0 control uses lowercase `\u{H}`. Quoted tokens never act as
comparators or vocabulary, vocabulary suggestions use a deduplicated word
sequence, and § 7's nearest-template order is exactly grounded-slot count
descending followed by template declaration order. There is no family
preference tier: a recognized family marker — the head verb (`set`/`change`
→ update, `delete`/`remove` → delete), `within`, a time phrase, an aggregate
head, `similar to` — grounds the matching template's marker slot and counts
as exactly one grounded slot; among equal counts the template whose marker
matched ranks first, then declaration order. So "delete all people over 40"
still hints the delete family first, "total revenu of sales" still leads with
the aggregate family, and a template that grounds strictly more of the
question can outrank either.

## 19. Explicit full-text search (NL_VERSION 10)

The all-token-consuming template is:

```
search <table> <column> for "<literal>"
```

After the existing normalization rules, exactly five tokens must remain.
The table grounds against node tables (existing folded/plural resolution);
the column grounds by exact ASCII-folded name and must be String. The query
must be one closed double-quoted literal. Its contents remain data, including
keywords, punctuation and escaped quotes; the normal plan parser validates
literal escapes. Unknown/ambiguous tables or columns, non-String columns,
unquoted/unclosed queries, and extra trailing tokens produce structured
NoParse reports with a search-template hint. No network or model is used.

The deterministic default is k=10. For Document the emitted plan is
`textscan(Document.body, "rust graph", k=10) as document`; bindings follow
existing table-binding rules. Tokenization, fixed checkpoint statistics,
empty-corpus bootstrap and ranking follow FULLTEXT.md. Pins store the emitted
plan and execute it without recompiling the question. Execution requires the
embedded `fts` feature (enabled in the default CLI); compilation remains
available without it. Existing semantic corpus assertions remain unchanged;
only version pins advance from 9 to 10.
