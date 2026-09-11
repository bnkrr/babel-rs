# September 2026 RFC audit record

This is a historical record of the audit and fixes leading to 0.6.0. Current
scope is maintained in [Protocol coverage](../development/conformance.md), user-facing peer
observations in [Support](../guide/support.md), and run results in
[0.6.0 validation](0.6.0-validation.md).

The 2026-09-10 source audit examined `b403ad8168269bbe49e7296df99018ae41d971c8`
and reproduced A01–A08. Commit `d60b4ac` repaired those findings; `5e3b564` added
route-policy APIs and `8b9a416` added MAC and overlapping Linux SADR. The tables
below preserve the IDs and evidence as recorded at that stage, including the
later closure of the historical round-170 investigation and the separate new
round-64 observation. They are not a live release checklist.

The audit's errata snapshot found [7373](https://www.rfc-editor.org/errata/eid7373),
Held for Document Update, among the six RFCs in its original comparison. Its
recovery clarification was already covered by `receive_update`; this is a dated
snapshot, not a claim about the current errata list.

## Repaired findings

Protocol regressions are in
[`rfc_audit_regressions.rs`](../../crates/babel-protocol/tests/rfc_audit_regressions.rs)
unless another path is named. Each of the original seven protocol defect
regressions failed before the implementation fixes; A07 has a Linux projection
regression. The structured prefix matrix reaches valid packet headers and
compression contexts rather than relying on random bytes being admitted.

| ID | RFC / previous defect | Current behavior and evidence |
| --- | --- | --- |
| A08 | 8966 §4.5/§4.6.9: short cached prefix sliced beyond its end | Full-width zero-extended prefix cache and checked access; short-to-long prefix regression plus structured AE 1/2/4 matrix. |
| A06 | 8966 §4.6.9: missing compression/Router-ID context rejected the packet | Ignore only the affected Update, preserving later valid TLVs and applicable parser state. |
| A01 | 8966 §3.8.1.1: split horizon turned a known-prefix request into a false retraction | Specific requests reply directly with the selected/local-origin route; split horizon still applies to general advertisements. |
| A02 | 8966 §3.8.1.2: a different Router-ID request for a local origin received no Update | Reply with that local origin without spuriously increasing its sequence. |
| A03 | 8966 §3.7.3: finite unicast request replies omitted source history | Maintain feasibility history before finite unicast advertisements, including forwarded request replies. |
| A04 | 8966 §3.7.2: selecting a new origin through a new neighbor used an ordinary deadline | Selected Router-ID changes use urgent timing independently of candidate replacement; default urgent timeout is 200 ms. |
| A05 | 9616 §3: MTU splitting separated timestamped IHU from Hello | Packetizer keeps consecutive timestamped Hello/IHU groups atomic and separates multiple Hellos; over-budget groups return an error. |
| A07 | 9079 §4: ordinary tombstone replaced a finite source-specific route | Finite and unreachable entries use the same source-specific precedence in supported views; both precedence directions are tested in Linux projection tests. |

A08's original 43-byte reproduction is retained here for traceability:

```text
2a 02 00 27
08 12 02 80 40 00 06 40 00 01 ff ff 20 01 0d b8 00 00 00 00
08 11 02 00 80 09 06 40 00 01 ff ff 00 00 00 00 00 00 00
```

## Capabilities and policy decisions

