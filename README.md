# babel-rs

Babel routing in Rust, as a protocol library, an embeddable runtime, and a Linux daemon.

babel-rs exchanges routes over existing network interfaces and selects paths as
links and neighbors change. Use it on Ethernet, wireless, or tunnel networks,
or embed the routing engine in another application. It manages routing over
those links; interface creation, address assignment, and tunnel encryption
belong to the host. babeld and BIRD are interoperability peers, not dependencies.

## Status

Version **0.6.0 is usable for routing and embedding within the supported scope**.
It has passed local protocol/runtime tests, package-consumer checks, and Linux
forwarding and recovery tests. A 7.5-hour mixed run with babeld and BIRD verified
511 topology-change rounds across two attempts; one attempt ended with an
unresolved kernel-route mismatch on a babeld node. See the
[validation record](docs/history/0.6.0-validation.md) and
[known limitations](docs/guide/support.md#known-observations) before deploying.

Most of this project's code was written by **OpenAI Codex**. Tests and RFC review
provide evidence, not a guarantee of correctness or an independent security
audit. Pin the version you deploy, test it on your topology, and keep a rollback
path. The [compatibility policy](docs/guide/support.md#api-compatibility) describes which
updates may change public APIs, configuration, or behavior.

## Choose a component

| Component | Use it for | Platform |
| --- | --- | --- |
| [`babel-protocol`](crates/babel-protocol) | Synchronous packet codec and routing engine; the host supplies time and I/O | OS-independent Rust with `std` |
| [`babel-router`](crates/babel-router) | Tokio runtime, live interface management, route subscriptions, and a custom exporter | Linux |
| [`babel-rs`](crates/babel-rs) | Standalone daemon, TOML configuration, Linux routes and policy rules, local control commands | Linux |

Rust **1.90 or newer** is required. The libraries do not depend on the standalone
daemon. See [platform support](docs/guide/support.md) for the tested scope.

## Capabilities

- IPv4 and IPv6 routing with Babel feasibility, route selection, withdrawal,
  and recovery; IPv6 or IPv4 control transport per interface.
- Source-specific routing with overlapping source prefixes and
  destination-first Linux forwarding.
- Wired, ETX, and RTT-based link costs; configurable route-selection hysteresis.
- Optional HMAC-SHA256 or BLAKE2s-128 authentication, replay protection, and key rotation.
- Dynamic interface attachment, configuration reload, route-table reconciliation,
  bounded resource queues, and status inspection.
- Library hooks for route admission, announcements, metrics, persistence, and export.

The implementation covers RFC 8966, 9079, 9229, 9616, and MAC authentication from
RFC 8967/9467 within the boundaries in the [protocol coverage](docs/development/conformance.md).
DTLS is not implemented. Kernel routes are not automatically redistributed;
local origins are explicit. The daemon does not yet expose a general TOML route
filter language; embedders use `RoutePolicy`.

## Run the daemon

Build from a checkout on Linux:

```sh
git clone https://github.com/bnkrr/babel-rs.git
cd babel-rs
cargo build --release --locked -p babel-rs
cp examples/babel-rs.toml babel-rs.toml
```

Edit `babel-rs.toml` for your interface names and the prefixes reachable through
this node. Do not advertise the documentation prefixes unchanged. A minimal
configuration has this shape:

```toml
[[interfaces]]
match = ["eth0"]
link_type = "wired"

[[origins]]
destination = "2001:db8:100::/64" # replace with a prefix reachable through this node

[export]
protocol = 203 # reserve this protocol number for this daemon in the namespace

[[export.views]]
table = 20000 # a dedicated table for ordinary Babel routes
```

The daemon generates and persists a Router-ID when none is configured. Each
participating interface must already exist and be up. IPv6 control is the
default and requires a link-local IPv6 address. For IPv4 control, set
`control_transport = "ipv4"` in the interface rule and supply an interface IPv4
address. Peers on a link must use the same control family and permit UDP/6696.
The control family and advertised route family are separate choices.

The host must provide routes to its local origins and enable IP forwarding and
appropriate firewall policy when carrying transit traffic. The example exports
ordinary routes to table 20000. On a host with the usual Linux policy rules, add
these host-owned lookups once so traffic can use that table before the main table:

```sh
sudo ip -4 rule add priority 20000 table 20000
sudo ip -6 rule add priority 20000 table 20000
```

Choose unused table IDs and priorities appropriate for your host. The daemon
manages source-specific rules separately; it does not create or remove these
ordinary-table lookup rules. See [configuration](docs/guide/configuration.md#host-networking-and-export-tables)
for ownership and cleanup.

Validate and run:

```sh
target/release/babel-rs check --config babel-rs.toml
sudo target/release/babel-rs run --config babel-rs.toml
```

In another terminal, inspect or stop the instance:

```sh
sudo target/release/babel-rs status
sudo target/release/babel-rs interfaces
sudo target/release/babel-rs neighbors
sudo target/release/babel-rs routes
sudo target/release/babel-rs shutdown
```

Control commands use `/run/babel-rs/babel-rs.ctl` by default. For a supervised
installation, use the supplied [systemd unit](packaging/systemd/babel-rs.service)
and the [installation instructions](docs/guide/configuration.md#systemd-installation).
Versioned `cargo install` and publication instructions are in
[Releasing](docs/development/releasing.md).

## Embed the libraries

Use `babel-protocol` when the application owns transport and scheduling. Use
`babel-router` to let Tokio handle the sockets and runtime while your application
consumes route snapshots or implements `RouteExporter`.

The repository includes runnable examples:

```sh
cargo run --locked -p babel-protocol --example packet
cargo build --locked -p babel-router --examples
```

Start with the [protocol example](crates/babel-protocol/README.md),
[runtime example](crates/babel-router/README.md), and
[embedding guide](docs/guide/embedding.md). Retain the runtime owner for as long as
routing should run, and await `shutdown()` for orderly cleanup. Hosts own stable
identity, restart sequence policy, and their forwarding backend.

## Documentation

Start with the [documentation index](docs/README.md) to choose a guide by task:

- **Run and operate:** [Configuration](docs/guide/configuration.md),
  [Control commands](docs/guide/control.md), and [Support / tested peers](docs/guide/support.md).
- **Embed:** [Library contracts](docs/guide/embedding.md) and
  [protocol coverage](docs/development/conformance.md).
- **Contribute or release:** [Testing](docs/development/testing.md),
  [release procedure](docs/development/releasing.md), and [changelog](CHANGELOG.md).

## Help and contributions

Report bugs or propose changes through the
[issue tracker](https://github.com/bnkrr/babel-rs/issues). Include the version or
commit, OS/kernel, relevant configuration, expected behavior, and a bounded
reproduction or diagnostic log. Remove keys and other credentials from reports.
See [CONTRIBUTING.md](CONTRIBUTING.md) for local checks and contribution scope.

## License

[MIT](LICENSE).
