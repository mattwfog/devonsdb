# Multi-process access

## Problem

devondb's concurrency boundary is currently one process. An open database owns
an `Arc<Shared>` whose published state, commit pipeline, and write-transaction
registry are guarded by Rust `Mutex` values; cloning a `Database` only clones
that process-local `Arc` (`crates/devondb/src/database/options.rs:75-88`,
`crates/devondb/src/database/options.rs:122-126`). A snapshot clones the local
published-state pointer, and a transaction registers only in that local
`write_txns` map (`crates/devondb/src/database/options.rs:255-272`). This is the
implemented form of the MVCC design's in-process `Arc<PublishedState>` model
(`docs/MVCC.md:263-279`, `docs/MVCC.md:301-306`).

Opening a second process does not join that state. Each process independently
opens the main file and WAL, replays the WAL once, constructs a new
`PublishedState`, and creates its own `CommitPipe`
(`crates/devondb/src/database/options.rs:175-224`,
`crates/devondb/src/database/options.rs:227-252`). Consequently, an already
open reader has no path that observes another process's later publication: its
snapshots keep cloning its own `published` pointer
(`crates/devondb/src/database/options.rs:105-110`,
`crates/devondb/src/database/options.rs:255-258`).

There is no OS advisory lock in either storage file path. `Pager` opens the
main file with `OpenOptions` and protects writes with a process-local
`std::sync::Mutex` (`crates/devondb-storage/src/pager.rs:27-34`,
`crates/devondb-storage/src/pager.rs:227-250`,
`crates/devondb-storage/src/pager.rs:300-321`). `WalWriter` likewise opens a
plain `File` and keeps its append offset and next LSN in that writer object
(`crates/devondb-storage/src/wal.rs:6-8`,
`crates/devondb-storage/src/wal.rs:15-46`). Two processes can therefore derive
the same `log_end`/`next_lsn`, seek to the same offset, and race writes; the
WAL's checksum and increasing-LSN checks can detect a damaged tail but do not
serialize its creation (`crates/devondb-storage/src/wal.rs:49-75`,
`crates/devondb-storage/src/wal.rs:129-165`).

Checkpoint has the same boundary. It serializes on the local commit mutex,
publishes fresh main-file pages and a superblock, then truncates and reopens
the WAL (`crates/devondb/src/database/checkpoint.rs:6-41`). Another process can
be opening or replaying that WAL at the same time because its commit mutex is a
different object. The current WAL rules make a checkpoint crash-safe within
one owner—complete commits at or below `checkpoint_lsn` are skipped and a torn
tail is ignored (`docs/FORMAT.md:402-430`)—but they do not make an unsynchronized
main-file/WAL observation atomic across processes.

A common deployment has a daemon, dashboard, server, and CLI sharing one
database. A lifetime exclusive lock would exclude every second opener,
including readers. Multiprocess coordination must allow read-heavy surfaces
to coexist with the daemon without permitting two owners of the WAL or
checkpoint path.

## Prior art

### SQLite WAL mode

SQLite keeps the database pages in the main file, appends new pages to the WAL,
and uses a shared-memory WAL index (`-shm`) for cross-process lookup and lock
state. A reader records an end mark and sees a stable prefix while one writer
continues appending. Reader marks tell checkpoint how far it may safely advance;
a long reader can therefore delay WAL reset and let the WAL grow.

