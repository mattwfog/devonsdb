# Index-aware bulk load — `COPY` into HNSW-indexed node tables

This design closes the `COPY`/HNSW seam without changing the on-disk format.
It requires full index coverage at publication, uses the original CSV ordinal
as the equal-sort-key tiebreaker, and shares one page-backed construction
accessor between `COPY` and ordinary `create hnsw index`.

## Scope

Without index-aware construction, the refusal is necessary: `execute_copy` calls
`refuse_indexed_table` before the preliminary checkpoint
(`crates/devondb/src/database/copy.rs:51`), and that helper rejects the first
HNSW index on the target table
(`crates/devondb/src/database/copy.rs:104-116`). The reason remains valid:
bulk-written groups bypass the ordinary `HnswDelta` commit path, so publishing
the rows without a coverage rule could make approximate KNN silently omit
them.

Indexed update/delete now uses exact fallback until checkpoint rebuilds topology
(`docs/HNSW.md` §5.7). COPY remains a distinct atomic bulk publication with its
full-coverage construction requirement; the mutation path's partial-root policy
does not relax COPY's acceptance contract.

The existing laws this design preserves are:

- `COPY` is a WAL-bypassing, quiescent writer that holds the commit pipe for
  the whole operation, checkpoints first, writes fresh pages, and publishes
  through catalog copy-on-write (`docs/SCALE.md` §5.3 and
  `crates/devondb/src/database/copy.rs`).
- HNSW represents a contiguous `covered_rows` prefix. Approximate KNN always
  exact-scans the uncovered suffix and unions it with ANN candidates
  (`docs/HNSW.md` §§1, 4.2, and 5.4;
  `crates/devondb-exec/src/hnsw.rs`).
- HNSW topology is immutable and copy-on-write. A root, layer directory, and
  changed `RCSR` groups are written before a catalog entry names the root
  (`docs/HNSW.md` §§3 and 5.5-5.6).
- HNSW construction uses the canonical scalar kernels, deterministic
  distance/node-offset ordering, and the persisted level seed
  (`docs/HNSW.md` §§2.3-2.4, 7.3, and 10 decision 7).
- Page cache, loader state, MVCC state, and HNSW construction all share one
  `memory_limit`; the reclaim ladder is binding (`docs/ARCHITECTURE.md`
  § Edge budget and `docs/MVCC.md` §7.2).

Only node-table `COPY` is in scope. Relationship bulk load, HNSW delete/vector
update repair, online compaction, a new `REINDEX` statement, and background
workers remain outside this design.

## Design

COPY performs synchronous construction inside the existing COPY fence, with
one atomic catalog publication covering both the new table groups and every
replacement HNSW root.

Rows are physically written first but remain unreachable. For each HNSW index
on the target table, in catalog order, COPY then constructs an immutable
replacement publication through the existing deterministic insertion/build
algorithm. Construction starts at the root's current `covered_rows` and ends
at the prospective table row count, so it includes any old exact tail as well
as the CSV rows. No published CSR page is repaired in place: changed cells,
the layer directory, and the root are all fresh pages. This is synchronous
append construction, not delete/update repair.

After every affected root has full coverage and every new data/index page is
durable, one new catalog value names:

1. the prospective table storage map, and
2. the replacement root for every HNSW index on that table.

One `catalog.save`/alternate-superblock publication makes that whole catalog
reachable. There is no catalog state in which the new rows are visible with
an accidentally incomplete index. Existing indexes on other tables and all
other catalog content are copied unchanged.

This choice has four important trade-offs:

- It preserves today's all-or-nothing COPY result. A CSV, PK, vector,
  corruption, or memory error occurs before the commit point and leaves the
  pre-COPY logical state authoritative.
- It holds the commit pipe for index construction, potentially much longer
  than row encoding alone. COPY already holds that pipe for its whole load;
  readers remain nonblocking on their pinned `PublishedState`, while later
  writers wait.
- It may write unreachable node-group and HNSW pages on an error or SIGKILL.
  On the error path (not SIGKILL), the COPY facade MUST queue every
  prospective-generation page id it allocated (T1 groups, batch roots, CSR
  cells, directories) in memory
  and the NEXT successful publication retires them into the ledger
  (FREE_PAGES § Retirement sequence step 5). SIGKILL-abandoned pages remain
  sweepable only through `doctor --sweep`. The write count must still be
  measured because eMMC wear is a product constraint.
