# devondb persistent MVCC-aware HNSW

This document defines the persistent HNSW design. Every byte-layout decision
is also specified in `docs/FORMAT.md`.

Read with `docs/ARCHITECTURE.md` §§ Vectors and Edge budget,
`docs/FORMAT.md`, and `docs/MVCC.md`. Following the NaviX architecture,
HNSW adjacency is stored in the graph database's own CSR pages and buffer
manager rather than in a separate ANN file or an unbudgeted in-memory graph.

---

## 1. Mandate, scope, and vocabulary

This design adds an optional persistent HNSW access path for one vector column and one distance metric. DevonPlan's `KnnScan` remains the logical operation; the existing brute-force executor remains the exact ground-truth access path (`docs/PLAN_IR.md:41-51`, `crates/devondb-exec/src/knn.rs:20-24`).

Indexed tables support scalar/vector updates, deletes and detach-delete. Visible update/delete history disables ANN until checkpoint rebuilds topology over the materialized offsets (§5.7). Filtered HNSW and online graph compaction remain follow-ups.

Terms used below:

- **index** — one named HNSW over `(node table, vector column, metric)`.
- **covered rows** — the contiguous node-offset prefix considered by the index. Null vectors count toward the prefix but do not become HNSW nodes.
- **exact tail** — visible table rows at offsets `covered_rows..row_count`. Every indexed query scans this tail exactly and unions it with ANN results.
- **persistent root** — the immutable `HNSW` page named by a checkpointed catalog version.
- **index delta** — immutable, committed overlay replacements for neighbor lists plus an optional entry-point replacement and coverage advance.
- **snapshot index view** — one persistent root plus exactly the index deltas reachable from the snapshot's immutable `PublishedState` chain.

The following rules are binding:

1.  HNSW adjacency MUST live in the main devondb file as ordinary paged CSR. There is no sidecar index file and no mmap.
2.  HNSW MUST use the same `Pager`, page cache, pin rules, checksums, memory accountant, catalog copy-on-write, and checkpoint publication as tables.
3.  The index MUST NOT contain a second copy of vector payloads. It stores node offsets and topology; vector and rescore bytes remain node-group columns.
4.  A snapshot MUST observe an index root and delta chain from the same pinned `PublishedState` as its node rows. Mixing a fresh index with an old table snapshot, or the reverse, is forbidden.
5.  A query MUST exact-scan the uncovered tail. Index lag therefore affects latency, never row visibility or correctness of tail participation.
6.  Budget exhaustion in the ANN access path MUST discard ANN working state and restart on the brute-force path. It MUST NOT spill HNSW search state.
7.  Index corruption is not budget exhaustion. A bad root, directory, checksum, neighbor bound, or degree bound MUST return `DevonError::Corrupt`; it MUST NOT be silently hidden by brute-force fallback.
8.  Topology construction MUST be deterministic for a fixed snapshot, index configuration, and insertion order. Distance ties use node offset.
9.  The initial implementation supports `l2` and `cosine` on f32/f16/i8 navigation, and `cosine` only on b1 navigation. Unsupported combinations use brute force.
10. Only one active index may exist for a `(table, column, metric)` tuple. This avoids optimizer ambiguity and duplicate edge-device write cost.

These rules implement the architecture's v2 vector direction (`docs/ARCHITECTURE.md:136-151`) under the edge rules for bounded memory, user-space caching, runtime-only SIMD, and eMMC-aware writes (`docs/ARCHITECTURE.md:74-105`).

---

## 2. Graph model and deterministic HNSW policy

### 2.1 Node identity

An HNSW node id is the table's existing u64 node offset. No HNSW-local id map exists. Node offsets are stable append positions in checkpointed storage (`docs/FORMAT.md:163-169`) and remain checkpointed position plus overlay position under MVCC (`docs/MVCC.md:248-256`).

Using node offsets has four consequences:

1.  CSR neighbors use the existing u64 neighbor encoding byte-for-byte.
2.  Result tie order equals source row order, matching the brute-force candidate comparator (`crates/devondb-exec/src/knn.rs:167-172`).
3.  The index may fetch vector and result columns without a mapping table.
4.  A neighbor is corrupt if it is outside the root/delta's covered prefix or names a null-vector row.

### 2.2 Parameters

The initial writer policy is:

| Parameter | Default | Valid range / rule |
|----|---:|----|
| `M` | 16 | 4..=64 |
| layer-0 degree `M0` | 32 | exactly `2 * M` |
| `ef_construction` | 200 | `M0..=4096` |
| `ef_search` | 128 | request policy; effective value is at least `k` |
| maximum level | 63 | fixed format/algorithm limit |
| b1 oversample | 3 | effective candidates at least `3 * k` |

These are writer/query policy except `M`, `M0`, `ef_construction`, the level seed, and metric, which are persisted because they define topology.