What it pays: a third shared-memory artifact, a platform-specific lock protocol,
reader-slot management, checkpoint starvation handling, and a same-host/shared-
memory requirement. What it gets: one writer concurrent with many
cross-process snapshot readers, cheap incremental discovery through the WAL
index, and bounded checkpoint decisions that account for every live reader.
See SQLite's [WAL documentation](https://sqlite.org/wal.html) and
[wal-index format](https://sqlite.org/walformat.html).

### LMDB

LMDB has one cross-process writer mutex and many MVCC readers over a read-only
memory mapping of copy-on-write B+tree pages. Its lock file contains a reader
table; reader transactions publish the transaction ID they pin, and page reuse
is bounded by the oldest live reader. The OS releases the writer lock after a
process death, while stale reader slots can be detected and cleared.

What it pays: mmap's address-space and fault behavior, a fixed reader-slot
table, explicit stale-reader cleanup, and file growth when readers retain old
pages. What it gets: no WAL replay, zero-copy reads, a very small engine, and
simple one-writer/many-reader behavior across processes. DevonDB deliberately
uses a budgeted user-space cache rather than mmap, so LMDB's lock/reader-table
lesson transfers but its read path does not (`docs/ARCHITECTURE.md:74-88`).

### DuckDB

DuckDB supports concurrency among threads in one process. Multiple processes
may attach to the same database only when all use `access_mode = READ_ONLY`;
cross-process writes are not supported. See DuckDB's
[concurrency documentation](https://duckdb.org/docs/stable/connect/concurrency).

What it pays: applications that need independent writers must add a server or
an external ownership protocol. What it gets: no shared-memory index, reader
registry, or cross-process checkpoint protocol in the embedded engine, while
retaining a sophisticated in-process transaction and buffer-manager design.

## Options

### A. Single writer, multiple readers: advisory locks plus WAL watch

Give each open an explicit role. A writer open takes a nonblocking exclusive
**writer lease** for the handle's lifetime. Read-only opens do not take that
lease. A second writer fails `Busy` with the holder role/path in the message;
it does not wait. The daemon normally owns the lease, and a mutating CLI can
take it only after the daemon exits.

Use a second, short-lived **publication gate**. The writer holds it exclusively
from before WAL append through WAL fsync and local-state publication, and for
the whole checkpoint publication/WAL-reset sequence. A reader attempts the
gate shared and nonblockingly before refresh. If busy, it immediately serves
its last published state; it never waits on the writer's fsync or checkpoint.
An initial read-only open has no prior state, so publication-gate contention
returns `Busy` and lets the caller retry rather than constructing a mixed-epoch
view.
This retains the existing promise that an executing snapshot does not wait on
commit I/O (`docs/MVCC.md:333-347`). Lock files are never unlinked during normal
operation: their existence is not ownership; the kernel lock is.

Each reader maintains `last_seen_checkpoint_lsn`, `last_seen_commit_lsn`, and a
WAL byte cursor. A filesystem notification is only a latency hint. A poll at a
configured maximum interval, and every fresh query/snapshot request, attempts
refresh. While holding the shared publication gate, the reader loads the
authoritative superblock and re-runs the complete feature-flag acceptance law
before it loads the catalog or decodes any newly visible WAL byte. It then:

1. incrementally parses complete committed groups after its WAL cursor when
   the checkpoint epoch is unchanged;
2. builds a new immutable overlay chain and swaps its local published pointer;
3. reloads the authoritative catalog and replays WAL groups newer than its
   `checkpoint_lsn` when checkpoint advanced, the WAL shrank, or the cursor no
   longer names the expected LSN; and
4. treats an incomplete/invalid tail as not-yet-published, never as a group to
   expose.

### Feature appears after follower open

A follower may open while a checkpoint-scoped feature bit is clear and see
that bit appear at a later refresh. Acceptance of the original superblock does
not grandfather the handle: every refresh applies `FORMAT.md`'s current
supported/read-safe masks to the newly authoritative superblock before any
bytes governed by that bit are interpreted. An unsupported, not-read-safe bit
therefore returns `VersionMismatch` (unsupported feature), never `Corrupt`, and
the follower publishes neither a prefix of the WAL group nor a new catalog.

Explicit `refresh()` reports that error without publishing the unsupported
state. Ordinary snapshot creation attempts a nonblocking refresh and may keep
serving the previous accepted state; it does not latch a terminal handle error.
Pinned snapshots remain valid on their immutable catalog and overlay. Every
later refresh opens a fresh pager and validates flags again, including when
checkpoint LSN is unchanged. Once the writer checkpoints, clears the fence and
releases the gate, a supported refreshed epoch can be accepted. This applies
to HNSW_MUTATION_WAL bit 14 as well as bits 6 and 10.

The shared gate makes the superblock/WAL pair one observation. An existing
snapshot does not refresh in place: it retains its old catalog and overlay.
That remains sound because published data and catalog pages are copy-on-write
and protected by generation pins from reuse; the two superblock slots are overwritten
(`docs/FORMAT.md:354-370`). A fresh snapshot sees the newly swapped state.

**SIGKILL semantics.** Killing a reader changes no database bytes, and the OS
releases any shared gate it held. Killing the writer releases both leases. A
successor first takes the writer lease and publication gate, then performs the
normal dual-superblock/WAL recovery before accepting writes. A kill during WAL
append leaves either a complete group or an ignored tail; a kill after WAL
fsync but before local publication is recovered from the WAL. A kill anywhere
in checkpoint selects a valid superblock and either replays or skips each
whole transaction according to `checkpoint_lsn`. A leftover lock-file inode is
not stale ownership and must not be deleted or PID-inspected.

**MVCC interplay.** Transactions remain concurrent inside the one writer
process, so current commit conflict detection is unchanged. Cross-process
write transactions cannot overlap because only one process owns the writer
lease. Read processes independently reconstruct immutable published-state
chains; their pinned snapshots remain stable across later external commits and
checkpoints. Before devondb ever reuses leaked pages, it must add a
cross-process oldest-reader registry; the current in-process rule only protects
snapshots known to one process (`docs/FORMAT.md:365-370`).

**Format impact.** Main-file page and WAL record layouts need no change. Safe
mixed-version operation does: a process without multiprocess coordination
ignores both locks and can race a checkpoint. The implementation should
therefore allocate a sticky,
not-read-safe `MULTIPROCESS_COORDINATION` feature bit before coordinated access
is enabled. Its implementation commit must register the bit, specify activation
and lock-sidecar lifecycle in `docs/FORMAT.md`, and add the required corpus
anchor under the permanent feature-bit law (`docs/FORMAT.md:574-625`,
`docs/FORMAT.md:837-845`). Activation is a one-time offline operation: no lock
protocol can evict an already-open legacy process that does not participate.
The lock artifacts contain no durable database state and are recreated safely.

**Amplification.** Writes keep the existing WAL record, one commit fsync, and
checkpoint writes, adding lock syscalls and filesystem notifications only.
Readers pay metadata polls and incremental WAL parsing. Each process has its
own buffer pool and decoded overlay, so hot pages and uncheckpointed rows may
occupy memory once per process; a checkpoint causes each reader to reload the
catalog on its next refresh. There is no shared WAL index, which keeps the
design small but makes very large or high-rate WALs less efficient than
SQLite's design.

**Measured result.** The daemon writes ticks while dashboard and MCP queries
and ordinary CLI commands read concurrently. Readers can be briefly stale
while the publication gate is held but never block behind it. A mutating CLI
fails clearly while the daemon owns the writer lease and succeeds, after
recovery, when the daemon is down.

### B. Lock-arbitrated multi-writer: rotating writer lease

Let every process begin a local optimistic transaction and rotate an exclusive
writer lease at commit. The winner refreshes from the latest superblock/WAL,
runs conflict detection against every commit since its snapshot, appends and
fsyncs, then releases the lease. A simpler variant holds the lease from
`begin` through `commit`, but that turns one slow or abandoned transaction into
global writer starvation and removes most value of optimistic concurrency.

**SIGKILL semantics.** OS release and WAL recovery make death of the current
lease holder recoverable as in option A. Death of a non-holder must also remove
its active-snapshot mark; otherwise checkpoint can be stalled forever. A kill
after transaction start but before lease acquisition discards only local
writes. A kill during commit leaves the same complete-group-or-tail outcome as
option A.

**MVCC interplay.** Commit-only leases need global conflict history and a
cross-process registry of active write snapshot LSNs. The current
`recent_summaries` and `write_txns` live only in `Shared`
(`crates/devondb/src/database/options.rs:75-88`), and current checkpoint prunes
summaries using only that map (`crates/devondb/src/database/checkpoint.rs:133-141`).
Without shared marks, a checkpoint can erase the WAL/history that a transaction
in another process needs to detect a conflict. The implementation must either
add SQLite/LMDB-style process slots, retain sufficient conflict summaries in a
specified sidecar, or prohibit checkpoint while any external writer snapshot
is live.

**Format impact.** The main/WAL record shapes can remain unchanged only if all
active-writer marks and conflict summaries are reconstructible coordination
state. Any structured persistent sidecar is new on-disk format and must be
specified and feature-bit fenced; the mixed-version coordination bit from
option A is required in either case. A fixed slot table also introduces sizing,
stale-slot, and generation/version rules that must be format law rather than an
undocumented lock-file convention.

**Amplification.** Every committing process refreshes and may replay other
processes' WAL before conflict checking. Lease/slot traffic rises with writer
count; checkpoint may retain a larger WAL for old transactions. Reads have the
same duplicated caches as option A. The gain is that any process can write
without daemon handoff, but a mostly-read topology does not supply enough
simultaneous writes to repay this machinery.

### C. Out-of-process page server

Run one local service that exclusively owns `Pager`, `WalWriter`, published
state, conflict summaries, and checkpoint. Embedded clients keep planning and
execution local but obtain snapshot handles/pages and submit transaction deltas
over IPC. The service batches page requests and validates snapshot IDs; a
query-server variant could instead execute whole DevonPlans, but that is a
larger product-surface change than a page server.

**SIGKILL semantics.** A killed client loses its IPC-owned transaction and
snapshot registrations; the server aborts them on connection loss. A killed
server leaves the ordinary WAL/main-file crash state and recovers before
reaccepting clients. All clients fail or reconnect—none silently switches to
direct file access while the server may still own it. Server restart invalidates
old snapshot handles by generation.

**MVCC interplay.** Central ownership makes the current `Shared` model global
again. It naturally retains conflict summaries and knows the oldest snapshot,
so multi-writer arbitration and future page reuse are easier. The IPC protocol
must define transaction/session lifetime, snapshot generations, cancellation,
backpressure, and error replay; those become compatibility commitments even
though they are not main-file bytes.

**Format impact.** No main-file/WAL layout change is intrinsically required.
The mixed-version coordination bit is still needed to prevent a legacy direct
writer from bypassing the service. Socket and lock artifacts are ephemeral; if
the server persists session or routing metadata, those bytes require their own
specified, feature-fenced format.

**Amplification.** A central buffer pool can eliminate duplicated disk reads
and most duplicate page-cache memory. In exchange, local scans add IPC round
trips and page copies unless requests are aggressively batched or use shared
memory; writes add serialization/copying before the same WAL write. All
processes gain access, but daemon absence now requires service
autostart/reconnect rather than the CLI simply taking a file lease. The service
is also a new resident process and operational failure domain on edge systems.

## Recommendation

Implement option A first: a lifetime single-writer lease, a nonblocking
publication gate, and polling-backed WAL refresh for read-only handles. It fits
a daemon-writes/readers-mostly-read topology and preserves the
existing embedded API and MVCC representation, and gives a down-daemon CLI a
clean writer-takeover path without introducing a server or cross-process
conflict registry.

Make the first implementation deliberately narrower than "multi-writer": one
writer process, concurrent in-process transactions inside it, and any number of
read processes. Claim the coordination feature bit before enabling this mode,
ship query-start nonblocking refresh plus an explicit `refresh()`/observed-LSN
surface, and defer reader slots until free-page reuse requires them.

## Invariants

1. **One writer owner.** At most one live process can hold the writer lease for
   a database identity; every WAL append, main-file write, and checkpoint comes
   from that process.
2. **No implicit waiting.** Writer-lease acquisition and reader publication-gate
   acquisition are nonblocking. A contended writer returns `Busy`; a contended
   reader serves its previously published immutable snapshot.
3. **Durable publication boundary.** The writer holds the publication gate
   exclusively from before the first WAL byte of a commit until after WAL fsync
   and local published-state swap. Readers never expose a partial transaction.
4. **Atomic checkpoint observation.** The writer holds the publication gate
   across main-file publication, WAL truncation/rotation, and coordination flag
   updates. A reader refresh observes either the pre-checkpoint epoch or a
   self-consistent post-checkpoint superblock plus WAL.
5. **Snapshot stability.** Refresh creates and swaps a new published state; it
   never mutates a state reachable by an existing snapshot. A pinned snapshot
   returns identical rows before and after external commit/checkpoint, and may
   finish after a later explicit refresh rejects an unsupported feature.
6. **Monotone freshness.** Within a reader process,
   `last_seen_commit_lsn` never decreases. Rebase may move data from overlay to
   main-file groups but cannot lose or duplicate a committed transaction.
7. **Bounded catch-up.** Once the writer releases the publication gate and no
   newer publication is active, a polling reader observes the acknowledged
   commit by its next configured refresh deadline; dropped/coalesced filesystem
   events do not weaken this bound.
8. **Crash-release, not file deletion.** SIGKILL of any lock holder releases
   ownership through the OS. Persistent lock artifacts are reused and never
   interpreted as a stale lock merely because the files exist.
9. **Takeover recovers first.** A new writer performs superblock/WAL recovery
   while holding both exclusive roles before it accepts a transaction. Every
   acknowledged commit is present; an unacknowledged tail is wholly present or
   wholly absent, never partial.
10. **Local-filesystem identity.** All participants resolve the same database
    identity and use a lock primitive with the required local-filesystem
    semantics. Unsupported/network filesystems fail open with a clear error;
    they do not silently downgrade locking.
11. **Legacy exclusion.** Coordinated access is enabled only after the
    `MULTIPROCESS_COORDINATION` feature bit is durably set during an offline
    activation. A build that does not implement the protocol cannot open that
    file. If any unsupported not-read-safe bit first appears after open, the
    follower rejects it before catalog/WAL decode and permanently refuses new
    snapshots on that handle with an upgrade/reopen message.

    The shipped offline activation surface is `devondb activate <path>`. After
    activation, `devondb <path> --read-only` (and `devondb ask ... --read-only`)
    opens a follower shell through the coordinated read-only engine role.
12. **Format discipline.** No main-file page, WAL envelope, or WAL payload
    changes for multiprocess coordination. Any later persistent coordination
    or reader-registry bytes require a registered feature bit, an exact
    `FORMAT.md` layout, and a golden-corpus entry.

## Test plan

Build a small integration helper with explicit roles (`writer`, `reader`,
`checkpoint`, `activate`) and phase barriers over pipes. The parent harness
must spawn real separate OS processes against one database path; threads are
useful inside the writer for existing MVCC tests but do not count as
multiprocess evidence. Every helper reports PID, role, observed checkpoint or
commit LSN, and phase so the parent kills a precise state rather than sleeping
and guessing.

The core topology test starts a daemon-shaped writer plus dashboard, MCP, and
CLI-shaped readers. It commits numbered batches while every reader repeatedly
pins and reruns snapshots. Each result must be a transaction-prefix, every
pinned snapshot must remain row-identical, every reader must eventually reach
the final LSN, a second writer must get `Busy`, and a CLI writer must acquire
and commit after the daemon exits.

**Busy-proof ordering:** verify the lease holder is present before a `Busy`
proof means anything.
A second-writer probe run while the intended holder is still inside its own
startup, recovery, or pre-lease phase can acquire the lease successfully and
return a false negative. Every activation runbook therefore orders: (1) start
the intended holder,
(2) observe it holding the target's writer-lease sidecar (lsof or the
holder's own readiness signal), (3) only then run the second-writer probe
and require `Busy` naming the lease path.

Run a SIGKILL matrix at each durable-state boundary:

- writer after writer-lease acquisition, mid-payload, after commit-record write
  but before fsync, after fsync but before local publication, and after local
  publication but before gate release;
- checkpoint while writing new groups/catalog, after alternate-superblock
  publication but before WAL truncate, after truncate but before feature-bit
  cleanup, and before gate release; and
- reader after shared-gate acquisition, mid-incremental WAL parse, after rebase
  construction but before pointer swap, and while holding an old snapshot
  across an external checkpoint.

After each kill, start a fresh writer under both locks, recover, and compare
the full database with the parent's acknowledgement log. Acknowledged
transactions must appear exactly once. The killed operation's final
unacknowledged transaction may appear or not, but no table/relationship/DML
subset may appear. Then run a full scan/expand and corruption checker, not only
a row count.

Stale-lock tests kill -9 every role while it owns each lock, retain the lock
files, and prove immediate acquisition by a successor. Repeat with PID reuse
simulated in any diagnostic metadata and with two same-process handles. A test
that unlinks and recreates a live lock artifact must fail the invariant suite,
guarding against split-brain-by-inode-replacement.

Freshness tests timestamp the writer's post-fsync/post-gate-release
acknowledgement and have independent readers record the first snapshot LSN
that includes it. Assert the configured poll bound plus a documented scheduler
tolerance on an idle test host. Disable/drop filesystem notifications to prove
polling alone meets the bound; separately prove query-start refresh observes an
available commit immediately. While the publication gate is held, assert that
queries complete from the old state within a read-latency bound and catch up by
the first deadline after release.

Finally, add activation/compatibility tests: activation refuses while a
cooperating handle is live, persists the feature bit before readers are
allowed, current binaries reopen and recover, and an old fixture binary that
does not implement multiprocess coordination refuses the file cleanly. A
second fixture opens a follower before a new not-read-safe bit appears, then proves refresh rejects the
new superblock before WAL decode, pinned snapshots remain byte-stable, and all
ordinary snapshots may keep the previous accepted state; explicit refresh
retries feature validation.
Run the process/crash matrix on Linux and macOS; unsupported filesystems must
exercise the explicit refusal path.

## Design decisions and deferred work

1. **Sidecar lock files use `flock`-semantics whole-file locks,
   taken through Rust std's stabilized file-locking API
   (`File::try_lock`/`File::try_lock_shared`/`File::unlock`, stable since
   1.89; workspace `rust-version` is 1.95 — zero new dependencies), on
   paths derived from the canonicalized main-file path
   (`<canonical-main>.lock-writer`, `<canonical-main>.lock-publish`).**
   Rationale: on both Linux and macOS these are open-file-description
   locks — they do not suffer POSIX `fcntl` record locking's
   close-any-descriptor-releases-all hazard, they conflict correctly
   between two opens in one process, and they supply the shared/exclusive
   modes the publication gate needs. Byte-range main-file-inode locking is
   rejected: it would ride the pager's descriptor and couple lock lifetime
   to unrelated file management. The lock semantics must be proven with
   the semantics with the mandated matrix — symlinks (canonicalization
   collapses them), duplicate opens, fork/exec inheritance, descriptor
   close, inode replacement (a test that unlinks and recreates a live lock
   file must fail the invariant suite) — and network filesystems remain
   explicitly unsupported: opens on a filesystem that cannot support the
   semantics fail with a clear error, never a silent downgrade.
2. **The public options surface defaults `refresh_interval` to
   100 ms**, finite and configurable, with query-start nonblocking refresh
   always on. 100 ms is imperceptible for an interactive dashboard while an
   order of magnitude cheaper in wakeups than 10 ms edge-hostile polling;
   a deployment may tune after measuring — the correctness bound
   (invariant 7) holds at any finite setting.
3. **Deferred:** cross-process oldest-snapshot marks (reader table versus
   server) wait until free-page reuse is designed. The initial protocol is
   safe without them only because current format law leaks
   superseded pages; reclamation may not land until this is answered and
   kill-safe stale-reader cleanup is specified.
