# devondb MVCC + memory budget

Companion specifications are `docs/ARCHITECTURE.md` §§ Concurrency and Edge
budget and the on-disk contract in `docs/FORMAT.md`. MVCC versioning and the
budgeted buffer pool form one design because eviction under pins and version
storage under a budget are the same problem. The page-pinning rule in §1.9 is
the load-bearing joint.

---

## 1. The model on one page

Ten decisions, each blunt, each justified below:

1. **Commit timestamps are WAL LSNs.** The WAL already assigns strictly
   increasing LSNs (`crates/devondb-storage/src/wal.rs:50–76`) and commit is
   physically serialized (§5), so the commit record's LSN is the
   transaction's version number. No second counter domain.
2. **A snapshot is an immutable published-state pointer, not a filter.**
   Readers pin an `Arc<PublishedState>` (§3) — a catalog version plus a
   chain of committed deltas. Visibility is structural: a snapshot's chain
   never grows, so nothing committed later can appear in it. No per-row
   visibility predicate executes on the read path.
3. **Checkpointed node groups and CSR groups carry ZERO per-row version
   bytes in v1.** Row visibility for checkpointed data is at **group
   granularity via catalog copy-on-write**: a group is wholly visible in
   every catalog version whose storage map lists it, and a group only enters
   a storage map at a checkpoint, which materializes exclusively committed
   data (§6). This is sound because checkpoint's tail rewrites and CSR
   merges always write **fresh pages** and leak the superseded ones
   (`node_table.rs:178–201`, `rel_table.rs:491–546`, FORMAT.md:214–217). A
   pinned old snapshot keeps reading the old pages. The group byte layouts in
   FORMAT.md §§ Node group pages / Rel table adjacency are unchanged.
4. **Row versions live in the committed in-memory overlay**, one immutable
   delta per commit, each entry stamped with its commit LSN; durable via WAL
   transactional framing (§2.1). This extends the existing delta-overlay
   architecture (`node_table.rs:26`, `rel_table.rs:43`, FORMAT.md:210–218)
   rather than replacing it.
5. **Commit is physically serialized** under one commit lock, DuckDB-style
   (ARCHITECTURE.md:131–132): write transactions build txn-local write sets
   concurrently, then commit one at a time — conflict check, WAL append,
   one fsync, publish (§5).
6. **The v1 write-write conflict** is (a) a node insert whose (table,
   primary key) was also inserted by a transaction that committed after this
   transaction's snapshot, or (b) a DDL name collision in the same window.
   Surfaced as `DevonError::TransactionConflict` (§5.4). v1 also introduces
   primary-key uniqueness enforcement (§5.3) — without it, insert-only
   commits would commute and "conflict detection" would be vacuous.
7. **One memory budget.** Page cache, MVCC overlay, write sets, scan
   working sets, and Sort/Aggregate buffers all charge the same
   `MemoryBudget` (ARCHITECTURE.md:76–83). `memory_limit` enters through
   `Database::open_with` (§7.1).
8. **No mmap. User-space, write-through page cache.** A cached frame's pin
   is its `Arc` strong count; eviction drops clean frames and re-reads the
   main file on demand and never rewrites them to temporary storage. v1 has
   no dirty frames at all: `write_page` is write-through (§7.3).
9. **Snapshots pin page IDs, never frames.** A pinned snapshot holds
   catalog page-id sets and overlay Arcs — it holds no cache frame. Because
   superseded pages are leaked, never reused (FORMAT.md:216–217), any page a
   snapshot references is re-readable from the file forever. Therefore MVCC
   never blocks eviction; only an in-flight decode (a held `PageRef`) pins a
   frame, and only for the duration of one group read. This is the
   designed-together property ARCHITECTURE.md:85–88 demands.
10. **Durability moves from statement to commit.** v0 fsyncs the WAL once
    per inserted row (`node_table.rs:52–59`, `rel_table.rs:101–119`); v1
    fsyncs once per commit (§5.5). The autocommit facade (§5.7) preserves
    v0's user-visible durability (statement returns ⇒ durable) while
    reducing fsyncs per multi-row statement and therefore eMMC write
    amplification.

---

## 2. On-disk format delta

The main-file page formats — superblock, catalog page, node group pages,
CSR group pages — are **byte-for-byte unchanged**. The delta is confined to
the WAL sidecar's payload registry and to documented semantics. The WAL is
transient and may be absent after clean close. Commit framing predates the v1
freeze; introducing it afterward would require a feature-compatibility gate.

### 2.1 WAL transactional framing

The record envelope (`len`, `crc32c`, `lsn`, payload — FORMAT.md:221–228,
wal.rs:196–207) is unchanged. The payload registry becomes:

| Record | Canonical payload (compact `serde_json`) | New in v1? |
|---|---|---|
| Node insert | `{"table":"<name>","row":[<tagged values>]}` | no (`node_table.rs:29–33`, `115–121`) |
| Rel insert | `{"rel":"<name>","from":<u64>,"to":<u64>,"values":[<tagged values>]}` | no (`rel_table.rs:59–65`, FORMAT.md:212–216); offsets now resolved at commit, §5.5 |
| Create node table | `{"ddl":{"create_node_table":<NodeTableSchema JSON>}}` | yes |
| Create rel table | `{"ddl":{"create_rel_table":<RelTableSchema JSON>}}` | yes |
| Commit | `{"commit":{"records":<u64>}}` | yes |