### 2.3 Level assignment

Maximum levels are derived, not stored. Let `h` be SplitMix64 initialized with `level_seed XOR node_offset`. Level starts at zero. While level is below 63 and the next SplitMix64 output modulo `M` is zero, increment level and consume another output. The first nonzero remainder stops the loop.

This is a geometric distribution with promotion probability `1/M`. It avoids platform-dependent logarithms, mutable RNG state, and a byte per node. The SplitMix64 step is exactly the step already specified for b1 rotation (`docs/FORMAT.md:390-400`).

Null-vector rows have a derived level but are absent from every layer. They cannot be an entry point or neighbor.

### 2.4 Neighbor selection and order

Insertion uses conventional HNSW greedy descent above the insertion level and best-first `ef_construction` search at participating layers. Diversified neighbor selection MUST be deterministic:

1.  Candidate order is `(distance.total_cmp, node_offset)`.
2.  The heuristic evaluates candidates in that order.
3.  A retained neighbor list is stored in that same order.
4.  A layer-0 list has at most `M0` entries; upper-layer lists have at most `M` entries.
5.  Every selected connection is reciprocal. Adding a reciprocal edge that exceeds the cap reruns deterministic pruning for the existing node.
6.  Duplicate neighbor ids and self-neighbors are forbidden.

An insertion therefore replaces complete ordered neighbor lists for the new node and any existing nodes changed by reciprocal connection/pruning. Full replacement, rather than add-only edge deltas, is required because pruning removes old neighbors.

The single entry point is the eligible node with the greatest level seen so far. A higher-level insertion replaces it; equal levels retain the older (lower-offset) entry. An empty index uses no entry point.

---

## 3. Persistent graph layout: HNSW layers as CSR groups

### 3.1 Catalog identity

The catalog gains a top-level `indexes` array after `storage`. Entries are in creation order and use this canonical compact JSON field order:

```json
{"name":"embedding_cos","kind":"hnsw","table":"Corpus","column":"embedding","root":42}
```

`name`, table name, and column name retain the spelling supplied by index DDL
and match by ASCII lowercase folding on both sides. Non-ASCII bytes compare
exactly. Index names and `(table, column)` tuples are unique under that fold:
DDL rejects a fold-equal collision as an invalid argument, and a persisted
catalog containing one is corruption. `root` is the page id of an immutable
HNSW root. The metric and topology parameters live in the root so there is one
authoritative persistent configuration.

The catalog entry is deliberately outside the existing shape-discriminated `storage` map. `TableStorage` and `RelStorage` reject unknown fields, while the top-level `Catalog` can gain a defaulted field (`crates/devondb-storage/src/catalog.rs:20-55`). Keeping index metadata at top level lets an older read-safe reader ignore it and scan base columns.

### 3.2 HNSW root page

All integers are little-endian. One root occupies one page:

| Offset | Size | Field | Rule |
|---:|---:|----|----|
| 0 | 4 | magic | ASCII `HNSW` |
| 4 | 2 | `layout_version` | `1` for this layout |
| 6 | 1 | `metric` | `0 = l2`, `1 = cosine` |
| 7 | 1 | `navigation_encoding` | `0=f32`, `1=f16`, `2=i8`, `3=b1` |
| 8 | 2 | `m` | 4..=64 |
| 10 | 2 | `m0` | exactly `2*m` |
| 12 | 4 | `ef_construction` | `m0..=4096` |
| 16 | 8 | `level_seed` | fixed for index lifetime |
| 24 | 8 | `covered_rows` | table offset prefix represented |
| 32 | 8 | `entry_node` | u64::MAX when no eligible node |
| 40 | 1 | `entry_level` | zero when empty; otherwise ≤ 63 |
| 41 | 1 | `layer_count` | zero when empty; otherwise `entry_level + 1` |
| 42 | 2 | reserved | zero |
| 44 | 4 | `group_count` | node groups intersecting the covered prefix |
| 48 | 8 | `layer_dir_first_page` | zero only if `layer_count == 0` |
| 56 | 4 | `layer_dir_byte_len` | exact payload length |
| 60 | 4 | `layer_dir_crc32c` | CRC-32C over payload |

Bytes 64 through page end are zero. Root decoding validates the vector column still exists, its physical encoding matches `navigation_encoding`, its metric is supported, and `covered_rows` does not exceed the table rows visible in the same catalog.

### 3.3 Layer directory payload

The layer directory is one contiguous payload run using the existing `first_page`, `byte_len`, CRC-32C, zero-tail, and contiguous-allocation rules. Its exact length is:

```text
layer_count * group_count * 8
```

It is a layer-major matrix of u64 CSR directory page ids. Cell `layer * group_count + group` names adjacency for that layer and base node group. Zero is the only spelling of an empty group. Layers are numbered from zero upward.

