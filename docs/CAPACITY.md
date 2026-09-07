# Capacity and overload isolation

Admission limits protect learned neighbor and candidate state. Defaults are:

```toml
[limits]
max_neighbors = 256
max_candidates = 16384
max_candidates_per_neighbor = 4096
```

The entire section and each individual field are optional. Embedders use
`ResourceLimits::default()` or `BabelRouter::builder().limits(...)`; a sans-I/O
consumer sets `EngineConfig.limits`. Zero prevents new admission for that
resource. Negative numbers and unknown configuration fields are rejected.
Limits are immutable for a running instance: reload rejects changes without
altering the active configuration. Explicitly writing an existing default is
not a change.

A neighbor is `(interface, address)`, independent of Router-ID. A candidate
is `(destination prefix, optional source prefix, neighbor)`. Two neighbors
announcing the same prefix consume two candidate slots, including an
unselected alternative. Different source prefixes consume separate slots.

At capacity, only new entries are refused. Existing neighbors still exchange
Hello/IHU; existing candidates still refresh, change Router-ID or metric,
retract, and expire. Refusing an Update does not prevent processing subsequent
TLVs in the packet. No rejected route backlog is retained. A rejected new
neighbor's Hello packet is ignored before neighbor state is allocated.

Retraction marks an existing candidate unreachable; it continues to occupy a
slot until normal protocol garbage collection. Candidate GC, neighbor expiry,
and interface removal release capacity. Later announcements can then be
accepted automatically, without reload or restart. An exception, process
restart, or neighbor-wide withdrawal is never an admission-limit action.

`status` reports effective `limits`, current `candidates`, `sources`,
`pending_requests`, and `unreachable_routes`, plus cumulative
`rejected_neighbors`, `rejected_candidates_global`, and
`rejected_candidates_per_neighbor`. Each `neighbors` item includes its
`candidates` and cumulative `rejected_candidates` since that adjacency was
created. Neighbor rejections count Hello-containing packets; candidate
rejections count finite Update TLVs, including repeated attempts. If both
candidate limits apply, the per-neighbor reason takes precedence. Admission
warnings summarize cumulative counts at most once every 30 seconds; status
counters update immediately.

The per-neighbor default leaves global room for other neighbors. Aggregating
routers legitimately advertise more prefixes than leaf routers: size limits
for the expected topology, accounting for alternate paths. Multiple neighbors
can still fill the global budget, at which point all new candidates are
refused. Admission is first-come, without route-priority eviction.

Each interface can occupy at most four slots in the common receive queue,
including an event being processed. Output actions for the same destination
and timing are batched before enqueueing, preserving TLV order and sequence-change
boundaries. Unchanged refreshes and rejected Updates skip full route selection;
batch withdrawal computes hold deadlines and request targets in single passes.

Output admission is nonblocking: a full interface queue refuses the new batch,
without delaying the protocol loop or another interface. Each interface has a
256-batch channel and a **16 MiB accounted-byte budget**, shared by channel,
scheduler and in-flight send. The charge conservatively includes semantic TLVs,
nested vector capacities and packetization overhead; it is not measured RSS.
A single oversized batch is refused too. Reservations remain held through
packetization until the last datagram of the associated batch is sent or
removed. These are internal transport constants, not extra configuration knobs.

Each socket send waits at most **100 ms**, and observes interface removal while
waiting. Queued work expires **1 second after its scheduling deadline or its
admission time, whichever is later**. The latter allows fresh work from a late
protocol tick to be sent. Immediate messages therefore get a transport grace
period; their scheduling deadline is still used to count lateness. Merging
fresh work never extends an older batch's expiry. Expiry also runs when MTU
lookup fails. Recovery does not replay expired backlog or require a restart.

All message types follow this loss policy. There are no priority queues or
transport-level retries. Lost Updates can be repaired by periodic announcements
and requests; lost retractions may leave a peer's old route until a later
response or route expiry. Repeated Hello/IHU loss can make the affected
interface's adjacencies fail. This policy protects other interfaces' progress;
it does not promise continuity on a congested interface, or prompt withdrawal
when output is being dropped. Persistence ordering is unchanged.

