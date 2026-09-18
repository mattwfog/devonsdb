# devondb full-text search — BM25 design

Hybrid retrieval combines vector search, BM25 ranking, and graph traversal.
devondb provides BM25 as a derived, budget-charged index alongside its graph
and vector capabilities.

Read with `docs/ONTOLOGY.md` §8 for the derive-first law and `docs/MVCC.md`
§7 for the memory budget charged by every allocation described here.

## 1. Goals and non-goals (v1)

Goals:

- Ranked keyword retrieval over one String column of one node table:
  a `TextScan` operator returning the top-k rows by BM25 score.
- **Deterministic scoring** — the same file and query produce identical
  rows in identical order on every platform, forever (the pin law).
- **Zero format change** — the index is derived, in-memory, sheddable.
  A v0.1.1 binary reads every file a FULLTEXT-capable binary wrote.
- Budget honesty: every postings byte is charged to `memory_limit`
  through the §7.2 ladder; under pressure the index yields.

Non-goals in v1 (each recorded, none silently dropped): phrase and
proximity queries · rel tables · stemming beyond case folding · CJK
segmentation · persisted index pages (v2 intent, §7) · a boolean
`matches()` filter expression (v2 intent — v1 is ranked retrieval only)
· incremental per-DML index maintenance (derived caches rebuild; see §3).

## 2. The operator

```
TextScan { table, column, query: String, k: u64, binding }
```

- Text: `textscan(Document.body, "rust graph", k=10) as d`. Canonical JSON
  uses `"op":"TextScan"` with the fields above. `k` is positive and fits
  the text form’s Int64 count range. The column must be String.
- Output: the table's rows (same binding semantics as ScanNodes),
  ranked by descending BM25 score, ties broken by primary key ascending
  — a TOTAL order, which is what makes determinism testable. The
  tiebreak key always exists: node tables require exactly one primary
  key by schema law (`devondb-types` `schema.rs`).
- Score exposure: `scoreof(d)` / `{"scoreof":"d"}` is a Float64 expression
  bound to this TextScan. The authoritative integer Q32.32 score is converted
  by `score as f64 / 4294967296.0` only after integer top-k selection.
  User columns named `score` and `_score` remain legal and independent.
- `query` is data, not grammar: it is tokenized by §4's rules, never
  parsed as syntax. There are no stopwords (§4); a query with zero
  recognized tokens yields zero rows, not an error.

## 3. The index: derived, sheddable, budget-charged

The inverted index is derived data on `PublishedState`, like the primary-key
caches:

- Built lazily on first TextScan against a given (table, column), by
  scanning the column through the per-column pruned-decode path and
  tokenizing per §4.
- Charged via `charge_or_reclaim` as it grows; shed by the reclaimer's
  derived-data rung and write-set ladder. Under pressure it transitions
  from Ready to Unbuilt, and a later TextScan rebuilds it.
- Carried across commits by adopting it when a commit chain is published;
  checkpoint deliberately re-derives it. Section 8 records this decision.
- Structure (in-memory only, no bytes on disk): per-(table, column) —
  term dictionary → postings (row ordinal + term frequency), plus
  per-row token counts and the aggregate stats BM25 needs (N, avgdl,
  per-term df).

Rebuild cost is the price of zero format change: one column scan per
shed or reopen. Section 7 records the persisted-pages v2 intent for when
measurement shows that rebuilding is the bottleneck.

The following laws apply:

- **Visibility law:** the index covers checkpointed group rows only, but
  MVCC.md §4.2's read path has scans emit OVERLAY rows as final chunks —
  so an index-only TextScan would diverge from ScanNodes on the very
  same snapshot even with zero uncommitted writes. Law: TextScan sees
  exactly what ScanNodes sees on the same view — index-scored group
  rows MERGED with exact-scored overlay-visible rows (tokenized on the
  fly, O(overlay) — bounded by checkpoint policy) under one top-k.
  Overlay rows score against the index corpus's statistics (N, avgdl,
  df) — a documented v1 approximation; determinism holds because the
  overlay is part of the snapshot.
