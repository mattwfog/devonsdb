# devondb on-disk format — spec v1 (frozen)

**Status: writers emit `format_version` 1 and the layout is frozen.** The
repository already enforces the [compatibility rules](#compatibility-rules)
mechanically; they become devondb's public release contract when `v0.1.0` is
tagged. Pre-freeze `format_version` 0 files remain permanent compatibility
anchors and must keep opening forever.

This file is the single source of truth for bytes on disk. Any commit that
changes the on-disk layout MUST update this spec and the golden corpus
(`tests/golden/`) in the same commit. Code comments never override this spec.

## Files

A database named `db` is two files:

| File | Purpose |
|------|---------|
| `db.devondb` | Main file: superblocks + pages |
| `db.devondb-wal` | Write-ahead log sidecar; may be absent after clean close |

## Main file layout

The main file is a sequence of fixed-size **pages**. `page_size` is recorded
in the superblock; the v1 default is 4096 bytes. Pages 0 and 1 are the two
superblock slots; data pages start at page 2.

### Superblock (pages 0 and 1)

All integers little-endian. Layout of one superblock slot:

| Offset | Size | Field | Notes |
|--------|------|-------|-------|
| 0 | 8 | magic | ASCII `DEVONDB\0` |
| 8 | 4 | `format_version` (u32) | version that WROTE this file |
| 12 | 4 | `min_reader_version` (u32) | oldest format a reader must understand to open this file |
| 16 | 8 | `feature_flags` (u64) | bitmap; unknown SET bits above the reader's known range → refuse write, allow read only if bit is in the read-safe mask (see rules) |
| 24 | 4 | `page_size` (u32) | bytes per page; power of two, 4096–65536 |
| 28 | 16 | `db_id` | random 128-bit id, fixed at creation |
| 44 | 8 | `checkpoint_lsn` (u64) | commit LSN of the newest transaction whose effects are materialized in the main file |
| 52 | 8 | `catalog_root` (u64) | page id of the catalog root; 0 = empty database |
| 60 | 4 | `crc32c` (u32) | CRC-32C over bytes 0–59 of this slot |

**Extension region.** Without the `FREE_PAGES` bit, writers leave every byte
after the 64-byte header zero, and that guarantee is fence-tested
(`crates/devondb/tests/format_freeze.rs`) over both freshly written files and
every committed corpus anchor. Readers load only the header — `read_slot`
reads exactly 64 bytes — so the region is neither covered by the slot CRC
(bytes 0–59) nor validated by slot arbitration. This asymmetry is deliberate:
a feature claiming this region is fenced by
its feature bit under the acceptance law, exactly as every other extension
is, so a read-side rejection would buy no additional unambiguity while
creating a new way for a file to become unreadable — directly against rule 1.
This is the one place where the format does NOT mirror the node-group/CSR
directory treatment, and the reason is that those regions sit on a page the
reader has already loaded, where the check is free.

**`FREE_PAGES` superblock extension (bytes 64–91 of each slot).** This is the
first claimant of the extension region; its design and crash analysis are in
`docs/FREE_PAGES.md`. Present only under feature bit 5:

| Offset | Size | Field | Notes |
|--------|------|-------|-------|
| 64 | 8 | `retire_ledger_head` (u64) | page id of the oldest ledger page; 0 = empty ledger |
| 72 | 8 | `retire_ledger_tail` (u64) | page id of the newest ledger page (append target); 0 = empty — chain walks stop here even when the tail's `next_page` is nonzero |
| 80 | 8 | `retired_total` (u64) | cumulative entries ever appended, strictly monotone; live entries derive as `retired_total` minus cumulative reuse (recoverable by walking the ledger) |
| 88 | 4 | `extension_crc32c` (u32) | CRC-32C over bytes 64–87 |

Bytes 92+ stay writer-zeroed and future-claimable under the zero-fence law.
Slot arbitration is UNCHANGED — header CRC + LSN only. A FREE_PAGES-supporting
binary validates the extension CRC of the slot arbitration chose before
trusting the ledger fields; a mismatch is NOT `Corrupt` and NOT a slot flip —
it is degraded mode: the ledger is treated as absent, allocation appends as
today (leak-only, sweepable), and the next publication rewrites a valid
extension. A torn extension therefore costs at most one ledger chain, never
readability.

**Free-page ledger pages.** A ledger page is an ordinary allocated page,
reachable only from the superblock extension and read only by `FREE_PAGES`
writers:

| Offset | Size | Field |
|--------|------|-------|
| 0 | 8 | magic `DEVONFPL` |
| 8 | 8 | `next_page` (u64; 0 = tail) |
| 16 | 4 | `entry_count` (u32) |
| 20 | 4 | `consumed_count` (u32) — entries before this index are already reused |
| 24 | 4 | `crc32c` over bytes 0–23 and the entry array |
| 28 | 4 | reserved (zero) |
| 32 | 16×n | entries: `(page_id u64, retired_lsn u64)`, append-ordered |

Entries are appended at publication time; `consumed_count` advances as the
allocator reuses entries, rewriting the ledger page in place under the
consumption durability law (`docs/FREE_PAGES.md` § Ledger pages): consumption
rewrites are written and synced BEFORE the superblock flip of the publication
whose catalog references the reused pages. Ledger pages are an exception to
data-page immutability (§ MVCC and page immutability), which is sound because
no catalog, snapshot, or golden anchor ever references one.

**Dual-slot protocol**: writers alternate slots; a superblock write goes to
the slot NOT currently authoritative, fsynced before any reference to it.
Readers validate both slots' CRCs and take the valid slot with the higher
`checkpoint_lsn`; on an equal-LSN tie, slot 0 wins. One valid slot is
sufficient to open.

`checkpoint_lsn` is specifically the commit LSN of the newest transaction
whose effects are materialized in the main file, not a separate checkpoint
counter. It advances only when checkpoint publishes at least one newer
committed transaction.

**Flag-only publication**: a feature-flag lifecycle change with no new
materialized state (the checkpoint-scoped `DML_WAL` and
`REL_TOMBSTONE_WAL` bits, § Feature flag registry) is written WITHOUT
advancing `checkpoint_lsn`: the updated
superblock lands in BOTH slots, non-authoritative first, each fsynced —
so the equal-LSN tie-break yields the new flags whichever slot it picks.
A crash between the two slot writes may surface either flag state on
reopen; every such outcome is safe by the callers' ordering contracts
(a set precedes the WAL records it governs; a clear follows WAL
truncation).

### Catalog page

The authoritative superblock's `catalog_root` points to the catalog page.
When `catalog_root` is zero, the catalog is empty and no catalog page exists.
All integers are little-endian. A payload that fits occupies exactly one
page (the v1 encoding, byte-for-byte unchanged); a larger payload uses the
multi-page encoding below under `MULTIPAGE_CATALOG` (feature bit 9). The
single-page layout:

| Offset | Size | Field | Notes |
|--------|------|-------|-------|
| 0 | 4 | `len` (u32) | payload length in bytes |
| 4 | 4 | `crc32c` (u32) | CRC-32C over the payload |
| 8 | `len` | payload | canonical `serde_json` encoding of the catalog struct |

The catalog JSON object has `node_tables`, then `rel_tables`, then
`storage`. The first two are arrays of the corresponding schema objects in
declaration order. `storage` maps a table name to its storage state:

Catalog identifiers are stored with their creation-time UTF-8 spelling and
are matched by ASCII lowercase folding on both sides. Only ASCII `A` through
`Z` change under folding; non-ASCII bytes compare exactly. Node-table and
relationship-table names share one folded namespace, and column names share
one folded namespace within their table. DDL rejects a fold-equal duplicate as
an invalid argument; decoding a catalog containing fold-equal table or column
duplicates is corruption. Relationship endpoint names are resolved by the
same folded rule while retaining their stored spelling.

- A node table maps to `{"groups": [<node-group directory page ids, oldest
  first>]}` — only the last listed group may hold fewer rows than the
  writer's group capacity.
- A rel table maps to `{"fwd": [<CSR-group directory page ids>], "bwd":
  [<CSR-group directory page ids>]}` (see
  [Rel table adjacency](#rel-table-adjacency-csr-groups)). `fwd[i]` covers
  the from-table's node group `i`; `bwd[i]` covers the to-table's node
  group `i`. An entry of `0`, or an array shorter than the endpoint
  table's group list, means those node groups have no edges in that
  direction.

Tables with no persisted rows or edges may be absent from `storage`
(readers treat absence as empty). The encoding is compact, with no
insignificant whitespace. Bytes after the payload through the end of the
page are zero.

The keys of the persisted `storage` object remain the catalog display spelling
and readers fold on access. They are not rewritten to canonical lowercase;
this preserves existing catalog and golden-corpus bytes. Two storage keys that
are fold-equal are corruption.

### Multi-page catalog

When the encoded payload exceeds `page_size - 8`, the save spills into
continuation pages and `catalog_root` names a directory page instead.
`MULTIPAGE_CATALOG` (feature bit 9, NOT
read-safe) is derived in the same superblock publication: set iff the
published catalog is multi-page. The flag — never byte-sniffing — decides
which decoder reads `catalog_root`, so a binary predating the bit refuses
the file instead of misreading the directory as a corrupt v1 page.

Directory page layout:

| Offset | Size | Field | Notes |
|--------|------|-------|-------|
| 0 | 4 | `len` (u32) | total payload length in bytes; must exceed `page_size - 8`, else corrupt |
| 4 | 4 | `crc32c` (u32) | CRC-32C over the whole payload |
| 8 | 4 | `cont_count` (u32) | must equal `ceil(len / page_size)`, else corrupt |
| 12 | 8 × `cont_count` | continuation page ids (u64) | payload order; id `0` is corrupt |

Payload bytes live only in the continuation pages: each holds one
full-page chunk in order, the last chunk zero-padded to the page end.
Bytes after the id list through the end of the directory page are zero.
Non-zero padding in either place is corruption. The payload reassembled
from the continuation pages must match `crc32c`, then decodes and
validates exactly as a single-page payload.

The directory must fit one page: a payload needing more than
`(page_size - 12) / 8` continuation pages (~2 MiB at the 4 KiB page
size) fails the save with an invalid argument error before any page is
allocated. Like the single-page catalog, the directory and continuation
pages are copy-on-write: every save writes fresh pages and publishes the
new root through the alternate-superblock protocol.

### Node group pages

A node group stores the property values of a contiguous run of a node
table's rows, one columnar payload per property column. Group capacity is
writer policy, not format: v1 writers cap a group at 2048 rows, and readers
accept any `row_count ≥ 1`. On disk a group is one **directory page** plus,
per column, a contiguous run of payload pages. A group is referenced by its
directory page id (a single u64, like `catalog_root`).

All integers little-endian.

**Directory page:**

| Offset | Size | Field | Notes |
|--------|------|-------|-------|
| 0 | 4 | magic | ASCII `NGRP` |
| 4 | 4 | `row_count` (u32) | rows in this group; ≥ 1 |
| 8 | 4 | `column_count` (u32) | property columns; ≥ 1 |
| 12 | 4 | directory flags (u32) | extension-section presence bits, below; zero = no extensions |
| 16 | 16 × (`column_count` + `b1_rescore_count`) | column entries | main entries in catalog declaration order, then derived b1 rescore entries; zero rescore entries for catalogs without `VectorEncoded` (see [b1 rescore payload runs](#b1-rescore-payload-runs)) |

Column entry:

| Offset | Size | Field |
|--------|------|-------|
| 0 | 8 | `first_page` (u64) — first page of the payload's contiguous run |
| 8 | 4 | `byte_len` (u32) — encoded payload length in bytes |
| 12 | 4 | `crc32c` (u32) — CRC-32C over the payload |

A payload occupies `ceil(byte_len / page_size)` consecutive pages starting
at `first_page`; unused bytes of its last page are zero. The directory —
header, entry array, and extension sections together — must fit one page:
`16 + 16 × (column_count + b1_rescore_count) + Σ(8 + section_len)` over
declared sections `≤ page_size`.
Directory page bytes after the last extension section (after the entry
array when the flags word is zero) are zero; a nonzero tail is corruption.

**Extension sections (directory-flags word).** Each set bit in the
directory-flags word declares one extension section. Sections appear
immediately after the column-entry array, in ascending bit order, each
framed as:

| Offset | Size | Field |
|--------|------|-------|
| 0 | 4 | `section_len` (u32) — payload byte length |
| 4 | 4 | `crc32c` (u32) — CRC-32C over the section payload |
| 8 | `section_len` | section payload |

A directory-flags bit may be set only when its governing superblock
feature bit (registry below) is set; a flags bit whose feature bit is
clear is corruption. A reader that supports a set bit validates its
section (frame and payload); a reader that does not support a set bit
skips the section by its length — a state reachable only when the
acceptance law admitted the file read-only under an unsupported
read-safe feature bit, since a not-read-safe feature's superblock bit
already refused the file, and a flags bit with no set feature bit is
corruption. A `section_len` that overruns the page or a CRC mismatch on
a supported section is corruption. An unsupported section is opaque, so
its framed CRC field is not checked while skipping it.

Directory-flags bit registry:

| Bit | Section | Governing feature bit |
|-----|---------|----------------------|
| 0 | `ZONE_MAP_STATS` (below) | 3 `ZONE_MAPS` |
| 1 | `COLUMN_ENCODINGS` (`docs/SCALE.md` §8.1: `4 × column_count` bytes, `encoding_id u8 · p0 · p1 · p2` per main column; absent when all payloads are plain) | 13 `COLUMN_ENCODINGS` |
| 2 | Reserved opaque read-safe fence; no section semantics assigned, used to enforce skip-by-`section_len` behavior | 12 `RESERVED_READ_SAFE_12` |

**`ZONE_MAP_STATS` section (directory-flags bit 0).** Per-group column
statistics for scan pruning (`docs/SCALE.md` §1 for the read-safety basis and
§4.3 for the stats scope). The payload
is exactly `column_count` 24-byte records — main columns only, catalog
declaration order; derived b1 rescore entries carry no stats — so
`section_len` must equal `24 × column_count`:

| Offset | Size | Field |
|--------|------|-------|
| 0 | 4 | `null_count` (u32) — null rows in this column; ≤ `row_count` |
| 4 | 4 | `stats_flags` (u32) — bit 0: min/max present; other bits zero |
| 8 | 8 | `min` — least non-null value, encoding by type below; zero when absent |
| 16 | 8 | `max` — greatest non-null value; zero when absent |

Min/max encodings and presence, by the column's logical type:

| Type | min/max encoding | Present |
|------|-----------------|---------|
| `Int64` | i64 LE two's complement | iff `null_count < row_count` |
| `Float64` | IEEE-754 binary64 LE | iff `null_count < row_count` AND no non-null value is NaN |
| `GeoPoint` | atom key u64 LE — the point's res-15 cell index (`docs/GEO.md` §3) computed through the determinism-pinned assignment path (`docs/GEO.md` §4) | iff `null_count < row_count` |
| `Bool`, `String`, `Vector(dim)`, `VectorEncoded` | — | never |

`Float64` ordering for min/max selection is IEEE-754 totalOrder
(`f64::total_cmp`): it refines numeric order on the NaN-free values the
presence rule guarantees, and it pins one byte spelling when `-0.0` and
`+0.0` tie for an extreme. Directory-level validation (no payload reads):
`null_count > row_count`, nonzero unknown `stats_flags` bits, a presence
bit violating the table above (set for a never-stats type, or clear for
an `Int64`/`GeoPoint` column with `null_count < row_count` — `Float64`
may be absent via the NaN rule), or `min > max` under the type's order
is corruption. Stats are format law, not hints: a non-null payload value
outside its declared `[min, max]` or a `null_count` disagreeing with the
validity bitmap is corruption — enforced by the corpus validator and
corruption tests, never re-checked on the scan hot path. Presence of the
section is per-group: groups written before the feature coexist with
stats-bearing groups in one file, and pruning simply skips groups
without stats.

**Column payload encoding.** A payload is a validity bitmap followed by a
values section:

1. **Validity bitmap** — `ceil(row_count / 8)` bytes. Bit `i % 8` of byte
   `i / 8` (LSB-first) is set iff row `i` is non-null. Trailing bits zero.
2. **Values**, by the column's logical type:

| Type | Values encoding |
|------|-----------------|
| `Bool` | A second bitmap, `ceil(row_count / 8)` bytes, same bit order; set = true |
| `Int64` | `row_count` × 8 bytes, two's complement |
| `Float64` | `row_count` × 8 bytes, IEEE-754 binary64 |
| `Vector(dim)` | `row_count` × `dim` × 4 bytes, IEEE-754 binary32, row-major |
| `VectorEncoded` | fixed-size quantized slots specified in [Quantized vector element encodings](#quantized-vector-element-encodings) |
| `GeoPoint` | 16 bytes per row: `lat_deg` f64 LE, then `lng_deg` f64 LE; canonical per `docs/GEO.md` §5, non-canonical is corruption |
| `String` | `(row_count + 1)` × 4-byte u32 offsets, then a UTF-8 heap |

GeoPoint columns are live for node tables and gated by feature bit 4
(`GEO_COLUMNS`, registry below); relationship properties reject the type.

String offsets start at 0, are non-decreasing, and end at the heap's byte
length; row `i`'s bytes are heap`[offset[i] .. offset[i+1]]`. A null or
empty string both have zero length — the validity bitmap alone
distinguishes them. Null slots in every values encoding are zeroed.

Column types are NOT recorded in the group; the catalog schema is the
single source of truth. `byte_len` must equal exactly the size implied by
`row_count` and the column's type (for `String`, the offsets must also be
internally consistent). Any mismatch — bad magic, wrong `byte_len`, CRC
failure, non-monotonic offsets, invalid UTF-8 — is corruption and fails
the read cleanly.

Encoding is deterministic: identical logical content always produces
identical bytes (null slots zeroed, trailing bytes zeroed), so golden
corpus files are byte-stable.

### Rel table adjacency (CSR groups)

A rel table's edges are stored TWICE, as two independent CSR structures:
**fwd** (grouped by source node, neighbors are destination nodes) and
**bwd** (grouped by destination node, neighbors are source nodes). Both
directions carry a full copy of the edge property columns, so a traversal
in either direction is a single self-contained read.

**Node offsets.** Adjacency references nodes by *node offset*: a node's
zero-based position in its table's checkpointed storage — the sum of the
`row_count`s of all preceding node groups (catalog order) plus its row
index within its group. Offsets are stable within one checkpoint epoch and
are never reused or densely renumbered while WAL/overlay state remains. A
checkpoint containing effective detach tombstones atomically starts a new
epoch: deleted old offset `d` maps to no row, while surviving old offset `x`
maps to `x - count(deleted offsets < x)`. The old domain covers checkpointed
rows plus every assigned overlay insert slot, including deleted holes and
reinsertion tails; implementations represent the map as sorted deleted
offsets plus rank, never a row-count-sized array. Node groups and both CSR
directions publish the same remapped epoch in one catalog save. Every offset
stored in a CSR group must be less than the endpoint table's total
checkpointed row count for that catalog epoch; a violation is corruption.

**CSR grouping.** The fwd direction is partitioned by the from-table's
node groups: `fwd[i]` is one **CSR group** holding every edge whose source
node lives in from-table node group `i`. Symmetrically, `bwd[i]` holds
every edge whose destination lives in to-table node group `i`. A CSR group
with zero edges is never written — absence (a `0` entry or a short array
in the catalog) is the only spelling of empty.

A CSR group is one directory page plus per-array payload runs, mirroring
node groups. All integers little-endian.

**CSR directory page:**

| Offset | Size | Field | Notes |
|--------|------|-------|-------|
| 0 | 4 | magic | ASCII `RCSR` |
| 4 | 4 | `row_count` (u32) | covered node slots; ≥ 1 |
| 8 | 4 | `edge_count` (u32) | edges in this CSR group; ≥ 1 |
| 12 | 4 | `column_count` (u32) | property columns; 0 allowed |
| 16 | 16 × (2 + `column_count` + `b1_rescore_count`) | array entries | offsets, neighbors, main property columns, then derived b1 rescore entries |

Each array entry is the node-group column entry shape: `first_page` (u64),
`byte_len` (u32), `crc32c` (u32), naming a contiguous payload run. The
directory must fit one page. Remaining directory bytes are zero.

**Arrays:**

1. **Offsets** — `(row_count + 1)` × u32. Node slot `s` of the covered
   endpoint group owns edge indices `offsets[s] .. offsets[s+1]`.
   `offsets[0] = 0`, entries are non-decreasing, and
   `offsets[row_count] = edge_count`.
2. **Neighbors** — `edge_count` × u64: the other endpoint's node offset
   for each edge. In fwd groups these are to-table offsets; in bwd
   groups, from-table offsets.
3. **Property columns** — `column_count` payloads, each encoded exactly
   like a node-group column payload (validity bitmap + values) with
   `row_count` := `edge_count`.
4. **b1 rescore columns** — the derived auxiliary payloads specified in
   [b1 rescore payload runs](#b1-rescore-payload-runs), if any.

`row_count` may be LESS than the endpoint node group's current row count:
node groups grow by tail rewrite, and a CSR group written earlier stays
valid — slots at or beyond its `row_count` simply have no edges. Within a
node slot, edges appear in insertion order (WAL log order); checkpoint
merges keep existing edges first, then append new edges in log order.
Like node groups, encoding is deterministic: identical logical content
always produces identical bytes.

**Durability and the delta overlay.** The on-disk main-file format is pure
node groups and CSR. New node rows and relationship edges remain in a
transaction-local write set until commit. They become durable when their
transaction's payload records and final commit record have been appended to
the WAL and the WAL has been fsynced; an autocommit statement performs that
commit before returning. The committed rows and edges are then published in
the in-memory delta overlay until checkpoint.

Relationship WAL records retain the compact JSON shape shown in the WAL
payload registry below. Their `from` and `to` offsets are resolved from the
statement's primary keys at commit time under the commit lock, after preceding
node inserts in the same transaction have been assigned offsets. Checkpoint
merges committed rows by writing fresh node groups and committed edges by
rewriting exactly the CSR groups whose edge sets changed onto fresh pages.
Readers overlay committed, uncheckpointed data on node groups and CSR during
scans and expands.

## MVCC and page immutability

Checkpointed node groups and CSR groups carry no per-row version metadata in
v1. Visibility is at group granularity through catalog copy-on-write: a group
is visible exactly in the decoded catalog versions whose storage maps list its
page id. The node-group directory's u32 at offset 12 is the directory-flags
word (§ node groups); a future feature-flagged delete-vector entry, when
deletes add version metadata, will claim a directory-flags bit and ride an
extension section like any other directory extension.

Every data page listed by any catalog version is immutable. A node-group tail
rewrite or CSR merge writes fresh pages; it never modifies the published group
in place. Catalog persistence likewise writes a fresh catalog page. Superseded
node-group, CSR, and catalog pages retire into the free-page ledger under
`FREE_PAGES` (§ Superblock, `docs/FREE_PAGES.md`) and are reused only past the
pin horizon; on a file without the bit they are leaked exactly as before. The
two superblock slots and free-page ledger pages are the only pages rewritten
in place.

Free-page reclamation never reclaims a data page while any in-process
snapshot's decoded catalog version references it: the pin horizon is keyed on
each snapshot's BASE CATALOG GENERATION (the `checkpoint_lsn` of the
publication that produced its catalog — never the snapshot's commit LSN,
which is always ≥ the generation and would un-pin pages the pinned catalog
still names), and an entry is eligible only when a strictly later publication
is durable, which covers the superseded superblock slot's recovery window.

## WAL sidecar

A sequence of records, each:

| Offset | Size | Field |
|--------|------|-------|
| 0 | 4 | `len` (u32) — payload length |
| 4 | 4 | `crc32c` (u32) — over lsn + payload |
| 8 | 8 | `lsn` (u64) — strictly increasing |
| 16 | len | payload |

The v1 payload registry is canonical compact `serde_json` (no insignificant
whitespace):

| Record | Canonical payload |
|--------|-------------------|
| Node insert | `{"table":"<name>","row":[<tagged values>]}` |
| Relationship insert | `{"rel":"<name>","from":<u64>,"to":<u64>,"values":[<tagged values>]}` |
| Node update | `{"update":{"table":"<name>","row":[<tagged values>]}}` — full replacement row addressed by the primary-key value it carries; legal only under the `DML_WAL` feature bit (§ Feature flag registry) |
| Relationship endpoint delete | `{"rel_delete":{"rel":"Knows","endpoint":"from","offset":7}}` — endpoint-role tombstone; legal only under the `REL_TOMBSTONE_WAL` feature bit (§ Feature flag registry) |
| Node delete | `{"delete":{"table":"<name>","key":<tagged value>}}` — tombstones the row with that primary-key value; legal only under the `DML_WAL` feature bit |
| Create node table | `{"ddl":{"create_node_table":<NodeTableSchema JSON>}}` |
| Create relationship table | `{"ddl":{"create_rel_table":<RelTableSchema JSON>}}` |
| Commit | `{"commit":{"records":<u64>}}` |

The two schema values use the exact catalog schema serialization, including
catalog field order and logical-type representation. Payload kind is
discriminated by exactly one of the mutually exclusive top-level keys
`table`, `rel`, `ddl`, `commit`, `update`, `rel_delete`, and `delete`; an
object with none or more than one is corruption. A `rel_delete` object has
exactly the fields `rel`, `endpoint`, and `offset`, in that order.
`endpoint` is exactly `"from"` or `"to"`, and `offset` is a u64. Unknown
fields, an unknown endpoint role, an unknown relationship table, a role whose
schema endpoint is not the node table being detached, or an offset outside
that endpoint table's physical domain is corruption during recovery.

Transactions use the following framing rules:

1. All payload records for one transaction are contiguous and its commit
   record is last. Uncommitted work writes no WAL records.
2. A commit's `records` is the number of preceding payload records in its
   transaction, excluding the commit itself, and is at least 1. Empty
   transactions write nothing.
3. Payload order is canonical: DDL in statement order; then node inserts with
   table names in lexicographic order and statement order within each table;
   then relationship inserts under the same ordering rule; then node updates;
   then relationship endpoint deletes; then node deletes (statement order).
   Relationship endpoint deletes use relationship names in lexicographic
   order; within one relationship, detach statement order is preserved and
   `from` precedes `to`; duplicate `(rel, endpoint, offset)` claims are folded.
   Within one transaction a primary key appearing in both an update and a
   delete resolves to the delete — the transaction's final intent. Node delete
   remains the last payload phase.
4. Recovery accumulates payloads through the next commit record. That commit
   record's LSN is the group's commit LSN and the begin-LSN stamped on every
   record in the group. A trailing group without a commit is silently
   discarded as an in-flight transaction.
5. A commit whose `records` differs from the accumulated group size is
   corruption, reported with the commit record's LSN.
6. A complete group whose commit LSN is less than or equal to
   `checkpoint_lsn` is skipped as already materialized. Replay applies later
   groups oldest-first.

The envelope reader stops at the first invalid CRC or truncated record (a torn
tail after a crash is normal and is silently discarded). Checkpoint applies
complete committed groups to the main file, writes the newest materialized
commit LSN as `checkpoint_lsn` in the alternate superblock, then truncates the
WAL. Transaction-level skipping covers a crash after the superblock write but
before WAL truncation.

## Quantized vector element encodings

These encodings are part of the format before the format_version 1 freeze.
They use safe 8-bit quantization and aggressive RaBitQ-family 1-bit candidates
followed by oversampling and rescoring.

The existing `Vector(dim)` catalog type and payload remain f32. A quantized
column instead uses the following `ty` value in its catalog column record;
`D` is a u32 dimension, `S` is a u64 seed, and object fields appear in the
order shown:

| Encoding | Canonical catalog `ty` value |
|---|---|
| `f16` | `{"VectorEncoded":{"dim":D,"encoding":{"kind":"f16"}}}` |
| `i8` | `{"VectorEncoded":{"dim":D,"encoding":{"kind":"i8","scale":"per_vector_f32_le","offset":"per_vector_f32_le"}}}` |
| `b1` | `{"VectorEncoded":{"dim":D,"encoding":{"kind":"b1","rotation_seed":S,"rescore":"none"|"f16"|"i8"|"f32"}}}` |

`VectorEncoded` has the same logical value type as `Vector`; it records the
physical element encoding per column. The i8 `scale` and `offset` fields name
their fixed per-vector metadata locations rather than catalog-wide numeric
values. A b1 rotation seed is fixed for the lifetime of its column. A writer
must not change any of these fields without rewriting every payload for that
column.

This catalog spelling is also the compatibility gate. Pre-freeze readers know
the closed `LogicalType` variant set but not `VectorEncoded`, so catalog
deserialization rejects the unknown variant before a node-group or CSR payload
can be interpreted as f32. These encodings therefore need no superblock
feature bit: they are in the core format_version 1 catalog vocabulary from its
first release.

### Quantized value slots

The validity bitmap remains the first part of a column payload. Its values
section contains one fixed-size slot per row in row order:

| Encoding | One non-null vector slot |
|---|---|
| `f16` | `D × u16` binary16 elements, each little-endian |
| `i8` | `scale` (f32 LE), `offset` (f32 LE), then `D × i8` codes |
| `b1` | `ceil(D / 8)` packed sign bytes |

Thus a quantized column payload's exact main-run length is
`ceil(row_count / 8) + row_count × slot_size`. Null slots, including i8
metadata and b1 padding, are all zero. The last b1 byte stores dimensions
LSB-first: bit `i % 8` represents dimension `i`, and unused high bits are
zero. In a non-null b1 slot, set means the rotated value is non-negative
(zero included) and clear means negative.

**f16.** Conversion from binary32 follows IEEE-754 round-to-nearest,
ties-to-even. Signed zero is preserved. Binary16 subnormals are supported;
finite magnitudes whose correctly rounded binary16 result overflows are
encoded as signed infinity. Database value ingress rejects non-finite input,
so encoders never receive a binary32 infinity or NaN.

**i8.** Quantization is symmetric about a per-vector offset. For a non-empty
vector let `minimum` and `maximum` be its extrema:

```
offset = minimum / 2 + maximum / 2
radius = max(abs(minimum - offset), abs(maximum - offset))
scale  = radius / 127
code[i] = clamp(round_ties_to_even((value[i] - offset) / scale), -127, 127)
decoded[i] = code[i] * scale + offset
```

The split sum used for `offset` avoids overflowing on a range spanning large
finite binary32 values. Code -128 is reserved and is corruption on read. For
a constant vector, `scale` is positive zero, `offset` is that constant
(preserving signed zero), and every code is zero. An empty vector has positive
zero for both metadata fields. A zero scale with a nonzero code, a negative or
non-finite scale, or a non-finite offset is corruption.

### b1 rotation and asymmetric scoring

The b1 transform is byte-stable and requires only the catalog seed. Every
arithmetic value and operation below is binary32. Let `x` contain the D input
elements and initialize a SplitMix64 state to `S`. One SplitMix64 output is:

```
state = state + 0x9e3779b97f4a7c15          (wrapping u64)
z = state
z = (z xor (z >> 30)) * 0xbf58476d1ce4e5b9 (wrapping u64)
z = (z xor (z >> 27)) * 0x94d049bb133111eb (wrapping u64)
output = z xor (z >> 31)
```

The exact forward construction is:

1. Consume D outputs. Negate `x[i]` iff output `i` has low bit 1.
2. Perform four rounds. At the start of each round create
   `p = [0, 1, ..., D-1]`; for `i` descending from `D-1` through 1, consume
   one output, set `j = output mod (i+1)`, and swap `p[i]` with `p[j]`.
3. Permute into `t[k] = x[p[k]]`. For adjacent pairs `(a,b)` replace them,
   in that order, with `((a+b)c, (a-b)c)`, where
   `c` is the binary32 value with bits `0x3f3504f3`. If D is odd, the final
   unpaired value is unchanged. The round output becomes the next round's
   input.
4. Pack the signs of the fourth-round output using the b1 slot rule above.

Each signed diagonal, permutation, and normalized pair butterfly is
orthogonal apart from binary32 rounding. Dequantization maps bits to `+1.0`
or `-1.0`, reverses the four rounds (the pair butterfly is its own inverse,
then the permutation is inverted), and finally reapplies the initial signs.

The asymmetric cosine-distance estimate used to choose oversampled candidates
does not quantize the query. Rotate the f32 query by the same construction,
decode the candidate bits to signs `s`, and compute in f64 accumulators:

```
similarity_estimate = clamp(dot(rotated_query, s)
                            / (norm(query) * sqrt(D)), -1, 1)
distance_estimate = 1 - similarity_estimate
```

An empty or zero-norm query is invalid. This estimate chooses candidates only;
the selected candidates are reranked from the configured rescore
representation.

### b1 rescore payload runs

`rescore:"none"` has no auxiliary storage. Otherwise every node-group or CSR
directory containing the b1 column has one additional 16-byte payload entry
for it. Main property entries remain first in catalog declaration order;
rescore entries follow them in catalog declaration order for only those b1
columns whose `rescore` is not `none`. `column_count` continues to count
logical property columns, and directory capacity checks include the derived
rescore-entry count.

Each rescore entry names its own contiguous payload run and uses the same
`first_page`, `byte_len`, and CRC-32C fields as a main entry. It is a complete
standalone column payload—validity bitmap followed by f16, i8, or f32 vector
slots as named by `rescore`—so a reader can omit the entire run from its I/O
and memory budget. Its row count and null positions must match the b1 main
run; disagreement is corruption. The b1 main run never embeds rescore bytes.

Scans expose dequantized f32 vectors: f16 and i8 columns decode their main
runs, a b1 column with a rescore run decodes that rescore representation, and
`rescore:"none"` decodes the inverse-rotated sign vector from the b1 main run.

## Compatibility rules

**FROZEN at `format_version` 1. These rules are
permanent for every v1 release and become the public contract with the
`v0.1.0` tag.** `SUPPORTED_FORMAT_VERSION` is 1 and every rule below is
already enforced mechanically by `crates/devondb/tests/format_freeze.rs` plus
the golden corpus, not merely documented here. The pre-freeze v0 files in
`tests/golden/` stay in the corpus permanently and must keep opening forever;
re-minting one at the current version would destroy that compatibility anchor.

**Geo is INSIDE the freeze**: the
`GeoPoint` catalog spelling, the geo column payload layout, and the
`GEO_COLUMNS` feature bit (4, not read-safe) are all part of v1 and carry the
permanent promise. DevonGrid's cell-index profile is likewise pinned
(`docs/GEO.md` § 3).

Permanent as of format_version 1:

1. **Reading old files always works.** A devondb binary at format_version N
   opens every file with `format_version ≤ N`. Upgrades never require
   export/reimport. The golden corpus enforces this rule mechanically.
2. **`min_reader_version` only rises on breaking layout change**, and rising
   it requires a major devondb release plus an in-place upgrade path from the
   previous format.
3. **Additive changes use feature flags.** A flag bit is either *read-safe*
   (old readers may ignore it and still read correctly — e.g. an optional
   index) or *read-unsafe* (changes interpretation of existing bytes; also
   requires `min_reader_version` bump). The flag registry lives in this file.
4. **Newer files fail cleanly.** A reader seeing `min_reader_version` above
   what it understands reports its own version, the file's version, and the
   upgrade path — never a parse error, never silent misreads.

### Feature flag registry

| Bit | Name | Read-safe? | Since |
|-----|------|-----------|-------|
| 0 | `HNSW_INDEX` | yes | pre-freeze; `docs/HNSW.md` §8 |
| 1 | `ONTOLOGY` | yes | pre-freeze; claimed from `RESERVED_READ_SAFE_1`; `docs/ONTOLOGY.md` |
| 2 | `PINNED_PLANS` | yes | pre-freeze; claimed from `RESERVED_READ_SAFE_2`; `docs/NL.md` § 9 |
| 3 | `ZONE_MAPS` | yes | pre-freeze; claimed from `RESERVED_READ_SAFE_3`; `docs/SCALE.md` §1 + §4, § node groups `ZONE_MAP_STATS` |
| 4 | `GEO_COLUMNS` | no | pre-freeze; `docs/GEO.md` §5 |
| 5 | `FREE_PAGES` | yes | post-freeze extension claimed from `RESERVED_READ_SAFE_5`; `docs/FREE_PAGES.md` |
| 6 | `DML_WAL` | no | post-freeze checkpoint-scoped extension; `docs/UI.md` §12.3 |
| 7 | `SCALAR_TYPES_V2` | no | post-freeze extension; set at catalog save iff any table carries a scalar-v2 column, with column layouts below |
| 8 | `MULTIPROCESS_COORDINATION` | no | post-freeze sticky extension; `docs/MULTIPROCESS.md` |
| 9 | `MULTIPAGE_CATALOG` | no | post-freeze extension; § Multi-page catalog |
| 10 | `REL_TOMBSTONE_WAL` | no | post-freeze checkpoint-scoped relationship endpoint-tombstone WAL; `docs/DETACH_DELETE.md` |
| 13 | `COLUMN_ENCODINGS` | no | post-freeze per-payload values-section encodings; set iff any node group carries a non-plain payload; `docs/SCALE.md` §8 |
| 12 | `RESERVED_READ_SAFE_12` | yes | replacement reserve; governs node-group directory-flags bit 2 as the opaque skip fence, with no section semantics assigned; bit 10 is `REL_TOMBSTONE_WAL` and bit 11 is the unknown-bit fixture's relocation target |
| 14 | `HNSW_MUTATION_WAL` | no | checkpoint-scoped interpretation fence for updates/deletes on HNSW-indexed tables |

Acceptance law: a reader accepts a set bit it fully SUPPORTS, or one in
the READ-SAFE mask (opening read-only when read-safe but unsupported).
A set bit outside both masks is an unknown future feature: refuse the
file entirely. `GEO_COLUMNS` is the first supported-but-not-read-safe
bit: it is set iff any node table carries a `GeoPoint` column, derived
at catalog save. It is NOT read-safe because interpreting a geo column
payload requires the codec — a binary without it cannot even decode the
catalog's `GeoPoint` `ty` spelling, so the clean refusal is the honest
outcome.

`ONTOLOGY` is set iff the catalog carries an `ontology` section (below).
It is read-safe by construction: the catalog decoder tolerates the unknown
top-level key (binaries predating the bit decode and read normally), and
the read-only law below is exactly what prevents such a binary from
re-saving a catalog and silently dropping the section.

`ZONE_MAPS` is set by the writer whenever it writes a stats-bearing
node-group directory (`ZONE_MAP_STATS`, § node groups) and persists once
set; it fences directory-flags bit 0 under the acceptance law. It is
read-safe by `docs/SCALE.md` §1: stats are derived data — a
build that ignores them reads correctly — and the read-only law below is
exactly what prevents an unsupporting binary from writing groups without
stats or letting stats go stale. The bit promises nothing per group;
presence is per-directory via the flags word.

`DML_WAL` is the first post-freeze feature-bit extension, and the first
with a **checkpoint-scoped lifetime** (`docs/UI.md` §12.3): it is set in
the superblock by the first transaction since the last checkpoint whose
WAL group contains a node update or delete record, and cleared by the
checkpoint that truncates the WAL. A cleanly checkpointed file therefore
never carries it — data at rest is ordinary node groups, readable by any
format-v1 binary. It is NOT read-safe: a binary predating the bit cannot
correctly recover a WAL holding update/delete records (recovery would
either fail on the unknown discriminator or, worse, be presumed partial),
so a crashed-with-pending-DML file must be refused entirely by such a
binary. Binaries carrying this registry entry recover normally.

`REL_TOMBSTONE_WAL` has the same **checkpoint-scoped lifetime** as
`DML_WAL`: it is set when a WAL group contains a `rel_delete` record and is
cleared only after checkpoint has published the materialized catalog and
truncated or rotated away every governed record. It is NOT read-safe because
a binary without the closed `rel_delete` discriminator cannot recover the WAL
correctly. A public detach-delete commit sets bits 6 and 10 together through
the dual-slot flag-only protocol before writing either governed record; a
transaction containing only internal relationship tombstones still requires
bit 10. A writable reopen that observes bits 6|10 with no governed WAL records
clears both through the same both-slot flag-only publication before returning;
a read-only open performs no healing write. A binary without runtime decode
support keeps bit 10 outside its supported and read-safe masks, refusing a file
that carries it before reading WAL bytes.

`FREE_PAGES` (claimed from `RESERVED_READ_SAFE_5`; design
`docs/FREE_PAGES.md`) is set by the first publication that writes
a free-page retirement ledger and is sticky from then on. It is read-safe
by the derived-data argument: the ledger and superblock extension are
reachable only from the extension region readers never load, so a binary
that ignores them reads correctly — and because bit 5 was already in
every shipped binary's read-safe mask, v0.1.1 binaries open FREE_PAGES
files read-only. That read-only law is exactly what protects the
ledger roots: a pre-bit writer would zero the extension region on its
next slot write, and the acceptance law forbids it from writing at all.

`RESERVED_READ_SAFE_12` continues the §8.1 pattern (`docs/HNSW.md`) that
bits 1, 2, 3, and 5 each carried before being claimed. Writers
do not emit it, but it governs node-group directory-flags bit 2 as the
permanent opaque-section fence: a reader that sees the feature opens the
file read-only, bounds-checks the bit-2 frame, and skips its payload without
checking its CRC or assigning it meaning. This keeps both the
read-safe→read-only machinery and directory skip-by-length behavior
permanently exercisable end to end; the permanent read-only-law fixture
tracks the reserved slot's stable Rust constant (`RESERVED_READ_SAFE_FLAG`)
and moves with it automatically.

`HNSW_INDEX` is set iff the catalog contains at least one `indexes` entry
(below). It is read-safe: base node-group and CSR bytes keep their
interpretation, and index pages are unreachable from base table storage.

`SCALAR_TYPES_V2` is set by `Catalog::save` iff any table carries a
`Timestamp`, `Bytes`, `Decimal(p, s)`,
or `Json` column, derived at catalog save — the `GEO_COLUMNS` mechanism
exactly. It is NOT read-safe for the same reason
`GEO_COLUMNS` is not: a binary predating the bit cannot even decode the
catalog's new `ty` spellings, so the clean refusal is the honest outcome.
Persisted encodings: `Timestamp` fixed 8-byte i64
epoch-microseconds UTC; `Bytes` var-length via the String heap framing;
`Decimal(p, s)` fixed 16-byte little-endian i128 unscaled digits, scale
from the catalog type; `Json` var-length canonical UTF-8 text via the
String heap framing. The schema floor admits these types on NODE tables
and rejects them on relationship tables (`schema.rs` `validate_columns`;
the rel-side codec is a later addition). Upsert deliberately claims no bit and
no WAL
discriminator: the facade lowers it to insert + update records in
existing `DML_WAL` vocabulary at execute time, so files written through
upsert stay recoverable by every binary that recovers DML today.

`MULTIPROCESS_COORDINATION` has a **sticky lifetime**: it is set by the
one-time offline activation operation and is never cleared; checkpoints
preserve it. It is NOT read-safe because a binary predating the bit does not
take the publication gate. Its unsynchronized superblock and WAL reads can
therefore race a checkpoint or writer — exactly the races the bit exists to
exclude — so clean refusal is the honest outcome. Coordination uses persistent
sidecars derived from the canonical main-file identity:
`<canonical-main>.lock-writer` for the writer lease and
`<canonical-main>.lock-publish` for the publication gate. They contain no
durable database state, are never unlinked during normal operation, and may be
recreated safely when absent. Legacy binaries ignore them, which is why this
feature bit, not the mere presence of lock files, is the compatibility fence.
Activation acquires both locks nonblockingly, recovers through the normal
writable open path, persists the bit through the dual-slot flag-only protocol,
and checkpoints the file clean. It cannot detect or evict an already-open
legacy process that does not participate in the lock protocol, so the operator
must ensure activation is truly offline with respect to legacy binaries.

`MULTIPAGE_CATALOG` is set iff the published catalog payload exceeds one
page (§ Multi-page catalog), derived at catalog save in the same
superblock publication as the catalog it governs — a catalog shrinking
back under one page clears it. It is NOT read-safe: the flag itself is
what tells a reader that `catalog_root` names a directory page rather
than a v1 payload page, so a binary predating the bit cannot locate the
catalog at all and the clean refusal is the honest outcome.

**Read-safe means read-only, not ignore-and-overwrite.** A reader that
recognizes a flag as read-safe but does not fully support it MUST open the
file in read-only mode: reads use base storage and ignore the feature's
metadata, while writes and checkpoints are refused (reserializing the
catalog could drop metadata the reader does not understand, and writes
would silently invalidate the feature's derived state). In code this is
the `READ_SAFE_FLAG_MASK` / `SUPPORTED_FLAG_MASK` pair in
`superblock.rs`; both change only together with this table.

### Catalog `indexes` entries

A catalog may carry a top-level `indexes` array after `storage`, in index
creation order, omitted entirely when empty. Each entry uses this
canonical compact field order:

```json
{"name":"embedding_cos","kind":"hnsw","table":"Corpus","column":"embedding","root":42}
```

`name`, `table`, and `column` retain the spelling supplied by index DDL and
match by ASCII lowercase folding, with non-ASCII bytes exact. Index names are
unique under folding, as are `(table, column)` tuples under folding. DDL
rejects fold-equal collisions as invalid arguments; a decoded catalog with
either collision is corruption. `root` is the page id of an immutable index
root and is never zero. Metric and topology parameters live in the root page
(`docs/HNSW.md` §3.2), not the catalog. Unknown fields in an entry are
corruption.

### Catalog `ontology` section

A catalog may carry a top-level `ontology` object after `indexes` (after
`storage` when no `indexes` array is present), omitted entirely when no
class or interface has been declared — absence keeps every pre-ontology
catalog byte-identical, including the golden corpus. Presence sets
feature bit 1, `ONTOLOGY`. Design: `docs/ONTOLOGY.md`.

```json
"ontology":{"interfaces":[{"name":"Nameable","columns":[{"name":"name","type":"String"}]}],
"node_classes":[{"table":"Person","display":"Person","plural":"people","label":"name",
"summary":["name","role"],"color":"#7aa2ff","description":"…","implements":["Nameable"]}],
"rel_classes":[{"table":"Knows","verb":"knows","inverse":"is known by"}]}
```

- Field order is canonical as shown; encoding is compact serde_json like
  the rest of the catalog. Within each entry, every field except the
  identifying `table`/`name` is optional and omitted when absent —
  consumers derive defaults (`docs/ONTOLOGY.md` § 2).
- `interfaces`, `node_classes`, and `rel_classes` are each optional
  arrays in declaration order, omitted when empty; at least one must be
  present for the `ontology` object itself to be present.
- All names retain DDL spelling and match by ASCII lowercase folding
  (non-ASCII exact), per the catalog-wide rule. `table` references must
  resolve (folded) to a declared node/rel table; `label` and every
  `summary` entry to a column of that table; every `implements` entry to
  a declared interface whose columns all resolve (folded, with matching
  type) against the table. Interface names are unique under folding, as
  is `table` within each class array. A decoded catalog violating any of
  these is corruption; DDL rejects the same as invalid arguments.
- Unknown fields in any ontology entry are corruption (matching the
  `indexes` rule).

### Catalog `pins` section

A catalog may carry a top-level `pins` array after `ontology` (after the
last present preceding section), omitted entirely when no plan is
pinned — absence keeps every pre-pin catalog byte identical. Presence
sets feature bit 2, `PINNED_PLANS`. Design: `docs/NL.md` § 9; the
product promise is `docs/PLAN_IR.md` § Versioning — a pinned plan
re-executes identically forever.

```json
"pins":[{"name":"ada friends","text":"who does ada know",
"plan":{"v":0,"plan":{…}},"created_lsn":41}]
```

- Entries in pin order. `name` retains its DDL spelling and is unique
  under ASCII folding; `text` is the ORIGINAL input the plan was
  compiled or parsed from (provenance — may be natural language);
  `plan` is the canonical plan JSON as a nested object, stored
  verbatim-canonical and NEVER re-canonicalized on load (its `v` is the
  plan's own pinned version); `created_lsn` is the publication LSN.
- A decoded pin whose `plan` fails plan decoding for its `v`, a
  fold-equal duplicate name, or unknown fields in an entry are
  corruption. Pin execution validates against the CURRENT catalog at
  run time — a pin referencing a since-removed table fails cleanly at
  run, never at load.

### HNSW index pages

These pages are reachable only from catalog `indexes` entries and are enabled
by feature bit 0, `HNSW_INDEX`. All integers are little-endian.

**HNSW root page.** One immutable root occupies exactly one page:

| Offset | Size | Field | Rule |
|---:|---:|---|---|
| 0 | 4 | magic | ASCII `HNSW` |
| 4 | 2 | `layout_version` | `1` for this layout |
| 6 | 1 | `metric` | `0 = l2`, `1 = cosine` |
| 7 | 1 | `navigation_encoding` | `0 = f32`, `1 = f16`, `2 = i8`, `3 = b1` |
| 8 | 2 | `m` | 4..=64 |
| 10 | 2 | `m0` | exactly `2 * m` |
| 12 | 4 | `ef_construction` | `m0..=4096` |
| 16 | 8 | `level_seed` | fixed for the index lifetime |
| 24 | 8 | `covered_rows` | table-offset prefix represented by the index |
| 32 | 8 | `entry_node` | `u64::MAX` when there is no eligible node |
| 40 | 1 | `entry_level` | zero when empty; otherwise at most 63 |
| 41 | 1 | `layer_count` | zero when empty; otherwise exactly `entry_level + 1` |
| 42 | 2 | reserved | zero |
| 44 | 4 | `group_count` | node groups intersecting the covered prefix |
| 48 | 8 | `layer_dir_first_page` | zero only when `layer_count == 0`; otherwise a data page id |
| 56 | 4 | `layer_dir_byte_len` | exact layer-directory payload length |
| 60 | 4 | `layer_dir_crc32c` | CRC-32C over the exact layer-directory payload |

Bytes 64 through the end of the root page are zero. A decoder rejects an
unknown magic, layout version, metric, or navigation encoding; an invalid
parameter range or cross-field relation; a nonzero reserved or tail byte; a
directory length other than `layer_count * group_count * 8`; and a directory
page reference to either reserved superblock page. When `layer_count` is zero,
the directory first page and byte length are zero and the checksum is the
CRC-32C of an empty payload. The entry node, when present, is below
`covered_rows`. `group_count` is zero exactly when `covered_rows` is zero and
cannot exceed `covered_rows`.

Root loading also validates against the catalog version that named it: the
indexed vector column still exists, its physical encoding matches
`navigation_encoding`, the metric/encoding combination is supported, and
`covered_rows` does not exceed the table rows visible in that catalog.

**Layer-directory payload.** The directory is one contiguous payload run
beginning at `layer_dir_first_page`. Its exact length is:

```text
layer_count * group_count * 8
```

The payload is a layer-major matrix of u64 CSR directory page ids. Cell
`layer * group_count + group` names the `RCSR` adjacency for that layer and
base node group. Layers are numbered from zero upward. Zero is the only
spelling of an empty group; every nonzero cell is a data page id. Nonzero
cells name ordinary `RCSR` groups with zero property columns, using the
unchanged CSR format above.

The payload CRC-32C must equal `layer_dir_crc32c`. It occupies
`ceil(layer_dir_byte_len / page_size)` consecutive pages; bytes after the
payload in the final page are zero. A dimension/length mismatch, checksum
failure, nonzero final-page tail, non-contiguous run, or reserved-page
reference is corruption.

### Planned format extensions (registry of intent)

Design block: `docs/SCALE.md`. These features are named here pre-freeze so
v1 ships with the extension story on record; **no bytes change until each
one lands**. Each claims its feature bit at implementation time per the
`ONTOLOGY` precedent (a read-safe feature claims the lowest
`RESERVED_READ_SAFE_*` bit; a not-read-safe feature allocates a new bit),
with exact layouts specified in this file and golden corpus entries added
in the same commit.

| Planned feature | Read-safe? | Mechanism |
|-----------------|-----------|-----------|
| Per-group statistics (zone maps), implemented as `ZONE_MAPS` (bit 3) + `ZONE_MAP_STATS` | yes | extended node-group directory (§ node groups) |
| Per-payload column encodings (constant/RLE/bit-pack+FOR/dictionary/FSST/ALP) — bit 13 + directory-flags bit 1, specified by `docs/SCALE.md` §8 | no — payloads become undecodable without the codec | extended node-group directory (below); CSR encodings remain a separate planned row |
| CSR adjacency encodings (delta + bit-packed neighbor arrays) | no | extended CSR directory (below) |
| `DEVONPACK` read-only container (zstd-framed runs of byte-identical main-file pages; `docs/SCALE.md` §7) | n/a — a second file type by magic, below the page seam; pages inside are unchanged | pager-backend seam (`docs/OBJECT_STORAGE.md`), never a feature bit |

**The extension mechanism.** Directory pages grow *only* through the two
regions today's readers already enforce as zero: the node-group
directory's reserved field (offset 12) becomes a directory-flags word,
and extension sections live in the enforced-zero region after the entry
array (both directories). Because v0 readers reject a nonzero reserved
field and nonzero directory padding as corruption, a v0 file can never
be ambiguous with an extended one, and extended files are additionally
fenced by their feature bits under the acceptance law. These two
rejections are therefore format law, fence-tested pre-freeze,
not incidental strictness.

## Golden corpus policy

`tests/golden/` holds database files produced by every tagged release,
plus a manifest describing each file's expected contents. CI opens and fully
reads every corpus file on every commit. The corpus only grows. A failing
golden test blocks merge with no exceptions — if a change can't read the
corpus, the change is wrong, not the corpus. The additive `FREE_PAGES` fixture
(`free-pages.devondb`) carries retired, reclaimed, and
reused pages with a live ledger spanning two or more pages; every anchor
minted before the bit stays byte-for-byte untouched.

## Not yet specified (v1 roadmap)

- Page header layout for data pages
- Exact layouts for the remaining planned extensions above (column
  encodings, CSR adjacency encodings) — mechanism and read-safety
  classes are pinned in [Planned format extensions](#planned-format-extensions-registry-of-intent);
  bytes are specified when each lands (`docs/SCALE.md` §3)
- Spill-file format for out-of-budget operators under `<db>.tmp/` (temp
  files, explicitly outside the compatibility promise)

### HNSW mutation WAL interpretation fence

`HNSW_MUTATION_WAL` (bit 14) governs ordinary node update/delete WAL records
when their table has an HNSW index in the commit's current catalog. It adds
no payload discriminator, page layout, or version bump. An indexed-table
mutation without the bit is corruption during replay. Writers set it with
the existing both-slot flag publication before appending the governed
transaction, alongside `DML_WAL` and, for detach, `REL_TOMBSTONE_WAL`.

Readers with any indexed-table mutation history in their captured overlay
(including a subsequently reinserted key) use exact KNN until checkpoint.
The base root and configuration still validate. Checkpoint rebuilds affected
indexes from scratch over the materialized/remapped node pages and publishes
rows and roots in one catalog save. A valid fresh partial or empty prefix is
legal under budget pressure, with an exact tail; an old offset-epoch root is
never retained. Only after WAL truncation may bits 6, 10 and 14 be cleared.
A crash can leave set bits without governed records; writable open/checkpoint
heals them after draining obsolete WAL, while read-only handles never heal.
