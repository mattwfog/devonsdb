# Free-page management — checkpoint CoW reclamation

## Evidence and scope

Without free-page management, append-only allocation leaked every superseded
page. `Pager::allocate_page` used the file length as its allocation cursor,
and superseded node-group, CSR, and catalog pages were never reused. Every
publication retires pages:

- **Checkpoint** rewrites the tail node group and merged CSR groups onto
  fresh pages and writes a fresh catalog page (MVCC.md §6 steps 2-3) —
  the superseded tail group, superseded CSR groups, and the previous
  catalog page all retire, every checkpoint.
- **HNSW** publication rewrites changed CSR cells, the layer directory,
  and the root onto fresh pages (HNSW.md §11) — per checkpoint
  and per COPY build batch, whose intermediate batch roots are unreachable
  by construction.
- **COPY** on error or SIGKILL leaks the entire prospective generation
  (groups plus every index build page) — the documented policy
  INDEX_BULK_LOAD.md accepts "until free-page management exists."
- **Multi-page catalog** spill chains (`MULTIPAGE_CATALOG`) retire
  wholesale on every catalog save that rewrites them.

Checkpoint-heavy write bursts can otherwise produce unbounded physical file
growth even when live data remains roughly constant. Edge storage cannot
assume large amounts of spare capacity, and unchecked copy-on-write garbage
can exhaust a shared host.

Scope: reclaim pages within the file for reuse by later allocations.
File truncation (shrinking) and offline compaction are follow-ups built
on the same inventory (§ The backlog); neither gates this design.

## Recommendation

**Epoch-based retirement with a durable oldest-first ledger, reuse-first
allocation, and pin-gated reclamation** — as a post-freeze feature-bit
extension, no format-version bump.

Three separations do all the work:

1. **Retirement is bookkeeping, not freeing.** The publication that
   makes pages unreachable (checkpoint, COPY, HNSW publish) knows
   exactly which page ids it superseded — the old catalog's storage
   maps and index roots versus the new one's. It appends
   `(page_id, retired_lsn)` entries to a durable ledger in the same
   publication. Retirement never asserts the pages are reusable — only
   that no catalog at or after `retired_lsn` references them.
2. **Reclamation is a comparison.** An entry is *reclaimable* when
   `retired_lsn < min_pin`, where `min_pin` is the oldest catalog
   generation any live snapshot can still be reading (§ The pin
   horizon). Because retirement LSNs are appended monotonically, the
   reclaimable entries are always a PREFIX of the ledger — eligibility
   is a head check, O(1), no scan.
3. **Allocation prefers the ledger head.** `allocate_page` pops a
   reclaimable entry and hands back its page id (zeroing the page);
   only an empty-or-ineligible head falls through to today's append.
   Append-only behavior is therefore the natural degraded mode — a
   conservative `min_pin` never breaks anything, it just leaks less
   aggressively than the optimum.

### The pin horizon (`min_pin`)

`min_pin` is the minimum over:

- **In-process snapshots.** The pin is each live snapshot's BASE
  CATALOG GENERATION — the `checkpoint_lsn` of the `PublishedState` it
  pinned at open — recorded for read snapshots and write transactions
  alike; the in-process component of `min_pin` is the minimum over
  those generations. The existing `write_txns` minimum over snapshot
  LSNs is not a safe substitute. A snapshot's LSN is always ≥
  its base generation: T at snapshot LSN 150 over base generation 100,
  with a checkpoint at LSN 120 retiring the gen-100 pages, gives a
  snapshot-LSN minimum of 150 — making those pages eligible
  (120 < 150) while T's catalog still names them. The registry records the
  pinned generation, never the snapshot LSN. This is
  the only component needed for a single-process database.
- **The superseded superblock slot.** Recovery arbitration can choose
  the non-authoritative slot only until the flip that supersedes it is
  durable (FORMAT.md § Dual-slot protocol: readers take the valid slot
  with the higher LSN). Conservatively, a generation retired at LSN L
  becomes eligible only after a LATER superblock publication is durable
  — a one-generation delay that removes any reasoning about torn-write
  windows. Concretely: entries appended by the publication at LSN L are
  reclaimable no earlier than the publication at LSN > L.
