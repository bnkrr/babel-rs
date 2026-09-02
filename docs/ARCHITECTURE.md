# Architecture

## Boundaries

`babel-proto` is the single protocol state owner. Calls provide an explicit
event and monotonic time; results are `Action`s for packet transmission,
selected-route replacement, or sequence persistence. It performs no I/O,
spawns no tasks, reads no clock, and contains no async or operating-system
types.

The wire decoder checks framing and TLV lengths before semantic decoding. Its
per-packet context tracks Router-ID, Next-Hop and compressed prefixes, as
required because an Update is not independently decodable. Unknown ordinary
TLVs are retained, unknown optional sub-TLVs are skipped, and an unknown
mandatory sub-TLV discards only its enclosing TLV.

`babel-router` owns one serialized engine plus orthogonal per-interface UDP
receiver tasks. Interfaces bind UDP/6696 with `SO_BINDTODEVICE`, join
`ff02::1:6`, use hop limit 1, and accept only non-local unicast link-local
sources. Bounded command and receive queues feed the engine. The public
runtime boundary is:

```text
BabelRouterBuilder -> BabelRouter -> RouterHandle
                            |
                            +-> RouteStream
                            +-> RouteExporter(reconcile + shutdown cleanup)
                            +-> SequenceStore
```

`babel-rs` adds strict TOML, signals, versioned state, a versioned Unix control
socket, an interface supervisor, and a Linux netlink exporter. Interface patterns are desired state. Netlink
events plus periodic snapshots reconcile them against name, ifindex,
administrative state and IPv6 link-local addresses. Removing or replacing an
interface sends `InterfaceDown` to the engine before a new socket is attached.

The exporter enumerates the configured protocol ownership scope, replaces
desired routes and rules, and deletes stale owned state. A periodic full
reconciliation repairs FIB drift and retries transient netlink failures even
when the selected RIB generation has not changed. It never imports routes into
the Babel RIB; origins are explicit configuration or library API calls.

Linux export is expressed as policy views. An ordinary view receives ordinary
routes. A source view receives ordinary fallbacks plus matching RFC 9079
routes, with an exact source route winning at the same destination. Both IPv4
and IPv6 are projected as destination-only routes in the view table; an
optional `from S` rule selects that table. This avoids relying on unsupported
IPv4 source-prefix route attributes. Dynamic route priorities start at 65535,
after the complete 0..65534 static metric range.

## Route model

The route key is `(destination prefix, optional source prefix)`. Destination
and source must share an address family. Next hop is independent, so an IPv4
route may legitimately use an IPv6 link-local Babel neighbour. Selected routes
retain Router-ID, sequence number, computed metric, interface and next hop.

The engine keeps neighbour, feasibility/source, candidate, selected,
originated and pending-sequence-request state separately. One event is applied
atomically; route selection then emits one new generation. Learned routes use
split horizon on their ingress interface. Selection changes are advertised
immediately, including an infinity retraction when the last selected path
disappears.

## Metric model

The engine owns RFC protocol observations, not a link-quality formula. It
maintains independent 16-bit Multicast and Unicast Hello histories, IHU hold
state, RFC 9616 timestamp echo state, and validated RTT samples. A
`MetricProfile` creates one `NeighborMetric` per adjacency. That state computes
the receive cost advertised in IHU, the peer-advertised transmission cost, and
the final link cost. A separate `MetricAlgebra` extends an advertised route
metric across the link.

The built-in profiles are RFC 8966 k-out-of-j wired sensing, RFC 8966 ETX, and
the RFC 9616 RTT policy composed over either base. Algorithm constants live in
those profiles; the engine retains only protocol constants such as infinity.
It rejects zero link costs and any finite extended metric that is not strictly
larger than the advertised metric, even for a custom implementation.

Candidates retain both the received advertised metric and their current
computed metric. Hello, IHU, RTT, or hold-timer changes recompute every affected
candidate without waiting for another Update. Route selection also maintains a
smoothed candidate metric with a three-Hello time constant and changes next hop
only when the alternative is better in both instantaneous and smoothed cost,
as recommended by RFC 8966 Appendix A.3.

RFC 9616 wire behaviour remains in the engine: Timestamp sub-TLVs use the
Hello/IHU Mills exchange, monotonic timestamps, modulo-32-bit arithmetic, and
the recommended three-minute stale-sample bound. EWMA smoothing and bounded
RTT-to-cost mapping belong to `RttMetric` and are therefore replaceable.

## Persistence and failure

Router-ID and local sequence number share a versioned TOML state file. Startup
increments the stored sequence before advertising. Each change is written to a
new file, fsynced, renamed, and followed by a parent-directory fsync.

Malformed datagrams are dropped without affecting protocol state. Export
failure leaves the selected RIB intact and is reported; the periodic reconciler
retries its complete snapshot. SIGHUP validates a full candidate before
applying interface, origin and export diffs; invalid input keeps the old active
configuration. Router-ID and state-file location remain immutable during a
process lifetime. Graceful shutdown sends infinity retractions for all current
local origins while interface sockets remain open, then exports an empty
snapshot and removes owned policy rules through the exporter's distinct
shutdown hook. Abrupt process death is recovered by neighbour expiry and by the
initial empty reconciliation on the next local start.

Long-running protocol, interface, exporter, and control tasks are a single
failure domain: an unexpected return or panic exits nonzero rather than trying
to reconstruct a possibly inconsistent subset in process. Transient external
I/O failures stay inside their task and retry. Sequence-state persistence is a
protocol invariant and failure is fatal.
