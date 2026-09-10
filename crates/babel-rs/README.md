# babel-rs

Independent Linux Babel routing daemon with dynamic interface attachment,
IPv4/IPv6 routing, source-specific policy views, optional RTT costs, atomic
configuration validation, a local control socket and reconciled netlink export.

Most of this project's code was written by **OpenAI Codex**. The project is
**pre-1.0**: public APIs, configuration, and behavior may change in breaking ways
between 0.x minor releases. Pin the version you deploy, review the
[changelog](https://github.com/bnkrr/babel-rs/blob/main/CHANGELOG.md), and test
upgrades in your own environment before production use. See the
[compatibility policy](https://github.com/bnkrr/babel-rs/blob/main/docs/SUPPORT.md#api-compatibility).

Install with `cargo install babel-rs --locked`. Start from the packaged
`examples/babel-rs.toml` or the
[configuration example](https://github.com/bnkrr/babel-rs/blob/main/examples/babel-rs.toml).

```sh
babel-rs check --config babel-rs.toml
sudo babel-rs run --config babel-rs.toml
sudo babel-rs status --socket /run/babel-rs/babel-rs.ctl
```

Linux is required. `control_transport = "ipv6"` defaults to link-local IPv6;
`"ipv4"` uses an IPv4 interface address and works with IPv6 disabled. Both use
UDP/6696 and can carry IPv4 and IPv6 routes when appropriate next hops exist. `ipv4_next_hop = "auto"` prefers ordinary IPv4 on numbered interfaces,
otherwise RFC 9229; `ipv4` and `ipv6` override this per interface.

This package is the executable. Embed `babel-router` for a Tokio runtime or
`babel-protocol` for a sans-I/O engine. No babeld or BIRD runtime dependency.

Optional RFC 8967 MAC authentication supports HMAC-SHA256, BLAKE2s-128 and live
key rotation; configuring keys enables strict reception. DTLS is deferred.
Linux SADR supports overlapping source prefixes and automatically materializes
complete source tables with destination-first forwarding. Static configurations
filter uncovered sources. Kernel routes are not automatically redistributed:
configure origins explicitly. Abrupt restarts may require minutes to recover
retained sequence history.

[Configuration](https://github.com/bnkrr/babel-rs/blob/main/docs/CONFIGURATION.md) ·
[Control API](https://github.com/bnkrr/babel-rs/blob/main/docs/CONTROL.md) ·
[Conformance](https://github.com/bnkrr/babel-rs/blob/main/docs/CONFORMANCE.md) ·
[Support](https://github.com/bnkrr/babel-rs/blob/main/docs/SUPPORT.md)

MIT licensed; see LICENSE.
