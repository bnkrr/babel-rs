# Release preparation

Use Rust 1.90+ to build the product. Use Cargo 1.96+ for the release checks below,
which package the three workspace crates together and stage their unpublished
dependencies locally. Python 3.11+ runs the archive consumer verification.

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
