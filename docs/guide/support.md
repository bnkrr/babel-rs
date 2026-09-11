# Support and compatibility

## Platforms and requirements

| Component | Supported scope |
| --- | --- |
| `babel-protocol` | OS-independent standard-library Rust codec/engine; no I/O or async runtime. Not `no_std`. |
| `babel-router` | Linux interface discovery and IPv4/IPv6 UDP sockets on Tokio; no daemon/netlink-export dependency. Non-Linux interface attachment returns Unsupported. |
| `babel-rs` | Linux daemon, netlink routes and policy rules, Unix control socket. |

Rust 1.90 is the minimum source-build version. CI includes Linux stable/MSRV
checks and Windows/macOS protocol tests. Binary releases target Linux x86_64
and ARM64 with static musl linking; they need no Rust toolchain or system musl
installation. See [binary installation](../../packaging/README.md) and the
selected version's workflow result for available artifacts and completed checks.

The runtime needs privileges to bind sockets to interfaces and use UDP/6696;
the daemon also modifies routes and policy rules. IPv6 control requires a
link-local address. IPv4 control needs an interface IPv4 address and works
with IPv6 disabled. The host provides interfaces, addressing, local reachability,
transit forwarding and firewall policy; see [Configuration](configuration.md).

## Operational limits

Most project code was written by **OpenAI Codex**. No independent security audit
is claimed. Pin deployed versions, test upgrades against your topology, and
retain a rollback path.

- [MAC](mac.md) authenticates routing messages without providing confidentiality.
  DTLS is not implemented.
- Abrupt restarts can need minutes to converge. Daemon state and `SequenceStore`
  provide orderly-exit checkpoints; embedding hosts own stable identity and
  restart policy. See [Embedding](embedding.md#export-and-shutdown-responsibilities).
- [Linux SADR](sadr.md) needs integration with external routes and policy rules.
  Materialization can grow quadratically; netlink changes are not atomic.
- Custom exporters must implement source-specific forwarding or gate unsupported
  sources, and opt in to IPv4-via-IPv6 only when the backend supports its forwarding
  and ICMPv4 requirements. See [Embedding](embedding.md#export-and-shutdown-responsibilities).
- [Capacity limits](capacity.md) bound admission and output queues, but do not
  guarantee protocol deadlines on an arbitrarily overloaded host.

## Interoperability

Focused Linux tests used babeld 1.13.1 and BIRD 3.1.7 on kernel
6.12.96+deb13-amd64. These are tested versions, not dependencies of babel-rs.
Validate other peer versions, kernels and network arrangements in the target
environment. [Testing](../development/testing.md#linux-network-suite) maps the
reproducible scenarios to their fixtures.

| Peer | Tested coverage |
| --- | --- |
| babeld 1.13.1 | IPv6, IPv4-via-IPv6, IPv6 SADR, numbered IPv4 on a shared LAN, RTT exchange, both MAC algorithms, withdrawal/recovery |
| BIRD 3.1.7 | IPv6, ordinary IPv4/IPv4-via-IPv6, IPv6 SADR, bidirectional route exchange |
| babel-rs peers | IPv4/IPv6 control, overlapping IPv4/IPv6 SADR transit, MAC rotation/restart, multipath and forwarding recovery |

BIRD tests use an ordinary IPv4 channel and an IPv6 SADR channel; IPv4 SADR
and Babel-DTLS interoperability are outside that coverage.

In the shared-LAN next-hop transition test, babeld retained an IPv4 gateway
after a neighbor switched its announcement to IPv6. Select a compatible
[IPv4 next-hop policy](configuration.md#ipv4-next-hop-policy) before establishing
routes with such peers; initial exchange does not establish support for every
live transition.

## API compatibility

All three crates use the same 0.x version. Breaking changes to public APIs,
configuration or documented behavior require a new minor version, such as 0.6
to 0.7. Patch releases preserve those contracts; corrections to documented
incorrect behavior are called out in the changelog. A routing fix can still
change path selection, so validate upgrades before deployment.

Following [Cargo's compatibility convention](https://doc.rust-lang.org/cargo/reference/semver.html),
`0.6` accepts compatible 0.6.x updates, while `=0.6.0` requests that exact version.
Applications should retain Cargo.lock; deployed daemons should use an explicit
version. The 0.x series has no 1.x compatibility commitment.
[CHANGELOG.md](../../CHANGELOG.md) records changes and migration guidance.

The control protocol's `api_version` is separate from the package version.
Runtime lifetime and completion contracts are in [Embedding](embedding.md).
