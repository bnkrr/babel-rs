# Architecture

## Boundaries

`babel-proto` is the single protocol state owner. Calls provide an explicit
event and monotonic time; results are `Action`s for packet transmission,
selected-route replacement, or local sequence-number changes. It performs no I/O,
spawns no tasks, reads no clock, and contains no async or operating-system
types.

Inbound resolved messages and semantic outbound messages are distinct types.
The wire decoder checks framing and TLV lengths before semantic decoding. Its
per-packet context tracks Router-ID, Next-Hop and compressed prefixes, as
required because an Update is not independently decodable. Unknown ordinary
TLVs are retained, unknown optional sub-TLVs are skipped, and an unknown
mandatory sub-TLV discards only its enclosing TLV.

The outbound packetizer is the sole owner of Router-ID and Next-Hop context.
It repeats context after packet boundaries and emits independently decodable
datagrams within the live interface MTU minus IPv6 and UDP headers. This
applies equally to finite updates and retractions.

`babel-router` owns one serialized engine plus orthogonal per-interface UDP
receiver and bounded sender tasks. Every engine Send action includes an
absolute deadline and permitted jitter. Each sender keeps semantic TLVs
unencoded during that jitter window, aggregates work for the same destination,
then reads the current Linux MTU and packetises at release time. Datagrams are
paced by default, while an earlier deadline preempts the pacing gap. Output admission
never waits: each interface has a 256-batch channel and a shared 16 MiB
accounted-byte budget across channel, scheduler and in-flight send. New work
is dropped at capacity. Socket sends wait at most 100 ms; work expires one
second after the later of its scheduling deadline and admission time. Removal
cancels an in-flight wait. Per-interface loss, usage and deadline counters
expose degradation; warnings are rate-limited. See [CAPACITY.md](CAPACITY.md). Interfaces bind UDP/6696 with
`SO_BINDTODEVICE`, join
`ff02::1:6`, use hop limit 1, and accept only non-local unicast link-local
sources. Bounded command, receive, and output queues isolate the engine. Each
interface may hold at most four receive slots, including the event currently
being processed, so one burst cannot occupy the whole common input queue.
Same-destination Sends with identical timing are batched within an engine
action run; sequence-change and route-snapshot actions remain ordering barriers.
This avoids consuming an output-channel slot for every changed prefix. Route
export is an independent capacity-one desired-state worker: a slow exporter
may skip obsolete generations but must converge to the newest snapshot. The
public runtime boundary is:

```text
BabelRouterBuilder -> BabelRouter -> RouterHandle
                            |
                            +-> RouteStream
                            +-> RouteExporter(reconcile + shutdown cleanup)
                            +-> SequenceStore
```

`babel-rs` adds strict TOML, signals, versioned state, a versioned Unix control
socket, an interface supervisor, and a Linux netlink exporter. Ordered
interface rules are desired state. Netlink events plus periodic snapshots
reconcile names, ifindex, administrative state, IPv6 link-local addresses and
the resolved interface policy. Removing or replacing an interface sends
`InterfaceDown` to the engine before a new socket is attached. Attachment sends
an immediate wildcard Route Request so startup and policy reload do not wait
for every neighbour's periodic Update interval.

The exporter enumerates the configured protocol ownership scope, replaces
desired routes and rules, and deletes stale owned state. A periodic full
reconciliation repairs FIB drift and retries transient netlink failures even
when the selected RIB generation has not changed. It never imports routes into
the Babel RIB; origins are explicit configuration or library API calls.

Route snapshots also carry exact unreachable tombstones. The Linux exporter
installs them as unreachable routes until Babel's hold time ends, so a removed
specific route cannot fall through to a covering route and form the transient
loop described by RFC 8966 section 3.5.4.

Linux export is expressed as policy views. An ordinary view receives ordinary
routes. A source view receives ordinary fallbacks plus matching RFC 9079
routes, with an exact source route winning at the same destination. Both IPv4
and IPv6 are projected as destination-only routes in the view table; an
optional `from S` rule selects that table. This avoids relying on unsupported
IPv4 source-prefix route attributes. Dynamic route priorities start at 65535,
after the complete 0..65534 static metric range.
Because Linux policy rules choose a source table before destination lookup,
the daemon rejects overlapping nonzero source views; within that admitted
subset, source-first and RFC 9079 destination-first lookup are equivalent.

## Route model

Global neighbor/candidate and per-neighbor candidate admission limits reject
new state before allocation. Existing candidates still update and expire;
normal garbage collection and interface removal release their slots. Limits
and rejection counters are described in [CAPACITY.md](CAPACITY.md).

The route key is `(destination prefix, optional source prefix)`. Destination
and source must share an address family. Next hop is independent, so an IPv4
route may legitimately use an IPv6 link-local Babel neighbour. Selected routes
retain Router-ID, sequence number, computed metric, interface and next hop.