- **Cross-process followers.** A follower's pinned snapshot has
  no lifetime bound (MULTIPROCESS invariants 2 and 5), so the writer
  must be able to SEE follower pins. Each follower maintains a **pin
  file** beside the existing lock sidecars (the suffix-derived
  `LockPaths` family, lock.rs:27-35; pin files live in one
  suffix-derived directory so the writer's scan is a bounded readdir):
  an OS-locked file holding the oldest catalog `checkpoint_lsn` that
  follower may still be reading. The pin file's lock is
  acquired at creation and NEVER dropped until process exit — every
  rewrite happens under the held lock; the pin value advances only
  AFTER the follower has released every snapshot of the older
  generation (advance-after-release); and a LOCKED pin file that is
  empty or unparseable pins EVERYTHING (the conservative floor — it
  covers the create-to-first-write window). Liveness is the held lock,
  never names or pids — a crashed follower's pin file unlocks through
  the OS and is ignored, exactly the invariant-8 philosophy. The writer's
  scan unlinks unlocked stale files as it goes, keeping the readdir bounded
  by live followers, and
  a follower whose file vanished recreates and relocks it on its next
  refresh. The writer computes the follower component of `min_pin`
  by scanning pin files under the exclusive publication gate it already
  holds at every publication. Persistent reader-registry bytes require a
  registered feature bit, an exact FORMAT.md layout, and a golden-corpus
  entry. A file with `FREE_PAGES` set but multiprocess
  coordination never activated has no followers and skips this
  component entirely.

## On-disk layout

### Superblock extension (bytes 64..92 of each slot — 64–91 inclusive + CRC)

The extension region below the 64-byte header is zero today, fence-tested
(`format_freeze.rs`), read by nobody (`read_slot` reads exactly 64
bytes), and deliberately claimable by a feature bit (FORMAT.md
§ Extension region). `FREE_PAGES` claims:

| Offset | Size | Field | Notes |
|--------|------|-------|-------|
| 64 | 8 | `retire_ledger_head` (u64) | page id of the oldest ledger page; 0 = empty ledger |
| 72 | 8 | `retire_ledger_tail` (u64) | page id of the newest ledger page (append target); 0 = empty |
| 80 | 8 | `retired_total` (u64) | cumulative entries ever appended, strictly monotone; live entries derive as `retired_total` minus cumulative reuse, where cumulative reuse is recoverable by walking the churn-bounded ledger |
| 88 | 4 | `extension_crc32c` (u32) | CRC-32C over bytes 64–87 |

Bytes 92+ of the extension region stay writer-zeroed and
future-claimable — the zero-fence law continues to cover every byte
this table does not claim.

The slot CRC still covers bytes 0–59 only; the extension carries its
own CRC because the frozen format keeps `read_slot` unchanged for
non-FREE_PAGES readers. **Slot arbitration is UNCHANGED — header CRC +
LSN only**, because readers never validate the
extension (FORMAT.md § Extension region). A FREE_PAGES-supporting
binary validates the extension CRC of the slot arbitration CHOSE before
trusting the ledger fields; a mismatch is NOT `Corrupt` and NOT a slot
flip (arbitration diverging between binary generations would be its own
corruption class) — it is **degraded mode**: the ledger is treated as absent,
allocation appends as today (leak-only, sweepable), and the next
publication rewrites a valid extension. A torn extension therefore
costs at most one ledger chain, never readability.

### Ledger pages

A ledger page is an ordinary allocated page:

| Offset | Size | Field |
|--------|------|-------|
| 0 | 8 | magic `DEVONFPL` |
| 8 | 8 | `next_page` (u64; 0 = tail) |
| 16 | 4 | `entry_count` (u32) |
| 20 | 4 | `consumed_count` (u32) — entries before this index are already reused |
| 24 | 4 | `crc32c` over bytes 0–23 and the entry array |
| 28 | 4 | reserved (zero) |
| 32 | 16×n | entries: `(page_id u64, retired_lsn u64)`, append-ordered |

Entries are appended at publication time; `consumed_count` advances as
the allocator reuses entries, rewriting the ledger page in place.

**Consumption durability law:** the in-place `consumed_count` (+ CRC)
rewrites covering every entry consumed since the last publication are
written and synced BEFORE the superblock flip of the publication whose
catalog references the reused pages — the same before-the-flip
discipline step 2 already imposes on entry appends. A crash BEFORE that
flip re-offers the same entries on reopen, which is safe precisely
because the catalog that referenced the reused pages was never
published (recovery arbitration cannot reach it); a crash AFTER the
flip finds the consumption durable. Without this ordering, reopen
could re-offer a page id that a durable catalog now names, causing
double allocation.

A fully consumed page is a retirement candidate of the next publication
(the ledger eats its own tail — its pages recycle through the same
mechanism, so ledger overhead is bounded by churn, not history), under
one ordering rule: **a ledger page never carries its own retirement entry.**
A fully consumed HEAD is unlinked (head advances to `next_page`) in the
publication that retires it, and its entry lands in a DIFFERENT page. When
head == tail and the page is fully consumed, the publication starts a fresh
tail page FIRST (from another eligible
ledger entry if one exists, else file append), then retires the old
page into it.

Ledger pages are the ONE exception to data-page immutability, which is
sound because no catalog, snapshot, or golden anchor ever references a
ledger page — they are reachable only from the superblock extension,
and only FREE_PAGES writers read them. **All ledger I/O goes through
the standard pager read/write path.** MVCC.md §7.3 rule 3 (write-
through: `write_page` removes the cached frame) keeps a cached ledger frame
coherent across in-place rewrites. Bypassing that path is forbidden because
it can expose a stale frame. Ledger pages join catalog pages and superblocks
as the only pages rewritten in place.

### Feature bit

`FREE_PAGES` is read-safe: a reader that ignores the ledger reads
correctly (the `ZONE_MAPS` derived-data argument), and the read-only
law is what prevents such a reader from re-saving a superblock — which
is also exactly what protects the ledger roots from a pre-bit writer's
zero-fence. Per the FORMAT.md extension law — "a read-safe feature
claims the lowest `RESERVED_READ_SAFE_*` bit" — **`FREE_PAGES` claims
bit 5 (`RESERVED_READ_SAFE_5`), not a new bit.** A new bit would sit outside
older binaries' SUPPORTED and READ_SAFE masks and cause them to refuse the
file entirely. Bit 5 is already in the v0.1.1 read-safe mask, so those
binaries open FREE_PAGES files read-only and refuse writes and checkpoints,
which provides the extension's writer gate. `RESERVED_READ_SAFE_12` at bit 12
is the replacement reserve — the lowest bit clear of
DETACH_DELETE's bit 10 and the unknown-bit fixture's
relocation target 11 — keeping the read-safe→read-only machinery
permanently exercisable; the permanent read-only-law fixture tracks
`RESERVED_READ_SAFE_FLAG` and moves with it automatically, and the
bit-10/11 relocation arithmetic now belongs to DETACH_DELETE alone.

The bit is set on the first publication that writes a ledger — existing
files never flip retroactively, every golden anchor stays byte-for-byte
untouched, and a new corpus fixture with the bit set is ADDITIVE.

## Retirement and reclamation sequence

Woven into the existing publication sites; no new publication points.

1. Under the commit/checkpoint lock, the publication computes its
   superseded set: old-catalog storage-map and index-root page ids not
   present in the new catalog, plus the old catalog page itself (and
   old multi-page-catalog chain pages, and consumed ledger pages).
2. Append entries `(page_id, publish_lsn)` to the ledger tail, writing
   new ledger pages from the free head or append as needed — and
   rewrite the `consumed_count` of every ledger page whose entries were
   consumed since the last publication (the consumption durability
   law). ALL ledger writes complete and sync BEFORE the superblock flip
   that references them (same discipline as the catalog page).
3. The superblock publication carries the updated extension fields in
   the same slot write — retirement is atomic with the publication that
   made the pages unreachable. A crash between 2 and 3 leaks the new
   ledger pages themselves (ordinary leak class, recoverable by the
   backlog sweep).
4. `allocate_run(n)` (writer-locked already) serves from the eligible
   ledger prefix (`retired_lsn < min_pin`): it scans at most `n + 4096`
   eligible entries from the head, coalesces them BY PAGE ADDRESS, and
   hands out the physically contiguous run of `n` pages whose last ledger
   position is earliest, zeroing every page; the prefix is consumed
   through that position and every skipped entry ahead of it stays
   eligible in the allocator's deferred pool (served first by later
   allocations, republished at the next flip if still unused). No fit
   appends at file length. Fresh ledger pages for a publication come
   from the head's next eligible entry, then from the deferred pool
   (never the last entry being published), then from the file tail.
   `min_pin` is computed at publication boundaries and cached on the
   pager (a stale-low `min_pin` is conservative; it is never allowed to
   move backward between publications).

   Why address coalescing: a checkpoint
   that rewrites one node group retires ~400 contiguous pages whose
   ledger neighbours are single pages from the same batch, and the
   ledger is a FIFO of such batches; a scan that only recognised runs
   contiguous IN LEDGER ORDER, and could skip at most one ledger page
   of entries, missed the retired generation about half the time and
   appended it instead — a 20,000-row table churned by single-row
   upserts grew 19.4 MB → 85 MB over 150 checkpoints with live bytes
   constant. With coalescing it plateaus at 24.4 MB by cycle 25.