The matrix replaces a catalog-sized array per layer. The catalog remains one small root reference even for large tables; only a query using the index reads the layer directory.

### 3.4 Exact reuse of `RCSR`

Every nonzero layer-directory cell names an existing-format `RCSR` group with zero property columns:

- offsets are `(row_count + 1) * u32`;
- neighbors are `edge_count * u64`;
- `column_count` is zero;
- no forward/backward duplicate is written;
- `row_count` is the number of covered table rows in that node group when the root was published;
- every neighbor is below `covered_rows` and eligible in that layer.

The current `CsrGroup` already stores offsets, u64 neighbors, and property columns separately (`crates/devondb-storage/src/csr_group.rs:23-30`), accepts zero property columns, and validates row/edge structure (`crates/devondb-storage/src/csr_group.rs:201-219`). Its directory and payload writer are reused byte-for-byte (`crates/devondb-storage/src/csr_group.rs:126-145`).

HNSW uses one directed CSR per layer. It MUST NOT use relationship storage's fwd+bwd duplication because every HNSW connection is already inserted in both nodes' outgoing lists. Writing both relationship directions would duplicate the duplicate and violate the eMMC rule.

The existing relationship checkpoint algorithm is the structural precedent: it groups mutations by endpoint node group, reads an old group, writes a fresh replacement, and swaps its page id (`crates/devondb-storage/src/rel_table.rs:524-579`). HNSW does the same per `(layer, node-group)`.

### 3.5 Required `csr_group` code extension

The byte format does not change, but the read API must. Today `read_inner` materializes the complete offsets array, neighbors array, and all columns (`crates/devondb-storage/src/csr_group.rs:163-198`), while `read_payload` collects a complete run (`crates/devondb-storage/src/csr_group.rs:526-549`). That is acceptable for relationship scans but not for random HNSW hops.

HNSW uses a read-only `CsrSlotReader` over the same `RCSR` bytes:

1.  Decode and validate the directory page.
2.  On first access to a group in one query, validate offsets and neighbors CRCs incrementally one page at a time without retaining the full runs.
3.  Read only offsets `[slot, slot + 1]`, then the corresponding neighbor byte range. At most one adjacency list is retained.
4.  Record the verified directory page id/checksum in a budget-charged per-query set so repeated visits do not rescan whole payloads.
5.  Validate monotonic endpoints, final offset, neighbor bounds, duplicate/self neighbors, eligibility, and layer-specific degree cap.

Checkpoint construction continues using `CsrGroup::new`, `push_edge`, and `write` (`crates/devondb-storage/src/csr_group.rs:39-79`). No HNSW-specific CSR magic or second adjacency codec is introduced.

---

## 4. Snapshot search path

### 4.1 Access-path selection

A `KnnScan` may use HNSW only when a visible index matches table, column, and metric; the query dimension matches; and the encoding/metric pair is supported. Otherwise it invokes the existing brute-force operator.

The brute-force implementation scans every source chunk once, holds a max-heap of at most `k` cloned rows, and uses runtime-dispatched distance kernels (`crates/devondb-exec/src/knn.rs:60-100`, `105-133`, `198-206`). It remains the oracle, fallback, and explicit exact path.

### 4.2 Search sequence

For a matching index, one snapshot query performs:

1.  Pin the snapshot's `PublishedState` once.
2.  Resolve the persistent root and reachable index deltas from that state.
3.  Derive the view's entry point, layers, neighbor replacements, and `covered_rows`; never consult the current database state again.
4.  If an entry exists, greedily descend upper layers with `ef = 1`.
5.  Search layer zero best-first with effective candidate width `E` from §6.2.
6.  For b1, retain the oversampled candidate set and load only those rows' rescore slots. For other encodings, navigation distances are also final candidate distances.
7.  Exact-scan visible rows from `covered_rows` through the snapshot row count, including committed overlay rows and transaction-own rows when applicable.
8.  Union indexed candidates and exact-tail candidates by node offset, keeping the best computed representation for duplicates.
9.  Sort by `(final_distance.total_cmp, node_offset)`, truncate to `k`, fetch result columns from the same snapshot, and emit the existing trailing `Float64 distance` column.

The tail scan is required even when index maintenance normally keeps up. It is the recovery path, the budget-pressure path for writes, and the correctness joint between a physical index and append-only MVCC.

### 4.3 Visibility invariants

- A base CSR neighbor is visible only if below the persistent root coverage.
- An overlay replacement is visible only if its `CommitLink` is reachable from the snapshot.
- Newest reachable replacement wins for a `(layer, node)` slot; deltas are immutable after publication.
- An entry-point replacement and coverage advance become visible with the same commit delta as their neighbor replacements.
- Old snapshots never see new topology or new rows. Fresh snapshots see both.
- A transaction reads its own uncommitted vector rows through exact-tail scanning; it does not build private HNSW topology for read-your-writes.