- **Pin law:** the
  operator clones the index `Arc` at open and holds it for the scan's
  lifetime; shedding removes the registry entry only. The budget charge
  TRAVELS WITH the Arc and releases when the last holder drops — the
  MVCC §3 rule 4 `CommitLink` rule. An `Arc` holder keeps the data alive;
  the rule also prevents charge-versus-resident divergence and mid-scan
  rebuild thrashing.
- **Working-memory law:** the operator's top-k
  heap, score accumulators, and tokenizer buffers charge the one budget
  through `charge_or_reclaim` exactly as Sort/Aggregate buffers do;
  nothing allocates uncharged.
- **Adoption-validation law:** cross-publication adoption
  validates that the adopting catalog still carries the (table, column)
  identity with the same type; any mismatch discards the cache instead
  of adopting it.

## 4. Tokenizer

- ASCII letters/digits fold to lowercase; runs of alphanumerics are
  tokens; everything else separates. Non-ASCII: exact bytes, no folding
  (the catalog-wide folding convention, FORMAT.md § Catalog).
- Token length cap 64 bytes (longer tokens truncate — cap recorded so
  determinism survives pathological inputs).
- **No stopword list.** IDF weighting handles corpus-specific term frequency,
  while an engine-baked English stopword list would produce incorrect results
  for other languages and domains.

## 5. Scoring: integer BM25 (the determinism answer)

BM25 uses k1 = 1.2 and b = 0.75. The determinism law
rules out platform-`libm` floating transcendentals (`ln` is not
bit-identical across platforms), so:

- All scoring arithmetic is FIXED-POINT integer (Q32.32 working shape;
  exact widths require overflow proofs at the
  format's row-count and dl bounds).
- `ln` is a deterministic integer approximation with coefficients
  pinned in the spec (accuracy target: rank-order fidelity vs f64 BM25
  on the verification fixtures, not bitwise f64 equality).
- The frozen constants are part of the contract: changing k1/b/ln
  coefficients changes pinned-plan replay output, so any future
  tunability must arrive as EXPLICIT plan fields defaulting to the
  frozen values — never a silent engine retune.

## 6. Verification requirements

1. Determinism: same file + query → byte-identical result rows across
   two processes and across a reopen; a pinned TextScan replays
   identically after kill -9 recovery.
2. Oracle ranking: exact-arithmetic reference BM25 (test-side, arbitrary
   precision) agrees with the engine's rank order on the fixtures.
3. Shed proof in both directions: disabling the reclaimer rung causes a
   budget refusal under cache pressure; enabling it lets the same query
   complete.
4. Charge equality: build → shed → rebuild → drop leaves the budget
   exactly where it started.
5. Zero-format-change fence: a file written while FULLTEXT indexes were
   live is byte-identical to one written without them (index leaves no
   bytes), and the golden corpus stays green untouched.
6. Visibility severed-proof: insert rows, do NOT checkpoint,
   TextScan must rank them (overlay leg); sever the overlay merge and
   the test must fail; then checkpoint and assert identical results
   through the index leg (the two legs agree on the same data).
7. Pin severed-proof: a mid-scan shed must not change the
   scan's results or the budget accounting — charge equality asserted
   after the scan's Arc drops; sever the pin (re-read the registry per
   batch) and the thrash/divergence must be detected.

## 7. Rejected shapes (recorded so they stay rejected)

- **Persisted index pages** (v2 intent, not rejected forever): claims a
  feature bit + FORMAT § planned-extensions registry entry when measurements
  show rebuilding dominates. Not v1: it would be the
  first not-read-safe bit shipped for a convenience, against the
  derive-first law.
- **External index library** (tantivy et al.): violates the zero-dep
  edge budget (ARCHITECTURE § Edge budget) and would own scoring
  determinism we must own.
