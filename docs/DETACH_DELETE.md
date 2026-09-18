# Detach-delete: node deletion with incident-edge removal

This specification defines PK-addressed detach-delete as devondb's first
relationship mutation. It is deliberately not a convenience wrapper around the existing
node delete. The public statement deletes one node and every visible
relationship edge incident to that node atomically; the storage work is a new
relationship-tombstone overlay, WAL vocabulary, merged read path, conflict
claim, and copy-on-write CSR rewrite.

## Current gap and scope

Plain `delete from` is intentionally schema-refused whenever any relationship
table names the target node table as either endpoint, even when the addressed
row has degree zero. The refusal is the offset-stability law, not an
incident-edge existence check (`docs/UI.md` §12.4-§12.5;
`crates/devondb/src/database/commit.rs`, `reject_referenced_delete`). Deleting
one node compacts its node table at checkpoint and shifts every later node
offset, so edges to other rows can become stale even when the deleted row has
no edges.

Relationship tables currently have only insert records and insert deltas.
Their read path concatenates checkpointed CSR with overlay inserts, and their
checkpoint path only appends those inserts while rewriting changed CSR groups
(`docs/FORMAT.md` §§ Rel table adjacency and WAL sidecar;
`crates/devondb-storage/src/rel_table.rs`, `RelTable::neighbors`,
`RelTable::checkpoint`, and `merge_direction`). There is no relationship DML,
relationship tombstone, or CSR deletion path. Detach-delete adds that CSR
rewrite path.

The node half is already proven. Committed links carry PK-keyed replacement
rows and node tombstones; scans merge those effects; commit reserves link and
summary bytes before WAL fsync; PK conflicts are retained across checkpoint;
and checkpoint writes ordinary compacted node groups
(`docs/UI.md` §12.1-§12.4;
`crates/devondb-storage/src/overlay.rs`, `CommitDelta`,
`node_dml_effects`, and `CommitSummary`;
`crates/devondb/src/database/commit.rs`, `publish_prepared_commit` and
`detect_conflict`; `crates/devondb/src/database/checkpoint.rs`,
`materialize_nodes`). Detach-delete extends that machinery rather than
creating a second transaction model.

This first surface removes all edges incident to one PK-addressed node. It does
not add a general relationship predicate, a single-edge identity, or a public
`delete rel` statement.

## Motivation

Insert-only relationship storage otherwise forces applications to retain
obsolete endpoint nodes or hide stale links through versioning. Detach-delete
allows an obsolete node and all of its links to be retired physically without
leaving dangling CSR. Applications may still retain historical versions as a
separate policy choice.

The UI must also support deleting a degree-zero row from an endpoint table;
plain delete cannot provide that behavior because it applies a schema-level
refusal (`docs/UI.md` §12.5-§12.6).

## Statement surface

The canonical text is:

```text
detach delete from <Table> where <pk-column> = <literal>
```

Example:

```text
detach delete from Person where id = 7
```

The IR variant is:

```rust
Statement::DetachDeleteNode {
    table: String,
    key_column: String,
    key: Value,
}
```

Its canonical tagged statement JSON, including declaration/serialization
field order, is:

```json
{"v":0,"stmt":{"stmt":"DetachDeleteNode","table":"Person","key_column":"id","key":{"Int64":7}}}
```

`print_statement` emits exactly `detach delete from`, a canonically quoted
table identifier, ` where `, a canonically quoted key-column identifier,
` = `, and the existing canonical `Value` literal. `detach` is a lowercase
contextual word in statement-head position, following the `pin`/`unpin`
precedent; it is not globally reserved, so an existing table or binding named
`detach` remains a legal bare identifier. `delete`, `from`, and `where` keep
their existing roles. Parse/print/JSON round trips obey the same
statement-envelope law as `UpdateNode` and `DeleteNode`
(`docs/PLAN_IR.md` § Statements; `crates/devondb-plan/src/statement.rs`;
`crates/devondb-plan/src/text/{parser,printer}.rs`).

Validation is exactly the §12.1 node-delete validation before the detach work:

- `table` resolves to a node table under catalog folding;
- `key_column` must resolve to that table's primary-key column, with the same
  predicate-driven-bulk-DML refusal for every other column;