These are structural visibility rules, matching MVCC's immutable published pointer model (`docs/MVCC.md:25-33`) rather than per-edge timestamp filters.

---

## 5. MVCC, commit, checkpoint, and recovery

### 5.1 In-memory index delta

`CommitDelta` gains a map keyed by stable index identity. Each value contains:

```rust
struct HnswDelta {
    old_covered_rows: u64,
    new_covered_rows: u64,
    replacements: BTreeMap<(u8 /* layer */, u64 /* node */), Arc<[u64]>>,
    entry: Option<(u64 /* node */, u8 /* level */)>,
}
```

This is a design shape, not a mandated public Rust spelling. The invariants are binding: immutable after publish; contiguous coverage only; complete ordered list replacements; no duplicate slot; degree/visibility checks before publish; and all allocated capacity charged as committed overlay memory.

The overlay extends the existing `CommitLink` vocabulary, whose rows and edges are immutable and released only after checkpoint plus the last old snapshot (`docs/MVCC.md:195-240`, `301-306`). The read shape mirrors persisted-CSR plus buffered-overlay relationship neighbors (`crates/devondb-storage/src/rel_table.rs:183-306`); it is not a mutable process-global HNSW.

### 5.2 Insert preparation and commit

Under the physically serialized commit pipeline, after row offsets are assigned but before WAL fsync/publication:

1.  For each matching index, compare current visible table rows with that index view's coverage.
2.  If an older uncovered tail already exists, do not extend this index during the commit. The new rows join the exact tail.
3.  Otherwise insert eligible new vectors in canonical transaction row order, searching the current index view plus earlier rows in this transaction.
4.  Build complete deterministic neighbor-list replacements and any entry-point change in transaction-local charged memory.
5.  If the index construction charge fails after cache reclaim, discard only this derived index proposal. The base transaction may still commit; these rows become exact tail.
6.  Append and fsync the ordinary base transaction WAL records exactly once.
7.  Publish base rows and successful HNSW deltas together in one new `PublishedState` after the WAL is durable.

HNSW proposals are derived state and are not added to the WAL. This avoids logging O(M) neighbor-list replacements per inserted vector and honors the eMMC write-amplification constraint (`docs/ARCHITECTURE.md:103-104`). Base data remains the source of truth from which missing index coverage can be rebuilt.

Crash between base WAL fsync and publication is already safe: recovery sees the committed row. It may not recover the ephemeral HNSW proposal, so it places that row in the exact tail (§5.4). Query-visible data is never lost.

### 5.3 Conflict semantics

Index topology is derived and MUST NOT add a user-visible write-write conflict. The base transaction keeps the conflict rules in `docs/MVCC.md:437-469`.

Commits are serialized while offsets and index proposals are finalized, so two successful proposals cannot concurrently replace the same list. A later commit builds from the already-published earlier view. If index maintenance cannot run within budget, tail fallback replaces conflict or transaction failure.

Index creation conflicts with another index creation of the same name or the same `(table, column, metric)` tuple. It also fails cleanly if its table/column identity changed before publication. These are catalog/DDL conflicts, not row adjacency conflicts.

### 5.4 Recovery

Recovery loads the HNSW root named by the checkpointed catalog, then replays ordinary committed WAL transactions as today. Because index deltas are not in the WAL:

1.  Persistent coverage is exactly the root's `covered_rows`.
2.  Every recovered row at or beyond that offset is an exact-tail row.
3.  No topology is recomputed during open; recovery latency stays proportional to ordinary WAL replay rather than ANN construction.
4.  A later explicit/automatic checkpoint may catch the tail up (§5.5).

This deliberately permits performance state to roll back to the last checkpoint after a crash while preserving table state and query visibility. It also avoids architecture-dependent topology reconstruction during recovery.

### 5.5 Checkpoint

Checkpoint runs under the existing commit lock; readers continue on pinned old roots and pages. The binding sequence is:

1.  Materialize committed node rows first, preserving stable offsets.
2.  Collect reachable HNSW deltas oldest-first.
3.  For every changed `(layer, node-group)`, load the old CSR if present, apply newest list replacements per slot, and write one fresh `RCSR` group.
4.  Copy unchanged CSR page ids into the new layer directory; use zero for empty groups. Never rewrite a published CSR page in place.
5.  Optionally attempt catch-up of an existing exact tail, row by row, through the same bounded construction path. If optional catch-up exhausts budget, stop at the last contiguous successful row; checkpoint itself may proceed.
6.  Write and fsync the new layer-directory run and `HNSW` root.
7.  Publish a fresh catalog containing the new root together with the freshly materialized node-group maps and the checkpoint commit LSN.
8.  Truncate the WAL only after alternate-superblock publication succeeds.
9.  Publish a new `PublishedState` without drained HNSW deltas. Old roots and deltas live until the last old snapshot drops.

