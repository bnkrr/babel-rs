# Protocol coverage

This is the current implementation scope for babel-rs 0.6.0. It distinguishes
mandatory behavior, recommendations, selected alternatives, and unsupported
extensions. It is an implementation map, not a formal conformance certificate.

## Implemented specifications

| Specification | Coverage | Boundary or selected policy |
| --- | --- | --- |
| [RFC 8966](https://www.rfc-editor.org/rfc/rfc8966.html) | Base packet/TLV handling, both control families, neighbors, feasibility, route selection, requests, propagation, withdrawals and holds; wired and ETX metrics | Configurable hysteresis; important announcements use initial send plus two repeats; Ack replies are implemented, but there is no general Ack controller |
| [RFC 9079](https://www.rfc-editor.org/rfc/rfc9079.html) | Source-specific wire format, canonical route keys and overlapping IPv4/IPv6 Linux source views | Destination prefix length takes precedence over source prefix length; custom exporters and external kernel policy need equivalent behavior |
| [RFC 9229](https://www.rfc-editor.org/rfc/rfc9229.html) | IPv4 routes with IPv6 next hops, AE 4 context, request/retraction semantics and capability gating | Ordinary IPv4 is preferred on numbered interfaces; a backend must support forwarding and ICMPv4 on unnumbered links |
| [RFC 9616](https://www.rfc-editor.org/rfc/rfc9616.html) | Timestamp exchange, wrap/age checks, atomic timestamp groups and bounded RTT cost | Default per-sample EMA alpha 0.836; elapsed-time smoothing is an explicit override |
| [RFC 8967](https://www.rfc-editor.org/rfc/rfc8967.html), updated by [RFC 9467](https://www.rfc-editor.org/rfc/rfc9467.html) | HMAC-SHA256/BLAKE2s-128, challenge/restart handling, strict authentication and separate unicast/multicast receive counters | Optional per interface; migration mode explicitly accepts unverified input; the optional replay window is not selected |
| [RFC 8968](https://www.rfc-editor.org/rfc/rfc8968.html) | Not implemented | DTLS has no committed target version |

## Choices and integration boundaries

- IPv6 control is the recommended default, not a requirement for every Babel
  deployment. IPv4 control works with IPv6 disabled. The control family and
  announced next-hop policy are independent; see [Configuration](../guide/configuration.md).
- Route selection uses configurable 5%/8-unit improvement margins and an 8-second
  dwell by default. This is an alternative hysteresis policy, not Appendix A.3's
  exact algorithm. Unavailable routes and initial discovery bypass that delay.
- Urgent output defaults to 200 ms. Insignificant metric and sequence-only changes
  do not trigger advertisements. Important changes use bounded repeats; withdrawal
  holds conservatively cover possible neighbor expiry instead of early Ack release.
- `RoutePolicy` offers read-only import acceptance and per-interface announcement
  control. Retractions bypass policy; replacing rules invalidates queued old sends
  and preserves feasibility history. Metric rewriting, per-neighbor multicast
  export, and a daemon TOML filter language are outside the current scope.
- Source-specific forwarding must be consistent throughout the routing domain.
  The Linux backend implements the selected Babel RIB and withdrawal holds;
  connected/static routes and other policy rules remain host integration work.
  See [SADR](../guide/sadr.md) and [Embedding](../guide/embedding.md).
- Domain-wide identity uniqueness, actual MTU, correct split-horizon assumptions,
  forwarding support, and scheduling under load depend on the deployment. The
  operational contracts are in [Support](../guide/support.md) and [Capacity](../guide/capacity.md).

Conditional requirements apply when an extension is enabled. An authenticated
or encrypted carrier does not itself implement Babel MAC or Babel DTLS.

## Evidence and unresolved observations

Protocol regressions include independent raw wire fixtures, structured prefix
matrices, sequence-recovery simulations, and a destination-first SADR oracle.
Linux tests exercise forwarding, peer exchange, authentication, restart, and
owned-state cleanup. [Testing](testing.md) describes how to reproduce them.

The [0.6.0 validation record](../history/0.6.0-validation.md) distinguishes tested
commits and completed checks from remaining publication steps. In particular,
the mixed campaign verified 511 mutation rounds across two attempts and retained
one unattributed babeld kernel-FIB mismatch; it was not a clean campaign pass.
Current peer observations are in [Support](../guide/support.md#known-observations).

The [September 2026 audit record](../history/rfc-audit-2026-09.md) preserves the
A01–A08 fixes, original decoder reproduction, historical decision IDs, and test
index. It is historical evidence; this page defines the current scope.
