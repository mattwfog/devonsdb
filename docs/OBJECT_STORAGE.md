# Object-storage pager backend and read-only attach

This design changes no format bytes. Its first increment makes one completed
DevonDB main-file object queryable through HTTP range requests. It does not
make object storage a writer or a WAL transport.

The binding constraints are:

- the main file remains the `docs/FORMAT.md` file, including dual superblocks,
  page checksums, feature acceptance, and immutable published data pages;
- remote bytes enter the existing budgeted page cache, under the one
  `memory_limit` accountant and the `docs/MVCC.md` §7.2 reclaim ladder;
- the local hot path does not acquire an unmeasured vtable dispatch;
- the library feature is off by default and brings no async runtime into the
  core (`docs/ARCHITECTURE.md` § Edge budget rules 3 and 4); and
- an HTTP attach is read-only and pins one object generation for the complete
  handle lifetime.

The design applies three transferable object-storage principles: batch remote
operations, cache read blocks locally, and retain a single-writer/multi-reader
model. It reuses DevonDB's in-memory cache without making object storage the
write source of truth or adding a local SST disk cache.

## Scope and terminology

An **object** is one byte-for-byte DevonDB main file. A **generation** is an
immutable version of that object, identified by an origin-provided strong ETag
or an object-store generation/version token with equivalent conditional-read
semantics. A **remote attach** is a read-only `Database` handle bound to one
object URL and one generation.

The initial public surface is explicit and feature-gated: a dedicated
read-only method takes a typed source argument, never an overloaded
`open(path)` string:

```rust,ignore
#[cfg(feature = "httpfs")]
pub fn open_read_only_with(
    source: DatabaseSource,
    options: HttpAttachOptions,
) -> DevonResult<Database>;
```

The exact enum spelling of `DatabaseSource::Http { url, .. }` is not specified;
the dedicated constructor and typed source are required.
`Database::open(path)` continues to mean a local filesystem database. It does
not guess that a string is a URL. The attach returns the pinned generation,
object length, authoritative `checkpoint_lsn`, and `db_id` through an attach
diagnostic so callers can log exactly what they queried. Only naming details
remain open.

## The pager-backend seam

### Derivation from the current pager

`crates/devondb-storage/src/pager.rs` has five kinds of file activity, but only
four belong in the byte-backend trait:

1. **Length.** Open validates the main-file length after choosing a
   superblock (`crates/devondb-storage/src/pager.rs:316-337`); every data-page
   read checks its end against the current file length
   (`crates/devondb-storage/src/pager.rs:381-393`); append-only allocation uses
   length as its cursor (`crates/devondb-storage/src/pager.rs:441-461`).
2. **Positional exact read.** A data-page miss fills exactly at its computed
   offset (`crates/devondb-storage/src/pager.rs:347-379,395-399`). Open reads
   exactly the 64-byte slot-zero header, then reads or probes slot one at the
   page-size offset (`crates/devondb-storage/src/pager.rs:611-632`), before
   applying the ordinary dual-slot choice
   (`crates/devondb-storage/src/pager.rs:316-324,634-640`).
3. **Positional exact write.** Creation writes both initial superblock pages
   (`crates/devondb-storage/src/pager.rs:278-313`); `write_page` writes one
   page (`crates/devondb-storage/src/pager.rs:401-420`); allocation appends a
   zero page (`crates/devondb-storage/src/pager.rs:441-461`); checkpoint
   publication writes the alternate superblock
   (`crates/devondb-storage/src/pager.rs:464-490`); and flag-only publication
   writes both slots (`crates/devondb-storage/src/pager.rs:493-525`).
4. **Durable sync.** Creation, the public `sync`, alternate-slot publication,
   and flag-only publication all call `sync_all`
   (`crates/devondb-storage/src/pager.rs:297-300,422-430,485-486,518-522`).
5. **Backend construction and local directory durability.** `OpenOptions`
   constructs a local file (`crates/devondb-storage/src/pager.rs:292-296,
   316-319`), while creation syncs the parent directory
   (`crates/devondb-storage/src/pager.rs:573-584`). These are factory concerns,
   not operations on an already constructed byte backend. HTTP and OPFS must
   not be forced to pretend they have a parent directory or Rust `Path`.