Schema JSON is the exact catalog serialization (e.g.
`{"name":"Person","columns":[{"name":"id","ty":"Int64","primary_key":true}]}`,
catalog.rs test line 429). Payload discrimination is by top-level key —
`table`, `rel`, `ddl`, `commit` are mutually exclusive; recovery tries them
in the order commit, ddl, node, rel. A payload matching none is corruption and fails
recovery cleanly.

**Framing rules (binding):**

1. All records of one transaction are contiguous in the WAL, appended under
   the commit lock, with the commit record last. Nothing is written to the
   WAL before commit begins — uncommitted work never touches disk.
2. `records` = the number of payload records in this transaction, excluding
   the commit record itself; always ≥ 1 (empty transactions never write to
   the WAL, §5.5 step 1).
3. Within one transaction the record order is canonical: all `ddl` records
   in statement order, then all node inserts (tables in lexicographic
   order, rows in statement order within a table), then all rel inserts
   (same ordering). Recovery replaying in file order therefore never sees
   an insert before its table's DDL.
4. Recovery groups records: accumulate payload records until a commit
   record; the group's **commit LSN is the commit record's LSN**, and every
   entry in the group gets it as its begin-LSN. A trailing group with no
   commit record is a transaction in flight at crash time: silently
   discarded, exactly like today's torn tail (wal.rs:159–165,
   FORMAT.md:230–232).
5. A commit record whose `records` does not equal the size of the
   accumulated group is corruption (contiguity is an invariant; a mismatch
   means interleaved or lost records) and fails recovery cleanly with the
   offending LSN in the error.
6. A group whose commit LSN is ≤ the superblock's `checkpoint_lsn` is
   skipped entirely (already materialized). This covers the crash window
   between superblock commit and WAL truncation in §6 step 5, and replaces
   v0's per-record LSN skip at transaction granularity.

### 2.2 `checkpoint_lsn` semantics

`checkpoint_lsn` in the superblock (FORMAT.md:38) is **the commit LSN of the
newest transaction whose effects are materialized in the main file.**
`Catalog::save(&self, pager, checkpoint_lsn)` receives that LSN from the
checkpoint caller (§6). The pager's strictly advancing rule still holds
because checkpoint only runs when at least one new commit exists. WAL
reopen continues to seed `next_lsn = checkpoint_lsn + 1`.

### 2.3 Node groups and CSR groups: unchanged, with a reserved path

- The node-group directory's reserved u32 at offset 12 (FORMAT.md:102)
  stays zero and readers keep rejecting nonzero (node_group.rs:355–357).
  It is reserved for future per-group version metadata
  (a delete-vector array entry) when DELETE arrives post-v1 — that change
  will be feature-flagged per FORMAT.md:277–280.
- FORMAT.md § "Durability and the delta overlay" defines edges and node rows
  as durable **at commit**,
  not at insert; rel-record offsets are resolved from primary keys **at
  commit time under the commit lock** (§5.5 step 5), not at statement time.
  The stored byte shapes do not change.
- FORMAT.md § "MVCC and page immutability" defines the invariant §1.9 rests
  on: *data pages listed in any catalog version are immutable;
  superseded pages are leaked, never rewritten and never reused, until
  free-page management arrives — and free-page management, when it comes,
  must not reclaim a page while any in-process snapshot's catalog version
  references it.* Catalog pages are copy-on-write and join the same
  retirement family. The only in-place page rewrites are the two superblock
  slots and free-page ledger pages.
- FORMAT.md § "Not yet specified" (lines 298–307): the MVCC line is
  resolved by this design; the spill-file line stays (spill files remain
  explicitly outside the compatibility promise, §8.2).

### 2.4 Compatibility note (dev-format honesty)

A v0-written WAL containing pending records has no commit framing; v1
recovery would discard those records as an unterminated group. This is a
dev-format break, permitted pre-freeze (FORMAT.md:3–5). Mitigation: none
needed — both golden anchors (`tests/golden/m1-person.devondb`,
`m2-social.devondb`) are checkpointed files without WAL sidecars, so the
golden corpus is unaffected.

---

## 3. In-memory structures

The shared overlay vocabulary is defined in
`crates/devondb-storage/src/overlay.rs`:

