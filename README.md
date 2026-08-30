# babel-rs

`babel-rs` is an independent Rust implementation of the standard Babel
routing protocol. It exchanges routes directly with `babeld` and BIRD; neither
is a runtime dependency.

The workspace has three deliberately narrow crates:

- `babel-proto`: sans-I/O packet codec and deterministic protocol engine;
- `babel-router`: embeddable Tokio UDP runtime and route-export API;
- `babel-rs`: Linux daemon with current-state to desired-state netlink
  reconciliation.

The protocol crates contain no Linux netlink or daemon configuration types. An
application can embed `babel-router`, subscribe to selected-route snapshots, or
implement `RouteExporter`; only daemon users opt into the Linux backend.

## Protocol profile

The v0.1 profile implements RFC 8966 base TLVs, neighbour maintenance,
feasibility, route selection, route/sequence requests, retractions and
multi-hop propagation. It also implements RFC 9079 source-specific routes and
RFC 9229 IPv4 routes with IPv6 next hops. RFC 9616 Timestamp sub-TLVs are
encoded and decoded; the default metric remains a replaceable fixed-cost
policy. RFC 8967/8968 authentication is not implemented.

See [CONFORMANCE.md](docs/CONFORMANCE.md) for exact claims and
[INTEROPERABILITY.md](docs/INTEROPERABILITY.md) for tested peers.

## Build and test

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --all-targets
cargo build --release -p babel-rs
```

The root-only black-box suite copies only the local binary and scripts to the
configured Debian VM:

```sh
BABEL_RS_E2E_HOST=router-test-vm tests/e2e/run-on-linux-vm.sh
```

`BABEL_RS_E2E_HOST` must name an SSH-accessible Linux VM with root privileges.
Set `BABEL_RS_SSH_CONFIG`, `BABEL_RS_CARGO_BIN`, or
`BABEL_RS_E2E_REMOTE_ROOT` when their defaults do not fit the local setup.

It tests `babeld`, BIRD, IPv4-over-IPv6, IPv6, source-specific routes,
withdraw/reannounce, persisted restart state, stale-route cleanup, three-node
propagation, link failure and recovery.

## Daemon

```sh
sudo babel-rs --config /etc/babel-rs.toml
```

Start from [examples/babel-rs.toml](examples/babel-rs.toml). Interface entries
are desired-state patterns: an entry without metacharacters is exact, while
`*` and `?` match multiple Linux interface names. The daemon may start before
any pattern matches. It continuously watches links and IPv6 addresses, attaches
administratively-up matches that have a link-local address, withdraws routes
when they disappear, and rebinds a same-name interface with a new ifindex.

The daemon owns only routes and rules matching its configured protocol. Every
route update and a two-second safety pass reconcile the complete selected-route
snapshot, so out-of-band deletion and stale owned state are repaired while
foreign protocols are untouched. Export views project ordinary and
source-specific routes into complete Linux tables. Standalone mode can manage
one source rule per view; an external manager can set `manage_rules = false`.

`SIGHUP` parses and validates a complete candidate before committing interface
patterns, origins, and export policy. An invalid candidate leaves the active
configuration unchanged. Router-ID and `state_file` identify persisted
protocol state and cannot change during reload. SIGINT and SIGTERM retract
local origins, then reconcile an empty snapshot. The process needs permission
to bind sockets to interfaces and modify routes.

## Embedding

Run the compile-checked examples:

```sh
cargo run -p babel-proto --example packet
cargo run -p babel-router --example embedded
```

`BabelRouter::builder()` accepts typed Router-ID, interfaces, originated
routes, a `LinkMetric`, `SequenceStore`, and `RouteExporter`. `RouterHandle`
supports originate/withdraw, dynamic interface changes, status, route
subscription and graceful shutdown. The exporter receives a generation-tagged
full desired-state snapshot rather than an unrecoverable stream of deltas.

## License

Licensed under the MIT License. See [LICENSE](LICENSE).
