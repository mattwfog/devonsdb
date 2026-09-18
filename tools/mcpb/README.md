# Native devondb MCP bundles

An MCP Bundle (`.mcpb`) packages the read-only `devondb mcp <path>` tool
server for clients supporting binary MCP extensions. The bundle contains
its manifest, license, third-party notices, and a native executable; no
Node.js or Python runtime is needed to run the installed server.

The current workspace still reports `0.1.1`. Bundles built from this
unreleased source are **development builds**, including filenames containing
`0.1.1`; they are not the published `v0.1.1` release artifacts. Prepare a new
version and complete [`docs/RELEASING.md`](../../docs/RELEASING.md) before
labeling or distributing them as a versioned release.

## Build and verify

From the repository root, with Rust/Cargo, POSIX sh, zip, and Python 3:

```sh
sh tools/mcpb/smoke.sh
```

This builds the current source with `--locked --profile dist`, packages it,
and tests the **extracted binary** through the manifest's command and
arguments. Build alone with `sh tools/mcpb/build.sh`. To test an existing
native artifact without rebuilding:

```sh
sh tools/mcpb/smoke.sh tools/mcpb/build/devondb-0.1.1-aarch64-apple-darwin.mcpb
```

Replace the version and target with the artifact being tested. A mismatched
filename, workspace version, native target, or manifest fails verification.
`sh tools/mcpb/portability.sh` checks shell syntax, all six supported target
manifest renderings, and unsupported-target refusal without compiling code.

The scripts write only under `tools/mcpb/build/` and `target/`. The Cargo host
triple controls the target explicitly, so `CARGO_BUILD_TARGET` or a cached
foreign binary cannot silently change the package. Build output lives at
`target/<target>/dist/devondb`. The script enforces the 10 MiB binary budget.

## Artifacts and platform scope

The filename is `devondb-<version>-<Rust target>.mcpb` and the layout is:

```text
manifest.json
LICENSE
THIRD_PARTY_LICENSES.md
server/<Rust target>/devondb
```

Native release jobs build and smoke-test these four targets using
[GitHub's standard hosted runner labels](https://docs.github.com/en/actions/reference/runners/github-hosted-runners):

| Rust target | Runner |
|---|---|
| `x86_64-unknown-linux-gnu` | `ubuntu-24.04` |
| `aarch64-unknown-linux-gnu` | `ubuntu-24.04-arm` |
| `x86_64-apple-darwin` | `macos-15-intel` |
| `aarch64-apple-darwin` | `macos-15` |

GNU Linux bundles inherit their runner's libc/system-library requirements;
these are native Ubuntu 24.04 artifacts, not promises of compatibility with
older distributions. The packager also recognizes native
`x86_64-unknown-linux-musl` and `aarch64-unknown-linux-musl` toolchains, but the
release workflow does not publish musl bundles. Cross-compilation, Windows,
universal binaries, signing, and notarization are outside this release lane.

The manifest declares only the matching `darwin` or `linux` platform, names
the full target in its display name, and points at that target's executable.
MCPB 0.3 has no architecture compatibility field; choose the correct download
for the CPU. Do not infer that a client automatically rejects a wrong-CPU
bundle. See the [MCPB manifest specification](https://github.com/modelcontextprotocol/mcpb/blob/main/MANIFEST.md).

## Install

On a compatible macOS Claude Desktop installation, open the matching `.mcpb`
and select **Company database (.devondb)** in its file picker. The manifest
resolves `${__dirname}/server/<target>/devondb mcp <picked file>` and exposes
`schema`, `ask`, `query`, and `explain`. The server refuses writes through
these tools; see [`docs/MCP.md`](../../docs/MCP.md) for its open-mode law.

Linux artifacts are for an MCPB-capable Linux host, or extract and register
the binary's stdio command manually. Producing a Linux bundle does not claim
that Claude Desktop is available for Linux.

## What the smoke proves

The test validates the archive's exact file layout, executable mode, size,
version, platform, architecture-specific manifest paths, and the unchanged
third-party notices from `devondb-geo`. It seeds a graph
using the packed executable and launches MCP with paths containing spaces.
It checks initialization, the exact four-tool surface, an `ask` answer with
Grace and Linus, mutation refusal, and absence of the refused inserted row
when reopened. Subprocesses have a 60-second timeout.

This verifies the native binary on the host running the test. The release
matrix must pass on each platform before foreign-target execution is claimed.
The script emulates manifest substitutions; Claude Desktop's installation
dialog, file picker, and OS trust prompts still need a manual release check.