```rust
/// The immutable committed state of the database at one instant.
pub struct PublishedState {
    /// Schemas (incl. committed-but-uncheckpointed DDL) + storage maps
    /// as of the last checkpoint.
    pub catalog: Arc<Catalog>,
    /// Newest committed delta; None immediately after a checkpoint.
    pub chain: Option<Arc<CommitLink>>,
    /// Commit LSN of the newest committed transaction (== chain head's
    /// LSN, or the superblock checkpoint_lsn when chain is None).
    pub last_commit_lsn: u64,
    /// Conflict summaries of recent commits, newest last. Survives
    /// checkpoint until no registered write transaction's snapshot
    /// predates them (§5.4, §6 step 6).
    pub recent_summaries: Vec<(u64 /* commit_lsn */, Arc<CommitSummary>)>,
}

pub struct CommitLink {
    pub prev: Option<Arc<CommitLink>>, // next-older commit
    pub commit_lsn: u64,
    pub delta: CommitDelta,
    pub charged_bytes: usize,          // released on drop (§7.2)
}

pub struct CommitDelta {
    /// Per node table, rows in statement order.
    pub nodes: BTreeMap<String, Vec<Vec<Value>>>,
    /// Per rel table, edges in statement order, offsets already resolved.
    pub edges: BTreeMap<String, Vec<OverlayEdge>>,
    /// Per rel table, endpoint-role tombstones for incident-edge removal.
    pub rel_tombstones: BTreeMap<String, RelEndpointTombstones>,
    /// DDL in statement order.
    pub ddl: Vec<DdlOp>,
}

pub struct OverlayEdge { pub from: u64, pub to: u64, pub values: Vec<Value> }

pub enum DdlOp { CreateNodeTable(NodeTableSchema), CreateRelTable(RelTableSchema) }

/// Everything conflict detection needs; deliberately tiny.
pub struct CommitSummary {
    pub inserted_pks: BTreeMap<String, Vec<Value>>, // node table -> PKs
    pub detach_pks: BTreeMap<String, Vec<Value>>,
    pub edge_endpoint_pks: BTreeMap<String, Vec<Value>>,
    pub created_tables: Vec<String>,                // node + rel names
}
```

A detach commit carries endpoint-role tombstones in its delta and its
asymmetric conflict claims in its summary; these fields are part of the
published immutable structures.

Rules:

1. Everything above is immutable once published — never mutated, only
   replaced (coding-style immutability rule, and the entire correctness
   argument of §4).
2. **Overlay read order** is chain links oldest-first (collect links, walk
   `prev`, reverse), concatenating each link's per-table vectors. This
   reproduces v0 scan order exactly — checkpointed groups first, then
   buffered rows in insertion order (`node_table.rs:92–105`) — because
   commits are serialized.
3. **Node offsets** remain "checkpointed position + overlay position in
   read order" (FORMAT.md:156–163). Offsets are
   assigned at commit and are stable until a checkpoint containing an
   effective detach tombstone for their node table; that checkpoint atomically
   applies the monotone epoch remap defined by DETACH_DELETE. Within one
   checkpoint epoch offsets are never reused or renumbered.
4. `CommitLink` must implement an **iterative Drop** (walk `prev` with
   `Arc::try_unwrap`, releasing `charged_bytes` per link) — a recursive
   drop of a long chain overflows the stack.
5. The facade's shared state combines the pager, budget, published state,
   commit pipe, and transaction registry:

```rust
pub struct Database { shared: Arc<Shared> }

struct Shared {
    pager: Pager,
    budget: Arc<MemoryBudget>,               // storage::budget (§7.2)
    published: Mutex<Arc<PublishedState>>,   // held only to clone/replace the Arc
    commit: Mutex<CommitPipe>,               // THE commit lock (§5.5, §6)
    write_txns: Mutex<BTreeMap<u64, u64>>,   // txn id -> snapshot last_commit_lsn
    next_txn_id: AtomicU64,
}

struct CommitPipe { wal: WalWriter, wal_path: PathBuf }
```

`Database` is `Send + Sync`. The `published` mutex is held only for an
`Arc` clone or swap — O(1), never across I/O.

---

## 4. Snapshots and the read path

### 4.1 API and lifecycle

```rust
impl Database {
    /// Pins the current committed state. Cannot fail, never blocks
    /// beyond an O(1) Arc clone.
    pub fn snapshot(&self) -> Snapshot;
}

pub struct Snapshot { shared: Arc<Shared>, state: Arc<PublishedState> }

impl Snapshot {
    pub fn run(&self, plan: &Plan) -> DevonResult<QueryResult>;
}
```

Pinning = cloning the published `Arc` under the `published` mutex. Dropping
a `Snapshot` drops its Arcs; when the last snapshot referencing a
`CommitLink` goes away (and a checkpoint has cleared it from the published
chain), the link's memory is released (§3 rule 4). Read snapshots register
nowhere — only **write** transactions enter `write_txns` (their snapshots
gate summary pruning, §5.4/§6).

### 4.2 The read path

`Snapshot::run` executes the pipeline against the pinned state:

1. Validation binds against `state.catalog` (schemas incl. WAL-only DDL).
2. **Node scan is streaming.** A per-group source replaces whole-table
   materialization. `NODE_GROUP_CAPACITY` (2048) equals `CHUNK_CAPACITY`
   (2048), so each `next_chunk` decodes exactly one node group from the snapshot's storage
   map (`catalog.table_storage`, catalog.rs:105–110) via the pager, then
   emits the overlay rows (§3 rule 2) as the final chunks. Peak scan memory
   drops from O(table) to O(group) + O(overlay) — required for the rlimit
   gate (§9.3).
3. **Expand** materializes adjacency as today (`graph.rs:27–54`), but from
   the snapshot: base CSR groups via the snapshot's rel storage map plus
   overlay edges from the chain, reusing the buffered-overlay merge shape
   of `rel_table.rs:273–305`. The implementation may build
   transient per-snapshot `NodeTable`/`RelTable` views populated through
   the existing `recover_row`/`recover_edge` paths (node_table.rs:62–66,
   rel_table.rs:121–135) so the existing scan/neighbor logic is reused
   untouched. The materialized `GraphSnapshot` charges the budget as scan
   working set (§7.2); streaming expand is deliberate future work.

### 4.3 The never-block guarantee, stated precisely

