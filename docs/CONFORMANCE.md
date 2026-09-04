# RFC conformance and audit map

This file is the single conformance index for the current implementation. It
separates protocol requirements that are checked deterministically from
deployment properties that code cannot prove. A green test run is evidence for
the former only; it is not a blanket certification of an installation.

## Implemented specifications

- [RFC 8966](https://www.rfc-editor.org/rfc/rfc8966.html), Babel version 2,
  including the base TLVs, neighbour and route state, feasibility, explicit
  requests, retractions, and the Appendix A wired and ETX metrics.
- [RFC 9079](https://www.rfc-editor.org/rfc/rfc9079.html), source-specific
  routes and the Source Prefix sub-TLV, with the Linux export restriction
  described below.
- [RFC 9229](https://www.rfc-editor.org/rfc/rfc9229.html), IPv4 routes with an
  IPv6 next hop (AE 4).
- [RFC 9616](https://www.rfc-editor.org/rfc/rfc9616.html), timestamp exchange,
  Mills RTT samples, smoothing, and bounded RTT cost.

RFC 8967 MAC authentication and RFC 8968 Babel over DTLS are not implemented
and are not claimed. The intended Velvet deployment runs Babel on authenticated
WireGuard links. Other deployments must provide an equivalent trust boundary
or add an authentication implementation.

## Deterministic checks

The requirement ID is stable and should be used in test names or commit
messages when a rule changes. `cargo test --workspace --all-targets` runs the
whole set. Raw byte fixtures do not use the encoder to construct their input,
so decoder and encoder bugs cannot mask each other.

| ID | RFC section | Checked property | Evidence |
|---|---|---|---|
| BABEL-WIRE-01 | 8966 §4.2–4.6 | Header, body bounds, big-endian fields, TLV lengths, address encodings, prefix compression, and packet-local parser state | `wire` unit tests and arbitrary-byte no-panic test |
| BABEL-WIRE-02 | 8966 §4.3–4.4 | Unknown TLVs are skipped; an unknown mandatory sub-TLV suppresses its enclosing TLV without corrupting parser state | `unknown_tlv_is_preserved_and_unknown_mandatory_subtlv_ignores_enclosing` |
| BABEL-WIRE-03 | 8966 §4.6 | Required nonzero Interval and Hop Count values are rejected; PadN MBZ bytes are zero on send and silently ignored on receive | `rfc8966_padn_mbz_is_ignored_on_receive_and_enforced_on_send`, `rfc8966_encoder_rejects_zero_required_control_values` |
| BABEL-WIRE-04 | 8966 §4.6.9 | Finite Updates require Router-ID and next-hop context; retractions do not | `finite_update_requires_router_id_but_retraction_does_not` |
| BABEL-TRANSPORT-01 | 8966 §4 | IPv6 link-local source and UDP source port 6696 are required; local looped-back packets are ignored | `rfc8966_transport_accepts_only_link_local_port_6696_sources` |
| BABEL-TRANSPORT-02 | 8966 §4 | Multicast and unicast hop limits are set to 1; output is packetised below the IPv6 minimum-MTU payload budget | transport construction and `packetizer_repeats_context_and_respects_the_datagram_budget` |
| BABEL-NEIGH-01 | 8966 §3.4, Appendix A | Multicast and Unicast Hello histories are independent; restart, fast-forward, undo, timers, IHU expiry, k-out-of-j and ETX arithmetic are deterministic | metric and engine unit tests |
| BABEL-ROUTE-01 | 8966 §3.5.3 | Route entries are indexed by destination and neighbour; Router-ID is route data and may change | engine candidate model and alternate-retraction tests |
| BABEL-ROUTE-02 | 8966 §3.5.1, §3.6 | No infinite or unfeasible route is selected, and sequence number is not a route-selection preference | `rfc8966_3_5_4_unfeasible_update_cannot_replace_the_selected_route`, invariant test, candidate ordering |
| BABEL-ROUTE-03 | 8966 §3.5.3 | First expiry changes a finite route to infinity; the next expiry garbage-collects it | `rfc8966_3_5_5_expiry_retracts_then_garbage_collects_the_route` |
| BABEL-ROUTE-04 | 8966 §3.5.4 | An exact unreachable tombstone prevents fallback to a covering prefix during hold time | engine expiry test, Linux `unreachable_tombstone_is_projected_into_matching_views` |
| BABEL-ROUTE-05 | 8966 §3.5.2, §3.7.3 | Metric extension is strictly increasing and source feasibility distance uses the advertised distance of finite updates sent | metric algebra and source-table tests |
| BABEL-ROUTE-06 | 8966 §3.5.3 | Changing the Router-ID of an existing route entry triggers a timely update even when selection is unchanged | `rfc8966_3_5_3_router_id_change_triggers_an_update_even_if_unselected` |
| BABEL-REQ-01 | 8966 §3.8.1.1 | A specific Route Request always gets either the selected Update or an exact retraction | `rfc8966_3_8_1_1_unknown_specific_route_request_gets_a_retraction` |
| BABEL-REQ-02 | 8966 §3.8.1.2 | A selected route with a different Router-ID satisfies a Seqno Request; a local origin advances by at most one per request | the two `rfc8966_3_8_1_2_*` response tests |
| BABEL-REQ-03 | 8966 §3.8.1.2 | A forwarded request is sent to exactly one unicast neighbour with decremented Hop Count and redundant requests are suppressed | `rfc8966_3_8_1_2_forwarded_seqno_request_is_unicast_and_decrements_hop_count` |
| BABEL-REQ-04 | 8966 §3.2.7, §3.8.2, Appendix B | Pending requests retain the requester, forward satisfying replies, and retry after 2/4/8 seconds before expiry | `rfc8966_appendix_b_pending_request_uses_bounded_exponential_retries` and engine response tests |
| BABEL-REQ-05 | 8966 §3.8.2.1–3 | Starvation keeps a Seqno Request active and a selected route is queried by unicast shortly before expiry | `rfc8966_3_8_2_1_starvation_keeps_a_seqno_request_active`, `rfc8966_3_8_2_3_selected_route_is_refreshed_before_expiry` |
| BABEL-FILTER-01 | 8966 Appendix C | The minimum dangerous destinations are not learned | `rfc8966_appendix_c_minimum_dangerous_destinations_are_filtered` |
| SADR-WIRE-01 | 9079 §7 | Exactly one valid nonzero Source Prefix is accepted; duplicate, malformed, wildcard-attached, wrong-family, or unknown mandatory forms suppress the enclosing TLV | source-specific wire tests and `rfc9079_wildcard_retraction_with_source_prefix_is_ignored` |
| SADR-MODEL-01 | 9079 §2–3 | Destination plus source forms every route/source/request key; `/0` source is the ordinary SADR domain | `rfc9079_zero_source_prefix_is_the_ordinary_sadr_domain` |
| SADR-FIB-01 | 9079 §4 | The shipped Linux exporter cannot silently implement source-first semantics for ambiguous routes | config overlap rejection and Linux projection tests |
| V4V6-WIRE-01 | 9229 §2, §4 | AE 4 has distinct compression state, uses IPv4 prefix encoding and IPv6 next-hop state, and is rejected in IHU/Next Hop | AE 4 codec and packetisation tests |
| V4V6-SEND-01 | 9229 §2.1 | Ordinary IPv4 AE is preferred on an interface with IPv4; AE 4 is used on IPv6-only interfaces | `rfc9229_prefers_ordinary_ipv4_ae_when_interface_has_ipv4` and interoperability E2E |
| RTT-WIRE-01 | 9616 §3, §5 | Hello and IHU Timestamp sub-TLV forms round-trip; every RTT Hello timestamp is stamped at the socket boundary | timestamp codec and `rfc9616_timestamp_is_stamped_at_the_transport_boundary` |
| RTT-ENGINE-01 | 9616 §3.2 | Origin/receive timestamps are recorded; timestamped IHU is co-located with timestamped Hello; wrap-safe Mills samples reject invalid elapsed values | timestamp exchange engine tests |
| RTT-METRIC-01 | 9616 §4 | RTT is smoothed over elapsed time and mapped monotonically to a bounded piecewise-linear penalty | `rtt_uses_rfc_bounded_penalty` and RTT multipath E2E |

## Properties requiring inspection or deployment tests

These constraints cannot be established by an in-process deterministic unit
test. They must stay on the audit checklist; a release must not silently turn
them into conformance claims.

| ID | Constraint | How to audit |
|---|---|---|
| MANUAL-ID-01 | RFC 8966 §3 assumes every 8-octet Router-ID is unique in the routing domain. A process can validate reserved values, but cannot prove domain-wide uniqueness. | Inspect generated/persisted IDs and query all live speakers through the control socket. |
| MANUAL-LINK-01 | Split horizon is enabled on every managed interface and RFC 8966 §3.7.4 permits that only on symmetric, transitive links. | Use only point-to-point/WireGuard or equivalent interfaces; a future generic wireless profile must make this per-interface. |
| MANUAL-MTU-01 | RFC 8966 §4 constrains packets by each live interface MTU and forbids IPv6 jumbograms. The current IPv6-only profile uses a 1232-byte UDP ceiling, valid for IPv6's 1280-byte minimum MTU. | Confirm every managed interface supports IPv6 and an MTU of at least 1280; capture traffic if an unusual tunnel stack is used. |
| MANUAL-PACING-01 | RFC 8966 recommends random jitter, aggregation, and urgent pacing. The current point-to-point profile aggregates route dumps and bounds queues but does not randomly delay general output. | Treat this as a documented SHOULD-level profile deviation; audit packet rate in larger shared-media deployments before use. |
| MANUAL-SADR-01 | RFC 9079 requires identical destination-first forwarding semantics throughout a routing domain. Linux policy rules are source-first. | The exporter rejects overlapping nonzero source views, reducing the accepted configuration to a subset where the two orders coincide. Audit other exporters independently. |
| MANUAL-ICMP4-01 | RFC 9229 §3 requires a forwarding router to originate ICMPv4 even when the egress has no IPv4 address. This is a kernel/platform property, not Babel wire state. | In every IPv4-via-IPv6 deployment, exercise TTL exceeded and fragmentation-needed paths from a namespace with no IPv4 link address. |
| MANUAL-RTT-01 | RTT quality depends on monotonic clock behaviour and queue placement in the deployed async runtime. | Timestamp sampling occurs immediately after receive and immediately before send; confirm with packet capture and injected delay in the RTT E2E. |
| MANUAL-SEC-01 | Base Babel accepts routing control from any speaker on the managed link; RFC 8967/8968 are absent. | Verify WireGuard or another authenticated, authorised link boundary and firewall UDP 6696 from every untrusted interface. |
| MANUAL-INTEROP-01 | Wire compatibility cannot be proved solely against our own codec. | Run `tests/e2e/run-on-linux-vm.sh`; it exchanges routes with current babeld and BIRD packages. |

## Explicit profile boundaries

- Control traffic is IPv6-only. RFC 8966 recommends this and still permits
  carrying both IPv4 and IPv6 routes.
- The Linux exporter rejects overlapping nonzero source prefixes. This is a
  deliberate Velvet-oriented restriction until a general destination-first
  disambiguation exporter exists.
- Route hold time uses the RFC's first-expiry-to-infinity, second-expiry-to-GC
  algorithm. It does not implement the optional faster all-neighbour
  acknowledgment algorithm.
- RTT smoothing is time-based and configurable. A configured half-life can
  intentionally differ from RFC 9616's recommended per-sample alpha; the
  mandatory monotonic bounded mapping and timestamp exchange remain enforced.
- RFC 8967, RFC 8968, diversity routing, and other optional extensions are not
  part of this conformance claim.
