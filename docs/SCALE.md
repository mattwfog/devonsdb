# SCALE — processing large data volumes on the edge

The design draws on adaptive lightweight compression, FastLanes, ALP, FSST,
object-native storage, and predicate transfer. Read with
`docs/FORMAT.md` (§ Compatibility rules, § Planned format extensions) and
`docs/ARCHITECTURE.md` (§ Edge budget — every decision here is subordinate
to it).

The thesis: the 2024–2026 compression frontier optimized for exactly the
properties the edge demands — branchless cheap decode, random access
without full-block decompression, no mandatory ISA baseline. devondb
adopts the field's proven encodings in DuckDB's adaptive-per-payload
shape, and treats object storage as a runtime capability over the
existing pager seam, not a format concern.

## §1 Decisions

1. **Compression is for scan speed, not ratio.** Every encoding adopted
   must decode faster than the page reads it saves on Pi-class hardware,
   or it is out. Heavy general-purpose codecs (ZSTD/LZ4 per block) are
   permanently out of the scan path; they may appear later in cold
   export/backup surfaces only.
2. **Adaptive per-payload selection, DuckDB-shaped.** The writer samples
   each column payload at group-write time and picks the cheapest
   admissible encoding. The chosen encoding is recorded per payload in
   the extended directory (see `FORMAT.md` § Planned format extensions).
   Encoding choice is writer policy, never format law — readers decode
   whatever a payload declares; goldens pin bytes per encoding, not the
   selection heuristic.