The engine keeps neighbour, feasibility/source, candidate, selected,
originated and pending-sequence-request state separately. One event is applied
atomically; route selection then emits one new generation. Split horizon is
decided independently by each egress interface; it defaults off for wireless
links and on for wired and tunnel links. Selection changes are advertised
immediately, including an infinity retraction when the last selected path
disappears.
Source entries are maintained only when a finite route is actually advertised,
are not refreshed by retractions, and expire after the RFC-recommended
three-minute source GC interval.

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
candidate without waiting for another Update. A reachable alternative must
beat the current route by both the configured absolute and percentage margins
for the complete `better_for_ms` interval. Falling below either margin resets
that interval, as does a recovery of the current metric by the same margin from
its worst value during the pending switch. Initial candidate discovery remains
unhindered until one route has stayed selected for a full dwell interval. Loss
of the current candidate also bypasses hysteresis and selects a reachable
replacement immediately.

RFC 9616 wire behaviour remains in the engine: Timestamp sub-TLVs use the
Hello/IHU Mills exchange, monotonic timestamps, modulo-32-bit arithmetic, and
the recommended three-minute stale-sample bound. RTT profiles may request an
independent per-neighbour unicast probe interval instead of waiting for the
regular IHU timer. Time-based EWMA smoothing and bounded RTT-to-cost mapping
belong to `RttMetric` and are therefore replaceable. Probe schedules use
per-neighbour jitter, enforce a 100 ms minimum interval, and cap work per
engine tick.

## Persistence and failure

The version 2 TOML state file retains a stable Router-ID. An optional local
sequence-number checkpoint is saved only on orderly shutdown; running sequence
changes remain in memory. Startup consumes a checkpoint belonging to the same
Router-ID and starts at its value plus one (modulo 65536). Without a matching
checkpoint it starts at a random sequence number. Version 1 checkpoints and
legacy single-line Router-IDs are migrated automatically.

Before advertising, startup durably replaces the file with identity-only state.
This prevents a crash from reusing a previous shutdown's checkpoint. Startup
fails if this write cannot complete. Writes use a temporary file, file fsync,
rename and parent-directory fsync. Orderly shutdown withdraws all origins in one
engine event, then checkpoints the final sequence number once, before exporter
cleanup. This save is best effort: errors or a one-second timeout produce a
warning and cleanup continues. The daemon uses one dedicated thread for this
save so stalled storage cannot hold Tokio runtime destruction open; the thread
retains protocol ownership until it finishes or the process exits.

After an unclean exit, a random sequence number can be older than neighbours'
retained feasibility history. Recovery can then take several minutes. No RIB or
neighbour state is written to disk. The embeddable runtime's `SequenceStore`
hook has the same once-per-orderly-shutdown policy; the host owns loading the
initial identity/sequence and handling any detached I/O it creates.

Malformed datagrams are dropped without affecting protocol state. Export
failure leaves the selected RIB intact and is reported; the periodic reconciler
retries its complete snapshot. SIGHUP validates a full candidate before
replacing desired state; invalid input keeps the old active configuration.
Origins are replaced as one engine event; interface and netlink state then
converge asynchronously. A changed per-interface policy is applied in place;
metric changes rebuild neighbour state from retained Hello/IHU observations,
while socket and selected-route state remain attached. Router-ID, state-file location,
route-selection policy, and Linux route protocol remain immutable during a
process lifetime. The route protocol is an exclusive ownership token within
one network namespace and is guarded by a process-life lock. Graceful shutdown
sends infinity retractions for all current
local origins while interface sockets remain open, then exports an empty
snapshot and removes owned policy rules through the exporter's distinct
shutdown hook. Abrupt process death is recovered by neighbour expiry and by the
initial empty reconciliation on the next local start.

The daemon bounds the entire cleanup sequence with one reloadable
`shutdown_timeout_ms` deadline (default five seconds), including control-response
flush and service joins. The checkpoint retains its separate one-second cap
within that budget. Expiry logs the unfinished stage, aborts outstanding router
and service tasks, and exits nonzero. A failed router also gets an explicit
best-effort exporter cleanup using the remaining time. SIGINT/SIGTERM can
interrupt a reload waiting for an asynchronous operation.

Linux export latches a stopping state before final cleanup waits for any
in-flight reconciliation. Later snapshots and periodic workers cannot restore
old routes after cleanup. The netlink connection retains protocol ownership
through runtime teardown. On restart, reconciliation removes obsolete owned
IPv4/IPv6 routes and rules across all tables, even after export-view changes or
with `manage_rules = false`; other protocols and namespaces remain untouched.
This also removes owned rules that do not match the exporter's source-rule
model. A changed protocol/namespace is a different ownership scope and is not
used to delete the previous scope's state.

Long-running protocol, interface, exporter, and control tasks are a single
failure domain: an unexpected return or panic exits nonzero rather than trying
to reconstruct a possibly inconsistent subset in process. Transient external
I/O failures stay inside their task and retry. A failed orderly-exit sequence
checkpoint is nonfatal; it may make the next restart converge more slowly.