The exact trait surface is therefore:

```rust,ignore
pub(crate) trait PagerBackend {
    fn len(&self) -> DevonResult<u64>;
    fn read_exact_at(&self, offset: u64, dst: &mut [u8]) -> DevonResult<()>;
    fn write_all_at(&self, offset: u64, src: &[u8]) -> DevonResult<()>;
    fn sync_all(&self) -> DevonResult<()>;
}
```

There is deliberately no `open`, `create`, `path`, `metadata`, `seek`, HTTP
range, ETag, cache, prefetch, or async method on this trait. Factories construct
a backend. `read_exact_at` accepts arbitrary lengths, so the pager may turn one
multi-page miss window into one HTTP request without teaching the storage
decoders about HTTP. The exact-read contract also preserves today's handling
of short reads: the backend returns an I/O/protocol failure and never exposes
partially initialized bytes.

Native implementations must be safe for the pager's concurrent `&self` use.
The native containing type therefore requires the needed `Send + Sync` bounds;
those bounds should not be baked unconditionally into the portable trait,
because a single-worker wasm32 OPFS handle has a different threading model.

### Dispatch shape and local-path gate

The first implementation should keep `Pager` non-generic at its consumers and
store a closed backend enum:

```rust,ignore
enum Backend {
    Local(LocalFileBackend),
    #[cfg(feature = "httpfs")]
    Http(HttpRangeBackend),
    #[cfg(all(target_arch = "wasm32", feature = "opfs"))]
    Opfs(OpfsBackend),
}

impl PagerBackend for Backend {
    // Each method is one exhaustive match and then a concrete call.
}
```

This is static enum dispatch, not `Box<dyn PagerBackend>`. With `httpfs` off,
the native enum has only its local variant and the compiler can erase the
match. With `httpfs` on, cache hits still return from the existing frame table
before any backend read; a miss pays one predictable enum branch before its
positional operation. The current length check occurs before the cache lookup
(`crates/devondb-storage/src/pager.rs:356-368,381-392`), so the design must
either retain and measure that enum-dispatched `len` call or move a
proven cached-page hit ahead of the length query. It must not silently replace
the enum with a trait object.

The gate is criterion evidence for local `read_page_ref` cache hits and forced
misses in both builds: default features and `httpfs`. Record distributions and
binary sizes, not one timing. A regression outside benchmark noise (more than
2% in median local cache-hit throughput) requires reconsidering the enum shape
in favor of a monomorphized `Pager<B>`.
Both shipping profiles must still satisfy the core and feature-complete binary
budgets. No claim that dispatch is free is accepted without these results.

### Read-only enforcement

The facade is the first fence. A remote attach constructs no `WalWriter`, no
writer lease, and no publication gate, and marks its shared state read-only.
Every transaction, DDL/DML, `COPY`, checkpoint, index build, feature-flag
publication, and other mutator fails through the existing writable gate; that
gate already produces `DevonError::ReadOnly`
(`crates/devondb/src/database/options.rs:61-79,105-123`).

The backend is a second fence. `HttpRangeBackend::write_all_at` and
`sync_all` always return `DevonError::ReadOnly`. Consequently, a missed facade
gate still cannot reach any write call enumerated above. `allocate_page` also
fails before returning a page id: its attempted append reaches the refusing
write method. A successful no-op remote sync is forbidden because it would
falsely claim a durability action occurred. Remote creation is not exposed.

## Remote attach, publication, and freshness

### What opening does

Attach bootstraps with a conditional-capable range read, obtains the total
object length and a strong generation token, and pins both before constructing
the pager. It then runs the normal pager open sequence against that fixed
length and token:

1. read and validate both 64-byte superblock headers from the same generation;
2. select the valid slot with the highest `checkpoint_lsn`, with slot 0 winning
   an equal-LSN tie, exactly as `docs/FORMAT.md` § Superblock requires;
3. validate length and page alignment;
4. apply the normal format-version and feature-flag acceptance law; and
5. load the catalog and subsequent pages through the ordinary pager/cache.

