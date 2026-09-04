# babel-rs

## Purpose

`babel-rs` is an independent Rust implementation of the standard Babel
dynamic routing protocol. It speaks Babel on selected network interfaces,
maintains neighbour and route state, selects feasible paths, and exposes the
selected routing information either to an embedding application or to Linux
routing tables.

It interoperates on the wire with `babeld` and BIRD; neither is a runtime
dependency.

The project can be used at three layers:

- `babel-proto` is a sans-I/O packet codec and deterministic protocol engine;
- `babel-router` is an embeddable Tokio UDP runtime with a route-export API;
- `babel-rs` is a Linux daemon that reconciles selected routes and policy rules
  through netlink.

The protocol and runtime crates contain no Linux netlink or daemon
configuration types. Applications may embed `babel-router`, subscribe to
selected-route snapshots, or implement `RouteExporter`; only standalone daemon
users opt into the Linux backend.

## Current scope

The v0.1 profile implements RFC 8966 base TLVs, neighbour maintenance,
feasibility, route selection, route and sequence-number requests, retractions,
and multi-hop propagation. It also implements RFC 9079 source-specific routes
and RFC 9229 IPv4 routes with IPv6 next hops.

Link quality is policy rather than an engine constant. The built-in profiles
implement RFC 8966 wired k-out-of-j sensing and ETX, plus RFC 9616 timestamp
sampling and its recommended RTT cost policy. Wired 2-out-of-3 with nominal
cost 96 is the default. Embedders can supply a different `MetricProfile` and
`MetricAlgebra` without replacing the protocol engine. RFC 8967/8968
authentication is not implemented. Deployments should therefore run Babel on
a protected link when authentication is required.

The standalone daemon is Linux-specific. It exports selected routes plus the
temporary exact unreachable routes required by RFC 8966 hold time. It owns only
its configured protocol and does not automatically redistribute the kernel
routing table; local origins come from configuration or the embedding API.

See [CONFORMANCE.md](docs/CONFORMANCE.md) for exact protocol claims and
[INTEROPERABILITY.md](docs/INTEROPERABILITY.md) for tested peers and
topologies. The conformance document also lists requirements that cannot be
proved by unit tests and must be checked in a deployment audit.

## Quick start

Build the daemon and validate the example configuration:

```sh
cargo build --release -p babel-rs
target/release/babel-rs check --config examples/babel-rs.toml
```

Run it with the privileges required to bind sockets to interfaces and modify
routes:

```sh
sudo target/release/babel-rs run --config examples/babel-rs.toml
sudo target/release/babel-rs status --socket /run/babel-rs/babel-rs.ctl
sudo target/release/babel-rs neighbors --socket /run/babel-rs/babel-rs.ctl
sudo target/release/babel-rs routes --socket /run/babel-rs/babel-rs.ctl
```

Start from [examples/babel-rs.toml](examples/babel-rs.toml). Each participating
interface must be administratively up and have an IPv6 link-local address.
`babel-rs` sends standard Babel packets over UDP/6696 to `ff02::1:6`; peers do
not need matching Linux interface names.

An interface entry without metacharacters is an exact name. `*` and `?` match
multiple names, and starting with no current matches is valid. The daemon
continuously attaches new matches, withdraws routes when interfaces disappear,
and rebinds a same-name interface created with a new ifindex.

The optional `[metric]` table selects a built-in policy. Omitting it uses the
RFC 8966 wired defaults:

```toml
[metric]
type = "wired"
nominal_cost = 96
received = 2
window = 3
```

RTT is an RFC 9616 modifier over a wired or ETX base. Its timestamp exchange is
backwards compatible with peers that do not implement the extension:

```toml
[metric]
type = "rtt"
probe_interval_ms = 2000
half_life_ms = 6000
min_rtt_ms = 10
max_rtt_ms = 120
max_penalty = 150

[metric.base]
type = "wired"
```