| ID | Status | Scope / decision |
| --- | --- | --- |
| C01 | Implemented | `ControlTransport::Ipv6` default or `Ipv4` per interface; matching multicast, source admission, IP header budget and daemon attach/reload. IPv4 works with IPv6 disabled. |
| C02 | Implemented | RFC 8967 HMAC-SHA256/BLAKE2s-128, multi-key trailers, PC/challenge/restart/expiry/rate limiting, strict default and explicit migration mode. RFC 9467 §3.1 split receive counters; optional window verification is not selected. Linux packet-info transport binds actual endpoints, signs after RTT stamping, and reserves authentication overhead. Key changes reattach the affected interface without restarting the daemon. Evidence: `mac/tests.rs`, `netns-mac-sadr.py`; deployment/API details in [MAC](../guide/mac.md). |
| C03 | Deferred; no target version | RFC 8968 DTLS, credentials, sessions and protected transport are absent. Backend selection and deployment requirements need validation before implementation. No claim of implementing its conditional MUST rules. |
| C04 | Implemented for Linux Babel RIB | Overlapping IPv4/IPv6 views inherit all covering-source and ordinary routes; equal destinations use most-specific source, including withdrawal holds. Automatic managed views cover learned/held sources; static configurations gate unsupported prefixes with `RoutePolicy`. Source rules follow specificity and tables reconcile before activation. Tests compare against a destination-first oracle and exercise real transit packets, withdrawal and cleanup. External kernel policy and custom exporters still need integration; see [SADR](../guide/sadr.md). |
| C05 | Implemented | Read-only `RoutePolicy` accepts/rejects imports using prefix/interface/neighbor/Router-ID metadata and allows/retracts exports per interface. Defaults preserve protocol-valid routes. Explicit replacement reselects, refreshes/retracts, requests newly permitted routes and invalidates queued old sends without clearing feasibility history. No metric rewriting or per-neighbor multicast export. The daemon has no TOML filter language yet. Evidence: `crates/babel-protocol/tests/route_policy.rs`, runtime queue-cancellation tests and `tests/e2e/netns-route-policy.py`. |
| R01 | Implemented | Important changes get an initial copy and two repeats, separated by one second; latest state is read for each repeat. This uses RFC 8966 §3.7.2's bounded-repeat alternative. |
| R02 | Implemented | Sequence-only and insignificant metric changes do not trigger advertisements. Requests still receive current values. Withdrawals keep the current sequence; origin metric increases still advance it for feasibility. |
| R03 | Default corrected; selection policy documented | RTT defaults to per-sample EMA alpha 0.836. `half_life_ms`/`RttMetric::new` explicitly select elapsed-time smoothing. Route selection retains configurable 5%/8-unit margins and 8 s continuous improvement; this is a tested alternative hysteresis policy, not the exact Appendix A.3 algorithm. |
| R04 | Timer alternative selected | Retain withdrawals through possible neighbor expiry. All-neighbor Ack-based early release is optional and is not implemented. This deliberately favors conservative retention over early release and must not be described as missing mandatory behavior. |
| R05 | Implemented | Urgent timeout defaults to the recommended 200 ms; observed multicast loss causes IHU feedback on every received Hello, in addition to cost-change/periodic feedback. |

## Boundary fixes and verification