`interfaces[].output` reports the budget, current charge, refused batches/TLVs,
expired batches, dropped/expired datagrams, socket-send timeouts and scheduling
deadline misses. These counters reset when the interface is reattached.
Warnings summarize per-interface losses/lateness at most once every 30 seconds.
Normal full dumps of 16,384 route candidates fit the default output budget;
larger route tables, including local origins, need fresh capacity validation.

These three learned-state limits and the output budget do not impose a hard
process-memory or packet-rate bound. Origins, feasibility history and request
state keep their existing lifetimes. A custom metric implementation can also
allocate state. The operational target is to contain excess announcements and
local output stalls while keeping healthy interfaces and forwarding working,
with automatic recovery.

## Validation

Output fault-injection tests use controlled transports and paused time to
exercise send timeout, MTU failure, cancellation, expiry and recovery. The real
engine/command loop is tested with a blocked sender beside a healthy sender;
status and origin changes must finish before the blocked send times out.
Separate accounting tests cover queued, pending, encoded and in-flight data,
including oversized nested payloads. A complete 16,384-route dump is decoded
at a 1,232-byte UDP budget twice to detect recurring missing prefixes.

```sh
cargo test -p babel-router
```

Small deterministic tests cover exact thresholds, alternate candidates,
source prefixes, interface identity, Router-ID changes at capacity, mixed
rejected/withdrawn TLVs, garbage collection, and interface recreation:

```sh
cargo test -p babel-proto --test capacity
```

The release-mode engine workload takes the candidate count per neighbor. It
creates four neighbors, learns routes in batches of 32 Updates, repeats a
stable refresh, sends 1,000 rejected batches, retracts one whole neighbor's
routes, and reannounces them. Output is JSON lines with event timings and
state counts; generation and input cloning are outside event timing. The
virtual protocol time is fixed to isolate those operations; timer behavior
is covered separately by deterministic and network tests. This is a synthetic
operation benchmark, not a convergence or packets-per-second guarantee.

```sh
cargo run --release -p babel-proto --example capacity -- 1024
cargo run --release -p babel-proto --example capacity -- 4096
```

Run on an idle machine, record the build and machine, and compare the same
batch size. Do not overlap compilation with measurement. Defaults are
operational admission budgets, not a guarantee that every topology at those
limits will meet every configured timer.

The root-only VM test uses a separate raw Babel generator and three daemons:

```text
A (excess announcements) --> B <-- C (healthy origin)
                            |
                            D (downstream)
```

It checks B's per-neighbor bound while C can add and withdraw routes through B
to D, all healthy B adjacencies stay reachable, and D can ping C over two hops.
The daemons use the shipped wired 2-out-of-3 policy and default 4-second Hello /
16-second Update timers. The faulty adjacency itself is allowed to fail.
It records RSS and control latency, stops A to exercise neighbor expiry and
capacity release, then starts A with different prefixes and verifies automatic
multihop recovery. It also verifies rejection of limit changes on reload.

```sh
BABEL_RS_E2E_HOST=router-test-vm tests/e2e/run-on-linux-vm.sh capacity
# Or directly on a disposable root test host with the binary and script:
BABEL_RS_CAPACITY_SECONDS=200 BABEL_RS_CAPACITY_PPS=250 \
  python3 netns-capacity.py ./babel-rs
# Exercise the default per-neighbor budget (smoke runs use 256):
BABEL_RS_CAPACITY_PER_NEIGHBOR=4096 \
  python3 netns-capacity.py ./babel-rs
```

The default run lasts 45 seconds under excess announcements, with a target of
250 packets/second and up to 32 Updates per packet. These are generator targets,
not measured receive rates. Ordinary CI checks protocol invariants rather
than wall-clock thresholds. Longer fixed-host runs are appropriate before
raising production limits.

An exploratory 4,096-candidate run with 200 ms Hellos showed transient
adjacency/forwarding loss during initial learning, even after queue isolation.
Those aggressive timers are not covered by this capacity validation; deployments
that shorten them need their own load test. The admission limits do not promise
lossless forwarding under every valid timer and offered-load combination.