Mandatory merge work for already-published HNSW deltas processes one CSR group at a time. If even one group cannot fit after reclaim, checkpoint returns `BudgetExceeded` and retains WAL plus the old published root. Optional tail catch-up may stop without failing checkpoint because uncovered rows remain queryable exactly.

This is the same copy-on-write argument as relationship checkpointing and old snapshot reads (`docs/MVCC.md:349-359`, `538-567`).

### 5.6 Index build and publication

CREATE HNSW INDEX stages validated schema/configuration intent and remains the only statement in its transaction. Publication takes the existing commit/publication lock, checkpoints the current visible rows, validates schema identity again, then constructs the root from those materialized pages using bounded adaptive batches. The lock remains held throughout construction. This longer writer hold is an accepted correctness/latency tradeoff: updates, deletes and offset remaps cannot race an older prepared topology into the new catalog. The call returns after the catalog/root publication is fsynced.

### 5.7 Indexed mutations and checkpoint repair

Any node update or delete in the captured commit chain or transaction's own delta makes every index on that table dirty. Inspect raw history, including delete/reinsert whose net tombstone disappears. Scalar-only changes conservatively qualify. Dirty approximate KNN validates the persisted root/configuration, then runs exact against the same MVCC view; no new insertion proposals derive from stale topology. Inserts remain visible in that exact scan. Clean tables retain the existing ANN and exact-tail policy.

Dirty exact scans reserve decoded group/overlay peaks before allocation, use tight source chunks, and reserve the exact executor's final chunk builder/conversion capacity. Candidate rows remain charged until output release. Large variable-length groups can return BudgetExceeded even when k is small; the scan does not exceed the configured memory budget to make progress.

Checkpoint first materializes effective node rows and remaps relationships, then rebuilds affected HNSW indexes from scratch over the new physical offsets, preserving persisted configuration and level seed. Page-backed construction halves batches under pressure and may publish a valid fresh partial or empty prefix with an exact tail. It never retains the old topology after remap. Catalog publication atomically installs materialized rows and repaired roots; pinned snapshots retain their previous rows/root/chain. Fresh successful insertion proposals and checkpoint catch-up resume on the repaired epoch.

HNSW_MUTATION_WAL (FORMAT bit 14) is a non-read-safe, checkpoint-scoped interpretation fence. Under the commit lock the CURRENT catalog determines whether DML requires the bit, including a transaction staged before a concurrent CREATE. Both superblock slots receive the bit before any governed WAL append. Recovery requires the bit for indexed DML and derives dirtiness from records; topology never enters WAL. Catalog save retains the fence until the whole WAL is physically truncated. Writable open heals independently set bits 6, 10 and 14 only with no live overlay, draining obsolete skipped WAL first; read-only open never heals. Live insert groups retain all fences until checkpoint.

## 6. One memory budget

### 6.1 Charge table

HNSW uses the single `MemoryBudget` described in `docs/MVCC.md:599-646`. Persistent file bytes are not memory charges; every resident representation is.

| Index category | Charged when | Released / reclaimed when |
|----|----|----|
| HNSW root page frame | pager caches it | ordinary clean-frame eviction |
| Layer-directory payload frames | pager reads directory pages | ordinary clean-frame eviction |
| CSR directory/offset/neighbor frames | slot reader touches pages | ordinary clean-frame eviction |
| Vector main-run frames | navigation reads a vector slot | ordinary clean-frame eviction |
| b1 rescore-run frames | final candidates are rescored | ordinary clean-frame eviction |
| Decoded root configuration | snapshot index view opens root | view/snapshot drop |
| Decoded layer page-id matrix | snapshot index view loads it | view/snapshot drop |
| Committed `HnswDelta` lists/maps | commit publishes delta | checkpoint plus last old snapshot drop |
| Transaction-local insertion proposal | commit starts index insertion | transfer to committed delta or proposal drop |
| Search candidate min-heap | ANN arena reservation | query/fallback drop |
| Search result max-heap | ANN arena reservation | query/fallback drop |
| Search visited hash table | ANN arena reservation | query/fallback drop |
| Verified-CSR group set | first group access in query | query/fallback drop |
| Adjacency-list scratch | slot neighbor range is read | next hop/query drop |
| Rotated-query/vector scratch | scoring begins | query/fallback drop |
| b1 rescore candidate array | candidate selection completes | final top-k built/fallback |
| Exact-tail/brute top-k rows | candidate enters final heap | replacement or query drop |
| Checkpoint CSR replacement builder | one changed group is merged | group written or checkpoint abort |
| Initial-build mutable batch | build processes bounded row batch | batch flushed/aborted |

