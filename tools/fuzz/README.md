# devondb local fuzz harness

This standalone stable-Rust project fuzzes the real DevonPlan and database
entry points without `cargo-fuzz`, nightly Rust, or a workspace/CI hook. Run:

```sh
sh tools/fuzz/run.sh smoke
FUZZ_MINUTES=15 FUZZ_SEED=0x276 sh tools/fuzz/run.sh standard
FUZZ_MINUTES=120 sh tools/fuzz/run.sh torture
sh tools/fuzz/run.sh regressions
```

The default seed is fixed. `FUZZ_SEED` accepts decimal or `0x` hexadecimal.
`standard` and `torture` give each target `FUZZ_MINUTES`; their defaults are
10 and 60 minutes. `smoke` is iteration-bounded and intended to finish in at
most three minutes, including regression replay.

## Targets and invariants

- `statements` builds grammar-valid, schema-coherent statement streams, then
  applies grammar-aware and raw-byte mutations. Each stream goes through
  `devondb::text::parser::parse` and the public `Database` executor in a fresh
  scratch database. Parse/execution errors are rendered as `error: ...`; after
  either success or error, the database must reopen.
- `plan_ir` mutates canonical `Plan` and `StatementEnvelope` JSON and calls
  both public decode paths. Inputs are charged to a 64 KiB byte budget; any
  successful canonical re-encoding is charged to a 4 MiB amplification
  budget. Invalid UTF-8 is a clean boundary error. These caps keep malformed
  lengths and nesting from turning into unbounded harness allocations.
- `container` builds checkpointed main-file, live-WAL, and DEVONPACK fixtures.
  It mutates dual superblocks, catalogs/directories, checksummed column
  payloads, CSR, WAL records, pack headers/directories, and compressed frames.
  Checksum-repaired mutations deliberately reach decoders below the checksum
  fence. Probes follow the recovered schema and scan both relationship
  directions including properties; a valid older empty superblock does not
  need to retain the original fixture's table names. Open and probe-query
  may succeed or return `DevonError::Corrupt` or a clean `BudgetExceeded`
  under the 8 MiB cap. Large positive row counts are legal reader inputs,
  and memory preflight may refuse before decoding a damaged payload. Other
  errors, panics and timeouts remain failures. The WAL fixture is captured
  with its writer still alive, and a regression proves its acknowledged
  insert is absent from the checkpoint base and recovered from the WAL.
- `nl` generates schema summaries with mixed scalar, timestamp, vector, and
  geo columns, relationship tables, and optional class plurals, labels, and
  verbs. Questions come from the versioned NL template vocabulary, then take
  token-aware and raw-byte mutations. Every case calls both
  `DeterministicCompiler::compile` and `compile_statement`: outcomes must be a
  plan, statement, or structured `NoParse`; duplicate compiles must serialize
  byte-identically; every plan must survive canonical print/parse equality;
  and every refusal must contain at most three nearest-template hints.

Every individual execution is a subprocess with a **5-second watchdog**.
Timeouts, nonzero exits, aborts, and fatal signals are failures. This catches
process-visible manifestations of UB; the harness does not claim the dynamic
instrumentation of a sanitizer.

## Corpora

Failures write exact input bytes to `corpus/crashes/*.bin`, plus a sibling
text file containing target, seed, outcome, full input hex, stdout, stderr,
and backtrace. `corpus/crashes/` is ignored.

Curate a fixed engine bug by copying its `.bin` into
`corpus/regressions/` without changing the filename. Filenames encode target
and seed as `<target>-<16-hex-seed>-...bin`. The regression directory is
committed, replayed before every fuzz tier, and can be replayed alone with
`run.sh regressions`.

Each target ends with:

```text
scoreboard target=<name> execs=<count> distinct_failures=<count>
```
