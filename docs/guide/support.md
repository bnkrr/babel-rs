# Support and compatibility

Version 0.6.0 is usable as a Babel protocol library, an embeddable Linux runtime,
and a standalone Linux routing daemon. The support scope below describes the
implemented interfaces and deployment requirements; version numbering describes
compatibility, not whether the software can run.

## Platforms and requirements

| Component | Supported scope |
| --- | --- |
| `babel-protocol` | OS-independent standard-library Rust codec/engine; no I/O or async runtime. Not `no_std`. |
| `babel-router` | Linux interface discovery and IPv4/IPv6 UDP sockets on Tokio; no daemon/netlink-export dependency. Non-Linux interface attachment returns Unsupported. |
| `babel-rs` | Linux daemon, netlink routes and policy rules, Unix control socket. |

Rust 1.90 is the minimum source-build version. Release tooling uses Cargo 1.96+
for workspace packaging and Python 3.11+ for archive-consumer checks. That Cargo
version is a packaging requirement, not the library's minimum Rust version.

CI covers Linux stable/MSRV and Windows/macOS protocol tests. Use the target
commit's CI result. The dated [validation record](../history/0.6.0-validation.md)
identifies tested source, kernels and peer versions; it is not live CI status.
Other kernels, architectures, and network arrangements need validation in the
target environment.

The runtime needs privileges to bind sockets to interfaces and use UDP/6696.
The standalone daemon also modifies routes and policy rules. IPv6 control is
the default and requires a link-local address; IPv4 control needs an interface
IPv4 address and works with IPv6 disabled. The host provides interfaces,
addressing, routes to local origins, transit forwarding, and firewall policy.
The software does not create tunnels or encrypt application traffic.

## Validation and operational limits

Most of the project's code was written by **OpenAI Codex**. RFC review, automated
tests, archive-consumer checks, and Linux forwarding tests provide evidence for
the documented behavior. No independent security audit is claimed. Review the
[known interoperability observations](#known-observations),
pin deployed versions, test upgrades against your topology, and retain a rollback
path. Successful tests do not establish correctness for every deployment.

- MAC authentication supports HMAC-SHA256 and BLAKE2s-128 with RFC 9467 replay
  counters. Keyed interfaces default to strict verification. Authentication
  does not provide confidentiality; DTLS is not implemented. See [MAC](mac.md).
- Abrupt restarts can need minutes to converge. `SequenceStore` and daemon state
  provide orderly-exit checkpoints, not crash-safe runtime sequence persistence.
  Stable identities and restart policy belong to embedding hosts.
- Linux SADR supports overlapping prefixes and destination-first forwarding.
  Additional connected/static routes and external policy rules need host
  integration. Materialization can grow quadratically; individual netlink
  operations do not form an atomic transaction. See [SADR](sadr.md).
- Custom exporters must implement source-specific forwarding or gate unsupported
  sources. They must opt in to IPv4-via-IPv6 only if they support RFC 9229
  forwarding and ICMPv4 on unnumbered links; the runtime rejects it by default.
- Admission limits, output budgets, and timing counters bound and expose load.
  They do not guarantee protocol deadlines on an arbitrarily overloaded host.
  See [Capacity](capacity.md).

The [0.6.0 validation record](../history/0.6.0-validation.md) separates protocol,
MAC/SADR/IPv4, packaging, and mixed-soak evidence. A retry or a planned stop does
not turn a campaign containing a failure into a clean pass.

## Interoperability

The recorded Linux tests used babeld 1.13.1 and BIRD 3.1.7 on kernel
6.12.96+deb13-amd64. These are tested versions, not dependencies of babel-rs.
Results apply to the documented scenarios; validate other peer versions and
live configuration transitions before relying on them.

| Peer | Recorded coverage |
| --- | --- |
| babeld 1.13.1 | IPv6, IPv4-via-IPv6, IPv6 SADR, numbered IPv4 on a shared LAN, RTT exchange, both MAC algorithms, withdrawal/recovery |
| BIRD 3.1.7 | IPv6, ordinary IPv4/IPv4-via-IPv6, IPv6 SADR, bidirectional route exchange |
| babel-rs peers | IPv4/IPv6 control, overlapping IPv4/IPv6 SADR transit, MAC rotation/restart, multipath and forwarding recovery |

BIRD tests use an ordinary IPv4 channel and an IPv6 SADR channel. They do not
establish IPv4 SADR or Babel-DTLS interoperability. Runtime lifecycle and fault
scenarios are mapped in [Testing](../development/testing.md#linux-network-suite).

## Known observations

### Live IPv4 next-hop-family changes

In the 2026-09-10 shared-LAN fixture, babeld retained its IPv4 gateway after a
neighbor changed its announcement to an IPv6 next hop; the babel-rs peer updated
its gateway. Select a compatible next-hop mode before establishing routes with
such peers. Initial numbered IPv4 exchange and withdrawal/recovery passed;
this does not establish support for every live next-hop-family transition.

### Mixed-run kernel route mismatch

In the 2026-09-11 mixed soak, babeld node 4 reported three routes installed via
`e17`, but its kernel FIB lacked them for the 600-second convergence limit after
a link flap. The root cause remains unconfirmed and is not attributed to
babel-rs. A second seed ran until its scheduled stop after verifying 448 rounds.
The [campaign record](../history/0.6.0-validation.md#2026-09-11-mixed-campaign) retains
the exact source, seeds, missing prefixes, failure and cleanup outcomes.

## API compatibility

All three crates use the same 0.x version. Public API, configuration, or behavior
changes that break a documented contract require a new minor version, such as
0.6 to 0.7. Patch releases preserve those contracts; a correction to documented
incorrect behavior must be called out in the changelog. A patch may still change
routing outcomes when fixing a defect, so validate upgrades before deployment.

This follows [Cargo's compatibility convention](https://doc.rust-lang.org/cargo/reference/semver.html):
`0.6` dependencies accept compatible 0.6.x updates, while `=0.6.0` requests that
exact version. Applications should retain Cargo.lock; deployed daemons should
use an explicit version. There is no 1.x compatibility commitment for the 0.x
series. See [CHANGELOG.md](../../CHANGELOG.md) for changes and migration guidance.

The control protocol has its own `api_version`; it is separate from the daemon's
package version. Runtime owner/drop/shutdown and exporter completion semantics
are documented in [Embedding](embedding.md).

## Getting help

Use the [issue tracker](https://github.com/bnkrr/babel-rs/issues) with the version
or commit, platform, relevant configuration, expected behavior, and a bounded
reproduction or diagnostic log. Do not include private keys or credentials.
There is no fixed support SLA. Contribution guidance is in
[CONTRIBUTING.md](../../CONTRIBUTING.md).