Page frames appear only in the existing page-cache row of the global charge table; HNSW MUST NOT double-charge them as search working set. Vec capacities, map buckets, Arc/list allocations, and allocator overhead are charged, not only logical element lengths.

### 6.2 Search working-set bound

Let:

```text
E = max(k, ef_search)
E = max(E, 3*k) for b1
D = M0
L = visible layer_count
V = min(covered_rows, E*(D+1) + L*(M+1))
```

All arithmetic is checked. Before the first ANN page read, the query reserves capacity for:

- two heaps of at most `E` `(distance, node_offset)` entries;
- one open-addressed visited table sized for at most `V` node ids at ≤ 0.5 load factor;
- at most `min(V, L * group_count)` verified CSR ids;
- one adjacency list of at most `D` u64 ids;
- the f32 query, one navigation-vector scratch slot, and one rescore slot;
- for b1, at most `E` rescore candidates;
- the final at-most-`k` row heap, charged by `Value::approx_bytes` policy.

Discovery stops at `V`; no visited bitmap proportional to all table rows is allowed. `ef_search` may be tuned upward by the caller, but an arena that does not fit does not silently lower `ef_search` because that would silently lower recall.

The page cache may hold more or fewer pages subject to its independent budget and clock eviction. No snapshot pins frames; a search pins only the `PageRef` currently being decoded, following `docs/MVCC.md:70-76`.

### 6.3 Exhaustion policy: exact fallback, no spill

If ANN arena reservation or a later ANN-only charge fails, the query:

1.  drops all ANN state and releases its charges;
2.  asks the normal reclaim ladder to evict clean frames;
3.  restarts from row zero through the existing brute-force `KnnScan` on the same pinned snapshot;
4.  returns exact results if that bounded top-k path fits;
5.  returns `BudgetExceeded` only if the brute path also cannot fit.

HNSW search state MUST NOT spill. Candidate/visited access is random and latency-sensitive; spilling it would create write amplification and eMMC wear, and the exact streaming fallback already has a smaller predictable working set. Failing over is preferable to unbounded thrash and preserves the requested query semantics.

Write-side exhaustion follows §5.2: commit the base row, leave an exact tail, and let a later checkpoint retry. Index creation may return `BudgetExceeded` because no previously usable index is being made stale.

---

## 7. Quantization and vector payload ownership

### 7.1 No vector duplication in the index

The HNSW root records the navigation encoding for validation, but HNSW pages contain no vector bytes. A hop resolves its node offset into the indexed node-group column and reads that column's existing main payload run. This separation of vectors from adjacency lets 4 KiB pages hold many adjacency lists.

Consequently:

- an f32 `Vector(dim)` column navigates and final-scores from f32;
- an f16 `VectorEncoded` column navigates and final-scores from decoded f16;
- an i8 `VectorEncoded` column navigates and final-scores from its per-vector scale/offset and i8 codes;
- a b1 `VectorEncoded` column navigates from packed signs and final-scores from its configured rescore run.

There is no hidden b1 shadow copy for an f32 column. Users choose quantized navigation by choosing the column's physical encoding. This avoids a second vector corpus, a second rotation lifecycle, and extra writes.

### 7.2 b1 candidate selection and rescore

For b1 cosine indexes, the f32 query is rotated once using the column's fixed seed. Every HNSW comparison uses FORMAT.md's asymmetric cosine estimate over the candidate sign bits (`docs/FORMAT.md:388-432`). The query itself is never quantized.

Layer-zero selection returns at least `max(ef_search, 3*k)` candidates. Only then does the operator read auxiliary rescore slots. Rescore payloads are standalone runs specifically so non-candidates cost no I/O or memory (`docs/FORMAT.md:434-449`). Final order and the output distance use the configured f16, i8, or f32 rescore representation.

A b1 column with `rescore:"none"` is not eligible for HNSW; it uses brute force. b1 HNSW with L2 is also unsupported because FORMAT.md defines an asymmetric cosine estimator, not an L2 estimator. Neither case may silently reinterpret the metric.

### 7.3 Schema and rewrite rules

The root's encoding must match the catalog column at open and publication. Changing `VectorEncoded` kind, dimension, b1 seed, or b1 rescore kind requires rewriting column payloads (`docs/FORMAT.md:321-337`) and rebuilding the index. An old root against a rewritten column is corruption, not a usable stale index.

Runtime SIMD dispatch remains allowed for SEARCH scoring, but CONSTRUCTION (initial build, insert proposals, checkpoint catch-up) MUST use the canonical scalar kernels (§10 decision 7). If two computed distances compare equal, node offset is the binding second key. A build/rebuild golden test pins identical topology bytes; dispatched construction kernels may be adopted later only with a cross-ISA golden proof of identical decisions on the mandated corpus.

