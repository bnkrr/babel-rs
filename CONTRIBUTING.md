# Contributing to babel-rs

babel-rs is an independent Babel implementation. Changes should serve its
protocol library, embeddable runtime, or Linux daemon.

Use the [issue tracker](https://github.com/bnkrr/babel-rs/issues) for reproducible
bugs and feature discussions. For a larger change, describe the use case and
proposed public behavior first. A pull request should explain the problem,
resulting behavior, compatibility impact, and relevant validation. Disclose
substantial AI assistance and review generated code as you would any other
contribution; authorship does not substitute for correctness evidence.

## Local checks

Use Rust 1.90 or newer. From the repository root:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --all-targets --locked
cargo test --workspace --doc --locked
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --locked
python3 -m unittest discover -s tests/endless -p 'test_*.py'
python3 -m unittest discover -s tests/release -p 'test_*.py'
```

Run checks appropriate to the change. Protocol fixes should include a regression
that fails on the old behavior, preferably with independent wire fixtures or
an oracle. Network changes need the affected Linux forwarding/lifecycle checks;
see [Testing](docs/development/testing.md). Documentation changes need valid commands,
configuration examples, and links. Package contents or public examples also
need the [archive-consumer check](docs/development/releasing.md#verify-before-uploading).

## Compatibility and releases

Keep protocol decisions in `babel-protocol`, runtime ownership in `babel-router`,
and Linux daemon configuration/export in `babel-rs`. Public API, configuration,
and behavioral changes follow [Support](docs/guide/support.md#api-compatibility).
Document user-visible changes in [CHANGELOG.md](CHANGELOG.md).

Release preparation follows [Releasing](docs/development/releasing.md).

Contributions are made under the repository's [MIT license](LICENSE).
