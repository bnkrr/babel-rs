# Releasing babel-rs

GitHub binary releases and crates.io publication have separate triggers. They
share one version namespace: a given version always identifies the same source
commit and the three crates use the same version. A GitHub release may be
published without uploading crates, and crate publication can happen later.

| Trigger | Workflow | Result after checks pass |
| --- | --- | --- |
| New `vX.Y.Z` tag | [release.yml](../../.github/workflows/release.yml) | GitHub Release with changelog notes, Linux x86_64/ARM64 musl bundles and `SHA256SUMS` |
| New `publish/vX.Y.Z` tag | [publish.yml](../../.github/workflows/publish.yml) | Upload the three crates in dependency order and verify registry consumers |
| Manual dispatch of either workflow | Same workflow, rehearsal only | Validation and build/test artifacts; no GitHub Release or crate upload |
| Local `freeze/...` tag | Neither | Local candidate bookkeeping |

`X.Y.Z` must be a numeric MAJOR.MINOR.PATCH matching the workspace and dependency
versions; `v0.6.0` is valid. Suffixes such as `-release` and prerelease/build
suffixes are not accepted by this automation. The project retains the ordinary
[0.x compatibility policy](../guide/support.md#api-compatibility).

Both tag workflows run reusable CI (including package consumers), Linux E2E,
and a 120-second steady-state check on the tagged source. A failed check blocks
publication. Only new tag creation can upload; moved, forced or deleted tag
pushes do not publish. Do not move version tags after sharing them.

## Freeze a local candidate

1. Update the workspace version, all versioned path dependencies and Cargo.lock
   together. Add a nonempty `## X.Y.Z` changelog section with changes and migration
   guidance. An optional date suffix or `[X.Y.Z]` heading is accepted.
2. Review user documentation, support boundaries and migration guidance. Commit
   the candidate. Validate a clean checkout; use a detached worktree if needed
   to keep unrelated work separate.
3. Run the applicable source, archive and binary checks below. Record the commit,
   toolchains, results, Cargo.lock checksum and artifact checksums locally.
4. Create an annotated local freeze tag, for example `freeze/0.6.0-candidate.1`.
   Keep its source and artifacts available for review. A content change requires
   a new candidate/tag and the affected checks; preserve existing freeze tags.

Runtime network evidence can be reused when Rust implementation and runtime
fixtures are unchanged. Packaging changes still require fresh artifact checks.
A local freeze is separate from a hosted-CI result or an external publication.
Keep per-candidate results and local investigation notes with ignored artifacts;
public documentation describes the maintained behavior and procedures.

## Verify before uploading

Use Rust 1.90+ for source compatibility, Cargo 1.96+ for workspace packaging,
and Python 3.11+ for release tooling. Run applicable checks from
[CONTRIBUTING.md](../../CONTRIBUTING.md) and [Testing](testing.md). From a clean
candidate checkout, verify real crate archives and independent consumers:

```sh
python3 tests/release/check-packages.py --output /tmp/babel-release-check
```

Use a fresh output directory per candidate. `--offline` uses cached dependencies.
The archive check verifies README/license/examples, tests extracted libraries and
doctests, checks rustdoc, and installs the daemon from its archive. Its consumer
patches only extracted archive dependencies. The script never uploads.

For a local x86_64 Linux binary rehearsal, install the native musl toolchain and
binutils (`musl-tools binutils` on Debian/Ubuntu), then run:

```sh
rustup target add x86_64-unknown-linux-musl
CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=musl-gcc \
  python3 scripts/release.py build --target x86_64-unknown-linux-musl
python3 scripts/release.py smoke --target x86_64-unknown-linux-musl
```

On an ARM64 Linux host use `aarch64-unknown-linux-musl` and
`CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER`. Build and smoke run the binary
on the build host; these commands are intended for native builds. `--assets`
changes the default `.local/experiments/release/assets` output directory. Cargo
cache, target directory and toolchain settings remain caller-controlled.

The build checks static linking, daemon version, packaged configuration and
source metadata. Smoke runs the actual archived executable through the three-node
network fixture; it invokes `sudo` for that fixture and needs iproute2 and ping.
The release workflow performs both checks on native x86_64 and ARM64 runners.
Manual workflow dispatch runs full checks and retains the binary bundles as
Actions artifacts, without granting upload credentials.

## GitHub binary release

Choose the reviewed commit with the version and changelog already updated. For
example, if that commit is HEAD:

```sh
git tag -a v0.6.0 HEAD -m 'babel-rs 0.6.0'
git push origin v0.6.0
```

The workflow extracts the matching changelog section as release notes and adds
the source commit. Links to repository documents resolve at that commit. It
builds two static musl bundles, including the configuration, systemd service,
license, installation guide and `build-info.json`, then creates `SHA256SUMS`.
[Binary installation](../../packaging/README.md) explains their use.

Publication first creates a draft, attaches the validated assets, and makes it
public after upload succeeds. Rerun a failed workflow to resume the same draft.
Existing notes, source and assets must match; the script never overwrites an
existing asset or modifies an already published release. If a rebuild produces
different bytes (for example after a toolchain update), inspect the draft and
original artifacts before retrying. A completed matching release is a no-op.

Only the final GitHub release job has `contents: write`. This workflow has no
crates.io token or OIDC permission. GitHub Release completion is not a prerequisite
for the independent crate workflow.

## Automated crate publication

After the ordinary version tag exists, select versions for crates.io explicitly:

```sh
git tag -a publish/v0.6.0 'v0.6.0^{commit}' -m 'Publish crates 0.6.0'
git push origin publish/v0.6.0
```

`publish/v0.6.0` must resolve to the same commit as `v0.6.0`; mismatches stop before
publication. You can skip crate uploads for some GitHub versions and publish a
later version. There is no separate crate version sequence and no implicit
upload when a GitHub Release is created or edited.

The crate workflow independently reruns all required checks, obtains a
short-lived OIDC token, publishes `babel-protocol`, `babel-router`, then `babel-rs`,
waits for each exact version to appear in the index, and verifies registry
consumers. A retry skips an existing crate only if its packaged commit matches
the clean source and the version is not yanked. Other errors stop the job.
Uploaded crate versions are immutable; changes require a new version.

## First publication and Trusted Publishing

Check live crate-name ownership immediately before the first upload. A local
package build does not reserve a name. The initial publication can use local
`cargo login` credentials with `publish-new` and `publish-update` scoped to the
three crate names. `yank` and `change-owners` are not needed. Keep credentials
outside version control and use the same `CARGO_HOME` for login and publication.

From the reviewed clean version-tag commit, after hosted verification passes:

```sh
cargo publish -p babel-protocol --dry-run --locked
cargo publish -p babel-protocol --locked
# Wait until this exact version resolves from crates.io before continuing.
cargo publish -p babel-router --dry-run --locked
cargo publish -p babel-router --locked
# Wait until this exact version resolves before publishing the daemon.
cargo publish -p babel-rs --dry-run --locked
cargo publish -p babel-rs --locked
python3 scripts/publish.py verify
```

The final command checks exact registry versions without workspace patches,
compiles a library consumer and installs the daemon from crates.io. Record the
actual publication date and commit separately from the source-freeze date.

Once each crate exists, configure its Settings / Trusted Publishing entry:

| Field | Value |
| --- | --- |
| Repository owner | `bnkrr` |
| Repository | `babel-rs` |
| Workflow filename | `publish.yml` |
| Environment | `crates-io` |

If an entry previously named `release.yml`, update it to `publish.yml`. Create
the repository environment `crates-io` with deployment tags matching
`publish/v*`. Only the crate publish job receives `id-token: write`. The official
[authentication Action](https://github.com/rust-lang/crates-io-auth-action) exchanges
that identity for a temporary token and revokes it when the job ends. Do not
copy a local registry token into GitHub Actions.

Website configuration does not require the local token's `trusted-publishing`
scope. Local token publication remains available unless the crate's separate
"Require trusted publishing" setting is enabled. See the
[crates.io configuration guide](https://crates.io/docs/trusted-publishing).

References: [Cargo publishing](https://doc.rust-lang.org/cargo/reference/publishing.html),
[package archives](https://doc.rust-lang.org/cargo/commands/cargo-package.html),
[GitHub hosted runners](https://docs.github.com/en/actions/reference/runners/github-hosted-runners),
[GitHub Releases API](https://docs.github.com/en/rest/releases/releases).