---

## 8. Format compatibility and the v1 freeze

### 8.1 Read-safe feature flag

Feature bit 0 is `HNSW_INDEX` and is classified read-safe. A reader that does
not support the index may ignore `indexes` and use base scans, but MUST open
the database read-only so that a write or checkpoint cannot drop unknown
catalog metadata. HNSW roots and layer-directory payloads do not change the
interpretation of node groups, vector payloads, or `RCSR` pages.

### 8.2 Read-safe argument

Ignoring HNSW is read-safe because:

- base node-group vector bytes are unchanged and authoritative;
- HNSW roots are referenced only by the new top-level catalog field;
- layer-directory and HNSW CSR pages are unreachable from base table storage;
- `RCSR` bytes keep their existing interpretation;
- no HNSW-only WAL payload is required to recover table rows;
- an old reader can execute brute-force KNN and ordinary graph queries.

It is not write-safe for an unaware reader because reserializing the catalog could omit `indexes`. Therefore unknown-but-read-safe means read-only, not "safe to ignore and then overwrite."

The bit is set iff at least one HNSW catalog entry is published. Index creation sets it in the same alternate-superblock publish as the catalog entry. Clearing it requires a checkpointed catalog with no HNSW entries and no reachable HNSW overlay state.

### 8.3 Validation and golden corpus

The HNSW format implementation adds corruption tests for every fixed field, reserved byte, exact payload length, CRC, page bound, layer matrix dimension, neighbor bound, degree cap, duplicate/self neighbor, and root/schema mismatch.

Golden corpus obligations are:

1.  All pre-HNSW files remain byte-readable and behavior-identical.
2.  One smallest empty-index file pins the root/catalog/flag spelling.
3.  One multi-layer file pins deterministic CSR and directory bytes.
4.  One b1+f32-rescore file proves main/rescore runs remain separate.
5.  A v1 read-safe fixture opens base tables read-only while ignoring the index.

This is additive corpus growth under `docs/FORMAT.md:476-482`.

---

## 9. Recall, correctness, persistence, and memory gates

### 9.1 Binding recall corpus

The acceptance corpus is exactly the existing public-facade KNN corpus:

- LCG multiplier `6364136223846793005`;
- increment `1442695040888963407`;
- corpus seed `0xd3_70_db_20_00`;
- L2 query seed `0x12_34_56_78_9a_bc_de_f0`;
- cosine query seed `0x0c_05_1e_20_26`;
- `N = 2_000`, `dim = 64`, metrics `{l2, cosine}`, `k = {1, 10, 100}`.

These values are pinned in `crates/devondb/tests/knn.rs:1-8,25-33` and MUST NOT be changed to improve ANN results. Exact ids come from the scalar ground truth sorted by distance and row id (`crates/devondb/tests/knn.rs:360-420`).

For result id sets `A_k` and exact id sets `G_k`:

```text
recall@k = |A_k intersection G_k| / k
```

Every table cell is an independent gate; metrics or k values are not averaged.

### 9.2 Proposed thresholds

With `M=16`, `M0=32`, `ef_construction=200`, `ef_search=128`:

| Physical column / metric  | recall@1 | recall@10 | recall@100 |
|---------------------------|---------:|----------:|-----------:|
| f32 / L2                  |     1.00 |      0.90 |       0.95 |
| f32 / cosine              |     1.00 |      0.90 |       0.95 |
| b1 + f32 rescore / cosine |     1.00 |      0.80 |       0.90 |

The b1 fixture uses rotation seed `0x48_4e_53_57_20_26_08_03` and the binding 3x oversample. At one query per metric, recall@10 moves in 0.10 increments; the thresholds intentionally use representable values.

All returned distances must equal the chosen final representation's scalar distance within the existing tolerance, be ascending, and use node offset for ties. Recall does not excuse wrong distance values, invisible rows, duplicates, or nondeterministic ordering.

### 9.3 MVCC and durability gates

| Gate | Binding assertion |
|----|----|
| Snapshot isolation | Pin S; commit indexed rows; S returns identical ids/distances before and after; fresh S2 may use the delta and sees new rows. |
| Tail visibility | Force index-proposal budget failure; commit succeeds; fresh query includes the new exact-tail nearest neighbor. |
| Checkpoint COW | Pin S; commit + checkpoint; S reads its old root/pages; S2 reads the new root. |
| Reopen | ANN ids/distances are identical immediately before checkpoint and after clean reopen with the same query settings. |
| Kill-9 before checkpoint | Every acknowledged base row is present after recovery; rows beyond persistent coverage participate through exact tail. |
| Kill-9 after root publish | Recovery uses the new root and skips already-checkpointed WAL groups; no row appears twice. |
| Index build race | CREATE builds current materialized rows while holding the commit/publication lock; earlier staged DML committing afterward sets bit 14 from the current catalog. |
| Conflict severance | Concurrent same-PK inserts retain existing one-success/one-conflict behavior; adjacency work never creates a second user conflict. |

