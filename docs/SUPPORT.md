# Support and compatibility

The 0.5 release targets reusable Babel protocol and Linux runtime consumers.

The [2026-09-10 RFC audit](CONFORMANCE.md) records repaired defects and deferred
extensions. Platform support and successful tests do not
constitute a complete RFC-conformance or release-readiness claim.

| Component | Supported scope |
| --- | --- |
| `babel-protocol` | OS-independent standard-library Rust codec/engine; no I/O or async runtime. Not `no_std`. |
| `babel-router` | Linux interface discovery and IPv4/IPv6 UDP sockets on Tokio; no daemon/netlink-export dependency. Non-Linux interface attachment returns Unsupported. |
| `babel-rs` | Linux daemon, netlink routes and policy rules, Unix control socket. |

Rust 1.90 is the minimum source-build version. CI checks MSRV, stable Linux
runtime tests, and protocol tests on Windows/macOS. The release-packaging check
uses Cargo 1.96 or newer for workspace packaging; that is a release-tooling
requirement, not a library build requirement. Live network verification uses
Linux namespaces and installed babeld/BIRD, with exact tested versions in
[INTEROPERABILITY.md](INTEROPERABILITY.md). CI definitions are not proof that
every configured job has run for an unpublished commit.

The runtime requires UDP/6696 and privileges to bind sockets to interfaces.
IPv6 control is the RFC-recommended default and requires a link-local address;
per-interface IPv4 control requires an IPv4 address and supports hosts with
IPv6 disabled. Next-hop policy is independent of the control family.

RFC 9229 forwarding requires a backend that supports IPv4 routes via IPv6 and
ICMPv4 on unnumbered links. `RouteExporter::supports_ipv4_via_ipv6` defaults to
false; unsupported route forms are excluded before selection. `MemoryExporter`
opts in as an abstract RIB. The Linux exporter opts in for the supported Linux
backend; deployments must provide kernel support for these operations. The VM
boundary test checks TTL-exceeded and fragmentation-needed messages both with
a loopback IPv4 address and with no usable IPv4 address (kernel source 192.0.0.8).
Custom exporters must opt in only when these requirements hold.

Authentication (RFC 8967/8968) is not implemented. Use authenticated, authorized
links such as WireGuard. This release does not promise safe deployment of Babel
directly on an untrusted network, or a completed independent security audit.

Stable identities and restart sequence policy belong to embedding hosts. The
provided checkpoint example demonstrates orderly restart without repeatedly
using zero; abrupt restarts may still need minutes to converge. Crash-safe
sequence persistence is not claimed. Linux source views reject overlapping
nonzero prefixes; this is not a complete general SADR forwarding backend.
That configuration check does not prevent selection/advertisement of received
routes outside the configured views. Finite-route/tombstone precedence within supported views is regression-tested.
General SADR support remains deferred, including gating uncovered source views.

## API compatibility

0.x versions follow Cargo's pre-1.0 SemVer convention: breaking public API or
behavior changes require a minor-version increment. Patch releases retain the
documented contracts, except where correcting a documented defect requires
otherwise and is explicitly called out. No 1.0 stability promise is made yet.
See [../CHANGELOG.md](../CHANGELOG.md) for migration notes.

Report bugs through the repository issue tracker with version/commit, platform,
configuration, expected behavior and a bounded reproduction or diagnostic log.
There is no fixed support SLA. Do not include private keys in issue reports.
