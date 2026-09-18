# Releasing devondb

This checklist is binding for every public tag. A release is a GitHub tag and
archive plus, when intended, an ordered set of crates.io publications. The
tag workflow never publishes to crates.io automatically: a partial workspace
publication is difficult to undo, so that step remains explicit.

The current checkout retains workspace version `0.1.1` while containing the
changes under `Unreleased`. Local archives and MCP bundles built from it are
development builds, even when their filenames and CLI version say `0.1.1`.
They must not be presented as the already published `v0.1.1` artifacts. A new
version, dated release notes, golden anchor, and completed gates below are
required before preparing a new versioned release.

## Before tagging

1. Choose the release commit only after the `CI` and `Release readiness`
   workflows are green.
2. Set `[workspace.package].version` and every exact internal dependency in
   `Cargo.toml` to the same version. Regenerate `Cargo.lock` and confirm
   `devondb --version` reports it.
3. Move the release notes in `CHANGELOG.md` from `Unreleased` to a dated
   `## [x.y.z] - YYYY-MM-DD` section and recreate an empty `Unreleased`
   section.
4. For every release that can write database files, mint
   `tests/golden/release-X.Y.Z.devondb` with that release candidate. Add it to
   `tests/golden/manifest.json` and to the post-freeze list in
   `crates/devondb/tests/format_freeze.rs`; never overwrite or re-mint an
   existing anchor. The tag workflow enforces the filename and both entries.
5. Run the release acceptance commands from the repository root:

   ```sh
   cargo fmt --all -- --check
   cargo clippy --locked --workspace --all-targets -- -D warnings
   cargo test --locked --workspace --no-fail-fast
   cargo package --locked --workspace --no-verify
   sh tools/mcpb/smoke.sh
   cargo build --locked --profile dist -p devondb-cli --no-default-features \
     --target-dir target/release-core
   ```

6. Confirm the feature-complete binary is no larger than 10 MiB and the core
   binary is no larger than 5 MiB. Confirm the release candidate opens every
   committed golden database without altering it or creating a WAL in the
   corpus directory.
7. Verify downstream consumers against the candidate: compile the C smoke
   programs in `crates/devondb-c`, build and import the Python extension in an
   isolated environment, and run the maintained integration examples/tests
   that use the embedded Rust API. Record their commands and results in the
   release report; workspace tests do not substitute for consumer checks.
8. On a clean macOS machine, install the matching MCP bundle in Claude
   Desktop, choose a sample database, and ask a known-answer question. The
   automated packed-binary smoke emulates the manifest and cannot certify
   the installation dialog, file picker, or platform trust prompts.
9. Tag the exact reviewed commit as `vX.Y.Z`. The tag must equal the workspace
   version with a `v` prefix.

## GitHub release artifacts

Pushing the tag runs `.github/workflows/release.yml`. It repeats formatting,
lint, tests, and packaging; verifies the tag, Cargo packages, and CLI version
agree; then builds feature-complete native Linux and macOS archives and MCP
bundles on x86_64 and aarch64 runners. The four native jobs also run on
release-related pull requests, so packaging is tested before tagging. Each
archive contains the binary, README, changelog, license, and the geo crate's
third-party notices. MCP bundles include both license files too. Every MCP bundle
is tested using its extracted binary on its own runner. The workflow publishes
both artifact types with a shared `SHA256SUMS` only after all gates pass.
Tags in the `v0.y.z` series are marked as GitHub prereleases to match their
alpha support status.

Download an archive and verify its checksum and version on a clean machine:

```sh
sha256sum -c SHA256SUMS
tar -xzf devondb-X.Y.Z-TARGET.tar.gz
./devondb-X.Y.Z-TARGET/devondb --version
```

On macOS, use `shasum -a 256 <artifact>` and compare it with the corresponding
line in `SHA256SUMS`. Bundle names also use full Rust target triples:
`devondb-X.Y.Z-TARGET.mcpb`. Select the matching architecture; the manifest's
platform constraint alone does not enforce CPU compatibility. Linux GNU
artifacts are built on Ubuntu 24.04 and inherit its system-library baseline.
See [`tools/mcpb/README.md`](../tools/mcpb/README.md) for the native matrix and
installation instructions. Foreign-runner success must be read from CI;
a local macOS smoke is not evidence of Linux execution.

## crates.io publication order

The publishable workspace crates must be published bottom-up because
crates.io resolves packaged registry dependencies rather than local paths. Wait for
each layer to become visible before publishing the next:

1. `devondb-geo`, `devondb-types`
2. `devondb-plan`, `devondb-storage`
3. `devondb-exec`
4. `devondb`
5. `devondb-nl`
6. `devondb-server`

Publish each crate with `cargo publish --locked -p <crate>`. Stop on the first
failure; do not skip a dependency layer. `devondb-c`, `devondb-python`, and
`devondb-cli` currently set `publish = false`; exclude them from crates.io
publication. Verify C/Python bindings through their local build and smoke
tests, and distribute the CLI through the native GitHub archives and MCP
bundles described above. Python wheel publication needs a separate build
and distribution matrix.

## After publication

Confirm the GitHub archives and crates.io pages carry the expected version,
license, repository link, and package description. Extract the matching CLI
archive in a clean environment, verify its checksum, run `devondb --version`,
create a database, reopen it, and run one query. Verify the published embedded
library from an isolated Rust consumer using registry dependencies. Finally,
add the released tag to the next release's
compatibility test matrix; the golden corpus itself is permanent and must
never shrink.
