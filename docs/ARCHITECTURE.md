# devondb architecture

devondb is an embedded graph database written from scratch in Rust, picking up
where Kuzu fell short after its October 2025 archival. It is MIT licensed and
built as a standalone OSS product.

The four promises (each one a documented Kuzu failure):

1. **Stable on-disk format** — versioned, forward-compatible, golden-corpus
   enforced (`FORMAT.md`). Kuzu required export/reimport on most releases.
2. **Concurrent writers** — MVCC snapshot isolation. Kuzu was single-writer.
3. **First-class vectors** — `VECTOR(dim)` in the core type system with KNN as
   a plan operator. Kuzu's vector support was a late bolt-on extension.
4. **Built-in UI** — `devondb ui` serves a graph explorer from the single
   binary. Kuzu never shipped a first-party UI.

Plus the product bet no graph database has made: **the primary query surface
is natural language**, compiled to a deterministic plan IR.

One standing engineering constraint shapes the system: **devondb is built for
the edge** — see [Edge budget](#edge-budget) below.

## The two-layer query model

```
 "who at Acme touched CLO deals with us last quarter"
        │
        ▼
 NL intent compiler  (pluggable: LLM endpoint / template matcher / local model)
        │  emits, and SHOWS THE USER for confirm/pin:
        ▼
 DevonPlan IR        (typed, versioned logical plan — docs/PLAN_IR.md)
        │  the real language: what bindings emit, tests pin, the wire carries
        ▼
 Vectorized executor (devondb-exec)
        │
        ▼
 Storage engine      (devondb-storage — docs/FORMAT.md)
```

Rules that keep this honest:

- The engine **never** depends on an LLM. NL is a front-end. A devondb with no
  network and no model configured is fully functional via DevonPlan.
- Every NL compilation is surfaced as a plan the user can read, confirm, and
  **pin**. Pinned plans re-execute identically forever — reproducibility is a
  property of the IR, not of the model.
- DevonPlan is versioned exactly like the storage format: additive changes
  behind version gates, canonical serializations (compact text + JSON) that
  old readers reject cleanly rather than misread.

## Crates

| Crate | Responsibility | May depend on |
|-------|----------------|---------------|
| `devondb-types` | Values, schema model, `VECTOR(dim)`, workspace error type | (nothing internal) |
| `devondb-storage` | Pager, dual superblocks, WAL, node groups, recovery | types |
| `devondb-plan` | DevonPlan IR: operator tree, expressions, text/JSON forms | types |
| `devondb-exec` | Vectorized (chunk-at-a-time) executor over storage | types, plan, storage |
| `devondb` | Public embedded API: open, transact, run plans | all of the above |
| `devondb-cli` | Shell/REPL (DevonPlan text form now, NL surface later) | devondb |

Dependency direction is strictly downward in this table. `devondb-types` stays
small and engine-free.

## Edge budget

devondb targets edge-AI deployments: aarch64 Linux SBCs (2–16 GB RAM,
eMMC/SD wear-sensitive storage), phones, and dev machines, running beside a
resident 1–4B-parameter LLM that already owns 3–5 GB of RAM. The database
gets **tens to a few hundred MB**.

Binding constraints enforced as hard CI gates:

1. **Bounded memory is a contract.** `memory_limit` is first-class config;
   the page cache, MVCC version storage, and blocking operators
   (Sort/Aggregate, later joins) all charge against one budget. Blocking
   operators spill to disk past the budget (DuckDB model); clean pages are
   evicted by re-reading the main file, never re-written to temp. No graph
   or vector engine ships this as a guarantee — it is a headline feature,
   verified in CI by running a golden workload under a hard rlimit.
2. **Buffer pool: budgeted user-space page cache, no mmap** (redb's path —
   mmap removed there for soundness; ~2× worst-case vs mmap is the accepted
   cost). Designed together with MVCC pin-counting in `docs/MVCC.md`:
   retrofitting eviction under pins is where storage engines grow their
   worst bugs.
3. **Binary budget: core CLI ≤ 5 MB stripped; feature-complete single
   binary (UI included) ≤ 10 MB.** Context: SQLite 0.6–0.75 MB, Turso
   15.7 MB, DuckDB 61.9 MB with no lite build. Heavy subsystems are cargo
   features (`ui`, `nl`, `hnsw`, `fts`), off by default in the library
   crate — omission is designed in from the start (SQLite's retrofitted
   OMIT flags mostly don't work; DuckDB never dieted, it modularized).
4. **No async runtime in the core.** The engine is synchronous. `devondb
   ui` uses a small synchronous HTTP server behind the `ui` feature — it is
   a local single-user explorer and does not need tokio.
5. **SIMD via runtime dispatch only — no mandatory CPU baseline.** Distance
   kernels compile per-ISA variants (NEON/NEON_F16/SVE on ARM;
   SSE/AVX2/AVX-512 on x86) selected once by a capability probe
   (SimSIMD/NumKong pattern). A binary that SIGILLs on an old CPU is a lost
   user — LanceDB lost paperless-ngx to exactly this.
6. **eMMC-aware writes.** WAL and checkpoint paths minimize write
   amplification; wear-sensitive storage is the default assumption.
7. **Targets:** aarch64-linux (gnu + static musl) and x86_64-linux/macOS are
   checked in CI. wasm32 (browser/OPFS) is a tracked post-v0.1.0 goal —
   the no-tokio core keeps it feasible; PGlite (<3 MB gzipped) is the bar.

## Storage engine

- **Single main file + WAL sidecar** (`db.devondb` + `db.devondb-wal`) — the
  SQLite/DuckDB model; a database is a file you can copy.
- Page-based with **dual superblocks** (pages 0 and 1, alternating writes,
  checksum + checkpoint-LSN arbitration) so superblock updates are atomic.
- **Node groups**: nodes are stored in fixed-size groups with columnar
  property chunks per group. Adjacency is CSR per node group **plus a delta
  overlay** merged at checkpoint — writes append deltas instead of rebuilding
  CSR (Kuzu's CSR rebuilds made writes expensive).
- **WAL + checkpoint** crash safety, with kill -9 recovery tests.
- **Budgeted user-space page cache** (no mmap): every cached page charges
  the global `memory_limit`; clean pages are evicted by re-reading the main
  file. Pin-counting is designed with MVCC in `docs/MVCC.md` so the cache
  is bounded from birth (Edge budget §1–2).
- Format discipline: see `FORMAT.md`. Any commit that changes bytes on disk
  updates the spec and the golden corpus in the same commit.

## Concurrency

- MVCC snapshot isolation. Readers take a snapshot and never block, ever.
- v1: concurrent transactions with optimistic commit-time conflict detection
  and a physically serialized commit pipeline (DuckDB-style).
- Later: parallel writers via per-node-group latching. Naming stays honest:
  we say "concurrent transactions" until physical write parallelism is real.

## Vectors

- `VECTOR(dim)` (f32) is a core column type from schema v0.
- v1: brute-force KNN as a DevonPlan operator (`KnnScan`), SIMD-dispatched
  per Edge budget §5.
- Quantized vector storage is shipped in the frozen v1 format
  (`FORMAT.md` § Quantized vector element encodings): f16 elements, i8 scalar
  quantization, and 1-bit RaBitQ-family rotational quantization with
  oversample+rescore.
- Persistent, MVCC-aware HNSW is shipped inside the storage engine
  (`docs/HNSW.md`; `FORMAT.md` § HNSW index pages). Blueprint:
  **NaviX (Kuzu, VLDB 2025)** — HNSW adjacency stored in the DB's own CSR
  structures and buffer manager, inheriting transactions and the memory
  budget; beats DiskANN in disk-bound regimes.
- Full-text (BM25) follows the same path afterward.

## Execution

Chunk-at-a-time (~2048 rows) Volcano-style pull executor first. Morsel-driven
parallelism later — edge devices have few cores; single-threaded performance
per watt comes first. Correctness and format stability before speed; every
operator has known-answer tests. Blocking operators (Sort/Aggregate,
later joins) spill to disk under `memory_limit` (Edge budget §1).

## UI

`devondb ui` starts a local server from the single binary and serves an
embedded SPA: NL query box, plan-confirmation view (the trust loop), table
results, and force-directed graph rendering. The UI is where the NL surface
lives — they are one product, not two features. It ships behind the `ui`
cargo feature on a small synchronous HTTP server (Edge budget §3–4) — the
library core never pays for it.

## Surfaces & bindings

Rust embedded crate → CLI REPL → Python bindings (pyo3) → C FFI → server
mode + UI → wasm32 (post-v0.1.0). Priority integration targets once bindings
exist include GraphRAG stacks that lack an embedded graph-and-vector backend.

## Testing strategy

- **Golden-file corpus** (`tests/golden/`): database files created by every
  released version must open under HEAD. Grows forever, never shrinks.
- **Known-answer query tests**: every executor operator against fixed data.
- **Property tests** (proptest) for storage encode/decode round-trips.
- **Crash tests**: kill -9 harness around WAL/checkpoint from the first
  storage milestone.
- A feature is complete only when a test drives the user-facing input and
  asserts the user-visible output.
