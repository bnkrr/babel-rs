# Releasing babel-rs

This is the maintainer workflow for freezing source, verifying packages, and
publishing the three crates. User installation and deployment instructions are
in the [README](../README.md) and [configuration guide](CONFIGURATION.md).

## Current release state — 2026-09-11

The local candidate is **0.6.0**, intended for use within the documented support
scope. A local source freeze fixes the content to publish; it does not upload
crates, create a remote release, or establish hosted-CI success.

| Stage | Evidence / remaining action |
| --- | --- |
| Implementation | Protocol/runtime/daemon source at `8b9a416`; freeze preparation updates documentation, package description, and example configuration. |
| Local validation | Stable and Rust 1.90 each passed 180 workspace tests; Clippy, doctests, release tooling, and scoped Linux MAC/SADR/IPv4 checks passed. Archive-only consumers and daemon installation passed before the documentation freeze; rerun package checks on the final clean candidate. |
| Long-running mixed validation | 7.5-hour budget completed; 511 verified mutation rounds across two attempts, one unattributed babeld kernel-FIB mismatch, both cleanups passed. See [the full result](TESTING.md#2026-09-11-mixed-campaign). |
| Hosted verification | The frozen 0.6.0 commit still needs CI, including Windows/macOS protocol checks and network workflows. The remote main branch was `5e3b564` when last checked. |
| Registry | `babel-protocol`, `babel-router`, and `babel-rs` each returned HTTP 404 from the public crates.io API on 2026-09-11. No version of these crates was published by this preparation. |
| Automated publishing | The [v0.5.0 workflow](https://github.com/bnkrr/babel-rs/actions/runs/34486917845) passed validation, then failed with `No Trusted Publishing config found for repository bnkrr/babel-rs`. Upload and registry verification were skipped. |

A timed mixed campaign with a retained failure must not be described as a clean
pass. The observed peer mismatch is documented in
[INTEROPERABILITY.md](INTEROPERABILITY.md#mixed-run-kernel-route-mismatch); its
cause is not established. DTLS and a general daemon filter language are outside
this version's scope, not unfinished requirements for its local freeze.

## Freeze a local candidate

1. Confirm all three crate versions, versioned path dependencies, and Cargo.lock
   agree. Update the changelog, user documentation, support boundaries, and
   known observations. Keep the version at 0.6.0 for this candidate.
2. Commit the candidate content. If the working tree contains unrelated work,
   validate a separate clean detached worktree at that commit; do not include
   unrelated changes in the release.
3. Run the applicable source checks and the archive checks below on that clean
   candidate. Record the exact commit, toolchains, test results, Cargo.lock
   checksum, package checksums, and installed binary checksum with the artifacts.
4. Mark the validated commit with a local annotated freeze tag such as
   `freeze/0.6.0-20260911`. Freeze tags are local bookkeeping and do not match the
   publishing workflow's `v*` trigger. Keep the source and artifacts available
   for review. A later content change requires a new candidate/tag and the
   checks affected by that change; do not move an existing freeze tag.

The runtime network evidence can be reused for unchanged protocol/runtime source;
new packaged documentation and examples still require fresh package validation.
Local freezing does not require uploading anything or rerunning a long network
campaign solely because prose changed.

## Verify before uploading

Use Rust 1.90+ for source compatibility, Cargo 1.96+ for workspace packaging,
and Python 3.11+ for the archive consumer. Run the appropriate checks from
[CONTRIBUTING.md](../CONTRIBUTING.md) and the affected Linux suites from
[TESTING.md](TESTING.md). Then, from a clean candidate checkout:

```sh
python3 tests/release/check-packages.py --output /tmp/babel-release-check
```

Use a fresh output directory per candidate. `--offline` uses cached registry
dependencies. `--allow-dirty` is available for an intermediate review; the final
freeze should be verified from a clean checkout.

The check builds actual `.crate` archives, verifies README/license/example
contents, extracts the archives, and runs an independent consumer. Library
archive tests, examples, doctests, and rustdoc are checked; the daemon is
installed from its archive and its packaged configuration is validated. Its
consumer patches only extracted archive dependencies, never workspace source.
This establishes package usability without claiming unpublished packages can
already be resolved from crates.io. The script never uploads.

## First publication

Check live crate-name ownership and registry availability immediately before
upload. A local package build does not reserve a name. Use the publishing account
and the same `CARGO_HOME` for login and publication; credentials must stay outside
version control.

Publish from the reviewed clean release commit after hosted verification passes.
The initial publication can use a local token with `publish-new` and
`publish-update` scoped to the three crate names. `yank` and `change-owners` are
not required. Publish in dependency order:

```sh
cargo publish -p babel-protocol --dry-run --locked
cargo publish -p babel-protocol --locked
# Wait until this exact version resolves from crates.io before continuing.
cargo publish -p babel-router --dry-run --locked
cargo publish -p babel-router --locked
# Wait until this exact version resolves before publishing the daemon.
cargo publish -p babel-rs --dry-run --locked
cargo publish -p babel-rs --locked
```

Initial manual publication is independent of CI authentication setup. Once the
crates exist, configure each trusted publisher for future automated updates.
Use the same clean source commit if the initial publication will later be
verified or resumed through the release workflow.

After upload, run the actual registry consumer outside the workspace:

```sh
python3 scripts/publish.py verify
```

This resolves exact versions without local patches, compiles against both
libraries, and installs the daemon from crates.io. A user can then install with:

```sh
cargo install babel-rs --version '=0.6.0' --locked
```

Record the actual publication date, commit, tag, and registry verification
result. A source-freeze date is not a registry publication date. Uploaded crate
versions are immutable; later changes require a new version.

## Trusted Publishing

After initial publication, register this GitHub publisher on each crate's
Settings / Trusted Publishing page:

| Field | Value |
| --- | --- |
| Repository owner | `bnkrr` |
| Repository | `babel-rs` |
| Workflow filename | `release.yml` |
| Environment | `crates-io` |

Create the repository environment `crates-io` and allow release tags to deploy
to it. The official [crates.io authentication Action](https://github.com/rust-lang/crates-io-auth-action)
exchanges the workflow's OIDC identity for a short-lived token and revokes it
when the job completes. Only the publish job has `id-token: write`; no local
registry token needs to be copied into GitHub Actions.

Configuring publishers through the crates.io website does not need the local
token's `trusted-publishing` scope. Local token publication remains available
unless the crate's separate "Require trusted publishing" setting is enabled.
See [crates.io's configuration guide](https://crates.io/docs/trusted-publishing).

## Automated release and rehearsal

`.github/workflows/release.yml` runs on `v*` tag pushes. Push a tag matching the
workspace version only when publication is intended and authentication is ready.
The workflow validates the tag, runs reusable CI, E2E, and 120-second steady-state
workflows, then publishes `babel-protocol`, `babel-router`, and `babel-rs` in order.
It waits for each exact version to resolve and verifies registry consumers last.

Actions / release / Run workflow defaults to `dry_run: true`. This runs the same
checks without obtaining an OIDC token or uploading packages. A selected tag
must match the source version. For real manual dispatch, choose the matching
release tag and set `dry_run: false`; publication from a branch is rejected.

A partial release can be rerun at the same tag. An existing version is skipped
only when its packaged Git commit matches the clean source and it is not yanked.
Different metadata, authentication failures, and registry failures stop the job.
Never move a published tag to another commit. Publication from the frozen source
and the subsequent registry verification remain distinct from a local freeze.

References: [Cargo publishing](https://doc.rust-lang.org/cargo/reference/publishing.html),
[package archives](https://doc.rust-lang.org/cargo/commands/cargo-package.html),
[token scopes](https://rust-lang.github.io/rfcs/2947-crates-io-token-scopes.html).
