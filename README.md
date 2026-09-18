# devondb

An embedded graph database written from scratch in Rust: one file on disk, one
small binary, no server. Graph, vector, full-text and geo queries in the same
engine — and you can ask it questions in plain English.

```text
$ devondb ask people.devondb "who does ada know"
$ devondb ask people.devondb "how many people have age over 30"
```

The question compiles to a typed, deterministic logical plan
([DevonPlan](docs/PLAN_IR.md)). devondb shows you the plan it understood and
lets you pin it. A pinned plan is reproducible forever and never needs a
language model again — **natural language is a front-end, never the execution
semantics.** The built-in English compiler is itself deterministic and runs
fully offline ([`docs/NL.md`](docs/NL.md)).

## Why it exists

[Kuzu](https://github.com/kuzudb/kuzu), the embedded graph database many
projects depended on, was archived in October 2025. devondb fixes the four
things that hurt its users:

1. **A stable on-disk format.** Versioned, forward-compatible, and enforced by
   a golden-file corpus that CI opens and fully reads on every commit.
   Upgrading never requires export/reimport. Spec: [`docs/FORMAT.md`](docs/FORMAT.md).
2. **Concurrent writers.** MVCC snapshot isolation — readers never block;
   concurrent transactions with commit-time conflict detection
   ([`docs/MVCC.md`](docs/MVCC.md)).
3. **First-class vectors.** `VECTOR(dim)` is a core column type with KNN in the
   plan language and a persistent HNSW index ([`docs/HNSW.md`](docs/HNSW.md)) —
   not a bolt-on extension.
4. **A built-in UI.** `devondb ui` serves a graph explorer and map view from the
   same binary ([`docs/UI.md`](docs/UI.md)).

It is also **built for the edge**: meant to run beside an on-device model on a
Raspberry Pi-class machine. Memory is bounded as a contract — `memory_limit`
covers the page cache, MVCC versions and every operator, enforced in CI under a
hard rlimit — with a small single binary, no async runtime, no mandatory SIMD
baseline, and wear-aware writes
([`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) § Edge budget).

And it speaks to agents: `devondb mcp` is a read-only
[MCP](docs/MCP.md) server over stdio, with a one-click Claude Desktop bundle.

## At a glance

- ~100k lines of Rust across 11 crates: storage (pager, WAL, node groups),
  plan IR, vectorized executor, NL compiler, geo, server/UI, CLI, Python and C
  bindings.
- ~1,850 tests, plus fuzz targets and a stress harness under `tools/`.
- The on-disk format is **frozen at v1**; a freeze fence asserts the current
  binary still reads every pre-freeze file byte-for-byte.

## Status

**Alpha release candidate — the on-disk format is frozen at version 1.** The
compatibility rules are already mechanically enforced: a golden corpus of
databases written by earlier builds is opened and fully read on every commit,
and a dedicated freeze fence asserts that a current binary still reads every
pre-freeze file byte-for-byte unchanged. The stable-format promise is the
public release contract as of `v0.1.0`. From that release forward, upgrading
devondb never requires export/reimport — [rule
1](docs/FORMAT.md#compatibility-rules) can change only in a major release with
an in-place upgrade path.

Working today, end to end, in one binary: create and query a graph in the
[text plan language](docs/PLAN_IR.md) or in [plain
English](docs/NL.md) (deterministic, no model required, fully offline);
crash recovery via WAL replay; MVCC snapshots with concurrent readers;
`VECTOR(dim)` columns with brute-force and persistent HNSW search;
`GeoPoint` columns with DevonGrid cells and `within` queries; zone-map scan
pruning; bulk `COPY` loading from CSV; `update` and `delete` statements,
editable inline from the UI grid; a browser UI (`devondb ui`) with a graph
explorer and a map view; and Python and C bindings.

The storage lifecycle now includes engine-owned autocheckpointing (set
`DEVONDB_AUTOCHECKPOINT=off` to disable it), continuous reclamation, and the
`compact`, `stats`, and `dump` maintenance surfaces for offline live-content
rewrites, exact physical accounting, and canonical statement export
([`docs/FREE_PAGES.md`](docs/FREE_PAGES.md),
[`docs/FORMAT.md`](docs/FORMAT.md), [`docs/PLAN_IR.md`](docs/PLAN_IR.md)).

Multiprocess coordination is also shipped: `devondb activate` performs the
one-time offline activation; afterward `devondb <path> --read-only`,
`devondb ask ... --read-only`, and `devondb mcp` can run as follower readers
alongside the single writer ([`docs/MULTIPROCESS.md`](docs/MULTIPROCESS.md),
[`docs/MCP.md`](docs/MCP.md)).

The development checkout also includes relationship-property queries through
`expand_rel ... via e` ([examples](docs/RELATIONSHIP_QUERIES.md)) and deterministic
BM25 search with `textscan`, `scoreof`, and offline NL
`search <table> <column> for "literal"` ([full-text guide](docs/FULLTEXT.md)).
The CLI includes search by default; embedded applications enable the `fts`
feature. HNSW-indexed tables now support updates, deletes, and detach-delete;
queries use exact search until checkpoint rebuilds the index
([mutation and memory rules](docs/HNSW.md#57-indexed-mutations-and-checkpoint-repair)).
These additions are recorded in the [Unreleased changelog](CHANGELOG.md),
separately from the published `v0.1.1` release.

Not yet: object-storage attach, planned as a read-only runtime capability over
the pager-backend seam ([`docs/OBJECT_STORAGE.md`](docs/OBJECT_STORAGE.md)).
Expect rough edges in the surface, not in the file on disk.

The current release is `v0.1.1`. Files it writes use the frozen v1 format and
pass the permanent compatibility gates, including a golden anchor written by
the release binary itself.

## Build and try it

devondb requires Rust 1.95 or newer. Build from a checkout of a tagged
release (or this tree):

```sh
cargo build --locked --profile dist -p devondb-cli
./target/dist/devondb --version
./target/dist/devondb example.devondb
```

The last command creates the database when it does not exist and opens the
interactive shell. A minimal session is:

```text
create node table Person (id Int64 primary key, name String)
insert into Person values (1, "Ada"), (2, "Grace")
nodes(Person) as person | project person.name
.exit
```

The shipped command forms below follow the usage-string order. Each line is an
independent example; run the `activate` line before either `--read-only`
example.

```sh
./target/dist/devondb example.devondb --read-only                          # follower shell after activation (docs/MULTIPROCESS.md)
./target/dist/devondb ask example.devondb "show all people" --yes --read-only # deterministic NL follower query (docs/NL.md; docs/MULTIPROCESS.md)
./target/dist/devondb activate example.devondb                             # one-time offline activation (docs/MULTIPROCESS.md)
./target/dist/devondb ui example.devondb                                   # local browser explorer (docs/UI.md)
./target/dist/devondb mcp example.devondb                                  # read-only MCP server over stdio (docs/MCP.md)
./target/dist/devondb pack example.devondb example.pack                    # read-only DEVONPACK container (docs/SCALE.md §7)
./target/dist/devondb dump example.devondb                                 # canonical statement stream (docs/PLAN_IR.md)
./target/dist/devondb compact example.devondb                              # offline live-content rewrite (docs/FREE_PAGES.md)
./target/dist/devondb stats example.devondb                                # exact physical page accounting (docs/FREE_PAGES.md)
```

Usage errors exit with status `2`; a structured `ask` refusal exits with
status `3`.

Build the one-click Claude Desktop bundle (`.mcpb`) of the same MCP server
with `sh tools/mcpb/build.sh`.

Tagged releases will provide checksummed native Linux and macOS archives.
The release process and crates.io publication order are documented in
[`docs/RELEASING.md`](docs/RELEASING.md); changes are recorded in
[`CHANGELOG.md`](CHANGELOG.md), and private vulnerability reporting is
described in [`SECURITY.md`](SECURITY.md).

## Layout

| Crate | Purpose |
|-------|---------|
| `devondb-types` | Values, schema, `VECTOR(dim)`, errors |
| `devondb-storage` | Pager, superblock, WAL, node groups |
| `devondb-plan` | DevonPlan IR — the typed, versioned logical plan language |
| `devondb-exec` | Vectorized executor |
| `devondb` | Public embedded API |
| `devondb-nl` | Deterministic plain-English compiler to DevonPlan |
| `devondb-geo` | GeoPoint columns, DevonGrid cells, distance kernels |
| `devondb-server` | HTTP API, browser UI, MCP server |
| `devondb-cli` | Command-line shell |
| `devondb-python`, `devondb-c` | Python and C bindings (Arrow C Data Interface) |

Architecture: [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md).

## License

[MIT](LICENSE). The geo crate ports a few tables and numeric kernels from
`h3o` (BSD-3-Clause) and `rust-lang/libm` (MIT, with the Sun Microsystems
msun notice); their licenses are reproduced in
[`crates/devondb-geo/THIRD_PARTY_LICENSES.md`](crates/devondb-geo/THIRD_PARTY_LICENSES.md).