"Readers take a snapshot and never block, ever" (ARCHITECTURE.md:130)
means:

1. No reader operation ever waits on the **commit lock**, a WAL append or
   fsync, conflict validation, or checkpoint I/O.
2. The only shared critical sections a reader enters are (a) the
   `published` mutex for one Arc clone, and (b) the pager's internal frame
   lock for one page lookup/insert — both bounded by O(1) map work plus at
   most one page memcpy. The pager lock is **never held across an fsync or
   more than one page I/O**.
3. Reader progress is independent of writer transaction lifetimes: a
   writer may hold an open transaction forever; snapshots neither observe
   nor wait on it.

### 4.4 Snapshots across checkpoint (the leaked-page argument)

A checkpoint rewrites tail groups and merged CSR groups onto fresh pages
and publishes a new catalog (§6). A snapshot pinned before the checkpoint
holds the OLD catalog Arc whose storage maps name the OLD directory pages.
Those pages still hold valid bytes — checkpoint never overwrites data
pages, it leaks them (node_table.rs:198–199 pops and re-pushes a freshly
written group; rel_table.rs:539–543 writes replacement CSR groups to new
pages; FORMAT.md:216–217). So the old snapshot keeps reading its exact
byte-stable groups through the (possibly evicted-and-re-read) cache. Test
T6 (§9.1) pins this behavior.

---

## 5. Write transactions and the commit pipeline

### 5.1 API

```rust
impl Database {
    pub fn begin(&self) -> DevonResult<Transaction>;
    // v0-compatible autocommit wrappers (§5.7):
    pub fn execute(&mut self, statement: &Statement) -> DevonResult<()>;
    pub fn run(&mut self, plan: &Plan) -> DevonResult<QueryResult>;
    pub fn checkpoint(&mut self) -> DevonResult<()>;
}

pub struct Transaction { /* shared, txn_id, state: Arc<PublishedState>,
                            writes: WriteSet, poisoned: bool */ }

impl Transaction {
    pub fn execute(&mut self, statement: &Statement) -> DevonResult<()>;
    pub fn run(&mut self, plan: &Plan) -> DevonResult<QueryResult>; // sees own writes
    pub fn commit(self) -> DevonResult<()>;
    pub fn abort(self);
}
```

`begin` clones the published Arc and registers `(txn_id, snapshot
last_commit_lsn)` in `write_txns`. `Drop` for an uncommitted `Transaction`
= `abort` (deregister, release write-set budget charges). A statement error
that leaves the write set ambiguous (only budget failures qualify, §7.4)
poisons the transaction: subsequent `execute`/`commit` return the poison
error; `abort` always works.

### 5.2 The write set

```rust
struct WriteSet {
    nodes: BTreeMap<String, Vec<Vec<Value>>>,     // statement order per table
    edges: BTreeMap<String, Vec<PendingEdge>>,    // KEYS, not offsets
    ddl: Vec<DdlOp>,
    inserted_pks: BTreeMap<String, Vec<Value>>,
    charged: usize,                               // budget charge (§7.2)
}
struct PendingEdge { from_key: Value, to_key: Value, values: Vec<Value> }
```

Edges hold primary keys because a transaction cannot know its rows' global
offsets until commit assigns its overlay position (§3 rule 3) — another
concurrent commit may land first. v0 resolved offsets at statement time
(`graph.rs:97–132`); v1 defers resolution to commit
(§5.5 step 5). The WAL rel-record shape is unchanged because the record is
only written at commit, after resolution (§2.1, §2.3).

### 5.3 Execute-time validation (fail fast, before any conflict can exist)

Per statement, against the **transaction view** = snapshot + own write set:

1. `CreateNodeTable` / `CreateRelTable`: name unused across both table
   kinds in the view (catalog.rs:72–89 rules); rel endpoints exist in the
   view.
2. `InsertNode`: table exists; arity + per-column type checks exactly as
   v0 (`node_table.rs:123–143`); **new in v1**: the primary-key value must
   be non-null (`InvalidArgument` naming the column), and must not already
   exist in the view — base groups, overlay chain, or own writes — else
   `InvalidArgument` "duplicate primary key `<k>` in node table `<t>`".
   (Uniqueness against *concurrent* commits is the commit-time conflict
   check, §5.4 — same rule, different enforcement point, different error.)
3. `InsertRel`: table exists; property values validated
   (`rel_table.rs:325–345`); both endpoint keys must resolve in the view
   (`NotFound` as today, graph.rs:124–131). The resolved offsets are
   discarded — only the keys enter the write set.

Pre-existing v0 files may contain duplicate PKs (v0 never enforced
uniqueness); reads and key resolution keep v0's first-match semantics
(graph.rs:124–127) for such rows. New inserts are held to the v1 rule.

### 5.4 Conflict detection (optimistic, at commit, under the lock)

The **conflict window** of a committing transaction T is every commit with
`commit_lsn > T.snapshot.last_commit_lsn`. Detection walks
`published.recent_summaries` newest-to-oldest while `commit_lsn` is in the
window:

| # | T's write set contains | Window commit contains | Outcome |
|---|---|---|---|
| C1 | node insert (table, pk) | node insert (table, pk), `Value` equality (exact, `PartialEq`) | `TransactionConflict` |
| C2 | DDL creating name N | DDL creating name N | `TransactionConflict` |
| C3 | `detach_pks[table,key]` | `edge_endpoint_pks[table,key]` | `TransactionConflict` |
| C4 | `edge_endpoint_pks[table,key]` | `detach_pks[table,key]` | `TransactionConflict` |
| — | any other combination not named above | — | has no conflict |

