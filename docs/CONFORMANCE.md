# Conformance

This matrix describes the v0.1 implementation; it does not claim every
optional Babel extension.

| Requirement | Status | Evidence |
|---|---|---|
| RFC 8966 header and TLVs 0–10 | implemented | wire round-trip and malformed-length tests |
| Stateful Router-ID, Next-Hop and prefix compression | implemented | compressed Update tests and both interop suites |
| Unknown TLV and mandatory sub-TLV disposition | implemented | `unknown_tlv_is_preserved_and_unknown_mandatory_subtlv_ignores_enclosing` |
| RFC 8966 separate Multicast/Unicast Hello histories and IHU expiry | implemented | history arithmetic, dynamic-cost, expiry and link-failure tests |
| Feasibility distance and 16-bit sequence arithmetic | implemented | model/engine feasibility tests |
| Route Request, Seqno Request and forwarding | implemented | local and remote Seqno Request tests; babeld reannouncement E2E |
| Triggered/periodic Update and retraction | implemented | engine retraction tests and three-node E2E |
| RFC 9079 Source Prefix sub-TLV | implemented | codec test and babeld/BIRD FIB checks |
| RFC 9229 AE 4 IPv4-via-IPv6 | implemented | codec test and babeld/BIRD IPv4 FIB checks |
| RFC 8966 wired k-out-of-j metric | implemented, default | 2-out-of-3/C=96 defaults; profile and delayed-acquisition tests |
| RFC 8966 ETX metric | implemented, optional | receive/transmit loss calculation tests; configurable window |
| RFC 8966 route-selection hysteresis | implemented | configurable metric margins and continuous better-path dwell |
| RFC 9616 Timestamp exchange and RTT sampling | implemented, optional | timestamp codec and Mills-exchange engine test |
| RFC 9616 RTT smoothing and bounded cost policy | implemented, optional | time-based EWMA, configurable probe interval, bounded cost tests |
| Custom metric profiles and algebra | library API | per-neighbour `MetricProfile`; guarded strictly monotonic extension |
| RFC 8967/8968 authentication | not implemented | use a protected link such as WireGuard |

The decoder is covered by an arbitrary-byte no-panic property test. The
runtime additionally bounds each datagram to the UDP receive buffer and uses
bounded Tokio channels. Larger-scale resource policy and authentication are
pre-1.0 hardening work rather than implied conformance claims.

Normative sources:

- [RFC 8966](https://www.rfc-editor.org/rfc/rfc8966.html), *The Babel Routing Protocol*;
- [RFC 9079](https://www.rfc-editor.org/rfc/rfc9079.html), *Source-Specific Routing in Babel*;
- [RFC 9229](https://www.rfc-editor.org/rfc/rfc9229.html), *IPv4 Routes with an IPv6 Next Hop in Babel*;
- [RFC 9616](https://www.rfc-editor.org/rfc/rfc9616.html), *Delay-Based Metric Extension for Babel*.