The attach does not request, create, replay, trim, or watch a WAL object. Its
published state has no overlay. It therefore promises exactly the checkpoint
image named by the selected superblock in the pinned object generation — and it
may make that promise only when that superblock actually names a completed
image.

If the selected superblock sets `DML_WAL`
(feature bit 6), attach refuses with a diagnostic naming the bit. `FORMAT.md`
defines that bit as set exactly when durable acknowledged update/delete commits
exist outside the checkpoint image and marks it not-read-safe: recovery would
"be presumed partial" for a binary that cannot replay its WAL. A remote handle
structurally cannot replay — it constructs no `WalWriter`, requests no WAL
object, and can take no writer lease or publication gate (`docs/MULTIPROCESS.md`)
— so serving such an image silently would hide acknowledged transactions from
the reader while reporting `last_commit_lsn == checkpoint_lsn`. A documented
"checkpoint-image-only" opt-in was considered and rejected: no consumer needs a
handle whose completeness depends on producer discipline the file itself
contradicts, and the honest v1 behavior mirrors what the format's acceptance law
already requires of any WAL-less reader. Uploading a crashed-with-pending-DML
file is a producer error; refusing it at attach is the reader-side fence.

### Relationship to multiprocess coordination

A remote object has neither of the local coordination artifacts described by
`docs/MULTIPROCESS.md`: it takes no lifetime writer lease and cannot take the
short-lived publication gate. It is not a multiprocess follower and does not
run WAL-watch refresh. It must never derive fake lock paths from a URL.

That omission is safe only because a generation is immutable. The generation
precondition supplies what the publication gate supplies locally: every byte
used to choose the superblock and every later page belongs to one atomic
observation. The sticky `MULTIPROCESS_COORDINATION` feature bit does not make a
completed object unreadable; a current reader understands the format feature,
and no process can mutate the pinned generation. It also does not grant remote
refresh or writer rights.

This produces a narrower freshness contract than the local follower contract:

- every snapshot and query on one handle sees one checkpoint generation;
- cached and newly fetched pages can never cross generations;
- freshness is monotone only within that fixed generation, where it is
  constant;
- there is no poll deadline and no claim that attach observes the latest
  object-store key value; and
- seeing a newer generation requires dropping the handle and attaching again,
  or attaching an explicit versioned URL/token as a new handle.

`refresh()` must not silently repin a remote handle. The explicit remote
surface should either lack refresh or return a clear read-only/unsupported
error. Reattach is observable because it returns a new generation and
checkpoint LSN.

The producer is responsible for uploading a completed main-file image. Because
publish-to-object-storage is outside this design, the first increment cannot promise
that a mutable local database was copied at its newest acknowledged commit. A
file copied without a preceding checkpoint remains a consistent older
checkpoint image if its selected superblock and referenced pages validate; its
unuploaded WAL commits are deliberately invisible. An object assembled
inconsistently fails ordinary range, length, checksum, or format validation.

## HTTP range policy

### Request contract

Every byte request sends `Range: bytes=start-end`, `Accept-Encoding: identity`,
and a conditional generation header (`If-Match` for a strong ETag, or the
store-specific equivalent against an explicit version). Successful range
reads require all of the following:

- HTTP status `206 Partial Content`; a `200` that ignored Range is refused so a
  server cannot turn one page miss into an unbudgeted full-object download;
- `Content-Range` exactly names the requested inclusive interval and the
  already pinned total length;
- `Content-Length` and the body are exactly the requested byte count;
- no content encoding transformed the bytes; and
- the response generation token is present
  and byte-for-byte equal to the pinned generation. A `206` without a
  comparable generation token is a remote protocol failure, not success: an
  origin that cannot echo the condition cannot prove which bytes it served.

### Redirects

Automatic redirect following is disabled. The default policy is to follow
**no** redirects: any `3xx` response is a non-retryable remote protocol failure
whose message names the redirect target host (sanitized, never echoing signed
query parameters). An explicit per-attach allow-list may admit same-origin
redirects only — same registrable host and scheme as the original URL. When a
target is allowed, each hop re-runs the credential/header hooks against the new
URI, re-sends `Range`, the conditional generation header, and
`Accept-Encoding: identity`, re-applies all response checks, and consumes one
bounded hop budget (default 2) before failing. Cross-origin redirects are never
followed, with or without the allow-list: presigned query strings and hook-
supplied credentials must not reach a host the caller did not name. Error
messages report the origin actually contacted, so a refused redirect cannot be
misattributed to the original bucket host.