`recent_summaries` — not the chain — is the conflict source of truth,
because checkpoint clears the chain but must not shrink any in-flight
transaction's window: summaries are retained until no registered write
transaction's snapshot predates them (§6 step 6). Test T4 (§9.1) pins this.

**The error surfaced to the user** is defined in
`devondb-types/src/error.rs`:

```rust
/// A concurrent transaction committed a conflicting write first.
#[error("transaction conflict: {context}")]
TransactionConflict { context: String },
```

Context shape (binding, tested in T3): `` transaction conflict: node table
`Person` primary key 7 was inserted by a concurrent transaction (committed
at LSN 214) `` — and for DDL: `` transaction conflict: table `T` was
created by a concurrent transaction (committed at LSN 214) ``. A conflicted
transaction is consumed; the caller retries by replaying its statements on
a fresh `begin()`.

### 5.5 Commit, step by step (binding)

1. Empty write set → deregister from `write_txns`, return `Ok` without
   touching the WAL or the published state.
2. Lock `shared.commit`. (Commits and checkpoints serialize here; readers
   are untouched.)
3. Clone the current published Arc.
4. Run §5.4 conflict detection. On conflict: deregister, unlock, return
   `TransactionConflict`.
5. **Assign offsets and resolve edges.** The transaction's node rows are
   appended (conceptually) after the published overlay in canonical order
   (§2.1 rule 3); each new row's offset = published total row count of its
   table + its position. Each `PendingEdge` resolves its keys against
   published state + this write set. A key that resolved at execute time
   always resolves here — rows are never deleted and concurrent duplicate
   PKs were excluded by §5.3/§5.4 — so a resolution failure at this step is
   `Corrupt` (invariant breach), never a user error.
6. **WAL append** in canonical order (§2.1 rule 3): ddl records, node
   records, rel records (with resolved offsets), then the commit record
   `{"commit":{"records":N}}`. One `wal.sync()` — the single fsync of the
   transaction (wal.rs:79–82). The commit record's LSN is the transaction's
   commit LSN.
7. **Publish**: build the new `PublishedState` — new `CommitLink` (prev =
   old chain head) with the delta stamped at the commit LSN; catalog Arc
   replaced by clone-plus-DDL if `ddl` is non-empty (catalog.rs:72–89);
   `recent_summaries` + this commit's summary; `last_commit_lsn` = commit
   LSN. Swap under the `published` mutex.
8. Deregister, unlock, optionally trigger checkpoint (§6 triggers), return
   `Ok`.

Crash between step 6 and 7: recovery replays the commit from the WAL —
durable. Crash mid-step-6: unterminated group, discarded (§2.1 rule 4) —
atomic. This is the whole atomicity argument; test T8 (§9.1) attacks it.

### 5.6 Recovery on `Database::open`

Recovery proceeds as follows:

1. Open pager, load catalog page (schemas + storage maps as of the last
   checkpoint), open `WalWriter` with `next_lsn = checkpoint_lsn + 1`
   as before.
2. Group WAL records into transactions per §2.1 rules 4–6; skip groups with
   commit LSN ≤ `checkpoint_lsn`; apply the rest oldest-first, rebuilding
   the chain and `recent_summaries` with begin-LSN = commit LSN. Canonical
   in-transaction order guarantees DDL lands before its table's rows.
3. Discard the trailing unterminated group silently; any framing violation
   (rule 5, unknown payload) is `Corrupt` naming the LSN and fails the
   open cleanly.
4. Publish the resulting `PublishedState`.

### 5.7 The autocommit facade and v0 surface compatibility

`Database::execute(&mut self, stmt)` = `begin` → `execute` → `commit`; one
transaction per statement. Multi-row `InsertNode`/`InsertRel` statements
(`statement.rs` shapes) therefore cost one fsync where
v0 cost one per row — strictly better on eMMC. `Database::run(&mut self,
plan)` = `snapshot()` + `Snapshot::run`. v0's immediate catalog save,
superblock commit, and WAL reopen per DDL are removed: DDL is WAL-logged and
reaches the catalog page only at checkpoint. The v0 durability test
`create_node_table_is_durable_without_checkpoint` must still pass, now via
WAL replay. Autocommit conflicts
(`TransactionConflict` bubbling from a single-statement commit) surface
as-is; the CLI prints them via `Display` like every other error.

---

## 6. Checkpoint under MVCC

Checkpoint drains the committed overlay into groups. Binding sequence:

1. Take the `commit` lock (commits stall for the duration; readers do
   not — §4.3). Read the published Arc; if `chain` is `None`, unlock and
   return `Ok`, preserving the existing no-op guarantees.
2. Concatenate the chain oldest-first (§3 rule 2). Materialize node tables
   first, then rel tables — the order that `validate_buffered_offsets`
   depends on — via tail rewrite + fresh groups
   (node_table.rs:160–201), CSR merge on fresh pages
   (rel_table.rs:491–546). Edge offsets were resolved at commit, so they
   are valid against the just-materialized node totals.