- It requires a page-backed construction accessor and bounded build driver.
  The current storage build is batched, but the facade's `ConstructionRows`
  first materializes `Vec<Option<Vec<f32>>>` for the complete table
  (`crates/devondb/src/database/hnsw.rs`). Reusing that facade shape unchanged
  would violate the edge-memory contract.

## Required invariants

The following invariants are binding.

1. `COPY` still refuses while any write transaction is open and still holds
   the commit pipe and publication gate through its final in-memory publish.
2. The ordinary checkpoint completes before CSV processing. Its durable
   result is the operation's pre-COPY baseline; the WAL is empty and no HNSW
   overlay delta remains to be paired with the prospective table state.
3. CSV parsing, header/type validation, PK validation, optional sort, and
   node-group encoding retain `docs/SCALE.md` §5 semantics.
4. Bulk construction uses the existing index's persisted metric, `M`, `M0`,
   `ef_construction`, navigation encoding, and `level_seed`. COPY does not
   choose new index parameters or a new seed.
5. Each replacement root has `covered_rows == prospective_table_row_count`.
   Null vector rows count toward coverage but are absent from topology, as in
   `docs/HNSW.md` §1.
6. Every root is validated against the prospective catalog schema and the
   exact indexed physical encoding before publication. A mismatch is
   corruption or a validation error, never a stale-index fallback.
7. If a table carries multiple HNSW indexes, they build sequentially under
   one budget. Either all replacement roots and the rows publish together, or
   none do.
8. No build result is put in the WAL. Base node pages remain the source of
   truth; HNSW remains derived state stored through its existing root/CSR
   format.
9. Old snapshots retain the old catalog, table groups, and roots. New
   snapshots receive the new catalog, groups, and roots. A snapshot never
   mixes generations.
10. COPY acknowledges success only after the catalog/superblock is durable
    and the matching `PublishedState` has been installed.

## Build sequence

The synchronous path is:

1. Require writable mode, acquire the commit pipe and publication gate, and
   verify write-transaction quiescence.
2. Resolve the target node table and collect all HNSW catalog entries whose
   table matches under the catalog's ASCII fold. Do not call the current
   refusal helper.
3. Run `checkpoint_locked`. This materializes committed rows and HNSW deltas,
   publishes their roots with the checkpointed table state, and truncates the
   WAL under existing law. Treat the resulting catalog as baseline `C0`.
4. Open and validate the CSV, validate all primary keys, optionally sort the
   incoming rows, and feed `BulkNodeWriter`. It writes prospective table
   groups `T1` to fresh pages without changing `C0`.
5. Finish the prospective table storage map. Drop the CSV sort buffer,
   existing/file PK sets, and other loader-only charges before HNSW
   construction. The groups remain durable/re-readable pages; retaining the
   logical rows in memory is forbidden.
6. Form an in-memory prospective catalog view `C1` that differs from `C0`
   only by naming `T1`. Do not save it. A page-backed
   `ConstructionVectorAccess` reads vectors by stable node offset through
   `C1`, pinning only the pages needed for one access.
7. For each affected index in catalog order, load and validate its current
   root, then build from that root's coverage through the row count in `C1`.
   Use bounded batches and copy-on-write CSR/root publication. Intermediate
   batch roots are unreachable and are inputs only to the next batch.
8. Require full target coverage. Retain only each final root page id and
   decoded validation metadata; release that index's construction arena
   before starting the next index. A budget stop is an error here, not a
   partially covered success.
9. Install all final root page ids into `C1`. Revalidate table identity,
   column type/encoding, metric/config, root coverage, and the fact that the
   set of affected indexes still equals the set captured under the fence.
10. Sync every prospective table and index page. Allocate the normal fresh
    COPY publish LSN, save `C1`, and fsync the alternate superblock. This
    superblock fsync is the single commit point.
11. Rotate the still-empty WAL using the existing COPY discipline and install
    one `PublishedState { catalog: C1, chain: None, ... }`. Then return
    success.

The build driver should extend the existing batched construction seam rather
than call checkpoint tail catch-up one row at a time. The storage path already
has deterministic range proposals and batched initial build
(`build_initial_index` in
`crates/devondb-storage/src/hnsw/index.rs`); a bulk catch-up driver can reuse
those pieces with the old root as its starting publication. This avoids
rewriting affected CSR groups once per imported row.

## Crash windows and exact publication order

Let `C0` be the authoritative post-checkpoint baseline and `C1` the catalog
that names both the prospective table groups and all full-coverage replacement
roots. The only visibility edge is the alternate-superblock publication of
`C1`.

