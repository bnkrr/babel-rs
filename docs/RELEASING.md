# Release preparation

Use Rust 1.90+ to build the product. Use Cargo 1.96+ for the release checks below,
which package the three workspace crates together and stage their unpublished
dependencies locally. Python 3.11+ runs the archive consumer verification.

## Publication status — 2026-09-11

The [v0.5.0 release workflow](https://github.com/bnkrr/babel-rs/actions/runs/34486917845)
completed all validation jobs successfully, including archive consumers,
Windows/macOS protocol tests, Linux tests, E2E and the short steady-state run.
The publish job then failed during the crates.io credential exchange:
`No Trusted Publishing config found for repository bnkrr/babel-rs`.
No upload or post-upload registry verification ran in that workflow.

The public crates.io API returned HTTP 404 for `babel-protocol`, `babel-router`
and `babel-rs` on 2026-09-11. Initial publication and the per-crate Trusted
Publishing setup below remain outstanding. The local 0.6.0 archive rehearsal
passed without uploading; it does not establish hosted CI or registry success
for that version. Current verification and follow-ups are in
[CONFORMANCE.md](CONFORMANCE.md).

## Verify before uploading

Run the normal format, Clippy, workspace tests, doctests, rustdoc and MSRV checks
from the README, and the Linux network suite described in TESTING.md. Then run:

```sh
python3 tests/release/check-packages.py
```

For an uncommitted local review use `--allow-dirty`. `--offline` uses cached
registry dependencies; `--output PATH` retains archives and consumer artifacts.
The check packages all crates, verifies unpacked builds, checks packaged README,
MIT license and daemon example files, extracts the actual `.crate` archives, runs
an independent consumer, runs library archive tests/examples/doctests/docs, and
installs the daemon from its archive and checks its packaged example config.

The independent consumer substitutes only extracted archive dependencies using
a generated Cargo patch file. It never depends on the original workspace source
paths. This verifies the distribution contents without claiming that unpublished
names are already resolvable from crates.io. CI runs the same script.

Review the version, changelog, platform and lifecycle contracts in SUPPORT.md and
EMBEDDING.md. Library API/behavior changes need a minor bump during 0.x. Ensure
all three manifests and their versioned path dependencies agree. README links
should refer to public documentation rather than missing workspace-relative files.

## Registry and upload

Crates.io names and ownership must be checked live immediately before first
publication. A nonexistent name is not reserved by a successful local build.
Use the publishing account to confirm access to any existing names and use
appropriately scoped credentials or trusted publishing. Do not commit credentials.

Once publication is authorized and the reviewed release commit is fixed:

1. Run `cargo publish -p babel-protocol --dry-run --locked`, then publish it.
2. Confirm that crates.io resolves the exact version. Dry-run and publish
   `babel-router`, then confirm its version resolves.
3. Dry-run and publish `babel-rs`.
4. Run an unpatched registry consumer and `cargo install babel-rs --locked
   --version VERSION` from outside the repository. Record the result.
5. Record the release commit/tag and update the changelog release date.

Dependency ordering avoids relying on an unpublished package from an ordinary
single-package upload. A post-upload registry check is intrinsically a release
step; the local rehearsal cannot replace it. Each uploaded version is immutable.
No upload is performed by the verification script.

Cargo references: [Publishing](https://doc.rust-lang.org/cargo/reference/publishing.html),
[Package archives](https://doc.rust-lang.org/cargo/commands/cargo-package.html).

## Local credentials and CI Trusted Publishing

Local `cargo login` can use a token scoped to the exact three crate names with
`publish-new` and `publish-update`. `yank` and `change-owners` are not required
for publishing. Configuring trusted publishers through the crates.io website
also does not require `trusted-publishing` on that local token. Keep login and
publish on the same `CARGO_HOME` (this development checkout uses `.local/cargo`).

Publish the initial version of each crate manually first. Then, on each crate's
Settings / Trusted Publishing page, register the following GitHub publisher:

| Field | Value |
| --- | --- |
| Repository owner | `bnkrr` |
| Repository | `babel-rs` |
| Workflow filename | `release.yml` |
| Environment | `crates-io` |

Create the GitHub repository environment named `crates-io` and allow the `v*`
release tags to deploy to it. This environment needs no crates.io secret. Do not
copy the local token into GitHub Actions: the official `crates-io-auth-action`
exchanges the workflow's OIDC identity for a short-lived token and revokes it
when the job completes. Only the publish job has `id-token: write`; test jobs
have read-only repository permissions. Local token publication remains available
unless the crate's separate "Require trusted publishing" setting is enabled.

## Automated releases

`.github/workflows/release.yml` triggers on `v*` tag pushes. Before tagging, bump
the workspace version and versioned dependencies, update Cargo.lock, set the
changelog release date, and commit the release. Push the reviewed commit and a
tag matching the version (for example `v0.5.1` for version `0.5.1`).

The workflow validates the tag/version match, runs the reusable CI, E2E and
120-second steady-state workflows, and only publishes after all pass. This
includes the archive consumer, Windows/macOS protocol, Linux MSRV, network and
recovery checks. Ordinary branch/PR workflows still run independently; tag
pushes run these checks through the release workflow rather than twice.

Actions / release / Run workflow defaults to `dry_run: true`. That runs the same
checks on the selected branch or tag without requesting OIDC credentials or
uploading packages. A tag selected for rehearsal must still match the source
version. For a real manual dispatch, select the matching release tag and set
`dry_run: false`; real publication from a branch is rejected.

Publication runs `babel-protocol`, `babel-router`, then `babel-rs`, waiting for
each exact version in the registry index. A partial release can be rerun at the
same tag: an existing version is skipped only if its packaged Git commit matches
the current clean source and it is not yanked. Different source metadata,
authentication errors and registry failures fail the job. Never move a published
release tag to a different commit. Initial manual publication should use the
same clean release commit if its tag will later be used to exercise this flow.

After upload, a temporary unpatched project compiles against both registry
libraries, and the daemon is installed from crates.io with the exact version;
its version and example config are checked. This final registry verification
cannot run successfully until the versions exist. Local test coverage mocks
registry/upload operations; it does not claim a real OIDC exchange occurred.

References: [Official authentication Action](https://github.com/rust-lang/crates-io-auth-action),
[Trusted Publishing](https://crates.io/docs/trusted-publishing),
[Token scope definitions](https://rust-lang.github.io/rfcs/2947-crates-io-token-scopes.html).