3. `catalog.save_at(&pager, last_commit_lsn)` with the **published**
   catalog (schemas incl. WAL-only DDL) and the new storage maps — writes
   the catalog page, then the alternate superblock slot (catalog.rs:
   138–156, pager.rs:143–155).
4. Truncate the WAL and reopen at `checkpoint_lsn + 1`.
5. (Crash window note: a crash after step 3 but before step 4 leaves
   already-materialized transactions in the WAL; §2.1 rule 6 makes replay
   skip them.)
6. Publish a new `PublishedState`: same schema catalog with new storage
   maps, `chain = None`, `last_commit_lsn` unchanged, `recent_summaries`
   pruned to entries with `commit_lsn >` the minimum snapshot LSN in
   `write_txns` (all pruned when no write transaction is registered).
7. Unlock. Old `CommitLink`s die when the last pinned snapshot drops,
   releasing their budget charge (§3 rule 4).

**Triggers:** explicit `Database::checkpoint()`; automatically at the
end of a commit (step 8 of §5.5, already holding the lock) when the
overlay's charged bytes exceed the high-water mark (writer policy:
`memory_limit / 4`); and — the backstop — `Database::execute` folds
BEFORE beginning its transaction when the overlay's charged bytes exceed
`memory_limit / 2`. Both automatic triggers key on the
SAME metric, `PublishedState::overlay_charged_bytes` (links plus retained
summaries), never on the budget total: the page cache legitimately holds
around half the budget, and a total-charge key serializes every warm-cache
autocommit behind a checkpoint. The backstop exists because an
end-of-commit drain failure is deliberately swallowed (a caller retrying a
"failed" commit would duplicate durable writes) and the emergency
checkpoint inside a write-set charge can never free the charging
transaction's own chain — its snapshot pins it. There is no checkpoint on
drop, preserving the recover-from-WAL behavior.

---

## 7. The budgeted buffer pool

### 7.1 Configuration surface

```rust
pub struct Options {
    pub page_size: u32,       // create only; default 4096 (FORMAT.md:22–24)
    pub memory_limit: usize,  // bytes; default 64 MiB; minimum 1 MiB
}

impl Database {
    pub fn create_with(path: impl AsRef<Path>, options: Options) -> DevonResult<Self>;
    pub fn open_with(path: impl AsRef<Path>, options: Options) -> DevonResult<Self>;
    // Existing create(path, page_size) / open(path) delegate with defaults.
}
```

`memory_limit < 1 MiB` is `InvalidArgument`. The default (64 MiB) reflects
the edge envelope — the database gets tens to a few hundred MB beside a
resident LLM (ARCHITECTURE.md:68–72) — and is writer policy, not format.
The CLI gains a `--memory-limit <bytes>` flag threading into `Options`.

### 7.2 The accountant (`crates/devondb-storage/src/budget.rs`)

```rust
pub struct MemoryBudget { limit: usize, charged: AtomicUsize }

impl MemoryBudget {
    pub fn new(limit: usize) -> Self;
    pub fn unlimited() -> Self;                     // limit = usize::MAX (tests)
    pub fn try_charge(&self, bytes: usize) -> bool; // CAS loop; false = would exceed
    pub fn release(&self, bytes: usize);
    pub fn charged(&self) -> usize;
    pub fn limit(&self) -> usize;
}
```

`devondb-exec` may depend on `devondb-storage` (ARCHITECTURE.md:59), so one
accountant serves pager, overlay, facade, and operators. **What charges the
one budget** (ARCHITECTURE.md:76–80):

| Category | Charged when | Released when |
|---|---|---|
| Page cache frames | frame inserted (§7.3) | frame evicted |
| Committed overlay (`CommitLink`s + summaries) | commit publishes (§5.5 step 7) | link dropped after checkpoint + last snapshot (§3 rule 4); summary pruned from `recent_summaries` |
| Write sets | statement buffers rows (§5.2) | commit (charge transfers to the link) or abort |
| Scan working sets (`GraphSnapshot` adjacency, §4.2) | pipeline build | pipeline drop |
| Sort/Aggregate buffers | operator accumulation (§8) | spill or operator drop |

Byte estimation for `Value` in `devondb-types`:
`Value::approx_bytes()` — Null/Bool/Int64/Float64 = 16; String = 32 + len;
Vector = 32 + 4·len. Estimation constants are writer policy.

**The reclaim ladder (binding).** When `try_charge` fails, the caller runs
its ladder, retries once, then errors:

1. Evict clean unpinned cache frames (always attempted first).
2. Write paths additionally trigger a checkpoint to drain the overlay.
3. Sort/Aggregate additionally spill (§8).
4. Still over → `DevonError::BudgetExceeded`:

```rust
/// The operation cannot proceed within the configured memory_limit.
#[error("memory budget exceeded: {context}")]
BudgetExceeded { context: String },
```

Context names the category, requested bytes, charged, and limit. Failing fast
avoids unbounded thrashing.

Ladder step 1 is centralized in `MemoryBudget::charge_or_reclaim`: the
facade installs `Pager::shed_cache` as the budget's reclaimer, and executor
Sort/Aggregate accumulation, post-spill row charges, spill-merge buffers, and
HashJoin build rows all use that entry point before their later ladder steps
or `BudgetExceeded` result.

### 7.3 The page cache inside `Pager`

