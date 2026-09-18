# Changelog

All notable user-visible changes to devondb are recorded here. The project
uses [Semantic Versioning](https://semver.org/spec/v2.0.0.html); releases in
the `0.y` series are alpha releases, while the on-disk format follows the
stronger compatibility contract in [`docs/FORMAT.md`](docs/FORMAT.md).

## Unreleased

### Added

- Read-only MCP stdio server with compact schema discovery, natural-language
  questions, plan queries, and explanations; native MCP bundles and release
  jobs for Linux/macOS on x86_64 and aarch64.
- Exact Decimal arithmetic and rounding, Timestamp/Bytes/Json node columns,
  UTC-day expressions, scalar subqueries, and Decimal median aggregation.
- `let` subplans with inner and left equality joins, ontology interface scans
  with `classof`, and scalar-subquery vectors for KNN queries.
- BM25 full-text queries with `textscan(Document.body, "rust", k=10) as d`
  and `scoreof(d)`. Integer scores rank results with primary-key tie breaks;
  scores compose with projections, traversals, joins, and correlated scalars.
  Execution uses the optional library `fts` feature, enabled by default in
  the CLI. Statistics use the snapshot's checkpoint corpus, with visible-row
  bootstrap when it has no documents or tokens. Checkpointing after mutations
  may change scores. See [`docs/FULLTEXT.md`](docs/FULLTEXT.md) for the approximation
  and memory-budget limits.
- Offline natural-language search with
  `search <table> <column> for "<literal>"` (`NL_VERSION` 10). The template
  resolves a String column and emits a deterministic top-10 TextScan;
  quoted query contents remain data, and unsupported forms receive a
  structured refusal.
- `ExpandRel` (`expand_rel Knows out as q via e`) binds relationship
  properties for filtering, projection, sorting, aggregation, joins, and
  scalar expressions. Traversals preserve parallel edges, direction order,
  transaction visibility, and snapshot results across checkpoint/reopen.
  Existing `expand` plans are unchanged. Supported properties are Bool,
  Int64, Float64, String, and vectors; relationship bindings cannot be used
  as traversal origins or with `classof`. Adjacency materialization may
  return `BudgetExceeded` when its working set exceeds the configured limit.
- Primary-key upsert and detach-delete, including incident relationship
  removal with transactional visibility and checkpoint/recovery support.
- Scalar/vector updates, deletes, and detach-delete on HNSW-indexed tables.
  Queries use exact search after a visible update or deletion until
  checkpoint rebuilds the affected indexes over the new row offsets.
  Rebuilds preserve index configuration and may retain an exact-search tail
  under memory pressure; scans that cannot fit return `BudgetExceeded`.
  Old snapshots, WAL recovery, and follower refresh preserve row visibility.
- Multiprocess activation and coordinated read-only followers with refresh.
- Relationship CSV loading, optional Parquet import, and compressed read-only
  `DEVONPACK` database distribution through the CLI, REPL, and MCP server.
- Adaptive column compression (constant, RLE, bit-pack/FOR, dictionary, FSST,
  and ALP) and typed column scans.

### Fixed

- Restore parsing at the supported 128-level scalar-expression nesting limit
  without overflowing the default thread stack; deeper input still refuses.
- Bound natural-language multi-hop refusal search so malformed relationship
  phrases cannot trigger exponential partition enumeration. Existing plans,
  ambiguity rules, and deterministic refusal hints are preserved.
- Report invalid persisted catalog root and continuation page references as
  `Corrupt`, preserving the distinction from invalid caller arguments.
- Reclaim clean cache frames before reserving Parquet row-group memory, so
  imports that fit the working-set budget do not fail because of cached
  endpoint pages. Insufficient memory still refuses atomically.
- Preserve rows deleted and reinserted under the same primary key within
  one transaction. Reinserts receive fresh physical identity: detach removes
  old incident edges while new relationships to the replacement survive
  commit, checkpoint, and WAL recovery.
- Serialize HNSW index construction with checkpoint/publication so concurrent
  mutations cannot install stale topology. Retire replaced index pages through
  the existing snapshot-aware reclamation path.
- Reuse retired checkpoint pages beyond the snapshot pin horizon to bound
  database growth under repeated churn; reclaim stale spill directories.
- Resolve schema and binding identifiers with consistent ASCII case folding
  while retaining their original spelling in catalogs and output.

### Release preparation

- Indexed mutation WAL uses the checkpoint-scoped `HNSW_MUTATION_WAL`
  compatibility flag (format bit 14). Older readers that do not support this
  flag refuse the file while it is set; checkpoint clears it only after WAL
  truncation. See [`docs/HNSW.md`](docs/HNSW.md) for mutation and rebuild limits.
- Native bundle verification exercises the extracted binary, manifest paths,
  MCP discovery and answers, and mutation refusal. Release archives and MCP
  bundles share checksums; actual foreign-platform execution is gated by CI.

## [0.1.1] - 2026-08-07

First crates.io release: the embedded library set (`devondb`,
`devondb-types`, `devondb-storage`, `devondb-plan`, `devondb-exec`,
`devondb-geo`, `devondb-nl`) is published so downstream Rust projects can
depend on `devondb = "0.1.1"` directly.

### Added

- Bulk `COPY` loading: a `copy` statement in the text language backed by a
  streaming RFC 4180 CSV reader and a WAL-free bulk node-group builder.
- `update` and `delete` statements in the text language, live end to end
  through MVCC overlays, checkpoint materialization, and the commit path.
- Inline cell editing in the UI grid with a show-the-plan trust loop over
  the new DML statements.
- A criterion scan benchmark baseline through the real `Database` read path.

## [0.1.0] - 2026-08-06

### Added

- The embedded Rust API, command-line shell, deterministic natural-language
  compiler, C API, and Python extension module.
- MVCC snapshots and concurrent transactions with commit-time conflict
  detection and WAL recovery.
- First-class vectors with exact and persistent HNSW search.
- `GeoPoint`, DevonGrid cells, and `within` queries.
- The single-binary local UI with graph, table, ontology, and map views.
- Frozen on-disk format version 1 with a permanent golden compatibility
  corpus.

### Release notes

- `v0.1.0` activates the public stable-format promise: from this release
  forward, every file a released devondb writes stays readable by every
  later release, enforced by the golden corpus and the format-freeze fence.