3. **The v1 encoding set** (each admitted only with the §1.1 measurement):
   constant · RLE · bit-packing + frame-of-reference (FastLanes
   1024-value transposed layout — decodes efficiently scalar AND at any
   SIMD width, matching Edge budget §5's no-baseline law) · dictionary ·
   FSST for strings · ALP for floats. Cascading (Vortex-style recursive
   encoding) only where sampling proves it pays; never speculatively.
4. **Zone maps ride a READ-SAFE feature bit.** Per-group min/max (+
   null-count) statistics are derived data: a build that ignores them
   reads correctly. Old binaries therefore open stats-carrying files
   read-only under the FORMAT.md §8.1 read-only law — never writing,
   so stale stats cannot exist. Pruning consults stats only when the
   bit is supported.
5. **Column compression rides a NOT-read-safe feature bit.** A build
   that cannot decode an encoding cannot read the file at all; the mask
   union law (bit-4 precedent) governs.
6. **Sorted bulk load is the pruning multiplier.** `COPY` builds node
   groups or relationship CSR directly (WAL-bypassing under an explicit
   bulk fence, eMMC-friendly). Node loads admit optional sort-on-load;
   zone-map effectiveness is ordering-dependent (DuckDB's measured
   order-of-magnitude), so the node loader controls ordering rather than the
   execution engine.
7. **Object storage = a pager backend, read-only attach first.** The
   pager's positional page I/O becomes a trait; the remote backend
   serves page misses via HTTP range requests into the existing budgeted
   cache (SlateDB's cache/batch lessons; sync client per Edge budget §4,
   off-by-default cargo feature per §3). Source of truth for writes
   stays local; "publish/checkpoint to object storage" is a later,
   separate increment. The same trait seam serves wasm32/OPFS.
8. **Compressed execution and predicate transfer are staged bets,**
   gated on criterion evidence: (a) decode-free filters over
   dictionary/RLE payloads; (b) Bloom-filter semi-join reduction for
   multi-hop traversals (predicate transfer, CIDR'24→VLDB'26 line) — no
   embedded graph engine ships this; it is devondb's differentiator
   candidate, and it needs no format change.
9. **The format freeze requires only specification text and fences.**
   Both extension mechanisms (reserved header field, feature bits,
   enforced-zero directory padding) already exist and are enforced in
   code (`node_group.rs` reserved+padding rejections, `csr_group.rs`
   padding rejection). The freeze prerequisite is: the § Planned format
   extensions registry text in FORMAT.md, and fence tests locking the
   reserved-field/padding rejections — NOT byte-layout
   changes. No golden churn pre-freeze.

## §2 What is deliberately out

GPU execution (edge budget) · LLM-based compression (research toy) ·
lakehouse/table formats (outside the product scope) · distributed execution
(single-node scale-up is the Kuzu-lineage thesis the successors all
retain) · compression of WAL frames (durability path stays simple; the
WAL is short-lived by checkpoint policy).

## §3 Feature boundaries

- Zone maps use a read-safe feature bit and an extended node-group directory.
- `COPY` and sort-on-load change runtime behavior but add no format surface.
- Column encodings use a not-read-safe feature bit because they change payload
  interpretation.
- Object-storage attachment is a runtime pager backend and does not change the
  database format.
- Compressed execution and predicate transfer require benchmark evidence but
  do not require format changes.

## §4 Zone maps

### §4.1 Stats ride the node-group directory-flags word

Stats use the `FORMAT.md` § Planned format extensions mechanism: the
offset-12 reserved field is a directory-flags word, and stats occupy a
length-framed extension section. This design has five properties:

1. **Zero extra pruning I/O.** Stats sit on the directory page the group
   loop already reads, so the skip decision costs no page read at all.
   A catalog-section design pays either an open-time decode held
   resident or per-scan stats-page reads, while directory-resident stats
   avoid both.
2. **Wear and page leaks.** Own-page stats mean extra pages written
   every checkpoint, and superseded stats pages now retire into the
   `FREE_PAGES` ledger for reuse after the LSN pin horizon advances
   (`docs/FREE_PAGES.md` § Retirement and reclamation sequence;
   `FORMAT.md` § MVCC and page immutability).
   Directory-resident stats add zero pages, which is the eMMC/edge
   budget's concern directly.
3. **Atomicity.** Same-page stats can never dangle from their group. A
   catalog route creates a new pairing invariant (group ref ↔ stats
   ref) and therefore a new corruption class to fence.
4. **The future case is a tie.** Length-framed skippable sections make
   v1→v2 read-safe directory extensions genuinely work — an unsupporting
   reader skips a section by its length rather than choking.
5. **The flags word is a bit registry, not a single slot.** Column
   encodings claim their own directory-flags bit under a not-read-safe
   superblock bit. The mechanism is shared, never spent by its first user.

Unknown flags bits remain corruption. The exact byte layout is specified in
`FORMAT.md` and changes only with the writer and golden corpus.

### §4.2 The bit-3 claim relocates the read-only fixture (collision)

Claiming bit 3 for zone maps moves it into `SUPPORTED_FLAG_MASK`, which
retires the permanently unsupported bit used to exercise the read-only law.
The reserved fixture moves to the next available read-safe bit:

| Constant | Before | After |
|---|---|---|
| `ZONE_MAPS_FLAG` | — | `1 << 3` (read-safe **and** supported) |
| `RESERVED_READ_SAFE_FLAG` | `1 << 3` | `1 << 5` (bit 4 is `GEO_COLUMNS`) |
| `READ_SAFE_FLAG_MASK` | 0·1·2·3 | 0·1·2·3·5 |
| `SUPPORTED_FLAG_MASK` | 0·1·2·4 | 0·1·2·3·4 |

`RESERVED_READ_SAFE_FLAG` keeps its stable NAME and is repointed to the
new reserved slot. The permanent read-only-law fixtures
(`crates/devondb/tests/read_only.rs:82,149`; `superblock.rs:356`) then
need **zero edits** — they track the registry's current reserved slot by
name, which is exactly what `superblock.rs:41-43` was written for.

### §4.3 Stats scope, v1

Per MAIN column of a persisted node group (derived b1 rescore entries
carry no stats): `null_count`, plus min/max for exactly three types —
`Int64`, `Float64`, and `GeoPoint` as its **atom-key** `u64`. The
GeoPoint case is the GEO §7 synergy: a group whose atom range misses
every covering range is skipped without decoding a page. `Bool`,
`String`, `Vector`, and `VectorEncoded` never carry min/max in v1.

Two rules that are easy to get wrong and are therefore format law, not
writer policy:

- **Float64 presence excludes NaN.** Min/max are present iff the column
  has a non-null value AND no non-null value is NaN. Selection ordering
  is IEEE-754 totalOrder (`f64::total_cmp`), which refines numeric order
  over the NaN-free set the presence rule guarantees and pins one byte
  spelling when `-0.0` and `+0.0` tie for an extreme.
- **Stats are law, not hints.** A non-null payload value outside its
  declared `[min, max]`, or a `null_count` disagreeing with the validity
  bitmap, is corruption — enforced by the corpus validator and the
  corruption tests, never re-checked on the scan hot path.

Presence is **per group**, via the directory-flags word: groups written
before the feature coexist with stats-bearing groups in one file, and
pruning simply scans the ones without. Absent stats are always legal —
pruning is an optimization, never semantics.

### §4.4 The severed-proof gate

Pruning is invisible to results by construction, so the E2E must assert
**work not done**: a scan whose predicate excludes a group must not read
that group's payload pages, asserted through pager read counters. A
severed pruning path leaves results identical and the counter assert
failing — that is the whole point. Result-equivalence is asserted
alongside it: pruned and unpruned runs return identical rows in
identical order.

## §5 `COPY` bulk load

### §5.1 Surface

One statement in the text plan language:

```text
copy <Table> from "<path>"
copy <Table> from "<path>" sort by <column>
```

`Statement::CopyNode { table, path, sort_by: Option<String> }` is
table-kind-agnostic at parse time: both node and relationship tables are
legal, and the facade routes by catalog lookup. The `CopyNode` tag name is
historical wire compatibility, not a node-only semantic promise; there is no
second relationship variant with an identical payload. `sort by` on a
relationship table is `invalid_argument`: edges load in CSR order and admit
no caller-selected ordering. No NL template in v1 — COPY is an operator's
verb with a filesystem path; grounding a path from prose has no deterministic
story yet.

### §5.2 CSV dialect (law, not writer policy)

RFC 4180 subset: comma delimiter, LF or CRLF records, `"` quoting with
`""` escape, no comment lines. The **first record is a mandatory
header** naming schema columns by exact name (exact matching only —
project convention), in any order; every schema column must appear exactly
once; unknown names are `invalid_argument` with a did-you-mean suggestion.
Rationale: header-mapped loads catch column drift that
positional loads silently mis-assign.

Field → `Value` uses the text language's own literal grammar
(`parse_value_literal`, `text/parser.rs:885`) for every non-String
column — `42`, `1.5`, `true`, `[0.1, 0.2]`, geo literals — ONE grammar,
zero drift between what the shell accepts and what a file loads.
String columns take the raw (unescaped) field text. An **empty unquoted
field is NULL**; a quoted empty string `""` is the empty String for
String columns and `invalid_argument` elsewhere. Parse errors name the
1-based line and column.

For a node table the header set is its declared columns. For a relationship
table it is the exact reserved endpoint-key names `from` and `to`, followed
conceptually by every property column under its exact schema spelling (file
order remains arbitrary). `from` and `to` carry the primary-key types of the
schema's endpoint node tables and lower to the same `from_key`, `to_key`,
then-property-values row shape as `insert rel`; relationship properties are
never primary keys. A relationship property whose folded name collides with
reserved `from` or `to` is refused because the mandatory exact header would
otherwise be ambiguous.

### §5.3 The bulk fence and crash safety

`COPY` bypasses the WAL entirely under these rules, enforced at the
facade (`Database`):

1. **Quiescence**: COPY refuses to start while any write transaction is
   open (`write_txns` non-empty, `options.rs:82`) —
   `invalid_argument("copy requires no open write transactions")`.
   Concurrent MVCC conflict semantics for a WAL-bypassing writer are a
   correctness cliff v1 does not walk.
2. It takes the commit pipe lock for the whole load (later writers
   block; **readers are unaffected** — published snapshots keep
   serving).
3. It first runs the ordinary checkpoint (flushing the buffered tail and
   truncating the WAL), so group building starts from persisted state.
4. Node rows stream into full groups on fresh pages (`BulkNodeWriter`).
   Relationship rows resolve both endpoint PKs against checkpointed nodes,
   then `BulkRelWriter` builds the canonical forward and backward CSR groups
   through the relationship checkpoint encoder. Nothing is written to the
   WAL.
5. The single atomic publish is `catalog.save` + published-state swap —
   the same fence checkpoint already trusts. **Crash at any earlier
   point leaves the database exactly as it was**; already-written bulk
   pages from a COPY failure are queued for retirement by the next
   successful publication, then reused after the pin horizon
   (`docs/FREE_PAGES.md` § Retirement and reclamation sequence;
   `FORMAT.md` § MVCC and page immutability).
   A kill -9 mid-copy recovery test is part of the gate.

COPY is all-or-nothing: any parse, validation, PK, or budget error
before publish aborts with the file's state unchanged.

6. **HNSW-indexed node tables build or catch up their indexes inside the
   same single-publication operation.** Bulk groups bypass `HnswDelta`
   maintenance, so publishing them without matching index coverage could
   make approximate KNN omit rows. Relationship tables carry no vector
   indexes.

### §5.4 Streaming, sort, and the budget

The unsorted path is **fully streaming**: memory high-water is one
node group (2048 rows) plus the PK set. `sort by <column>` buffers and
sorts the whole load **in memory under a `ChargedBytes` charge**
(`budget.rs:96`) — `BudgetExceeded` is the honest answer on tight
devices; loads larger than memory arrive pre-sorted. Sortable columns
are exactly the three stats-carrying types (§4.3): `Int64`, `Float64`
(IEEE-754 `total_cmp`, the §4.3 ordering), and `GeoPoint` by atom key —
sorting by any other column is `invalid_argument`. **Equal sort keys
keep original CSV record ordinal** (stable sort): an unstated tiebreak would leave HNSW
insertion order — and therefore index topology — to a sort
implementation detail. This is the zone-map
multiplier: sorted loads make §4.3 pruning effective, which is the reason to
support sort-on-load.

Relationship COPY retains the edge set until CSR construction because the
canonical checkpoint path groups it once forward and once backward; there is
no relationship `sort by`. The retained edge buffer and endpoint PK→offset
maps charge the shared budget through `charge_or_reclaim`, so clean pager
frames are offered to the §7.2 reclaim ladder before `BudgetExceeded` is
returned. Endpoint lookup matches relationship INSERT: a missing or
wrongly-typed source/destination PK uses the same error wording, annotated
with the first failing CSV line, and aborts before catalog publication.

**PK enforcement matches insert exactly** (`commit.rs:307-326`): null
PK and duplicate PK — within the file AND against existing rows — are
errors with the same message shapes. The existing-key set is built by
one streamed pass over the persisted PK column; both key sets live
under `ChargedBytes`.

### §5.5 Stats come free; the gate proves the multiplier

`NodeGroup::write` already computes and writes §4 zone maps
(`node_group.rs:170-194`), so bulk-built groups carry stats with zero
additional loader work. The E2E gate closes the loop: a sorted COPY followed by
a selective filter must read **fewer pages** than the same data loaded
unsorted (pager counters, the §4.4 severed-proof pattern) — proving
sort-on-load actually multiplies pruning, not merely that code ran.

### §5.6 Parquet source format (feature `parquet`)

`copy <table> from "<path>"` sniffs the first four bytes of the file:
`PAR1` selects the Parquet reader — the magic, not the extension, decides
(a `.csv` that starts with `PAR1` IS Parquet); anything else is the CSV
path unchanged. Without the `parquet` cargo feature a `PAR1` file is
`invalid_argument` ("Parquet COPY requires the `parquet` feature"), never
a CSV parse error. The §5.2 header law applies to Parquet top-level
fields by exact name (every schema column exactly once, unknown names
refused with did-you-mean, order arbitrary; relationship tables use the
reserved `from`/`to` fields). Rows feed the SAME sink as CSV (`load_rows`
is generic over the row iterator), so the bulk fence, PK law, and `sort
by` of §5.3-5.4 apply unchanged; row groups stream one at a time with
each group's uncompressed size charged to the budget while buffered.

Type mapping is refuse-never-round: INT32/INT64 → Int64 · FLOAT/DOUBLE →
Float64 · BOOLEAN → Bool · BYTE_ARRAY+UTF8/STRING → String · unannotated
BYTE_ARRAY/FIXED_LEN_BYTE_ARRAY → Bytes · TIMESTAMP(ms/µs/ns) → Timestamp
(µs; ms ×1000; ns only when divisible by 1000) · DECIMAL(p,s) over
INT32/INT64/FLBA → Decimal only at equal scale with digits fitting the
declared precision · LIST<FLOAT|DOUBLE> → Vector(n) at exact length
(DOUBLE narrows to the f32 vector type, the same narrowing the text
literal grammar performs) · BYTE_ARRAY+JSON → Json re-serialized through
serde_json (compact, sorted keys; the output must match the text parser's
canonical JSON representation) ·
group{lat_deg, lng_deg: DOUBLE} → GeoPoint via `from_canonical` ·
anything else `invalid_argument` naming the column and the Parquet type.

Dependency: `parquet` 59.2 with `default-features = false, features =
["snap", "zstd"]` (record API; no arrow-* crates), OFF by default in the
library, ON by default in `devondb-cli`. The measured distribution-size
delta is 598,976 bytes (2,584,576 bytes with the feature and 1,985,600 bytes
without), below the 10 MB feature-complete limit.

### §5.7 Criterion scan baseline

`crates/devondb/benches/scan.rs` measures a full scan, a selective
zone-map-pruned scan, and projection over a COPY-loaded table at 100,000 or
more rows through the real `Database` read path. This baseline determines
whether typed chunks improve scan performance.

## §6 Typed chunks

### §6.1 Evidence and target

The executor's chunk is column-major but each cell is a boxed enum:
`Chunk { types: Vec<LogicalType>, columns: Vec<Vec<Value>>, row_count }`
(`crates/devondb-exec/src/chunk.rs:14-18`), built row-at-a-time through
`ChunkBuilder::push_row` type validation (`chunk.rs:81-110`) and consumed
as `&[Value]` slices (`chunk.rs:40-55`; ~251 `Value::` sites in
`eval.rs` alone). The criterion baseline measured the cost: at 100,000
rows, projection (17.1ms) ≈ full scan (16.5ms) — row materialization
dominates, so per-value enum boxing is the target.

### §6.2 The Column representation

`Column` becomes an enum over typed storage, extensible (adding a variant
is never format law — encodings declare payload bytes, `Column` is only
their decode target). **Home: `devondb-types` (`devondb_types::column`),
re-exported by `devondb-exec`**. Section 6.5 requires storage decoders to
build typed columns directly, and
`devondb-storage` cannot depend on `devondb-exec`, so the decode target
must live in the crate both already depend on.

- `Int64(Vec<i64>)` · `Float64(Vec<f64>)` · `Bool(Vec<bool>)` ·
  `Timestamp(Vec<i64>)` (epoch micros) · `Decimal(Vec<i128>)` (unscaled
  digits; precision/scale live in the column's `LogicalType`, exactly as
  `Decimal128` pairs digits+scale, `devondb-types/src/decimal.rs:3-37`).
- `Boxed(Vec<Value>)` — the initial carrier for String, Bytes, Json,
  Vector, and GeoPoint. FSST graduates strings to an arena variant
  (`Utf8 { offsets, bytes }`); Vector/GeoPoint graduate
  only when an encoding or measured operator needs them.

### §6.3 Validity

Nullability leaves the value enum: each typed column carries
`validity: Option<Bitmap>` — `None` means all rows valid; `Bitmap` is
`Vec<u64>` words, bit `i` (word `i/64`, bit `i%64`) SET = row `i`
non-NULL. Typed vectors hold **zero** at null slots — pinned so chunk
equality, goldens, and spill round-trips stay deterministic.
`Boxed` columns keep `Value::Null` inline and carry no bitmap.

### §6.4 Migration law (two phases, both gated)

- **Phase A — mechanical seam swap, zero behavior change.** `Chunk`
  keeps `types()/row_count()/column_count()`; `column(i)` returns
  `&Column`; a materializing accessor `Column::value_at(row) -> Value`
  (and a `rows()` built on it) bridges every unmigrated consumer.
  `ChunkBuilder::push_row` keeps its exact validation and errors. The
  severed-proof is the EXISTING workspace suite green with zero test
  edits outside the chunk module — behavior is byte-identical.
- **Phase B — per-operator typed fast paths** (filter comparisons,
  projection, aggregate accumulation) proceed independently and are
  admitted only with a criterion delta against the §5 baseline (§1.1:
  decode/compute must beat what it replaces on Pi-class
  hardware, no SIMD baseline assumed — Edge budget §5).

### §6.5 The encodings contract

An encoding decoder's output type IS `Column`'s typed storage — never
`Vec<Value>`. The scan path builds typed columns directly from payload
bytes; FastLanes blocks are 1024 values, `CHUNK_CAPACITY` stays 2048
(`chunk.rs:8`): exactly two full blocks per full chunk column, partial
tail blocks legal. Boxing values between decode and chunk violates this
contract.

### §6.6 Budget

Typed columns charge the same shared budget with the same accounting
seam the boxed path uses today: fixed-width storage charges
`len × width` + bitmap words; `Boxed` charges per-value approx bytes,
unchanged. No unbudgeted intermediate (docs/ARCHITECTURE.md §
memory-bounds law; the §7.2 ladder applies via `charge_or_reclaim`).

### §6.7 Performance and validation

The scan path constructs typed columns directly. In the 100,000-row baseline,
projection measured 13.47 ms versus 26.56 ms for a full scan, and every §5.7
benchmark improved by 19–52%. Decoded groups are not budget-charged; §6.6
accounting lives in `Column::approx_bytes` and is applied at working-set
sites. Sort, aggregate, join, and spill buffers hold materialized rows and
remain charged per value. Each encoding requires byte goldens, a corruption
matrix, never-panics fuzzing, and the §1.1 performance measurement. Operator
fast paths require a criterion improvement over the baseline.

## §7 `pack` — the read-only distribution container

devondb supports distributing one compact, read-only file containing a
complete database. Section 1 keeps heavy codecs out of the scan path but
permits them on cold surfaces; a packed file is such a surface and opens
through the pager-backend seam in `docs/OBJECT_STORAGE.md`.

### §7.1 What a pack is

`devondb pack <in.devondb> <out.devondb>`: open the source normally
(recovery runs, a checkpoint is taken so the WAL is empty and the main
file is the whole database), then write every main-file page into a
container:

```
magic "DEVONPACK" (9 B) · u8 container_version = 1 · u16 codec (1 = zstd)
u32 page_size · u64 page_count · u32 frame_pages (pages per frame; writer
policy, default 256) · u64 frame_count · u32 crc32c(header bytes above)
frame directory: frame_count × { u64 offset, u32 compressed_len, u32 crc32c(compressed bytes) }
u32 crc32c(directory bytes)   — frames begin at 40 + 16·frame_count + 4
frames: zstd-compressed runs of frame_pages consecutive pages (last frame short)
```

All integers little-endian. Page `i` lives in frame `i / frame_pages` at
in-frame offset `(i % frame_pages) × page_size`. Nothing in the main
format changes: the pages inside are byte-identical to the source's, so
every existing reader (catalog, node groups, CSR, HNSW, zone maps, free
pages ledger) reads them unchanged — the container is transparent below
the page seam. A pack is NOT a `FORMAT.md` feature bit; it is a second
file type recognized by its magic at open.

### §7.2 The read side

`Database::open*` on a file whose first 9 bytes are `DEVONPACK` selects
the `Pack` variant of the closed `Backend` enum (OBJECT_STORAGE § Dispatch
shape; static dispatch, never `dyn`). The backend implements
`PagerBackend` exactly: `len() = page_count × page_size`;
`read_exact_at(offset, dst)` maps the byte range to frames, decodes each
needed frame into the backend's ONE decoded-frame buffer (`frame_pages ×
page_size` bytes — the documented, fixed working set; a second frame
evicts the first; no frame cache beyond it — the pager's own budgeted page
cache is the cache), verifies the frame crc32c BEFORE decoding and
returns `Corrupt` on mismatch, and copies the requested bytes out.
`write_all_at` and `sync_all` return `DevonError::ReadOnly` — the second
fence of OBJECT_STORAGE § Read-only enforcement; the facade's read-only
shared state is the first (no WAL writer, no lease, no publication gate;
mutators fail through the existing `ReadOnly` gate). The decoded-frame
buffer is charged to the memory budget once at open by the facade (it is
the only unbudgeted allocation the backend would otherwise hold); a
`memory_limit` below one frame refuses the open honestly.

Truncation anywhere (header, directory, a frame) is `Corrupt` with the
offending region named; a `page_count` that disagrees with the source's
superblock is `Corrupt`; a directory crc that disagrees is `Corrupt`.

### §7.3 Codec and dependency

`zstd` (the libzstd binding) behind cargo feature `pack` on
`devondb-storage`, forwarded by `devondb` and ON by default in
`devondb-cli`; the library core stays free of it. The stripped distribution
binary must remain below the 10 MB feature-complete limit in
`docs/ARCHITECTURE.md` § Edge budget item 3. A pure-Rust decoder (`ruzstd`)
is the fallback for a decode-only edge build if size measurements require it.

### §7.4 Validation

The pager backend uses a closed `Backend` enum with no `dyn PagerBackend` on
the pager path, and local cache-hit and cache-miss benchmarks allow no
regression beyond the 2% noise threshold. Pack validation covers round-trip
equivalence over every read path, mutation refusal at both fences, corruption,
the memory bound, and binary size. The measured distribution delta is 48
bytes when libzstd is already present through Parquet; representative fixtures
compress by 13.8× and 78.8×. Object-storage HTTP attachment follows the
acceptance requirements in `docs/OBJECT_STORAGE.md`.

## §8 Column encodings

Section 1 decisions 1–3 and 5 govern: speed over ratio; adaptive per-payload
selection is writer policy; the v1 set is constant · RLE · bitpack+FOR ·
dictionary · FSST · ALP; and the feature bit is not read-safe. Section 6.5
governs the decode target:
every decoder produces `devondb_types::column::Column` typed storage,
never `Vec<Value>`.

### §8.1 Where an encoding lives in the bytes

A column payload stays `validity bitmap ‖ values` (`docs/FORMAT.md` §
Node group pages, "Column payload encoding"). Encodings apply to the
**values section only**; the validity bitmap is never encoded (it is
already 1 bit/row and the zone-map `null_count` law reads it). The
directory declares which encoding each payload carries:

- Superblock feature bit **13 `COLUMN_ENCODINGS`** (not read-safe: a
  build without the codec cannot read the file; mask-union law,
  bit-4 precedent). Set at checkpoint iff any node group carries a
  non-plain payload.
- Directory-flags bit **1 `COLUMN_ENCODINGS`** → section payload of
  exactly `4 × column_count` bytes, main columns in catalog order:
  `encoding_id u8 · p0 u8 · p1 u8 · p2 u8` (parameters are
  encoding-specific, zero when unused; documented per encoding). The
  section is ABSENT when every payload is plain — a group written
  before the feature is byte-identical to one written after it with
  all-plain selections (goldens prove this).
- `byte_len`/`crc32c` in the column entry cover the ENCODED payload.

Encoding ids are format law:

| id | encoding | admissible value types |
|----|----------|------------------------|
| 0 | plain (today's layout) | all |
| 1 | constant | Int64 · Float64 · Bool · Timestamp · Decimal · String |
| 2 | rle | Int64 · Timestamp · Decimal · Bool |
| 3 | bitpack_for (FastLanes 1024-value transposed layout + frame of reference) | Int64 · Timestamp |
| 4 | dictionary | String |
| 5 | fsst | String |
| 6 | alp | Float64 |

An id outside the table, or an id applied to an inadmissible type, is
corruption. Each codec defines the exact values-section layout for its id;
`FORMAT.md` carries the registry above.

### §8.2 The code seam

`crates/devondb-storage/src/node_group/encodings/mod.rs`: `pub(crate)
enum Encoding { Plain, Constant, Rle, BitpackFor, Dictionary, Fsst, Alp }`
with `id()`/`from_id()`, admissibility per §8.1, and two dispatch
functions with FIXED signatures every encoding module implements:

```rust,ignore
pub(crate) fn encode_values(encoding: Encoding, column: &Column, ty: &LogicalType) -> DevonResult<(Vec<u8>, [u8; 3])>;
pub(crate) fn decode_values(encoding: Encoding, params: [u8; 3], bytes: &[u8], validity: &Bitmap, row_count: usize, ty: &LogicalType) -> DevonResult<Column>;
```

One module per encoding (`constant.rs`, `rle.rs`, `bitpack_for.rs`,
`dictionary.rs`, `fsst.rs`, `alp.rs`) exposes `encode`/`decode` with
those shapes. Decoders
validate everything (lengths, bit widths, offsets, UTF-8, dictionary
indexes in range) and return `Corrupt`, never panic — the
`vector_encoding.rs` / `fuzz_decode.rs` precedent governs.

### §8.3 Admission and goldens

Per encoding: byte goldens for a fixed input (frozen bytes in the test),
a corruption matrix with exact messages, a seeded never-panics fuzz over
the decoder, a round-trip property over deterministic value sets
including every NULL pattern, and a criterion bench `decode(encoded) vs
decode(plain)` for the same column. §1.1's admission (decode faster than
the pages it saves on Pi-class hardware) is determined from the benchmark
results. Each encoding has a `tests/golden/encoding-<name>.devondb` fixture,
a manifest entry, and a `format_freeze.rs` fence.

### §8.4 Selection

Writer policy per §1 decision 2: sample each payload at group-write time,
pick the cheapest admissible encoding by a documented cost model, record
the choice in the section. Never format law; goldens pin bytes per
encoding, not the heuristic. The default writer runs this selection. An
all-plain group still writes no section and stays byte-identical to a
pre-feature file.