| SIGKILL point | Recovery-visible state | KNN consequence |
|---|---|---|
| Before or during the preliminary checkpoint | The ordinary checkpoint/WAL rules choose the prior state or its logically equivalent checkpointed form | Existing table/index pairing; no CSV row is visible |
| After checkpoint, during CSV parsing or node-group writes | `C0`; new group pages are unreachable | Existing table/index pairing |
| During any HNSW batch, CSR write, directory write, or root write | `C0`; prospective groups and all build pages are unreachable | Existing table/index pairing |
| After data/index sync but before catalog save | `C0` | Existing table/index pairing |
| During catalog-page write or before the alternate superblock is durable | The old valid superblock still names `C0`; a torn catalog/superblock is ignored by existing CRC/dual-slot law | Existing table/index pairing |
| After alternate-superblock fsync but before WAL rotation or `shared.publish` | Reopen chooses `C1`, which names the rows and every full-coverage root together | New rows are represented; no uncovered omission is possible |
| After `shared.publish` or after return | `C1` | New snapshots use the matching table/root generation; old pinned snapshots continue on `C0` |

The WAL is empty after step 3 and cannot gain records while the commit pipe is
held. Therefore a crash after the `C1` superblock publication but before the
empty-WAL rotation cannot replay a CSV row or duplicate one. The new
`checkpoint_lsn`/catalog root is authoritative exactly as in current COPY.

There is also no live-process mixing window. Until `shared.publish`, readers
starting from the in-memory head still pin `C0`; after it, readers pin `C1`.
Both are self-consistent. The disk becoming newer just before the in-memory
head is swapped does not mutate either snapshot.

This is stronger than the minimum HNSW correctness law. A two-phase design
could safely publish rows with the old root because exact-tail union would
cover them. The recommendation deliberately does not expose that intermediate
state, preserving COPY's existing all-or-nothing outcome and avoiding an
unbounded post-success latency cliff.

## Memory and Pi-class behavior

### The non-negotiable bound

An unsorted CSV whose data is larger than `memory_limit` must still be
loadable when the index's bounded working set fits. Dataset size alone is not
a reason to materialize all vectors or refuse. The bulk path must replace the
facade's whole-table `ConstructionRows` shape with a page-backed accessor that
returns:

- the decoded construction vector for the current node,
- the exact encoded main-run navigation slot for that node, and
- for b1, the configured rescore representation only when construction or
  final scoring requires it.

The accessor pins no snapshot-wide set of frames and retains no
`Vec<Option<Vec<f32>>>`. Page frames charge the pager budget once and remain
evictable after the access.

The resident build set is bounded by these measured components:

| Component | Bound |
|---|---|
| Vector access | The pages for one validity/main/rescore access plus decoded and encoded scratch proportional to vector dimension |
| Construction search | `ef_construction` heaps, the bounded visited table, one adjacency list, and scalar scorer scratch under `docs/HNSW.md` §6-style discovery bounds |
| Batch proposal | Complete charged replacements for at most the selected batch size; batch size is adaptive and never the total CSV row count |
| CSR output | One changed `(layer, node-group)` builder at a time; at most the actual group rows (writer policy currently 2048) times the layer degree cap |
| Directory/root | The actual layer-major `8 * layer_count * group_count` matrix, its encoded publication buffer, and root/page overhead |
| Multiple indexes | One index's working set at a time; only already-written final root metadata survives between indexes |

The layer directory is intentionally called out: it is small for ordinary
edge datasets but grows with node-group count. If that metadata alone does not
fit, the implementation must refuse rather than pretend the build is
constant-memory.

### Bound calculation and adaptive batches

Before writing the first HNSW batch, compute a checked `BuildMemoryBound` from
the prospective row/group counts, vector dimension and physical slot sizes,
index config (`M`, `M0`, `ef_construction`), eligible levels derived with the
persisted seed, allocator-overhead constants, and actual container
capacities. The same helpers that reserve search, replacement, CSR, and
publication memory must feed this calculation; a second optimistic formula
would drift.

Start with the existing initial-build policy of 64 rows as a writer-policy
target, then reduce the batch until the conservative bound fits. Batch
reduction changes flushing and page ids, not insertion order or topology. A
one-row batch is the minimum. All arithmetic is checked.

On charge failure, apply `docs/MVCC.md` §7.2 in this order:

1. shed clean, unpinned pager frames and retry once through
   `charge_or_reclaim`;
