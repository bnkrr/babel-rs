# Architecture

This guide explains ownership and data flow for contributors. Public lifecycle
contracts are in [Embedding](../guide/embedding.md); deployment settings are in
[Configuration](../guide/configuration.md).

## Boundaries

| Layer | Owns | Delegates |
| --- | --- | --- |
| `babel-protocol` | Synchronous engine, codec, metrics, feasibility and route selection | Clock, I/O, scheduling, forwarding and persistence to the host |
| `babel-router` | One serialized engine, Linux sockets, interface workers, control handles and snapshot delivery | Forwarding and orderly-exit persistence through host callbacks |
| `babel-rs` | TOML, interface supervision, state file, signals, control socket and Linux netlink export | Link creation, addresses, local reachability and host policy to the deployment |

```text
UDP receive -> admission / optional MAC -> decoder -> serialized engine
                                                       |          |
                                             send actions     route snapshot
                                                       |          |
                                           sender scheduler   export worker
                                                       |          |
                                         packetizer / MAC    RouteExporter
                                                       |          |
                                                   UDP send   Linux netlink
```

The protocol library performs no I/O and reads no clock. Each event supplies
monotonic time; the host executes returned actions in order. Local origin state,
learned candidates, selected routes, feasibility history, and pending sequence
requests are separate stores. A route key is `(destination, optional source)`;
its prefixes share an address family, while the next-hop family can differ.

## Packet processing and scheduling

The decoder owns per-packet Router-ID, Next-Hop, and prefix-compression context.
The packetizer builds independent datagrams, repeating context after a split.
It uses the live interface MTU minus IP/UDP and configured authentication overhead.
Timestamped Hello/IHU groups stay together; RTT timestamps are stamped at send time.

Linux sockets bind to the receiving interface. IPv6 admission requires a
non-local link-local source; IPv4 admission checks the interface subnet or
point-to-point peer. Source UDP port 6696 is required. When MAC is configured,
real UDP endpoints feed authentication before normal decoding, and the selected
source is pinned when signing/sending. See [MAC](../guide/mac.md).

Per-interface senders retain semantic TLVs until their jitter window expires,
aggregate compatible work, and pace packets within engine deadlines. Sequence
changes and snapshot actions are ordering barriers. Queue admission is bounded
and nonblocking; an overloaded interface cannot indefinitely stall the shared
engine. Budget, expiry, and loss semantics live in [Capacity](../guide/capacity.md).

Interface recreation, control-family changes, and MAC-key changes replace the
socket instance. Queued input/output is tied to that instance and cannot cross
into a newly attached authentication session. Metric/timing updates retain the
live adjacency. Attachment requests current routes immediately.

## Route selection and export

The engine owns protocol observations; a `MetricProfile` creates per-neighbor
metric state and a `MetricAlgebra` extends advertised costs. Even custom metrics
must preserve positive link costs and strict finite metric increase. RTT wire
sampling remains in the engine; smoothing and cost mapping belong to the profile.
[Configuration](../guide/configuration.md#metrics-and-route-selection) defines the user-facing
smoothing and hysteresis policy. Changes to observations recompute candidates
without waiting for another Update.

A capacity-one export worker coalesces full desired-state generations. Snapshots
include selected learned routes and exact unreachable withdrawal holds; local
origins are advertised separately. The Linux backend periodically reconciles
the newest snapshot to repair drift even without a new RIB generation. It scopes
ownership by network namespace and route protocol, preserving other protocols.

Source views materialize destination-first lookups, inheriting covering-source
and ordinary routes. New tables are filled before their source rules activate;
obsolete rules are removed before table reuse. Linux route metrics are 65535 plus
the Babel metric, separate from source-rule priority. [SADR](../guide/sadr.md) describes
forwarding, host-policy integration, and non-atomic reconciliation limits.

## Persistence and failure

The daemon stores a stable Router-ID and consumes an orderly sequence checkpoint
before advertising. Durable writes use file fsync, atomic rename, and parent
fsync. A matching checkpoint starts at its sequence plus one; without it, the
daemon uses a random sequence. Version 1 and legacy identity files migrate to
the current format. Running sequence changes stay in memory.

Orderly shutdown retracts origins, attempts a checkpoint, waits for export work,
and removes owned routes/rules within one deadline. A dedicated daemon checkpoint
thread prevents stalled filesystem I/O from blocking Tokio runtime destruction;
it retains protocol ownership until completion or process exit. A stopping latch
prevents periodic reconciliation from restoring routes during cleanup.

An unexpected return or panic in a critical service ends the daemon with an error;
it does not attempt to reconstruct a subset of live protocol state. Transient
external I/O errors retry within their worker. Restart begins from an empty RIB
and removes stale state in the same ownership scope. Timing, drop behavior,
callback obligations, and returned errors are specified in
[Embedding](../guide/embedding.md#export-and-shutdown-responsibilities) and
[Configuration](../guide/configuration.md#shutdown-and-resource-limits).

## Module map

| Area | Implementation responsibilities |
| --- | --- |
| `babel-protocol::engine` | Event dispatch; interface/neighbor state; RIB and source history; timers; semantic output |
| `babel-protocol::wire` | Framing, contextual decoding, prefix encoding, packetization and timestamp stamping |
| `babel-protocol::mac` | Per-interface authentication, replay counters and challenge state |
| `babel-protocol::validation` | Shared configuration and local-event checks |
| `babel-router::router` | Builder/owner/handle API and serialized runtime loop |
| `babel-router::output*` | Scheduling, queue accounting and cancellation |
| `babel-rs::linux` | Source projection, kernel identity, reconciliation and cleanup ordering |

Tests beside private modules cover local invariants; crate-level integration
tests exercise public APIs and wire behavior. [Testing](testing.md) maps commands
to network fixtures. Historical defect reproductions are in the
[audit record](../history/rfc-audit-2026-09.md).