An early EOF, reset, or shorter body is a
remote I/O failure. Extra bytes, malformed range headers, a server ignoring
identity encoding, or a cross-origin redirect are remote protocol failures.
They are not `Corrupt`: no complete candidate format bytes were delivered.
Only after an exact response from the pinned generation has arrived may
superblock, CRC, layout, or semantic validation classify those bytes as
`DevonError::Corrupt`. One law governs length mismatch everywhere in this
document: a response total length differing from the pinned length **is** a
generation change — evidence the object was replaced — and therefore poisons
the handle under the generation-pinning section below, exactly like a changed
token or `412`; likewise `416` for a range already proved in bounds against the
pinned length is length-change evidence and takes the same poisoned path. It is
never classified as an ordinary retryable protocol failure.

### Batching, coalescing, and prefetch

One HTTP GET per 4 KiB page is not an acceptable steady-state scan. The pager,
not the trait and not each decoder, owns a deterministic miss-window planner:

- The first nonsequential miss fetches only its demanded page.
- Consecutive forward misses grow the window 1, 2, 4, 8, then 16 pages. A
  nonconsecutive miss resets it to one page.
- A request is capped at 16 pages **and** 256 KiB, whichever is smaller, and is
  clipped at the pinned object length. It is always page-aligned and contains
  at least the demanded page.
- Adjacent pages in one window are coalesced into one contiguous range even
  when they belong to different higher-level payloads; page validation remains
  per existing consumer. Random requests are not joined across gaps.
- An in-flight table keyed by generation and page interval provides
  single-flight behavior. A concurrent miss covered by an existing request
  waits only for that request rather than issuing a duplicate. There is no
  timer-based gather delay, background fetch thread, or async task.
- The global frame-table mutex is never held across network I/O. The initiating
  thread reserves the missing frames under the mutex, performs the synchronous
  request after releasing it, then installs or releases reservations and wakes
  joiners. A failed request installs no page from that batch.

Single-flight failure semantics are fixed: when a batch fails, every joiner
observes the initiator's error class and
message — joiners fail with the leader's error; they do not independently retry
the failed interval as fresh demand misses. (A caller above the pager may retry
the query; the point is one network storm per interval, not N.) The in-flight
entry, frame reservations, and wake handles are owned by an RAII drop-guard on
the initiator: if the initiating thread panics mid-request, the guard releases
the reservation charges, removes the in-flight entry, and wakes joiners with a
deterministic internal error rather than leaking the charge and parking them
forever.

These values are initial writer policy, not format law. Measurements must
report request count, transferred bytes, wasted prefetched pages, and
wall time for sequential scans, CSR-like runs, and random reads. The hard
page/byte caps are required even if later measurements tune the numbers.

There is one in-memory cache: `PageCache`. HTTP code does not keep a second
unaccounted response cache. Prefetched pages use the same clock, pin, and
eviction rules as demanded local pages. This is the useful portion of
SlateDB's block-cache lesson without introducing the disk-cache layer that
this design deliberately excludes.

### One budget and the §7.2 ladder

Remote reads use the same `Arc<MemoryBudget>` installed on the pager. Before a
range is issued, the initiator reserves:

1. `page_size + FRAME_OVERHEAD` for every page that will become a cache frame,
   using the same constants and release points as local frames; and
2. every byte of transient response storage the synchronous client cannot
   stream directly into those frames, including its bounded read buffer.

