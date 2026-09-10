# Changelog

## 0.5.0 — unreleased

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

Migration: add `ipv4_next_hop: Ipv4NextHop::Auto` to existing `InterfacePolicy`
struct literals. Keep the owning `BabelRouter` (or its `wait` future) alive;
dropping it no longer leaves routing running. Handle shutdown requests do not
wait: use the owner's `shutdown().await` or request then await `wait`. Expect
`originate`/`withdraw` to wait for engine application. Handle the newly surfaced
shutdown error variants; `RouterError` is non-exhaustive. Interfaces with IPv4
addresses now prefer IPv4 gateways; use `ipv6` to retain RFC 9229 behavior.
Exhaustive `Event` matches must handle `InterfaceAddressesChanged`;
`RouterInterfaceStatus` also exposes the configured mode and selected IPv4 address.

## 0.4.2

Avoid redundant sequence requests through worse/equal infeasible alternates;
expose Linux export progress and add combined-failure and mixed-daemon endless
testing. Published as a repository release, not a crates.io release.