- **Trigram/substring index**: answers LIKE, not ranked retrieval.
- **Float BM25 with a documented tolerance**: a tolerance is a
  determinism-law violation wearing a disclaimer.
- **Engine stopword list**: see §4.

## 8. Design decisions

1. **Single-column in v1.** Multi-column concatenation and boosts are refused
   at validation with the limitation named; a plan-side union remains
   expressible. Per-field boosts would multiply the determinism surface.
2. **Score spelling is `scoreof(binding)`**, following the
   `classof(binding)` discriminator mechanism. PLAN_IR.md and §9 define its
   canonical JSON, provenance, and collision rules.
3. **The NL template follows NL.md's regime** (template
   family + golden corpus + refusals + NL_VERSION bump). The quoted
   literal is data under grounding law R2; the binding constraint is
   recorded here, the grammar is NL.md's to own.
4. **Q32.32 is the default.** Its implementation must prove overflow safety
   at the format's row-count and document-length bounds,
   and may move to wider intermediates ONLY if a proof forces it —
   the accuracy target (rank-order fidelity vs f64 on fixtures) is the
   contract, the width is implementation.
5. **Cache adoption follows the primary-key cache pattern.** Publish adopts
   the cache when a commit chain exists, while checkpoint re-derives it. The
   adoption-validation law in §3 guards this behavior.

## 9. Executable TextScan refinements

The embedded `devondb` library enables execution with `features = ["fts"]`;
this feature is off by default. The CLI enables and forwards it by default,
and the server exposes the same optional forwarding feature. IR parsing,
validation, printing, pin bytes and deterministic NL compilation remain
available without execution support; executing a TextScan without `fts`
returns a clear capability error, including through filters and subqueries.

Statistics are fixed to the snapshot's **checkpointed** non-NULL String
rows: document count, total token length and term document frequency.
Updates, deletes and reinserts do not recompute those baseline statistics.
Unchanged checkpoint rows use cached postings; replaced and newly inserted
rows tokenize their visible text against those same statistics. A queried
term absent from the checkpoint vocabulary has `df = 0` for overlay scoring.
NULL text is not a document; an empty String is a zero-length document.

When checkpoint document count is zero **or** total token length is zero,
statistics bootstrap from all non-NULL visible rows of the same immutable
snapshot, including own writes. This permits search before the first
checkpoint and after a checkpoint containing only empty/NULL text. Once the
checkpoint has a nonempty corpus, its approximation is used consistently.
Checkpointing after mutations may therefore change scores; row visibility,
integer arithmetic, and score/PK ordering are deterministic in each snapshot.

A refused cache uses a checkpoint-only column pass retaining just query-term
frequencies and corpus counters, then the same visible top-k pass. It never
substitutes live-row statistics for nonempty checkpoint statistics. Query
terms, group decode expansion (including projected non-text result columns),
visible overlay bookkeeping, retained candidates and output conversion peaks
are precharged against the shared budget. Only one group and a k heap are
needed; a group or working set that cannot fit returns `BudgetExceeded`.
The index Arc remains pinned through scan completion and output consumption;
shedding registry entries does not release its charge prematurely. Fixed
stack tokenizer buffers and fixed-size operator wrappers require no dynamic
charge; all variable-capacity storage is charged before allocation.

Top-k selects descending integer score and ascending Int64/String primary
key before downstream operators execute. In particular, a filter after a
k=1 TextScan can return empty even if a lower-ranked document would match.
Tokenless queries return typed empty results without allocating a k heap.
Score metadata survives projection, filter, sort, limit, expand, joins and
correlated scalar evaluation; aggregate consumes score operands and removes
source score bindings. Left joins give unmatched right scores NULL.

Metadata is not a property column. To prevent forged JSON aliases colliding
across joins, every plan containing any TextScan (including scalar subplans)
reserves aliases and binding names beginning with the ASCII-folded private
prefix `\0devondb-scoreof\0`. Non-TextScan plans retain their prior arbitrary
alias behavior. Ordinary `score`/`_score` properties are unaffected.