Reservations call `MemoryBudget::charge_or_reclaim`, whose first failure path
evicts clean unpinned page-cache frames and retries once
(`docs/MVCC.md:630-652`; `crates/devondb-storage/src/budget.rs:84-115`). A
prefetch reservation failure shrinks the tail until only the demanded page
remains. If the demanded page and required bounded transport buffer still do
not fit, the query returns `DevonError::BudgetExceeded` with category,
requested bytes, charged bytes, and limit. HTTP attach does not use the local
pager's uncharged one-shot fallback (`crates/devondb-storage/src/pager.rs:
354-375`), because an opaque network client buffer would otherwise weaken the
edge memory contract.

If the implementation receives into one contiguous batch and then copies into
per-page frames, both the transient batch and destination frames are charged
during the peak and the batch charge is released immediately after the copy.
An implementation that streams directly into reserved frame storage may avoid
that double peak, but it must prove the HTTP client does not buffer another
whole response internally. Retries retain the same reservation and overwrite
or clear the same buffers; they do not multiply the charge.

## Generation pinning and dual superblocks

The dual-slot law arbitrates two superblock headers within one main-file
generation. It does not authorize combining slot 0 from generation A with slot
1 or a catalog page from generation B. The attach bootstrap therefore pins the
generation before the first slot read, and **every** later range read carries
that condition.

On `412 Precondition Failed`, a changed generation token, or a changed total
length (per the request contract), the current range
fails with a generation-changed error. The backend is then poisoned: it serves
no later uncached request and never retries against a new token. Already
returned `PageRef` values remain immutable generation-A bytes, so a query
completed entirely from them is still sound; a query needing the failed range
errors rather than mixing. A new attach is the only automatic way to select
generation B.

Weak ETags, `Last-Modified`, content length alone, and an origin that cannot
perform conditional ranges are insufficient. Such an origin is refused at
attach unless the URL itself addresses an immutable object version and the
adapter can prove equivalent generation semantics. A user assertion that a URL
"usually does not change" is not a consistency mechanism.

Capability proof requires a negative conditional-range probe during attach
bootstrap, after the initial metadata
read pins the token: issue one ranged `GET` for a small in-bounds window
carrying `If-Match` set to a strong token guaranteed not to match the pinned
generation (for store-specific tokens, the equivalent impossible-condition
form). The origin must return `412 Precondition Failed` with no body consumed
into cache. A `200` or `206` proves the origin ignored the condition and the
origin is refused. If the probe instead returns `206`, the adapter then issues
one ranged `GET` carrying `If-Range: <pinned token>` (or equivalent) and
requires `206` with a `Content-Range`/token echo proving the response is bound
to the pinned generation; a full `200`, a missing or incomparable token echo,
or mismatched bytes fails attach. From then on every data read carries the
pinned condition and the response-token comparison of the request contract is
mandatory, not advisory — the generation pin exists only because these probes
and comparisons hold on every request.

## Failure semantics

Remote attach needs to distinguish transport/protocol failures from invalid
database bytes. Its structured error surface must not flatten HTTP status into
`Corrupt`. At minimum callers must
be able to distinguish:

- connection, DNS, TLS, reset, and timeout failures;
- HTTP authorization/not-found/rate-limit/server failures;
- malformed or incomplete range responses; and
- pinned-generation change.

All are non-`Corrupt`. `Corrupt` begins only after an exact byte range from the
pinned generation fails the existing DevonDB format rules. `ReadOnly` remains
the result for attempted mutation, and `BudgetExceeded` remains the result for
memory pressure. Error messages include the sanitized origin, operation,
range, attempt count, and status/error kind; they never echo credentials or
signed query parameters.

The default retry policy is synchronous and bounded:

- connect timeout: 2 seconds; whole-attempt timeout: 10 seconds;
- one initial attempt plus two retries;
- retry delays: 50 ms then 200 ms, capped by any `Retry-After` at 1 second;
- retry only idempotent bootstrap/range reads after timeout, connection reset,
  HTTP 408/429, or HTTP 500/502/503/504; and
- never retry authentication/authorization/not-found errors, malformed
  responses, invalid ranges, or generation change.

A partial body is discarded and the same complete conditional range is
retried; suffix-resume is out for the first increment. Every retry retains the
original generation token. Timeouts and retry count are configurable within
finite implementation caps, but "unbounded" is not an admitted setting.

## Cargo and dependency boundary

`httpfs` is an additive, off-by-default feature in `devondb-storage`, forwarded
by the `devondb` facade and any shipping binary that elects to expose attach.
The default feature set does not compile the HTTP backend, URL/auth parsing, or
TLS stack and gains no dependency. The read-only attach surface
and its option and diagnostic types are absent or explicitly cfg-gated when the
feature is off.

`HttpAttachOptions` carries, at minimum, the optional credential/header hook pair
(`on_request` receiving the outgoing request plus a redaction-safe writer sink;
the hook structurally cannot return strings that could reach logs), the
same-origin redirect allow-list switch (off by default, § Redirects), bounded
timeout/retry overrides within finite caps, and `spill_dir: Option<PathBuf>` —
the blocking-operator spill override. Default spill location is the per-process
collision-proofed subdirectory scheme of the OS temp dir (`docs/MVCC.md` §8.2:
pid + process token + attempt, `.lock` held for the handle
lifetime); an explicit `spill_dir` relocates that scheme under the given root.
Spill is never promoted into a persistent object cache.

The selected client must expose a blocking response reader and bounded
timeouts. It must not depend on Tokio, async-std, an executor, or a resident
background runtime. TLS and HTTP dependency candidates must be compared by
stripped `dist` binary delta, supported
TLS roots/platforms, conditional-range behavior, and evidence that response
buffering is budgetable. Any new dependency must remain behind `httpfs`.

## wasm32/OPFS as the second consumer

The trait is deliberately a positional byte-store trait rather than an HTTP
trait so OPFS can consume it later. The OPFS implementation requires:

- exact reads and writes at byte offsets, a current byte length, and durable
  flush, mapping directly to the four methods above;
- a synchronous `FileSystemSyncAccessHandle`, so the pager runs in a dedicated
  Web Worker rather than importing an async runtime into the core or blocking
  the browser main thread;
- explicit rejection when an offset or length cannot be represented exactly by
  the JavaScript/OPFS numeric API;
- short-read/write loops that satisfy the trait's exact-operation contract;
- the same pager cache, memory accountant, pinning, and page-size rules; and
- a capability choice made by its factory: a writable local OPFS backend may
  implement all four methods, while any read-only browser source refuses both
  write and sync just like HTTP.

Nothing in `PagerBackend` mentions native file descriptors, advisory locks,
HTTP headers, object generations, JS promises, or OPFS handles. Native thread
bounds and wasm worker ownership stay in their containing implementations.
The wasm target must compile the core without Unix `FileExt`; the present Unix
adapter (`crates/devondb-storage/src/pager.rs:219-238`) moves wholly inside
`LocalFileBackend`.

OPFS does not inherit HTTP generation semantics: it is a local mutable store
and will need its own publication/worker-ownership design before writes ship.
Sharing the byte seam is not permission to copy remote consistency rules onto
it.

## Deliberately out

- Writes, WAL append/replay over HTTP, checkpoint, compaction, and
  publish-to-object-storage. Source of truth for writes stays local; batching
  PUTs are a later increment under `docs/SCALE.md` §1.
- Treating a mutable remote key as a live multiprocess follower, including WAL
  watch, publication-gate emulation, or automatic refresh.
- Object listing, bucket discovery, multi-store catalogs, URL-prefix databases,
  and selecting among snapshots by listing generations.
- A persistent local disk cache, cache directory management, cache coherence,
  and offline reopening. Ordinary operator spill files are query scratch, not
  an object cache, and retain their existing budget/lifecycle rules.
- Full-object download, mmap of a downloaded copy, background prefetch workers,
  and an async runtime.
- Changes to `docs/FORMAT.md`, feature flags, main-file bytes, or WAL bytes.

## Verification requirements

1. Trait tests that run the same pager open/read corpus through local and a
   deterministic in-memory backend; compile-time/default-feature checks prove
   there is no `dyn PagerBackend` on the pager path.
2. A scripted HTTP origin recording requests and capable of exact `206`, short
   bodies, ignored ranges, malformed `Content-Range`, retryable statuses,
   timeouts, `412`, and generation swaps. Each failure asserts its non-Corrupt
   class.
3. A local-vs-HTTP end-to-end query equivalence test over catalog, node-group,
   CSR, multi-page catalog, and HNSW pages. The request log proves coalescing
   and the 16-page/256-KiB caps.
4. A severed generation test that changes the object between the two
   superblock reads and another that changes it mid-query. Neither may return a
   mixed result; no request after the change may omit the original condition.
5. Budget tests with a hard small `memory_limit`: prefetched frames and response
   buffers appear in the one accountant, eviction is attempted first, the
   window shrinks before failing, and all charges return to baseline on every
   error path.
6. Mutation tests covering every public write surface plus direct pager
   `write_page`, `allocate_page`, `sync`, `commit_superblock`, and
   `commit_feature_flags`; all return `ReadOnly` and the origin observes no
   write request.
7. Criterion local cache-hit/miss measurements with default features and
   `httpfs`, sequential/random HTTP request-and-byte measurements, and stripped
   binary-size results against both edge budgets.
8. Target checks proving the default native build has no HTTP/async dependency
   and a wasm32 seam compile check that does not import Unix `FileExt`.

## Rejected shapes

### `Arc<dyn PagerBackend>` in every pager

It is mechanically easy but adds a vtable call to local length/read/write/sync
without evidence. The closed enum keeps one concrete `Pager` for current
consumers, compiles to the local implementation when optional variants are
absent, and has an explicit measurement gate when they are present.

### A read-only trait with writes left above the seam

Making the backend trait expose only reads would leave `Pager::write_page`,
allocation, and both superblock publication paths reaching around it to a
`File`. That is not a pager backend and gives remote attach no defense in
depth. The four-operation trait represents every real pager I/O call; the
remote implementation refuses its write half.

### One GET per page

It preserves the current call shape but converts scan locality into API and
round-trip amplification. Bounded adaptive windows and single-flight
coalescing preserve random-read behavior while amortizing sequential misses.

### Download the whole object to a temporary file

This evades the trait, spends disk proportional to database size, delays first
query until full transfer, creates a disk-cache lifecycle, and makes freshness
look stronger than it is. It is outside the scope.

### Automatic ETag refresh

Repinning after `412` could combine already cached pages from generation A
with new misses from generation B. Clearing the cache is also insufficient for
already pinned `PageRef` values and executing snapshots. Generation change
poisons the handle; reattach constructs a new pager and cache.

### Reuse the multiprocess follower protocol remotely

The follower protocol relies on same-host lock sidecars and an atomically
observed main-file/WAL pair. An object URL has neither. Pretending otherwise
would invent a freshness bound and could omit or partially observe commits.

### Async HTTP in the core

It violates the synchronous-engine edge law, adds runtime/binary cost to an
optional reader, and does not help the current pull executor. The initiating
query thread performs bounded blocking requests; concurrency comes from normal
caller threads plus single-flight coordination.

### Last-Modified or length as a generation

Both can repeat across mutations and neither conditionally binds all range
responses to identical bytes. They cannot uphold the no-mixed-generation law.

## Open questions and decisions

1. **Sync HTTP/TLS client.** Which blocking client has the smallest measured
   `dist` delta while supporting conditional ranges, bounded streaming, proxy
   policy, and the required TLS roots on all native targets?
2. **Attach surface.** A typed source argument goes to a dedicated read-only
   attach method; `open(path)` is never overloaded. See § Scope and terminology.
3. **Generation token vocabulary.** Is a strong ETag sufficient for the first
   provider-neutral feature, or should options admit explicit S3 VersionId/GCS
   generation tokens from day one? The invariant is fixed; the token adapters
   are not.
4. **Redirects and credentials.** Redirects are same-origin only behind an explicit
   allow-list; credential/header hooks receive a writer sink and can never
   return strings that reach logs. See § Redirects and § Cargo and dependency
   boundary.
5. **Batch constants.** Do the proposed 1→2→4→8→16-page growth and 256-KiB cap
   win on the required sequential, CSR, HNSW, and random workloads? The hard
   bounded-policy requirement stands if measurements choose different values.
6. **Spill location.** The default is a per-process collision-proofed
   subdirectory of the OS temp dir, overridable via `HttpAttachOptions::
   spill_dir`; spill is never a persistent cache. See § Cargo and dependency
   boundary.
7. **Uncharged local fallback.** HTTP deliberately returns `BudgetExceeded`
   where today's local pager may allocate an uncached one-shot page. Should the
   implementation unify the local behavior under a charged transient
   reservation, or keep this documented backend difference until a separate
   memory-accounting change?