2. treat the write-path checkpoint rung as already exhausted by the
   preliminary checkpoint—recursively checkpointing inside the COPY fence
   cannot create headroom from an empty overlay;
3. release loader-only state and reduce the HNSW batch;
4. do not spill HNSW candidate, visited, proposal, or CSR-construction state
   (`docs/HNSW.md` §6.3); and
5. if the measured one-row bound still cannot fit, return
   `DevonError::BudgetExceeded` before `C1` publication.

The error context must name the index and report at least `required`,
`available`, current `charged`, configured `limit`, prospective rows, vector
dimension/encoding, chosen batch size, and the dominant component. This is the
honest measured refusal required on a tight device. A hard-limit test must
also assert that observed peak charge never exceeds `memory_limit`.

`COPY ... sort by ...` retains its separate binding rule: the current sort
buffers the whole incoming load under `ChargedBytes` and may refuse before
index construction. Index-aware COPY does not disguise that sort-buffer
limit. The unsorted path is the proof that a data corpus larger than
`memory_limit` can be loaded and indexed with bounded memory.

## Quantized encodings

The catalog spelling is `VectorEncoded`. It records a vector column's
physical encoding; it is not a second logical vector value type. Index-aware
COPY must preserve the existing `docs/FORMAT.md` and `docs/HNSW.md` rules:

- `Vector(dim)` uses f32 navigation.
- `VectorEncoded` f16 navigation reads the exact f16 main slot and uses the
  canonical decoded f16 values for construction distance.
- `VectorEncoded` i8 navigation reads the exact per-vector
  scale/offset/codes main slot and uses its canonical decode for construction
  distance.
- `VectorEncoded` b1 navigates from the exact packed-sign main slot, supports
  cosine only, and requires a non-`none` rescore run. Its construction query
  comes from the configured rescore representation; candidate navigation must
  not be reconstructed from those rescore values. b1+L2 and
  `rescore:"none"` remain ineligible for an HNSW index.

This distinction matters for both determinism and recall. Bulk groups already
contain the authoritative quantized main and rescore payloads. Re-encoding a
dequantized `Value::Vector` can introduce a second rounding step and, for b1,
can disagree with the stored navigation bits. The page-backed accessor must
therefore expose the stored main slot directly instead of synthesizing it.

HNSW pages continue to store only node offsets and topology. They do not gain
a copy of f32, f16, i8, b1, or rescore payloads. A root's navigation encoding
must exactly match the prospective catalog column, and corruption is not
hidden by brute-force fallback.

## Determinism

The seed is not a COPY option. For an existing index, COPY preserves the
`level_seed` persisted in its root for the entire index lifetime. That seed
is derived by the rule in `docs/HNSW.md` §10 decision 2:

```text
db_fold = le_u64(db_id[0..8]) XOR le_u64(db_id[8..16])
level_seed = db_fold XOR fnv1a64(index_name_utf8)
```

Level assignment is SplitMix64 over `level_seed XOR node_offset`.
Construction uses scalar kernels on every ISA; candidate and retained-list
ties use node offset. COPY must feed one canonical insertion order:

- existing offsets first, unchanged;
- unsorted incoming rows in CSV record order; and
- sorted incoming rows by the binding sort key, with original CSV record
  ordinal as the final tie-breaker and null placement unchanged.

The source ordinal tie-breaker is needed because equal sort keys otherwise
leave HNSW insertion order to a sort implementation detail. It does not alter
zone-map usefulness.

Under the same pre-COPY snapshot, schema/physical encodings, HNSW config and
seed, CSV bytes, and canonical insertion order, the ordered neighbor lists and
CSR topology payloads are identical. This design does **not**
claim that “same CSV + same seed” alone makes every physical root/directory
byte identical: node offsets and the existing graph are inputs, and root and
directory pages contain allocated page ids. A killed attempt may leak pages,
so a retry can allocate different ids while producing identical topology and
answers. That is consistent with `docs/HNSW.md`'s binding promise—determinism
for a fixed snapshot, configuration, and insertion order—without promising an
allocator-independent file image.

## Format and golden-corpus impact

No on-disk change is expected. The implementation composes existing encodings:

- ordinary node groups and quantized payload/rescore runs;
- ordinary zero-property `RCSR` groups for HNSW layers;
- the existing HNSW root and layer-directory layout;
- existing top-level catalog `indexes` entries and `HNSW_INDEX` feature bit;
  and
- the existing dual-superblock catalog publication.

