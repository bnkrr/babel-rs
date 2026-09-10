# babel-rs

Independent Linux Babel routing daemon with dynamic interface attachment,
IPv4/IPv6 routing, source-specific policy views, optional RTT costs, atomic
configuration validation, a local control socket and reconciled netlink export.

Install with `cargo install babel-rs --locked`. Start from the packaged
`examples/babel-rs.toml` or the
[configuration example](https://github.com/bnkrr/babel-rs/blob/main/examples/babel-rs.toml).

```sh
babel-rs check --config babel-rs.toml
sudo babel-rs run --config babel-rs.toml
sudo babel-rs status --socket /run/babel-rs/babel-rs.ctl
```

Linux is required. Participating links need IPv6 link-local addresses. The
daemon uses UDP/6696 over IPv6 for control traffic and can route both IPv4 and
IPv6. `ipv4_next_hop = "auto"` prefers ordinary IPv4 on numbered interfaces,
otherwise RFC 9229; `ipv4` and `ipv6` override this per interface.

This package is the executable. Embed `babel-router` for a Tokio runtime or
`babel-protocol` for a sans-I/O engine. No babeld or BIRD runtime dependency.

Authentication is absent; deploy on protected links. Abrupt restarts may require
minutes to recover retained sequence history. The Linux source-specific exporter
rejects overlapping nonzero source views. Kernel routes are not automatically
redistributed: configure origins explicitly.

[Configuration](https://github.com/bnkrr/babel-rs/blob/main/docs/CONFIGURATION.md) ·
[Control API](https://github.com/bnkrr/babel-rs/blob/main/docs/CONTROL.md) ·
[Conformance](https://github.com/bnkrr/babel-rs/blob/main/docs/CONFORMANCE.md) ·
[Support](https://github.com/bnkrr/babel-rs/blob/main/docs/SUPPORT.md)

MIT licensed; see LICENSE.
