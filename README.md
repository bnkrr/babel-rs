# babel-rs

## Development status

Most of this project's code was written by **OpenAI Codex**.

The project is **pre-1.0**: public APIs, configuration, and behavior may change
in breaking ways between 0.x minor releases. Pin the version you deploy, review
the [changelog](CHANGELOG.md), and test upgrades in your own environment before
production use. See the [compatibility policy](docs/SUPPORT.md#api-compatibility)
for the versioning contract.

## Purpose

`babel-rs` is an independent Rust implementation of the standard Babel
dynamic routing protocol. It speaks Babel on selected network interfaces,
maintains neighbour and route state, selects feasible paths, and exposes the
selected routing information either to an embedding application or to Linux
routing tables.

See [SUPPORT.md](docs/SUPPORT.md) for platform and compatibility contracts,
[CHANGELOG.md](CHANGELOG.md) for migration, and [RELEASING.md](docs/RELEASING.md)
for archive/consumer verification.

The [current RFC audit](docs/CONFORMANCE.md) records confirmed implementation
defects, missing capabilities and unverified cases. The current version is not
claimed to be a complete RFC implementation; successful tests and packaging
checks do not resolve those findings.

It interoperates on the wire with `babeld` and BIRD; neither is a runtime
dependency.

The project can be used at three layers:

- `babel-protocol` is a sans-I/O packet codec and deterministic protocol engine;
- `babel-router` is an embeddable Tokio UDP runtime with a route-export API;
- `babel-rs` is a Linux daemon that reconciles selected routes and policy rules
  through netlink.

The protocol and runtime crates contain no Linux netlink or daemon
configuration types. Applications may embed `babel-router`, subscribe to
selected-route snapshots, or implement `RouteExporter`; only standalone daemon
users opt into the Linux backend.

Embedding applications can implement `RoutePolicy` to accept learned routes and
control announcements per interface. Rules are read-only and replaced explicitly
through the engine or runtime; replacement reselects routes and retracts denied
announcements. See [route policy contracts and examples](docs/EMBEDDING.md#route-admission-and-announcement-policy).

## Current scope

The v0.6 implementation includes RFC 8966 base TLVs, neighbour maintenance,
feasibility, route selection, route and sequence-number requests, retractions,
and multi-hop propagation. It also implements RFC 9079 source-specific routes
and RFC 9229 IPv4 routes with IPv6 next hops.

Link quality is policy rather than an engine constant. The built-in profiles
implement RFC 8966 wired k-out-of-j sensing and ETX, plus RFC 9616 timestamp
sampling and its recommended RTT cost policy. Wired 2-out-of-3 with nominal
cost 96 is the default. Embedders can supply a different `MetricProfile` and
`MetricAlgebra` without replacing the protocol engine. Optional RFC 8967 MAC
authentication supports HMAC-SHA256 and BLAKE2s-128 with RFC 9467 replay protection.
RFC 8968 DTLS remains deferred.

The socket runtime and standalone daemon currently support Linux. The sans-I/O
protocol engine is independent of the operating system. It exports selected routes plus the
temporary exact unreachable routes required by RFC 8966 hold time. It owns only
its configured protocol and does not automatically redistribute the kernel
routing table; local origins come from configuration or the embedding API.

Outbound TLVs carry explicit monotonic deadlines from the protocol engine.
Each interface has an independent scheduler that adds bounded jitter,
aggregates compatible TLVs, and paces datagrams unless doing so would miss a
deadline. Packet boundaries are selected at release time from the live Linux
interface MTU; changing MTU does not require restarting the daemon.

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
interface must be administratively up. The default `control_transport = "ipv6"`
uses an IPv6 link-local address and multicast `ff02::1:6`. Set
`control_transport = "ipv4"` on a rule to use IPv4 UDP/6696 and `224.0.0.111`;
that mode requires an IPv4 interface address and works with IPv6 disabled.
Neither mode requires public IPv6 connectivity. Peers on a link must use the
same control family; Linux interface names need not match. Control transport
and the `ipv4_next_hop` route-announcement policy are independent.

Structured interface rules are checked in order and the first matching rule
wins. `link_type` supplies documented metric and split-horizon presets; timing
defaults remain common across all link types. Explicit values override the
preset for that interface:

```toml
[[interfaces]]
match = ["test-*"]
link_type = "tunnel"
ipv4_next_hop = "auto" # prefer numbered IPv4; ipv6 forces RFC 9229
```

An entry without metacharacters is an exact name. `*` and `?` match multiple
names, and starting with no current matches is valid. The daemon continuously
attaches new matches, withdraws routes when interfaces disappear, and rebinds
a same-name interface created with a new ifindex. See
[CONFIGURATION.md](docs/CONFIGURATION.md) for the complete default matrix,
override rules, and interval constraints.

RTT is an RFC 9616 modifier over a wired or ETX base. Its timestamp exchange is
backwards compatible with peers that do not implement the extension:

```toml
[interfaces.metric]
type = "rtt"
probe_interval_ms = 2000
# Optional time-based override; omit for per-sample alpha 0.836.
# half_life_ms = 6000
min_rtt_ms = 10
max_rtt_ms = 120
max_penalty = 150

[interfaces.metric.base]
type = "wired"
```

RTT is sampled independently on every live adjacency; one link cost is shared
by every route learned through that neighbour. The default filter uses the
RFC-recommended per-sample weight; an explicit half-life override instead
smooths by elapsed time. Route changes use a separate
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

## Daemon behaviour

Learned state has default global neighbor/candidate limits and a per-neighbor
candidate limit. Excess new entries are ignored while existing routes continue
to update, retract and expire; freed capacity is reused automatically. Optional
`[limits]` overrides require a restart. See [CAPACITY.md](docs/CAPACITY.md) for
defaults, status counters, and overload-isolation tests.

Selected-route generations are complete desired-state snapshots. A dedicated
worker coalesces intermediate generations and the two-second safety pass
reconciles the newest snapshot. Out-of-band deletion and stale owned state are
repaired while routes and rules owned by other protocols remain untouched.
Control status exposes the last successfully applied route and export-config
generations, together with the last success age and export error.
Export views support overlapping source prefixes with RFC 9079 destination-first
forwarding. Source tables inherit ordinary and covering-source routes, with
more-specific sources winning equal destinations. By default, managed export
allocates views for newly learned source prefixes. Static/external configurations
filter unsupported sources before selection and announcement. See [SADR.md](docs/SADR.md)
for table ownership, rule priorities, and migration from 0.5.0.

Optional per-interface [MAC authentication](docs/MAC.md) supports RFC 8967
HMAC-SHA256 and BLAKE2s-128, RFC 9467 replay counters, and live key rotation.
Configuring keys enables strict authentication; DTLS remains deferred.

`SIGHUP` parses and validates a complete candidate before committing interface
rules, origins, and export policy. An invalid candidate leaves the active
configuration unchanged. A changed interface policy is applied in place;
metric changes rebuild neighbour costs from retained Hello/IHU observations.
Router-ID, `state_file`,
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
cargo run -p babel-protocol --example packet
cargo run -p babel-router --example embedded -- wg0 /var/lib/my-router/state 0102030405060708
```

`BabelRouter::builder().start().await` starts an owned runtime; `wait()` observes
its result and `shutdown().await` performs bounded orderly cleanup. Dropping the
owner cancels its tasks without asynchronous cleanup. The builder accepts typed Router-ID, interfaces, originated
routes, a default `MetricProfile`, optional `MetricAlgebra`, `SequenceStore`,
and `RouteExporter`. `interface_with_policy` and
`RouterHandle::add_interface_with_policy` select metric, timing and split
horizon per interface. A profile creates independent per-neighbour state and
receives typed Hello, IHU, and RTT observations. `RouterHandle` also supports
originate and withdraw operations, dynamic interface changes, status, route
subscription, and graceful shutdown. The exporter receives a generation-tagged
full desired-state snapshot rather than an unrecoverable stream of deltas.

See [EMBEDDING.md](docs/EMBEDDING.md) for validation, command completion,
exporter and shutdown contracts, and [ARCHITECTURE.md](docs/ARCHITECTURE.md)
for the protocol, runtime, and exporter boundaries.

## Development and testing

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --all-targets
cargo test --workspace --doc
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps
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
reannounce, orderly-exit checkpoints, crash and lost-state recovery, stale-route cleanup, three-node
propagation, link failure and recovery, plus live-MTU packetisation under a
large route announcement.

The `shutdown-recovery` mode checks the total cleanup deadline and startup
removal of leftover routes/rules. It builds a test-only netlink fault preload
with `${CC:-cc}` locally and copies that fixture to the VM; the `all` suite
includes this mode. Configure the deadline with the top-level, reloadable
`shutdown_timeout_ms` (default 5000); see [CONFIGURATION.md](docs/CONFIGURATION.md).

The `control-clients` mode verifies actual 30-second client deadlines and
connection-slot reuse with 64 simultaneous Unix clients, including blocked
response readers; `all` includes it. Deterministic source-history churn across
multiple GC windows is part of `cargo test --workspace --all-targets`.

Network CI also runs capacity isolation, restart recovery, shutdown deadlines
and slow-client regressions as independent jobs. Combined-failure coverage
checks partition/merge and simultaneous primary-link loss with a standby-relay
crash, while an unaffected path continues forwarding. A separate steady-state job
checks a healthy three-node forwarding path while a fourth leaf repeatedly
fails and recovers: two minutes on pushes/PRs and one hour weekly or on demand.
Run it explicitly with `tests/e2e/run-on-linux-vm.sh steady-state`; see
[TESTING.md](docs/TESTING.md) for assertions, schedules and reproduction.

## License

Licensed under the MIT License. See [LICENSE](LICENSE).