`Pager` uses interior mutability: file
I/O moves from seek+read (pager.rs:96–98, 110–112) to positional
`FileExt::read_at`/`write_at`, all methods take `&self`, allocation state
sits behind an internal mutex, and every `&mut Pager` in storage/facade
becomes `&Pager`. The page cache is part of `Pager`:

```rust
struct PageCache {
    frames: Mutex<FrameTable>,      // page_id -> Frame; clock hand
    budget: Arc<MemoryBudget>,
}
struct Frame { data: PageRef, referenced: bool }

/// Holding a PageRef IS the pin.
pub struct PageRef(Arc<[u8]>);       // Deref<Target = [u8]>
```

Binding rules:

1. `read_page(page_id) -> DevonResult<PageRef>`: hit → set `referenced`,
   clone the Arc. Miss → `try_charge(page_size + FRAME_OVERHEAD)` (ladder
   §7.2; `FRAME_OVERHEAD` = 64), `read_at` into a fresh frame, insert,
   return. Existing bounds checks stay (pager.rs:83–99, 238–245).
2. **Pin = Arc strong count.** A frame is evictable iff its
   `Arc::strong_count == 1` (only the cache holds it) and its `referenced`
   bit was already cleared once (clock second-chance). A held `PageRef`
   can never dangle — eviction of a pinned frame is unrepresentable, which
   is the point of designing pins with MVCC instead of retrofitting
   (ARCHITECTURE.md:85–88).
3. **Write-through, no dirty frames.** `write_page` validates as today
   (pager.rs:102–114), `write_at`s the file, and **removes** any cached
   frame for that page. Eviction therefore never writes — clean pages are
   re-read from the main file on the next miss, never re-written to temp
   (ARCHITECTURE.md:78–80). A reader still
   holding the old `PageRef` keeps a stale copy — harmless by the
   immutability invariant of §2.3: only the catalog page, superblocks,
   and free-page ledger pages, which are reachable solely from the
   superblock extension, read only by FREE_PAGES writers, and coherent
   across in-place rewrites precisely because of this rule's
   write-through) are rewritten in place, and no snapshot re-reads any
   of them.
4. Eviction: clock sweep over evictable frames until the charge fits.
   Nothing evictable → the charge fails up the ladder. The cache keeps a
   floor of 8 frames (writer policy) so tiny budgets still make progress.
5. The frames mutex is never held across an fsync or more than one page
   I/O (§4.3 rule 2). `sync` (pager.rs:117–120) and `allocate_page`
   (pager.rs:123–140, append-at-EOF unchanged) do not touch frames beyond
   invalidation.
6. `wal.rs` is untouched — WAL I/O is append/replay, not paged.

### 7.4 Budget failures at the statement level

A `BudgetExceeded` bubbling out of a write statement poisons the
transaction (§5.1) — the write set may be partially charged and its
contents are no longer trustworthy to commit. Read pipelines surface it as
a plain query error; the snapshot stays valid.

---

## 8. Sort/Aggregate spill

### 8.1 Operator contract

`Sort` and `Aggregate` (operators.rs:161–264) stay blocking pull operators
with their existing comparator and semantics. `drain_upstream`
(operators.rs:267) — buffer-everything — is
replaced by budget-aware accumulation. Constructors gain a config
parameter; `SpillConfig::unbounded()` preserves the current behavior for
pure-exec tests:

```rust
pub struct SpillConfig { pub budget: Arc<MemoryBudget>, pub tmp_dir: PathBuf }
impl SpillConfig { pub fn unbounded() -> Self; } // uses MemoryBudget::unlimited + std temp dir
```

The facade passes the real config at pipeline build.

**Sort:** accumulate rows, charging `approx_bytes` per row; when a charge
fails (after ladder step 1), sort the accumulated run in memory (stable,
binding comparator), stream it to a run file, release the charge, continue
draining. Zero runs spilled → the in-memory path, byte-identical behavior
to today. Otherwise, flush the final run and k-way merge with one charged
front row per run over a 64 KiB `BufReader` per run. Chunk-deep row buffers
are slower and create unreclaimable charges that can starve another run's
mandatory front row. On equal keys the merge takes the earlier run
first, and runs are written in drain order, so **global stability is
preserved** across spill.

**Aggregate:** accumulate the group map as today; when a charge fails,
switch to external mode: spill all buffered input rows — and keep draining
straight to runs — as sort runs ordered by the group-key tuple (binding
comparator), then merge the runs and aggregate each equal-key span
streamingly. Output remains ascending key-tuple order under the existing
deterministic emission rule. All aggregate rules (null handling, `checked_add`
overflow, avg promotion, empty-group_by row) apply unchanged in both modes.

### 8.2 Spill files (outside the format promise)

- Location: `<db path>.tmp/<handle dir>/` beside the database, created
  lazily. Each
  database handle owns one subdirectory `h-<pid>-<process token>-<attempt>`
  holding an exclusively locked `.lock` file for the handle's lifetime;
  runs inside it are `sort-<pid>-<process token>-<operator id>-<run
  index>[-r<attempt>].run`; the token plus a bounded `AlreadyExists` retry
  prevent pid recycling after a crash from colliding with leftovers.
- Encoding: repeated `[u32 LE row_len][canonical serde_json row
  (Vec<Value>)]`. Slow-but-simple is deliberate; `Value` already
  round-trips serde_json in the WAL (node_table.rs:115–121). A short read,
  bad length, or JSON error is `Corrupt` and **fails the query cleanly** —
  never a partial result.
