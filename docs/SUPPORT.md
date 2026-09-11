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

CI covers Linux stable/MSRV and Windows/macOS protocol tests. A workflow's
existence does not establish success for an unpublished commit; the current
[verification register](CONFORMANCE.md) identifies completed and pending checks.
Linux interoperability evidence records the exact kernel and babeld/BIRD
versions in [INTEROPERABILITY.md](INTEROPERABILITY.md). Other kernels,
architectures, and network arrangements need validation in the target environment.

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
[known interoperability observations](INTEROPERABILITY.md#known-observations),
pin deployed versions, test upgrades against your topology, and retain a rollback
path. Successful tests do not establish correctness for every deployment.

- MAC authentication supports HMAC-SHA256 and BLAKE2s-128 with RFC 9467 replay
  counters. Keyed interfaces default to strict verification. Authentication
  does not provide confidentiality; DTLS is not implemented. See [MAC.md](MAC.md).
- Abrupt restarts can need minutes to converge. `SequenceStore` and daemon state
  provide orderly-exit checkpoints, not crash-safe runtime sequence persistence.
  Stable identities and restart policy belong to embedding hosts.
- Linux SADR supports overlapping prefixes and destination-first forwarding.
  Additional connected/static routes and external policy rules need host
  integration. Materialization can grow quadratically; individual netlink
  operations do not form an atomic transaction. See [SADR.md](SADR.md).
- Custom exporters must implement source-specific forwarding or gate unsupported
  sources. They must opt in to IPv4-via-IPv6 only if they support RFC 9229
  forwarding and ICMPv4 on unnumbered links; the runtime rejects it by default.
- Admission limits, output budgets, and timing counters bound and expose load.
  They do not guarantee protocol deadlines on an arbitrarily overloaded host.
  See [CAPACITY.md](CAPACITY.md).

The 7.5-hour 0.6.0 mixed campaign verified 511 mutation rounds across two attempts.
One ended on a babeld internal-status/kernel-FIB mismatch; its cause remains
unconfirmed. The second reached its scheduled stop without an adjudicated test
failure. That is useful running evidence, not a clean pass for the entire campaign.
Focused MAC, SADR, and IPv4 checks are separate from that IPv6 ordinary-route soak.

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
series. See [CHANGELOG.md](../CHANGELOG.md) for changes and migration guidance.

The control protocol has its own `api_version`; it is separate from the daemon's
package version. Runtime owner/drop/shutdown and exporter completion semantics
are documented in [EMBEDDING.md](EMBEDDING.md).

## Getting help

Use the [issue tracker](https://github.com/bnkrr/babel-rs/issues) with the version
or commit, platform, relevant configuration, expected behavior, and a bounded
reproduction or diagnostic log. Do not include private keys or credentials.
There is no fixed support SLA. Contribution guidance is in
[CONTRIBUTING.md](../CONTRIBUTING.md).
