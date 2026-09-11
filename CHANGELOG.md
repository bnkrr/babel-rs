# Changelog

## 0.6.0 — Local freeze, 2026-09-11

This version is usable within the documented support scope. This date identifies
the local source freeze, not a crates.io publication. Registry and hosted-CI
status are recorded in [RELEASING.md](docs/RELEASING.md).

- Implement optional RFC 8967 MAC authentication (HMAC-SHA256/BLAKE2s-128),
  RFC 9467 split replay counters, challenge/restart handling, and live key rotation.
  Add safe UDP packet-info reception/source-pinned transmission and MTU reservation.
- Support overlapping IPv4/IPv6 SADR sources through inherited destination tables,
  automatic source-view allocation, withdrawal holds, and unsupported-source
  filtering in static configurations. Source rule priorities now follow prefix
  specificity; existing overrides must be migrated as described in docs/SADR.md.
- Add independent digest/babeld interop, MAC rotation/restart, destination-first
  oracle, and actual source-specific transit forwarding regressions. DTLS is deferred.
- Rewrite project and crate introductions for standalone routing and library
  consumers, with consistent AI authorship, compatibility, and operational notes.
  The daemon example now generates its Router-ID and uses generic interfaces.
  Installation documents the host-owned lookup rules needed for its dedicated
  ordinary table; source-rule ownership remains unchanged.
- Record the 7.5-hour mixed campaign: 511 verified mutation rounds across two
  attempts, one unresolved babeld internal-state/kernel-FIB mismatch, and clean
  termination. This is not a clean full-campaign pass.

Migration from 0.5: overlapping source views no longer need disjoint prefixes.
Automatic source views are enabled when the daemon manages rules; review table
allocation and source-rule priorities in [SADR.md](docs/SADR.md). MAC is optional;
keyed interfaces start in strict mode, and changing keys reattaches the affected
interface. Existing ordinary export tables continue to work when their host-owned lookup
rules are configured. Example/documentation changes do not change how an
existing configuration is interpreted.

## 0.5.0 — 2026-09-10

Repository version; the crates.io publish attempt stopped at authentication
before any upload.

- Add read-only `RoutePolicy` import/export hooks, defaulting to `AllowAllRoutes`.
  Explicit engine/runtime replacement reevaluates candidates, retracts denied
  announcements, requests newly allowed routes and preserves feasibility history.
  Invalidate old queued/pending sends before replacement output. Add a packaged
  interactive example and real-socket runtime/FIB policy transition regression.

- Repair the RFC audit defects: compressed-prefix panic/ignore behavior,
  specific/sequence request replies, unicast feasibility history, urgent
  selected-origin changes, timestamp group packetization and SADR tombstone order.
- Add per-interface IPv4 control transport, backend IPv4-via-IPv6 capability
  gating, microsecond receive timestamps and independent processing time.
- Repeat important announcements, suppress insignificant/sequence-only triggers,
  retain local withdrawals through the advertised lifetime, and export hold expiry.
- Use RTT per-sample alpha 0.836 by default, retain time-based smoothing as an
  explicit override, and use the recommended 200 ms urgent timeout. Preserve
  sequence numbers on withdrawals and protect infinity with custom metric algebras.
- MAC, DTLS and general SADR are deferred to the next version.

- Rename the core package and Rust import from `babel-proto` / `babel_proto` to
  `babel-protocol` / `babel_protocol`: the original registry name belongs to a
  different project. No ownership or compatibility with that crate is claimed.

- Default to ordinary IPv4 advertisements with an explicit IPv4 next hop on
  numbered interfaces. Add per-interface `ipv4_next_hop = auto|ipv4|ipv6`, live
  address refresh, status of the effective mode, and immediate policy updates.
  Forced IPv4 without an address retracts IPv4 routes while IPv6 continues.
- Add explicit runtime `start`, `wait`, and owner `shutdown().await`. Start
  acknowledges initial engine configuration; origin/withdraw commands now wait
  for engine application. `build`/`run` remain aliases. The synchronous handle
  `shutdown` remains a compatibility alias of `request_shutdown`.
- Dropping the runtime owner or its wait future now cancels owned tasks.
  Explicit shutdown is required for asynchronous route cleanup. Reconcile and
  final cleanup are ordered; the overall shutdown deadline defaults to five
  seconds and is configurable/reloadable. Checkpoint, cleanup and deadline
  failures are returned instead of reporting successful cleanup.
- Declare Linux runtime support separately from the OS-independent engine.
  Package README/license files and the daemon configuration example, and add
  archive-consumer/install checks and a host-owned checkpoint example.
- Add shared-LAN dual-stack, policy/address transition and asymmetric-loss
  regressions alongside public lifecycle and wire-decoded policy tests.

Migration: add `control_transport: ControlTransport::Ipv6` and
`ipv4_next_hop: Ipv4NextHop::Auto` to existing `InterfacePolicy` literals. Set
`EngineConfig::ipv4_via_ipv6` according to host forwarding capability. Add
`route_policy: Arc::new(AllowAllRoutes)` to `EngineConfig` literals (or start
from `recommended`). Direct hosts must handle `Event::ReplaceRoutePolicy` and
execute `Action::InvalidatePendingSends` by cancelling older queued output. Custom
exporters must opt in via `supports_ipv4_via_ipv6()` to select those routes. Keep the owning `BabelRouter` (or its `wait` future) alive;
dropping it no longer leaves routing running. Handle shutdown requests do not
wait: use the owner's `shutdown().await` or request then await `wait`. Expect
`originate`/`withdraw` to wait for engine application. Handle the newly surfaced
shutdown error variants; `RouterError` is non-exhaustive. Interfaces with IPv4
addresses now prefer IPv4 gateways; use `ipv6` to retain RFC 9229 behavior.
Exhaustive `Event` matches must handle `InterfaceAddressesChanged` and
`PacketReceivedWithTimestamp`; `RouterInterfaceStatus::local_addresses` now
contains `IpAddr` rather than `Ipv6Addr`, and includes `control_transport`.
`RouterInterfaceStatus` also exposes the configured mode and selected IPv4 address.

## 0.4.2

Avoid redundant sequence requests through worse/equal infeasible alternates;
expose Linux export progress and add combined-failure and mixed-daemon endless
testing. Published as a repository release, not a crates.io release.