- Lifecycle: files deleted on operator drop; a handle's subdirectory is
  removed when the handle drops. Writable opens sweep `<db path>.tmp/` of
  provably stale state only — subdirectories whose `.lock` is NOT held
  (the owning process is gone) and flat files; a directory whose lock is
  held belongs to a live handle (a read-only follower mid-merge) and
  survives. FORMAT.md keeps spill explicitly outside the compatibility
  promise.

---

## 9. Test strategy

### 9.1 Targeted interleavings (`crates/devondb/tests/concurrency.rs`)

Std threads + `std::sync::Barrier` to force orderings; `Database` shared by
reference (`Send + Sync`, §3). Iteration counts read
`DEVONDB_STRESS_ITERS` (default 25; nightly/local runs raise it).

| # | Interleaving | Binding assertion |
|---|---|---|
| T1 | Reader pins snapshot S; barrier; writer commits K rows; barrier | S's scan is row-identical before and after the commit; a fresh snapshot sees exactly K more rows. Loop ×iters. |
| T2 | 4 writers × M txns each, disjoint PK ranges, jittered | Every commit `Ok`; final count = 4·M·rows; each PK present exactly once. |
| T3 | Two txns insert the same PK; barrier-aligned commits, both orders | Exactly one `Ok`, one `TransactionConflict` with the §5.4 message shape; final state holds exactly one row for the PK. Loop ×iters. |
| T4 | A begins (snapshot S0) → B inserts PK X, commits → **checkpoint** → A inserts PK X, commits | A gets `TransactionConflict` — pins summary retention across checkpoint (§5.4, §6 step 6). |
| T5 | Two txns `CreateNodeTable "T"` concurrently | One `Ok`, one conflict; catalog holds exactly one `T`. |
| T6 | Pin S → commit rows → checkpoint → scan S | S returns exactly its original rows (leaked-page reads, §4.4); a fresh snapshot sees everything. |
| T7 | 1 writer committing batches + 1 checkpoint loop + 4 readers scanning for ~2 s | Every scan equals exactly the first k committed batches for some k (prefix consistency); zero errors. |
| T8 | Child process commits sequentially, writing each txn id to stdout AFTER `commit` returns; parent SIGKILLs at a random moment; reopen | Every acknowledged txn fully present; the unacknowledged tail txn fully present or fully absent — never partial. ×10. |

T3/T4/T5 must **fail against a build with conflict detection severed**:
deleting the §5.4 walk must make the suite fail.

### 9.2 What the stress tests do not cover

Model-checked interleavings (loom) require instrumented sync primitives and
are out of scope. Budget-under-concurrency is asserted by §9.3's rlimit, not
by sampling `charged()` racily.

### 9.3 The rlimit-enforced golden-workload memory test

- **`crates/devondb/tests/memory_budget.rs`:** drives the real
  `devondb-cli` binary through a generated text-form script: create the
  social schema, insert N nodes
  and edges in batches (periodic checkpoints via the §6 auto-trigger),
  reopen, run scan + filter + expand queries, assert exact known-answer
  output. The child process runs under `RLIMIT_AS` set in a
  `pre_exec` hook (`std::os::unix::process::CommandExt` + `libc::setrlimit`);
  the test is `#[cfg(target_os = "linux")]` (macOS rlimit semantics are
  unreliable; CI is the enforcement point per ARCHITECTURE.md:80–83).
- **`crates/devondb/tests/memory_budget_spill.rs`:** same harness,
  workload extended with sort and aggregate over the full table, forcing
  spill under the budget.

Provisional parameters — N = 200_000 nodes / 400_000 edges, ~64-byte
string payloads, `--memory-limit 33554432` (32 MiB), `RLIMIT_AS` 256 MiB —
are writer policy subject to one hard requirement: **each test must fail
when its memory control is severed** (unbudgeted overlay/scan for the first
test; unbudgeted Sort/Aggregate for the second). Both tests print peak
`MemoryBudget::charged()`.

### 9.4 Coverage mapping

| Requirement | Evidence |
|---|---|
| Concurrent reader observes a consistent snapshot while a writer commits | T1, T6, T7 |
| Write-write conflict detected and reported | T3 + a facade test asserting the exact `Display` string of §5.4 through the REPL error path |
| Stress suite green under repetition | §9.1 with raised `DEVONDB_STRESS_ITERS` |
| Golden workload under `memory_limit` with rlimit | §9.3 both stages |

Main-file bytes are unchanged by this design. A golden anchor created from a
concurrent workload is additive corpus coverage; the corpus only grows.

---

## 10. HNSW mutation integration

A snapshot's raw update/delete history, including its own writes and
same-key delete/reinsert, makes indexed topology dirty. Approximate requests
validate the base metadata then use charged exact execution. Checkpoint
publishes new materialized offsets and freshly rebuilt affected roots in one
catalog generation; old snapshots retain their old immutable graph and rows.
Commit derives the HNSW_MUTATION_WAL fence (bit 14) from its current prepared
catalog under the publication lock. Recovery requires the fence for indexed
DML and reconstructs dirty history without logging topology. Bits 6/10/14
clear only after physical WAL truncation; skipped obsolete records must be
truncated even when replay yields no live commit chain.