5. COPY error paths queue their known prospective-generation page ids
   in memory; the NEXT successful publication retires them (step 1
   includes the queue). A crash before that publication leaks them —
   unchanged from today's policy, now bounded by the sweep.

## Crash windows

| Crash point | Recovery-visible state | Consequence |
|---|---|---|
| During ledger-page writes, before superblock flip | Old superblock; new ledger pages unreachable | Ledger pages leak (sweepable); no data-page effect |
| After flip | New extension names the ledger; entries' `retired_lsn` all < the NEXT publication, so nothing is reclaimable until a later publication is durable (the one-generation delay) | No reuse can precede durability of the publication that orphaned the pages |
| Torn extension write | Arbitration unchanged (header CRC + LSN); the chosen slot's extension CRC fails → degraded mode, ledger treated absent | One ledger chain leaks (sweepable); reuse remains disabled until a valid extension is published; readability untouched |
| Crash after reuse of P, before the flip publishing the catalog that references new-P | Old superblock; `consumed_count` regressed to pre-reuse | The entry is re-offered on reopen — SAFE: no catalog reachable by arbitration names new-P (the consumption durability law's before case) |
| Crash after reuse of page P and after that flip | New superblock; consumption durable (synced pre-flip) | P is never re-offered; every catalog that referenced old-P was retired ≥ one durable generation ago, and recovery arbitration can no longer select any superblock naming it — no reachable state reads recycled bytes |

The invariant the whole table reduces to: **a page id is handed to
`allocate_page` only when no catalog reachable by recovery arbitration,
no in-process snapshot, and no live follower pin can name it.** Severing
any one of the three `min_pin` components must make a named gate fail
(§ Verification).

## Memory, Pi-class behavior, wear

- O(1) resident state: the ledger head and tail pages pin one frame each
  during an operation and are evictable between; `min_pin` is one u64.
  No in-memory free set, no scan proportional to history.
- Steady-state file size becomes live data + one generation of churn +
  the ledger, which is itself churn-bounded.
- eMMC wear improves twice over: the file footprint stops growing (the
  FTL wear-levels over a smaller LBA range), and reuse writes land on
  pages the allocator hands out anyway. Ledger counter rewrites add one
  in-place page write per publication and per consumption burst —
  measured against the pages NOT appended, this is strictly wear-negative.
- The write-amplification measurement demanded by INDEX_BULK_LOAD.md
  ("the write count must be measured because eMMC wear is a product
  constraint") gains its accounting surface for free: `retired_total`
  minus cumulative reuse (derived by the churn-bounded ledger walk) IS
  the leak inventory, exposed via a stats hook for the doctor tool.

## The backlog (existing stores)

Reclamation at publication fixes future growth; it does nothing for pages
already leaked in files written before the bit or through later crash windows.
The separately specified companion is `devondb doctor --sweep`, an offline
operation that holds the writer lease and walks every page reachable from
both superblock slots' catalogs and the ledger. Every allocated page not reached is appended to
the ledger with `retired_lsn = 0` (reclaimable immediately — nothing can
reference it). A sweep recovers the backlog without export/reimport, keeping
the stable-format promise intact. Truncation, which returns a free tail run to
the OS, layers on the same inventory later; it is
deliberately out of scope here because reuse alone stops the growth
rate, and truncation interacts with cross-process follower reads.

## Format and golden-corpus impact

- `FREE_PAGES` claims bit 5 from `RESERVED_READ_SAFE_5` (the ONTOLOGY/
  PINNED_PLANS/ZONE_MAPS claiming precedent); replacement reserve
  `RESERVED_READ_SAFE_12` allocated at bit 12; acceptance law
  unchanged in shape.
- Superblock extension bytes 64–91 + extension CRC — the first claimant
  of the extension region; `format_freeze.rs`'s zero-fence becomes
  bit-conditional: zero without the bit, exact layout with it.
- Ledger page layout as above (magic `DEVONFPL`).
- Every existing golden anchor unchanged and still opened by the fence;
  one ADDITIVE fixture: a database that has retired, reclaimed, and
  reused pages, plus a ledger spanning ≥2 pages.

## Verification gates

1. Churn workload (insert → checkpoint loop):
   file size plateaus at live + one churn generation; the identical
   workload on a pre-bit build grows linearly.
2. Snapshot safety severed-proof: pin a snapshot, checkpoint until its
   generation's pages would be eligible, assert the pinned scan is
   byte-identical throughout; sever the in-process component of `min_pin` and
   the same test must fail. The
   test also substitutes the unsafe snapshot-LSN minimum for the
   pinned-generation minimum and must fail under it.
3. Follower safety severed-proof (two handles, one process): follower pins a
   snapshot, writer churns far past it, and the follower's scan stays
   identical; sever the pin-file
   component and the gate must fail. Crashed-follower liveness: an
   unlocked stale pin file does NOT hold `min_pin` back.
4. Recovery arbitration: kill between ledger write and flip (leak, no
   corruption), after flip (no entry eligible before a later durable
   publication), torn extension (other slot wins). Reopen after each
   kill point must pass the full read suite.
5. Reuse correctness: allocate → verify zeroed; a reused page serving a
   new group passes the existing CRC/decode gates; accounting balances
   as `retired_total` − live entries (ledger walk) = the allocator's
   reuse counter.
6. Golden fence: every anchor byte-identical; the new fixture opens;
   the read-only path is exercised through the permanent
   `RESERVED_READ_SAFE_FLAG` fixture machinery. Bit 5 is in every shipped
   binary's read-safe mask; the mask constants carry the compatibility
   guarantee.
7. COPY-failure retirement: fail an indexed COPY, run one successful
   publication, assert the prospective generation's pages entered the
   ledger and are reused by the next load.
8. Consumption durability kill-pair: kill between reuse and
   the flip → reopen re-offers the entry and the full read suite
   passes; kill after the flip → the reused page is never re-offered
   (drain the ledger head and assert its absence).
9. Tail-recycle ordering: a single-page fully-consumed
   ledger (head == tail) is retired by the next publication without
   self-reference; the chain stays walkable; the old page is later
   reused without ledger damage.
10. Pin-file hygiene: an unlocked stale pin file is
    unlinked by the writer scan and never holds `min_pin` back; a live
    LOCKED file survives the scan; an empty LOCKED file pins
    everything; a follower whose file was swept recreates it on the
    next refresh.

## Rejected shapes

- **Bitmap allocator (page-per-N-pages bitmap).** O(file size) scan or
  resident bitmap; rewrites hot bitmap pages constantly (wear); the
  ledger's monotone-prefix eligibility does the same job in O(1) with
  entries that carry their own safety proof (`retired_lsn`).
- **Immediate reuse at retirement (no pin horizon).** Corrupts pinned
  snapshots — the snapshot-stability and MULTIPROCESS invariant 5 rules both
  forbid it; the
  FORMAT.md constraint is explicit.
- **Reference counting per page.** Per-page metadata the frozen v1
  format has nowhere to put; count maintenance touches every
  publication path; crash-consistency of counts is a harder invariant
  than the ledger's append-then-flip.
- **Compaction/vacuum instead of reuse.** Rewrites live data, increasing wear
  and taking substantial time on large files; it needs the writer offline or
  a shadow copy, doubling disk use exactly when disk is scarce, and still
  needs the same
  reachability analysis. Kept only as the offline backlog sweep, where
  it is the right tool.
- **In-memory free list rebuilt at open by scanning.** An open-time
  O(file) scan on a large file violates the open-latency
  budget; and it silently re-leaks everything on every crash between
  opens.

## Design decisions

1. **Default-on.** `FREE_PAGES` is set on the first ledger-writing
   publication of any writable open — no staging flag. The bit is read-safe
   on shipped masks, v0.1.1 opens it read-only, and degraded mode matches the
   prior append-only behavior.
2. **Pin files coarsen by generation**: rewrite only when the pinned generation
   changes, not per refresh tick. This avoids a write on every follower poll.
3. **Sweep is a separate operation.** It recovers existing leaked pages
   without rebuilding the database, and the doctor already walks pages for
   CRC validation.

## HNSW subtree retirement

Ordinary atomic Catalog::save now diffs changed HNSW roots through contiguous
layer-directory pages and all CSR directory/payload pages. Equal roots prune
traversal. Pages shared by any final root remain live, including partial CSR
groups, and superseded pages follow the existing publication-LSN/pin reuse law.
Published old topology never enters the prospective queue. Newly flushed CSR
cells and newly allocated layer-directory/root pages enter it immediately,
including intermediate or failed adaptive batches. Final catalog reachability
filters out live pages before retirement; a failed publication cannot retire
its old published graph. Incomplete CSR writes failing with I/O errors remain
eligible for offline sweep; completed cells from budget retries do not leak.
