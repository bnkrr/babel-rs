# Changelog

## 0.5.0 — 2026-09-10

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