It adds no feature bit, WAL record, catalog field, root field, `RCSR` spelling,
vector encoding, or DevonPlan statement. Consequently `docs/FORMAT.md` and
`docs/PLAN_IR.md` need no semantic/layout change, and every
existing golden file must remain byte-for-byte untouched and continue to
open. An indexed-COPY fixture is additive corpus coverage; it
must not replace or rewrite an old anchor. Most validation should be
end-to-end and kill-injection tests over temporary databases because the new
property is publication behavior, not a new byte grammar.

## Verification

Tests must prove the user-visible behavior, not only helper plumbing:

0. **COPY-failure retirement:** fail
   an indexed COPY, run one successful publication, assert the prospective
   generation's pages entered the ledger and are reused by the next load;
   sever the in-memory queue and the gate must fail.

1. Copy into a table with one f32 HNSW index; after success, assert the root
   covers the full row count and approximate KNN includes a CSV row that is
   the nearest result.
2. Repeat for `VectorEncoded` f16, i8, and b1 with each admitted rescore kind;
   compare distances/recall under the existing HNSW gates.
3. Copy into a table with multiple HNSW indexes. Inject failure while building
   the last index and prove neither the rows nor any earlier replacement root
   became visible.
4. Kill at every crash-table boundary: mid-node-group, mid-CSR, after root
   sync, during catalog save, after alternate-superblock fsync, and before the
   in-memory swap. Reopen must see either `C0` or complete `C1`.
5. Run an unsorted corpus whose encoded data exceeds `memory_limit` but whose
   one-row construction bound fits. It must succeed, remain under both the
   shared accountant and OS hard limit, and create no spill file.
6. Set the limit below the measured one-row/directory bound. COPY must return
   `BudgetExceeded` with the required measurements and leave `C0`
   authoritative.
7. Hold an old read snapshot throughout COPY. It must return identical rows
   and KNN results before and after publication; a fresh snapshot sees `C1`.
8. Run the same logical build twice from identical cloned pre-COPY files and
   compare canonical adjacency/topology payloads. Separately allow physical
   page references to differ after an injected leak.
9. Severing any one of full-coverage validation, root/table co-publication,
   stored-slot access, scalar construction scoring, or memory charging must
   make a named gate fail.

## Rejected shapes

- **(a) Keep refusing, then require a manual reindex statement.** This moves
  the same expensive build into a second operation, requires a new public
  `REINDEX`/drop lifecycle, and makes correctness/performance depend on the
  operator remembering it. A crash or budget error between statements leaves
  an operationally surprising state. The synchronous build machinery is
  needed either way, so manual choreography buys no implementation shortcut.
- **(b2) Publish rows first, then synchronously publish a caught-up root.**
  This intermediate is correctness-safe because the old root plus exact-tail
  union includes every new row. It is nevertheless rejected for the first
  implementation: a post-row-publish build error cannot preserve COPY's
  all-or-nothing error contract, it adds a second catalog/superblock write on
  wear-sensitive storage, and it exposes an arbitrarily large exact-tail
  latency cliff. The exact-tail property remains the recovery proof and a
  possible future relaxation, not the normal success path.
- **(c) Background build with a `not-ready` state that refuses KNN.** HNSW
  already has a correct not-fully-covered representation: a root plus exact
  tail. Inventing a KNN-refusing state discards that property, adds catalog
  lifecycle/format semantics, cancellation and restart policy, and a
  background execution facility to a synchronous no-async core. It also
  denies exact KNN even though brute force is available.
- **Publish the rows with a stale root but omit exact-tail union.** This is the
  precise silently-wrong result forbidden by `docs/HNSW.md` and
  `docs/UI.md` §12.5. No performance argument can admit it.
- **Materialize all construction vectors.** The facade's current
  `ConstructionRows` is acceptable only while the corpus fits. Reusing it for
  bulk load makes peak memory proportional to `rows * dimension`, violates
  `memory_limit`, and fails the Pi-class target.
- **Spill HNSW search/proposal state.** Candidate, visited, and reciprocal
  replacement access is random and write-amplifying; `docs/HNSW.md` §6.3
  explicitly chooses bounded failure rather than HNSW spill. Page-backed
  vector access and adaptive batches solve the sequential-data part without
  inventing a spill format.
- **Rebuild every index from an empty root unconditionally.** It is correct
  but rewrites the valid prefix and amplifies eMMC writes. Append-only COPY can
  start from the persisted immutable root and deterministically construct its
  missing suffix. A full rebuild remains a separate operator for corruption,
  parameter changes, or future compaction—not the default bulk-load path.