- `key` must have the primary-key type and resolve to one visible row in the
  transaction view; a miss is `NotFound`;
- an HNSW index on the target table uses exact fallback and checkpoint rebuild
  (see [HNSW-indexed endpoints](#hnsw-indexed-endpoints)); and
- the operation is allowed whether the table has zero, one, or many incident
  relationship schemas. On an unreferenced table it is semantically the same
  as plain delete and produces no relationship tombstone records.

Statement order remains observable inside a transaction. An earlier pending
edge insert that touches the node is folded out when detach-delete is staged;
it never reaches the WAL. A later edge insert through that deleted key fails
endpoint resolution in the transaction view. Deleting a node inserted by the
same transaction folds out the node row plus its pending incident edges, just
as current node delete folds out an own insert. An empty result writes no WAL.

Plain `delete from` remains unchanged and retains its schema-level refusal.
There is no implicit upgrade from plain delete to detach-delete: the distinct
spelling makes the potentially large edge/CSR work visible in the trust loop.

## Relationship endpoint tombstones

### Representation and identity

Relationship rows have no primary key or edge id, and duplicate edges with
identical endpoints and properties are legal. A tombstone keyed by
`(from, to, values)` would therefore be ambiguous, while assigning edge ids is
a format and language expansion unrelated to deleting every incident edge.

The overlay representation is an endpoint-role predicate:

```rust
pub struct RelEndpointTombstones {
    pub from_offsets: BTreeSet<u64>,
    pub to_offsets: BTreeSet<u64>,
}

pub struct CommitDelta {
    // Existing nodes, node_updates, node_deletes, edges, ddl, hnsw...
    pub rel_tombstones: BTreeMap<String, RelEndpointTombstones>,
}
```

For each relationship schema naming the deleted table as `from`, the commit
adds the target's current physical offset to `from_offsets`. For each schema
naming it as `to`, it adds the offset to `to_offsets`. A self-relationship adds
both roles. An edge is tombstoned when its `from` is in the first set OR its
`to` is in the second. Thus every duplicate edge and self-loop is removed
without enumerating it into the committed delta.

The write set retains detach intent by canonical node-table name and PK, not
by a guessed offset. Under the commit lock, after ordinary conflict detection,
the target key is re-resolved against the current published state plus the
transaction's own writes. The derived offset is what enters `CommitDelta` and
the WAL. This matches relationship insert, whose pending endpoint keys are
also resolved only at commit because checkpoint or an earlier serialized
commit may have changed the current offset layout (`docs/FORMAT.md`
§ Durability and the delta overlay; `crates/devondb/src/database/commit.rs`,
`resolve_edges_for_view`). A transaction-local resolved copy supplies
read-your-writes before commit.

Endpoint tombstones are immutable after publication and union oldest-first
across a snapshot's reachable commit links. There is no “newer insert revives
the same offset” rule: offsets are not reused within a checkpoint epoch, an
edge insert after the detach cannot resolve the deleted key, and concurrent
inserts are excluded by the conflict law below. Reinserting the same node PK
creates a new tail slot and new edges target that new offset.

This representation is intentionally narrower than general rel DML. It is the
first relationship mutation because recovery, reads, checkpoint, and snapshot
visibility must all understand edge removal; compact metadata does not make it
a wrapper.

### Merged read view

A snapshot derives one effective `RelEndpointTombstones` per relationship
table from its immutable chain and transaction-own delta. Reads merge it as
follows:

1. Forward CSR and overlay edges first reject any edge whose source is in
   `from_offsets`, then reject any surviving neighbor in `to_offsets`.
2. Backward CSR applies the symmetric rule: a grouped destination in
   `to_offsets` has no edges, and source neighbors in `from_offsets` are
   filtered.
3. `both` on a self-relationship applies the same union before its existing
   self-loop de-duplication rule. Each stored occurrence is filtered; duplicate
   relationship rows remain distinct when they survive.
4. Full relationship scans use the forward copy as today and apply the same
   predicate, so scan and expand cannot disagree.

Node visibility needs one companion change. Before checkpoint, detach-delete
must hide the node row without renumbering any other live row. Persisted scans
already retain their physical offset while suppressing a tombstoned row, but
the current overlay iterator compacts surviving insert positions
(`crates/devondb-storage/src/overlay.rs`, `effective_node_rows`). Detach-delete
must add an offset-carrying overlay-row view that counts every assigned
insert slot, including invisible deleted slots. New inserts append after that
physical span. Expand's source count and neighbor lookup use the span and a
slot-preserving `Option<Row>`-style view, not a dense vector of visible rows.
Every edge into a hole must already have been filtered by the relationship
tombstones; otherwise the merged view is corrupt.

This “invisible hole until checkpoint” rule is in-memory only. It preserves
the offsets named by pending relationship WAL records and old CSR while a
fresh snapshot hides the node and incident edges immediately. An old snapshot
whose chain predates the detach sees both; a snapshot containing the detach
sees neither. Checkpoint later compacts the hole and remaps every surviving
reference atomically.

## WAL and feature-bit law

Each endpoint-role tombstone has one canonical WAL payload:

```json
{"rel_delete":{"rel":"Knows","endpoint":"from","offset":7}}
```

The `rel_delete` object has exactly the fields `rel`, `endpoint`, and `offset`
in that order. `endpoint` is exactly `"from"` or `"to"`; `offset` is a u64.
Unknown fields, an unknown role, an unknown relationship table, a role whose
schema endpoint is not the node table being detached, or an out-of-domain
offset is corruption during recovery. A detach group also carries the
existing canonical node-delete record by PK:

```json
{"delete":{"table":"Person","key":{"Int64":7}}}
```

Within a transaction, rel-delete records are emitted after relationship
inserts and node updates but before node deletes. Relationship names are in
lexicographic order; within one relationship, detach statement order is
preserved and `from` precedes `to`; duplicate `(rel, endpoint, offset)` claims
are folded. Node delete remains the last payload phase and the final node-row
intent (`docs/FORMAT.md` § WAL sidecar rule 3).

Bit 6 `DML_WAL` is not a sufficient compatibility fence. A binary that
supports bit 6 today considers the file supported but its closed WAL decoder
does not know `rel_delete`; reusing the bit would turn a future-feature refusal
into a misleading corruption/unknown-discriminator failure. The feature
registry's acceptance law requires an unsupported not-read-safe feature to be
identified before its bytes are interpreted (`docs/FORMAT.md` § Feature flag
registry).

The format therefore uses bit 10, `REL_TOMBSTONE_WAL`, which is not read-safe
and has a checkpoint-scoped lifetime. A detach
commit sets the union of bit 6 and bit 10 in one flag-only dual-slot
publication before writing either governed record. A transaction containing
only the internal rel-tombstone vocabulary would need bit 10; the public
detach statement always also writes or folds a node delete. Setting a bit and
then crashing before WAL append is a safe downlevel refusal.

Recovery replays `rel_delete` into the same immutable endpoint-role sets as a
live commit. After catalog publication and WAL truncate/rotation, checkpoint
clears every checkpoint-scoped bit whose governed records are gone—bits 6 and
10 together for detach-delete. Clearing occurs only after truncation, using
the existing both-slot flag-only protocol.

**Reopen recovery rule:** when reopening a writable database observes bits
6|10 set while its WAL contains no governed
records, reopen clears both bits through the both-slot flag-only publication
before returning a writable handle. This is an explicit recovery action, not a
reliance on checkpoint: MVCC §6 step 1 makes every checkpoint—including an
explicit one—a no-op when `chain == None`, and both automatic triggers key on
overlay charge, which is zero after a completed drain. Read-only open performs
no superblock write and continues to refuse unsupported bit sets under the
acceptance law. Rejected alternative: healing via the empty-chain checkpoint
no-op path. It would require changing the shared no-op guarantee in MVCC §6
step 1 and still misses a read-mostly deployment where no checkpoint is invoked.

The WAL discriminator registry, canonical phase order, supported mask, feature
table, freeze fixtures, and pending-detach recovery corpus anchor must all
include this record and feature bit.

## Commit-time conflict law

Current node conflicts compare a transaction's `inserted_pks ∪ dml_pks`
against the same two maps in every retained concurrent `CommitSummary`.
First committer wins, and summaries survive checkpoint while an older write
transaction exists (`docs/UI.md` §12.2;
`crates/devondb/src/database/commit.rs`, `detect_conflict`;
`crates/devondb-storage/src/overlay.rs`, `CommitSummary`). Detach remains in
`dml_pks`, so detach/update, detach/delete, detach/detach, and relevant node
insert races retain that law and error shape.

Edge insertion adds one asymmetric conflict that cannot be represented by
putting every endpoint in `dml_pks`: doing so would incorrectly conflict two
ordinary edge inserts and would conflict an edge insert with a position-
preserving node update. Add two summary categories instead:

```rust
pub detach_pks: BTreeMap<String, Vec<Value>>,        // subset of dml_pks
pub edge_endpoint_pks: BTreeMap<String, Vec<Value>>, // endpoint table -> keys
```

`edge_endpoint_pks` is derived from pending relationship rows and their
catalog schemas before keys become offsets. Keys are de-duplicated per node
table for summary size, but edge rows themselves remain duplicates.

| Transaction claim | Concurrent committed claim | Verdict |
|---|---|---|
| existing `inserted_pks ∪ dml_pks` | existing `inserted_pks ∪ dml_pks` | existing PK conflict |
| `detach_pks[table, key]` | `edge_endpoint_pks[table, key]` | conflict |
| `edge_endpoint_pks[table, key]` | `detach_pks[table, key]` | conflict |
| edge endpoint | edge endpoint | no conflict |
| edge endpoint | ordinary node update | no conflict |

The cross-check is symmetric even though the maps are distinct. If an edge
insert commits first, a concurrent detach loses and retry sweeps that edge. If
the detach commits first, the edge insert loses before commit-time endpoint
resolution can report a misleading `NotFound`. The whole losing transaction
errors with the existing `TransactionConflict` shape, naming the node table,
PK, winning LSN, and that a concurrent detach/incident-edge insert touched it.

The claims must use `recent_summaries`, not only reachable rel tombstones,
because checkpoint may clear the chain while the loser still holds its old
snapshot. A process restart has no surviving pre-restart write transaction;
therefore conflict-only endpoint-key claims reconstructed from old WAL groups
may be omitted after recovery, but live summaries must retain them until the
existing minimum-write-snapshot pruning rule permits release.

## Checkpoint materialization and offset remapping

### Epoch remap

`docs/FORMAT.md` currently says node offsets are stable because checkpoint
only appends and “deletes do not exist yet.” `docs/UI.md` §12.4 explicitly
names whole-CSR offset remapping at checkpoint as one way to lift the delete
refusal. This specification uses that path. The FORMAT semantic paragraph
defines the revised offset law even though no node-group or CSR byte layout
changes.

Checkpoint retains the old catalog/layout for reads and builds a new catalog
copy for publication. For each node table with effective detach tombstones it
derives a monotone epoch remap:

```text
deleted old offset d  -> None
surviving old offset x -> x - count(deleted offsets < x)
```

The old domain includes checkpointed rows and every assigned overlay insert
slot, including a deleted slot and a later reinsertion of the same PK. Node
groups materialize visible rows in old-offset order, then ordinary overlay
append order, producing exactly the remap above. The mapping may be represented
as sorted deleted offsets plus rank, not an O(row-count) array.

Every relationship insert and relationship tombstone in the committed chain
uses old-epoch offsets. Checkpoint consumes them before WAL truncation, removes
the tombstoned edges, remaps both endpoints of every survivor, and emits only
ordinary CSR in the new epoch. No rel tombstone or remap metadata persists in
the clean main file.

### Both physical directions are mandatory

For a relationship `R from A to B`, forward CSR is grouped by `A` and stores
`B` offsets as neighbors; backward CSR is grouped by `B` and stores `A`
offsets. Deleting from either endpoint therefore affects both copies:

| Deleted table role | Forward (`fwd`) work | Backward (`bwd`) work |
|---|---|---|
| `A` / `from` | remove source `d`; later source slots/groups shift | remove neighbor `d`; every surviving `A` neighbor above `d` remaps |
| `B` / `to` | remove neighbor `d`; every surviving `B` neighbor above `d` remaps | remove source `d`; later source slots/groups shift |
| self-rel (`A == B`) | both source-slot and neighbor rules | both source-slot and neighbor rules |

An “affected source group” means a group in that direction's grouping domain
(`A` for fwd, `B` for bwd) for which at least one of these is true:

- its covered slot layout changes because an endpoint row before or inside it
  was compacted;
- it contains an incident edge removed by an endpoint tombstone;
- it contains a surviving neighbor whose offset changes under the other
  endpoint's remap; or
- it receives a committed overlay edge insert.

Checkpoint must inspect enough of both directions to prove a group unaffected;
neighbor remapping can make a group far from the deleted node require rewrite.
In the worst case every CSR group in both directions is affected. A page id may
be reused only when the source slots, neighbor offsets, properties, edge order,
and row count are byte-identical under the remap. Rewriting only the groups
that contained an incident edge is incorrect, including when the deleted node
has degree zero.

For each affected group, checkpoint reads the old immutable group, streams
surviving edges in existing per-slot insertion order, applies the endpoint
maps, appends committed overlay edges in WAL order, writes a fresh `RCSR`
group, and swaps the direction's catalog entry. A group becoming empty uses
the existing zero/short-array spelling. Source adjacency lists that cross a
new node-group boundary move to the corresponding replacement group without
reordering their edges. Self-loops are removed once logically even though both
physical directions contain them.

Node groups are materialized first, as today, but relationship reads during
the rewrite use the retained old catalog/layout while destinations are built
against the new layouts. Both direction rewrites, node groups, and the catalog
are copy-on-write and become authoritative in one `catalog.save` at the
current commit LSN. Only after that publication may the WAL be truncated and
bits cleared (`docs/MVCC.md` §6; `docs/FORMAT.md` § MVCC and page
immutability). Any failure leaves the old catalog, WAL, and published chain
usable; newly written unreachable pages leak under current format law.

Old snapshots remain correct: one pinned before detach sees old nodes/CSR; one
pinned after detach but before checkpoint holds the old catalog plus its
tombstone chain; a fresh post-checkpoint snapshot sees compacted nodes and
remapped CSR. Hidden offsets are internal columns, so logical node and edge
order is preserved across the epoch change.

## HNSW-indexed endpoints

Detach-delete supports indexed targets. Raw node-delete history disables ANN
for the target table, and checkpoint rebuilds every affected index from scratch
after node compaction and relationship remapping. An index only on the other
endpoint keeps its unchanged topology. The whole operation is fenced by bits
6, 10 and, for indexed targets, 14 before WAL append (`docs/HNSW.md` §5.7).

CREATE HNSW INDEX remains the sole statement in its transaction; a mixed
CREATE/detach transaction is rejected before sweep or WAL work. Its actual
construction runs after checkpoint under the commit/publication lock, so
concurrent offset remaps cannot publish stale prepared topology.

## Multi-process follower publication

The multiprocess publication gate defines the required atomic boundary. A
writer holds it exclusively while it publishes feature bits, appends the whole
WAL group, fsyncs, and swaps local state. A follower taking the gate shared
must replay the node delete and all rel-delete records into one new immutable
`PublishedState` before its pointer swap. It may observe the old node plus old
edges or neither; it may never publish a node tombstone without its incident
edge tombstones (`docs/MULTIPROCESS.md` option A and invariants 3-5).

Checkpoint holds the same gate across compacted node groups, both CSR
directions, catalog/superblock publication, WAL reset, and bit clear. A
follower rebase therefore observes either the pre-checkpoint old-offset
catalog plus WAL tombstones, or the post-checkpoint remapped catalog plus the
new WAL epoch. Pinned follower snapshots remain on their old immutable pages
and chain; page leak-not-reuse is still the safety argument.

Every follower refresh must re-run feature-flag acceptance on the newly
authoritative superblock before decoding new WAL bytes. This matters for a
follower binary that opened a clean file while bit 10 was clear and then sees
a detach commit set it. A binary without `REL_TOMBSTONE_WAL` support must fail
the refresh as an unsupported not-read-safe feature, not decode `rel_delete`
as corruption and not publish a partial group. Query-start refresh and the
100 ms default polling bound then give detach-delete the ordinary multiprocess
visibility guarantee (`docs/MULTIPROCESS.md` invariants 6-7 and 11).

## Budget and write-amplification accounting

The endpoint-role representation keeps commit cost proportional to incident
relationship schemas, not node degree. It does not hide the physical edge
sweep; it moves it to bounded read/checkpoint work where CSR already lives.

- **Write set:** charge the canonical table/key detach intent, per-rel map
  entry/name, each role/offset set entry, and the `detach_pks` summary values.
  Pending edge inserts already charge their endpoint keys; the new
  `edge_endpoint_pks` summary storage also charges. Folding an own insert/edge
  may retain an over-reservation until commit/abort, but no allocation is
  uncharged.
- **Committed overlay:** extend `CommitDelta::estimated_bytes` and
  `CommitSummary::estimated_bytes`. The exact link plus summary reservation is
  established before feature-bit/WAL publication, preserving the current “no
  fallible allocation after fsync” law
  (`crates/devondb/src/database/commit.rs`, `transfer_commit_charge` and
  `publish_prepared_commit`).
- **Read path:** charge endpoint-tombstone sets, physical-slot/hole indexes,
  and the existing adjacency materialization. Sizing must count edges read
  before filtering when their bytes are resident; charging only the smaller
  surviving result would under-account the sweep
  (`crates/devondb/src/database/view.rs`, `rel_overlay_bytes` and
  `adjacency_working_set_bytes`).
- **Checkpoint sweep:** never materialize a graph-wide incident-edge list or a
  whole relationship twice. Decode and build at most one affected CSR group
  per direction at a time, charge old decoded arrays plus the replacement
  builder before allocation, shed clean cache frames on pressure, write the
  fresh group, then release scratch before the next group. The committed
  overlay remains charged until successful publication.
- **Failure and back-off:** if one affected CSR group cannot fit after reclaim,
  checkpoint returns `BudgetExceeded`
  with relationship, direction, group, requested, charged, and limit. It does
  not truncate the WAL or report the already durable detach commit as failed;
  reads continue through the tombstone overlay (`docs/MVCC.md` §7.2).
  Retrying before capacity changes amplifies leakage: each execute-fold
  backstop attempt can durably write fresh pages for groups processed before
  the failing group. Therefore the execute-fold backstop records the failing
  `(relationship, direction, group)` as a persistent-session memo named
  `DETACH_CHECKPOINT_BLOCKED` and does not run detach compaction again until
  that recorded group's measured request fits under the current limit (a
  larger configured limit or successful release of competing charge). Ordinary
  checkpoints may proceed only when they can skip the blocked detach work
  without publishing a partial epoch; otherwise they return the same honest
  error without writing detach compaction pages.

  On every such failure, all prospective page ids written during the failed
  detach compaction are queued in memory for retirement, exactly like the
  indexed-COPY failure queue: FREE_PAGES § Retirement sequence step 5 is the
  mechanism and INDEX_BULK_LOAD gate 0 is the template. The next successful
  publication retires those pages into the ledger; SIGKILL-abandoned pages
  remain sweepable only. A severed-proof must show that a failing group no
  longer causes per-statement file growth once the memo is installed, and that
  severing the queue reintroduces it.

Offset compaction can require O(E) reads and, in the worst case, fresh writes
for both copies of every edge in an incident relationship table. That is real
eMMC write amplification. The affected-group reuse proof avoids unnecessary
writes, while the explicit `detach` spelling exposes that the operation is not
cheap. Avoiding this cost would require persistent node deletion vectors and
a different offset law, rejected below as a separate format design.

## Verification requirements

- text, printer, and tagged JSON are exact round-trip forms, including quoted
  identifiers and all PK value kinds;
- zero-degree deletion from a referenced table succeeds and remaps unrelated
  later-node edges correctly;
- outgoing, incoming, self-loop, duplicate, multi-rel-table, and both-role
  edges disappear in the transaction view, fresh snapshots, after recovery,
  and after checkpoint/reopen;
- an edge inserted before detach in one transaction folds out; after detach it
  fails; both commit orders of concurrent edge-insert versus detach yield one
  winner and one `TransactionConflict`, including a checkpoint between them;
- rows later than a deleted overlay row keep their old offsets until
  checkpoint, and revived PKs receive a new tail offset;
- old local and follower snapshots remain byte-stable across checkpoint while
  fresh snapshots see remapped CSR;
- kill-9 at bit set, mid-group, post-fsync, catalog publication, WAL truncate,
  and bit clear yields old-or-new atomic state, never node/edge halves;
- a bit-6-aware but bit-10-unaware fixture refuses pending detach WAL cleanly;
- HNSW on the target table rebuilds after mutation, while HNSW only on the
  other endpoint does not;
- tombstone-heavy reads and a high-degree checkpoint stay within charged
  memory or fail `BudgetExceeded` without losing the durable commit;
- own-transaction `CREATE INDEX` on the detach target refuses before sweep or
  WAL work, including when the transaction also contains the detach statement;
- a writable reopen observing bit-set + empty governed WAL clears bits 6|10,
  while a pre-bit binary still refuses that reopened file until the heal; and
- a persistent undersized-budget failure writes the failing group at most once
  until capacity changes, retires already-written prospective pages on the next
  successful publication, and resumes only when its request fits.

## Rejected shapes

- **Implement detach as “scan edges, then call plain delete.”** Plain delete is
  schema-refused, and separate statements/WAL groups expose half-applied state.
  The first rel mutation needs one atomic commit and one snapshot view.
- **One exact tombstone per incident edge.** Degree-proportional WAL/overlay
  memory is hostile to hubs, and `(from, to, values)` cannot distinguish legal
  duplicate edges. Endpoint-role predicates express this operation exactly.
- **A generic public `delete rel` riding the endpoint predicate.** Deleting one
  chosen edge needs identity and duplicate semantics that detach-delete does
  not supply. It gets its own evidenced design.
- **Reuse bit 6 `DML_WAL`.** Existing bit-6 readers do not know the new closed
  discriminator; a new not-read-safe checkpoint-scoped bit is required for a
  clean compatibility refusal.
- **Rewrite only CSR groups containing edges incident to the deleted row.** A
  degree-zero deletion still shifts later node offsets, and neighbor remaps
  can affect any group in the opposite direction.
- **Always materialize the whole relationship graph for the sweep.** It breaks
  the one-budget contract. Direction/group-at-a-time rewrite is the bounded
  unit, with honest failure when one unit cannot fit.
- **Allow edge-insert/detach races and rely on endpoint `NotFound`.** That makes
  outcome depend on post-conflict resolution order and can let an unseen edge
  escape a sweep. Retained PK claims give deterministic first-committer-wins.
- **Persist deletion vectors in node/CSR directory extensions now.** They can
  avoid offset remapping but change clean-file interpretation, scan density,
  HNSW behavior, and reclamation. `docs/UI.md` §12.4 and `docs/HNSW.md` §11
  correctly reserve that as a separate FORMAT design.
- **Repair HNSW synchronously during detach.** HNSW uses the deleted table's
  offsets throughout its topology and has no proven delete algorithm. Silent
  ANN staleness is worse than the explicit refusal.
- **Leave invisible overlay rows densely renumbered before checkpoint.** WAL
  relationship offsets and old CSR would point at the wrong nodes. Holes are
  mandatory until the atomic remap publication.

## Design decisions

- **CSR writer.** The implementation reuses the full-group `CsrGroup` API
  with complete scratch charging and an honest `BudgetExceeded`. The durable
  detach remains readable from WAL and can be checkpointed under a larger
  limit. A streaming payload writer is a separate follow-up.
- **Feature appearing after follower open.** Existing pinned snapshots may
  finish from immutable state. The refresh error prevents new snapshots and
  queries on that handle and instructs the caller to upgrade and reopen.
- **Unreferenced tables.** `detach delete` is accepted on an unreferenced
  table. It is deterministic, emits no relationship records, and lets callers
  use one deletion policy without a schema preflight.