| ID | Current status and limits |
| --- | --- |
| V01 | Local-origin withdrawal now creates a hold, repeats/periodic dumps include retractions, and expiry/reorigination publishes a new export snapshot. Holds cover outgoing intervals and the last possible expiry of prior finite advertisements, including old longer intervals. Existing restart tests exercise persisted identity, SIGKILL and stale state. Globally unique identity still belongs to the deployment. |
| V02 | Exporters declare IPv4-via-IPv6 capability; the runtime gates engine selection before export. Defaults reject the route form for custom exporters, while the abstract MemoryExporter and supported Linux backend opt in. VM tests exercise TTL and fragmentation-needed ICMPv4 on unnumbered links, including kernel source 192.0.0.8 with no usable IPv4 address. Linux SADR is covered by C04. |
| V03 | Receive-boundary timestamps and send timestamps use microseconds. Queued receive events use current processing time for protocol timers, preserving clock order; an explicit timestamp event carries original arrival time. Atomic timestamp groups and existing queue/backpressure/deadline tests cover the identified paths. A heavily overloaded host can still miss deadlines; counters report this and cannot prove arbitrary-host timing. |
| V04 | Engine checks positive finite link cost, strict metric increase and infinity propagation even with custom algebras, on admission and recomputation. Arbitrary custom policy quality remains the host's responsibility. |
| V05 | Maintained A01–A08 regressions, pure IPv4 E2E, live family switching and independent babeld RTT are added. MAC and overlapping-source transit coverage is added by `netns-mac-sadr.py`; DTLS remains deferred. |
| V06 | Closed by maintainer decision on 2026-09-11; no further investigation of historical mixed-soak round 170 is planned. Its cause was not established, and closure does not mean a defect was reproduced or fixed. The old interrupted run and passing fresh-state replay remain historical evidence, separate from new campaigns. |
| V07 | The 2026-09-11 mixed run observed babeld node 4 reporting three routes installed while its kernel FIB lacked them after a link flap. The 600-second limit expired; diagnostics and cleanup were preserved. The cause is unconfirmed and is not attributed to babel-rs. See [Support](../guide/support.md#mixed-run-kernel-route-mismatch). |

## Historical regression index

These stable IDs preserve links to the previous test inventory. Each row is
evidence for its tested subset, not an unconditional conformance result. The
A/C/R/V register above records gaps identified in that audit. Some wire tests use independent raw fixtures; round-trip tests share the codec and cannot
establish interoperability by themselves.

| ID | RFC section | Tested subset / limitation | Evidence |
|---|---|---|---|
| BABEL-WIRE-01 | 8966 §4.2–4.6 | Header/body bounds, field encoding, zero-extended compression cache, checked short-to-long reuse and missing-context Update discard | `wire` unit tests and arbitrary-byte no-panic test |
| BABEL-WIRE-02 | 8966 §4.3–4.4 | Unknown TLVs are skipped; an unknown mandatory sub-TLV suppresses its enclosing TLV without corrupting parser state | `unknown_tlv_is_preserved_and_unknown_mandatory_subtlv_ignores_enclosing` |
| BABEL-WIRE-03 | 8966 §4.6 | Required nonzero Interval and Hop Count values are rejected; PadN MBZ bytes are zero on send and silently ignored on receive | `rfc8966_padn_mbz_is_ignored_on_receive_and_enforced_on_send`, `rfc8966_encoder_rejects_zero_required_control_values` |
| BABEL-WIRE-04 | 8966 §4.6.9 | Finite Updates require Router-ID and next-hop context; retractions do not | `finite_update_requires_router_id_but_retraction_does_not` |
| BABEL-TRANSPORT-01 | 8966 §4 | Per-interface IPv6 link-local or IPv4 on-link source admission; UDP source port 6696 is enforced for both control families (C01) | transport source-admission tests and `netns-rfc-boundaries.py` |
| BABEL-TRANSPORT-02 | 8966 §4 | Multicast and unicast hop limits are set to 1; the live interface MTU is converted to the selected IP/UDP budget, with MAC overhead reserved when enabled | transport budget and packetizer tests, live-MTU and MAC netns E2E |
| BABEL-OUTPUT-01 | 8966 §3.1 | Output actions carry deadlines; scheduler jitter and selected Router-ID urgency have regressions | `rfc8966_3_1_output_actions_preserve_their_deadlines`, output scheduler tests |
| BABEL-OUTPUT-02 | 8966 §3.1 | TLVs aggregate and datagrams are paced; timestamped Hello/IHU groups remain atomic at MTU boundaries | output scheduler aggregation, deterministic jitter, packetisation, and pacing tests |
| BABEL-NEIGH-01 | 8966 §3.4, Appendix A | Multicast and Unicast Hello histories are independent; restart, fast-forward, undo, timers, IHU expiry, k-out-of-j and ETX arithmetic are deterministic | metric and engine unit tests |
| BABEL-ROUTE-01 | 8966 §3.5.3 | Route entries are indexed by destination and neighbour; Router-ID is route data and may change | engine candidate model and alternate-retraction tests |
| BABEL-ROUTE-02 | 8966 §3.5.1, §3.6 | Selection rejects infinity/infeasibility; candidate ordering does not prefer newer seqnos; unicast reply history is maintained | `rfc8966_3_5_4_unfeasible_update_cannot_replace_the_selected_route`, invariant test, candidate ordering |
| BABEL-ROUTE-03 | 8966 §3.5.3 | First expiry changes a finite route to infinity; the next expiry garbage-collects it | `rfc8966_3_5_5_expiry_retracts_then_garbage_collects_the_route` |
| BABEL-ROUTE-04 | 8966 §3.5.4 | Learned/local holds, outgoing lifetime, expiry snapshots and source-specific projection precedence have regressions | engine expiry test, Linux `unreachable_tombstone_is_projected_into_matching_views` |
| BABEL-ROUTE-05 | 8966 §3.5.2, §3.7.3 | Finite multicast/unicast history and custom-algebra infinity protection have regressions | metric algebra and source-table tests |
| BABEL-ROUTE-06 | 8966 §3.5.3 | Changing the Router-ID of an existing route entry triggers a timely update even when selection is unchanged | `rfc8966_3_5_3_router_id_change_triggers_an_update_even_if_unselected` |
| BABEL-REQ-01 | 8966 §3.8.1.1 | Unknown-prefix requests retract; specific known-prefix requests bypass split horizon and receive a unicast finite reply | `rfc8966_3_8_1_1_unknown_specific_route_request_gets_a_retraction` |
| BABEL-REQ-02 | 8966 §3.8.1.2 | Learned and local-origin different-ID replies and own-ID origin increments are tested | the two `rfc8966_3_8_1_2_*` response tests |
| BABEL-REQ-03 | 8966 §3.8.1.2 | A forwarded request is sent to exactly one unicast neighbour with decremented Hop Count and redundant requests are suppressed | `rfc8966_3_8_1_2_forwarded_seqno_request_is_unicast_and_decrements_hop_count` |
| BABEL-REQ-04 | 8966 §3.2.7, §3.8.2, Appendix B | Pending requests retain the requester, forward satisfying replies, and retry after 2/4/8 seconds before expiry | `rfc8966_appendix_b_pending_request_uses_bounded_exponential_retries` and engine response tests |
| BABEL-REQ-05 | 8966 §3.8.2.1–3 | Starvation keeps a Seqno Request active and a selected route is queried by unicast shortly before expiry | `rfc8966_3_8_2_1_starvation_keeps_a_seqno_request_active`, `rfc8966_3_8_2_3_selected_route_is_refreshed_before_expiry` |
| BABEL-FILTER-01 | 8966 Appendix C | The minimum dangerous destinations are not learned | `rfc8966_appendix_c_minimum_dangerous_destinations_are_filtered` |
| BABEL-FILTER-02 | 8966 Appendix C, §3.7.2/§3.7.3 | Host allow/deny policy covers admission and every finite output path; explicit replacement retracts and recovers without resetting feasibility | `route_policy.rs`, `policy_change_cancels_channel_scheduled_and_in_flight_output`, runtime policy netns E2E |
| SADR-WIRE-01 | 9079 §7 | Source Prefix validation and wildcard handling have raw fixtures; malformed sub-TLV framing can reject a whole packet | source-specific wire tests and `rfc9079_wildcard_retraction_with_source_prefix_is_ignored` |
| SADR-MODEL-01 | 9079 §2–3 | Destination plus source forms every route/source/request key; `/0` source is the ordinary SADR domain | `rfc9079_zero_source_prefix_is_the_ordinary_sadr_domain` |
| SADR-FIB-01 | 9079 §4 | Overlapping views inherit ancestor/ordinary routes, same-destination source order applies equally to finite routes and holds, dynamic allocation covers learned sources and static mode gates unsupported sources | `source_views_match_destination_first_oracle_for_ipv4_and_ipv6`, `inherited_routes_refresh_withdraw_and_keep_tables_until_hold_expires`, `netns-mac-sadr.py` |
| MAC-01 | 8967 §§3–6; 9467 §3.1 | Keyed packet admission precedes engine decoding, metadata binds both IP endpoints and ports, challenge/replay state is bounded and expires; unicast/multicast counters are independent | `crates/babel-protocol/src/mac/tests.rs`, safe Linux recvmsg/sendmsg integration, independent babeld tests |
| V4V6-WIRE-01 | 9229 §2, §4 | AE 4 has distinct compression state, uses IPv4 prefix encoding and IPv6 next-hop state, and is rejected in IHU/Next Hop | AE 4 codec and packetisation tests |
| V4V6-SEND-01 | 9229 §2.1 | Auto prefers ordinary IPv4 AE with an IPv4 next hop when a usable address exists, otherwise AE 4; per-interface overrides are supported | `ipv4_policy.rs`, `rfc9229_prefers_ordinary_ipv4_ae_when_interface_has_ipv4`, shared-LAN E2E |
| RTT-WIRE-01 | 9616 §3, §6 | Hello/IHU timestamp forms round-trip; runtime send/receive timestamps use microseconds and a separate processing clock | timestamp codec and `rfc9616_timestamp_is_stamped_at_the_transport_boundary` |
| RTT-ENGINE-01 | 9616 §3.2 | Engine output pairs timestamped Hello/IHU, checks sample ages and preserves the group through packetization | timestamp exchange engine tests |
| RTT-METRIC-01 | 9616 §4 | RTT defaults to per-sample alpha 0.836, with an explicit elapsed-time override, and maps to a bounded piecewise-linear penalty | `rtt_uses_rfc_bounded_penalty` and RTT multipath E2E |

## Properties requiring inspection or deployment tests

These constraints cannot be established by an in-process deterministic unit
test. They must stay on the audit checklist; a release must not silently turn
them into conformance claims.

| ID | Constraint | How to audit |
|---|---|---|
| MANUAL-ID-01 | RFC 8966 §3 assumes every 8-octet Router-ID is unique in the routing domain. A process can validate reserved values, but cannot prove domain-wide uniqueness. | Inspect generated/persisted IDs and query all live speakers through the control socket. |
| MANUAL-LINK-01 | RFC 8966 §3.7.4 recommends split horizon on symmetric, transitive links and recommends against it otherwise; configuration cannot prove those properties. | Confirm that `wired` and `tunnel` interfaces are symmetric and transitive, or explicitly disable split horizon. The `wireless` preset disables it. |
| MANUAL-MTU-01 | RFC 8966 §4 forbids IPv6 jumbograms. The implementation caps UDP payload even when the reported device MTU is unusually large, but a tunnel stack can still add hidden overhead below its advertised Linux MTU. | Confirm managed interfaces report the effective tunnel MTU; capture traffic if an encapsulation layer does not propagate it to Linux. |
| MANUAL-PACING-01 | RFC 8966 §3.1 timing ultimately depends on async-runtime and kernel scheduling under host overload. | Monitor `missed_outbound_deadlines`; load-test packet timing for deployments with unusually large RIBs or shared media. |
| MANUAL-SADR-01 | RFC 9079 requires identical destination-first forwarding semantics throughout a routing domain. Linux policy rules are source-first. | Verify the reserved table/priority range precedes ordinary lookups and foreign routes do not alter automatic tables. The Linux projector implements the Babel-RIB ordering; audit external policy and other exporters independently. |
| MANUAL-ICMP4-01 | RFC 9229 §3 requires a forwarding router to originate ICMPv4 even when the egress has no IPv4 address. This is a kernel/platform property, not Babel wire state. | In every IPv4-via-IPv6 deployment, exercise TTL exceeded and fragmentation-needed paths from a namespace with no IPv4 link address. |
| MANUAL-RTT-01 | RTT quality depends on monotonic clock behaviour and queue placement in the deployed async runtime. | Code samples microseconds at receive/send boundaries; inspect actual socket scheduling and packet timing under deployment load. |
| MANUAL-SEC-01 | Unauthenticated interfaces accept routing control from any speaker on the managed link. Strict MAC verifies configured keys but does not encrypt packets; migration mode admits unverified traffic. DTLS is absent. | Verify strict MAC key distribution or an authenticated, authorised link boundary. Use an encrypted carrier when confidentiality is needed, and restrict UDP 6696 to intended interfaces. |
| MANUAL-INTEROP-01 | Wire compatibility cannot be proved solely against our own codec. | Run `tests/e2e/run-on-linux-vm.sh`; it exchanges routes with current babeld and BIRD packages. |
