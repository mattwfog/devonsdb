# devondb local stress suite

This directory is a local-only torture harness for the shipped `devondb`
binary. It is deliberately outside the workspace and is not referenced by CI
or Rust tests.

Run the smoke gate with:

```sh
sh tools/stress/run.sh smoke
```

The other tiers are `standard` and `torture`. `torture` defaults to three
hours and a 12 GiB artifact ceiling; lower caps are accepted:

```sh
STRESS_MINUTES=90 STRESS_GB=6 STRESS_PROFILE=release \
  sh tools/stress/run.sh torture
```

`DEVONDB_BIN=/absolute/path/to/devondb` selects a prebuilt binary.
`STRESS_PROFILE=debug|release` selects the workspace build otherwise.
`STRESS_SKIP_BUILD=1` reuses existing binaries. Scenario-specific knobs are
named in each script header.

Every scenario writes evidence below `out/<scenario>/`, prints its seed, and
ends with one `PASS`, `FAIL`, or `PARK` line. `PARK` is reserved for a named
missing public surface. It does not conceal a failed content gate.

`followers.sh` activates a database through the real CLI, holds one real REPL
writer open, and drives persistent MCP and `--read-only` shell followers across
atomic write batches. It checks monotone committed-prefix reads, live-writer
`busy:` refusal, and successful writer takeover. `FOLLOWER_READERS` and
`FOLLOWER_BATCHES` scale the lane without introducing in-process helpers or
golden fixtures.

The volume battery validates every node value type exactly. Relationship
property values cannot currently be projected through DevonPlan (the public
`Expand` binding names the neighbor node), so the relationship gate validates
COPY acceptance, exact edge count, both endpoint directions, and the allowed
relationship property types at write/checkpoint/reopen time. A public
relationship-property binding is the exact missing surface for value-by-value
relationship checks.