RTT is sampled independently on every live adjacency; one link cost is shared
by every route learned through that neighbour. The time-based half-life keeps
filter behaviour stable if probe timing varies. Route changes use a separate
local policy: after a newly discovered prefix has settled, an alternative must
clear both margins continuously for the configured dwell time. Initial
candidate discovery and loss of the current route bypass this delay. A
meaningful recovery of the current route cancels a pending switch, preventing
the tail of the RTT filter from moving traffic after a transient has ended.

```toml
[route_selection]
switch_margin_percent = 5
switch_margin_metric = 8
better_for_ms = 8000
```

ETX uses `type = "etx"` and an optional `window` in `1..=16` (default 6).
All algorithm defaults remain configurable in the daemon rather than being
embedded in the engine.

## Daemon behaviour

Selected-route generations are complete desired-state snapshots. A dedicated
worker coalesces intermediate generations and the two-second safety pass
reconciles the newest snapshot. Out-of-band deletion and stale owned state are
repaired while routes and rules owned by other protocols remain untouched.
Export views project ordinary and source-specific routes into complete Linux
tables. Standalone mode can manage one source rule per view; an external
manager can set `manage_rules = false`. Nonzero source-view prefixes must not
overlap, because the Linux policy-rule exporter admits only the unambiguous
subset where its lookup order is equivalent to RFC 9079 destination-first
selection.

`SIGHUP` parses and validates a complete candidate before committing interface
patterns, origins, and export policy. An invalid candidate leaves the active
configuration unchanged. Router-ID, `state_file`, metric policy,
route-selection policy, and the exclusive Linux route `protocol` identify live
protocol state and cannot change during reload; changing them requires a
restart. All locally originated routes are replaced in one serialized engine
operation, so a valid reload does not expose a partially updated origin set.
SIGINT and SIGTERM retract local origins and then reconcile an empty snapshot.

`babel-rs --config ...` remains accepted for v0.1 compatibility, while the
explicit `run` command enables the default control socket. Use
`babel-rs check --config ...` for a side-effect-free configuration check. See
[CONTROL.md](docs/CONTROL.md) for the bounded NDJSON API and all status,
inspection, transactional reload, and graceful shutdown commands.

A hardened standalone systemd unit is provided at
[`packaging/systemd/babel-rs.service`](packaging/systemd/babel-rs.service). Do
not enable it when another supervisor owns the daemon instance.

## Embedding

Run the compile-checked examples:

```sh
cargo run -p babel-proto --example packet
cargo run -p babel-router --example embedded
```

`BabelRouter::builder()` accepts typed Router-ID, interfaces, originated
routes, a `MetricProfile`, optional `MetricAlgebra`, `SequenceStore`, and
`RouteExporter`. A profile creates independent per-neighbour state and receives
typed Hello, IHU, and RTT observations. `RouterHandle` supports originate and
withdraw operations, dynamic interface changes, status, route subscription,
and graceful shutdown. The exporter receives a generation-tagged full
desired-state snapshot rather than an unrecoverable stream of deltas.

See [ARCHITECTURE.md](docs/ARCHITECTURE.md) for the protocol, runtime, and
exporter boundaries.

## Development and testing

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --all-targets
```

The root-only black-box suite builds locally, copies only the binary and test
scripts to an SSH-accessible Linux VM, and creates disposable network
namespaces:

```sh
BABEL_RS_E2E_HOST=router-test-vm tests/e2e/run-on-linux-vm.sh
```

Set `BABEL_RS_SSH_CONFIG`, `BABEL_RS_CARGO_BIN`, or
`BABEL_RS_E2E_REMOTE_ROOT` when their defaults do not fit the local setup. The
suite covers `babeld`, BIRD, IPv4-over-IPv6, IPv6, source-specific routes,
RFC 9616 RTT sampling, delayed multipath selection and hysteresis, withdraw and
reannounce, persisted restart state, stale-route cleanup, three-node
propagation, link failure, and recovery.

## License

Licensed under the MIT License. See [LICENSE](LICENSE).
