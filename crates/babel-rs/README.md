# babel-rs

A Linux Babel routing daemon with IPv4/IPv6 routing, overlapping source-specific
routes, optional MAC authentication, dynamic interfaces, and a local control API.

The daemon runs over existing interfaces and reconciles learned routes through
netlink. The host owns interface creation, addresses, forwarding, and firewall
policy. babeld and BIRD are supported peers, not runtime dependencies.

For library integration, use `babel-router` for Tokio or `babel-protocol` for
a synchronous engine. See the [project overview](https://github.com/bnkrr/babel-rs).

## Status and compatibility

Version 0.6.0 is usable within the documented support scope. Most project code
was written by **OpenAI Codex**. Automated tests and RFC review do not guarantee
correctness or replace an independent security audit. Pin deployed versions,
validate upgrades on your topology, and review the
[compatibility policy](https://github.com/bnkrr/babel-rs/blob/main/docs/guide/support.md#api-compatibility):
breaking API or behavior changes use a new 0.x minor version.

## Install and run

Linux is required. For a published crate, install with Rust 1.90 or newer:

```sh
cargo install babel-rs --version '=0.6.0' --locked
```

Alternatively, published [GitHub Releases](https://github.com/bnkrr/babel-rs/releases)
provide static Linux x86_64/ARM64 binaries. Follow the
[binary installation guide](https://github.com/bnkrr/babel-rs/blob/main/packaging/README.md);
these bundles do not require a Rust toolchain. GitHub binaries and crates.io
packages have independent publication schedules.

To build a repository checkout, run
`cargo build --release --locked -p babel-rs` and use `target/release/babel-rs`.

Start from the packaged `examples/babel-rs.toml` or the
[configuration example](https://github.com/bnkrr/babel-rs/blob/main/examples/babel-rs.toml).
Edit interface names and origin prefixes for the host before starting:

```sh
babel-rs check --config babel-rs.toml
sudo babel-rs run --config babel-rs.toml
```

From another terminal:

```sh
sudo babel-rs status
sudo babel-rs neighbors
sudo babel-rs routes
sudo babel-rs shutdown
```

The default control socket is `/run/babel-rs/babel-rs.ctl`. The example exports
ordinary routes to table 20000. Arrange host-owned IPv4/IPv6 policy rules that
query that table before the main table; see
[table setup](https://github.com/bnkrr/babel-rs/blob/main/docs/guide/configuration.md#host-networking-and-export-tables).
Each daemon owns an exclusive route protocol number in its network namespace
and leaves other protocols' routes alone.

## Deployment notes

- Interfaces must be up. IPv6 control needs a link-local address; IPv4 control
  needs an interface IPv4 address and works with IPv6 disabled. Peers must agree
  on the control family and permit UDP/6696.
- The host supplies routes to local origins and enables forwarding/firewall
  policy for transit traffic. Kernel routes are not automatically redistributed.
- Configured MAC keys enable strict authentication. MAC protects routing
  messages, not data traffic or confidentiality. DTLS is not implemented.
- Source views support overlapping prefixes and destination-first lookup.
  Integration with connected/static routes and other Linux policy rules belongs
  to the host. Netlink reconciliation is not an atomic FIB transaction.
- Abrupt restart can require minutes to recover retained sequence history.
  Await orderly shutdown where possible and monitor export health and forwarding.

## Documentation

- [Configuration and systemd installation](https://github.com/bnkrr/babel-rs/blob/main/docs/guide/configuration.md)
- [Control commands and health fields](https://github.com/bnkrr/babel-rs/blob/main/docs/guide/control.md)
- [MAC authentication](https://github.com/bnkrr/babel-rs/blob/main/docs/guide/mac.md)
- [Source-specific routing](https://github.com/bnkrr/babel-rs/blob/main/docs/guide/sadr.md)
- [Platform support and tested peers](https://github.com/bnkrr/babel-rs/blob/main/docs/guide/support.md)
- [Changelog](https://github.com/bnkrr/babel-rs/blob/main/CHANGELOG.md)
- [Issues](https://github.com/bnkrr/babel-rs/issues)

## License

MIT; see the packaged LICENSE.