### 9.4 Memory and fallback gates

One integration test runs the corpus with a memory limit too small for the ANN arena but large enough for brute force. It asserts:

1.  the HNSW path attempts and releases its charge;
2.  the brute path runs on the same pinned snapshot;
3.  output is 100% identical to scalar ground truth for both metrics and all k;
4.  peak `MemoryBudget::charged()` never exceeds the configured limit;
5.  no spill file is created.

A second test makes checkpoint catch-up stop under budget pressure, reopens the database, and proves uncovered rows remain searchable. Raising the budget and checkpointing again must advance coverage and remove the tail without changing row visibility.

The would-fail-if-severed condition is explicit: disabling exact-tail union must fail the nearest-new-row and crash-recovery tests; disabling snapshot root pinning must fail the old-snapshot checkpoint test; bypassing charges must fail the hard-limit test.

---

## 10. Design decisions

1.  **Logical exactness surface — explicit policy.** `KnnScan` gains an
    explicit `exact | approximate` policy in DevonPlan. `exact` is the
    default AND the absent-field spelling, so every existing and pinned plan
    keeps today's exact semantics byte-for-byte. An installed index serves
    only `approximate` scans. Rationale: PLAN_IR's "k nearest" must never
    silently change meaning under a pinned plan; determinism of the IR is a
    fixed requirement.
2.  **Index creation syntax — one pin-able DDL surface.** Index create/drop
    is an explicit DevonPlan statement that reaches the text surface like
    other DDL. There is no automatic index policy or Rust-only shadow API.

    **Level-seed derivation.** For a new HNSW index, fold the
    128-bit database id into `db_fold = le_u64(db_id[0..8]) XOR
    le_u64(db_id[8..16])`. Compute the 64-bit FNV-1a hash of the index name's
    UTF-8 bytes (offset basis `0xcbf29ce484222325`, prime
    `0x100000001b3`, wrapping multiplication), then persist
    `level_seed = db_fold XOR name_hash`. This derivation is deterministic
    and platform-independent.
3.  **Non-WAL index deltas.** Checkpointed topology is
    persistent; crash recovery rolls ANN state back to the root and
    exact-scans the WAL tail. Chosen for the eMMC write-amplification budget
    (`docs/ARCHITECTURE.md` § Edge budget) and because base rows remain the
    source of truth from which coverage rebuilds. The design does not use
    WAL replacement records for topology.
4.  **Commit-lock latency.** Incremental proposals build
    inside the serialized commit section. §5.2 rule 2 (an existing tail
    stops further extension) plus §5.2 rule 5 (proposal failure never fails
    the base commit) bound the worst case. If measured latency on reference
    edge hardware demands it, a reservation/rebase protocol is a later,
    separately specified change.
5.  **Read-safe compatibility.** Bit 0 `HNSW_INDEX` and the generic read-only
    handling described in §8.1 protect unknown index metadata from writes.
6.  **b1 scope.** b1 HNSW is cosine-only and requires a rescore
    run. `rescore:"none"` and b1+L2 use brute force;
    neither may silently reinterpret the metric. A b1 L2 estimator would be
    a separate FORMAT-specified design with its own recall gates.
7.  **Construction reproducibility — scalar construction kernels.**
    Topology construction (initial build, insert proposals, checkpoint
    catch-up) MUST use the canonical scalar distance kernels on every ISA.
    Search may use runtime-dispatched kernels. Dispatched construction is
    permitted later only with a cross-ISA golden test proving identical
    topology bytes on the mandated corpus. Determinism of persisted bytes
    outranks build speed on the write path.
8.  **Recall target.** The per-cell thresholds in §9.2 apply independently
    to the unchanged mandated corpus. Additional query seeds may add gates;
    they never replace or average with the mandated corpus.
9.  **Deletes and vector updates — supported through exact fallback and rebuild.**
    Raw visible mutation history disables topology until checkpoint atomically
    publishes a fresh root over materialized offsets (§5.7; FORMAT bit 14).
10. **Filtered HNSW — follow-up.** Unfiltered MVCC, recovery, memory, and
    recall gates come first. NaviX-style adaptive filtering is future work.
11. **Page reclamation — atomic subtree retirement.** Catalog save compares
    changed old/new root, layer-directory and complete CSR page inventories,
    preserving shared pages and ordinary generation/pin laws. Equal roots prune
    traversal. New cells and directory/root allocations enter the prospective
    queue immediately; successful publication filters all final reachable pages.
    An I/O failure inside an incomplete CSR flush may leave pages for offline
    sweep; adaptive budget retries do not strand completed cells.
