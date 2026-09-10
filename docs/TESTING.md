# Automated regression and steady-state testing

The standard Rust workflow runs unit/integration tests, executable rustdoc
examples, Clippy and documentation checks, plus the Rust 1.90 compatibility
check. The network workflows build the release daemon and exercise it inside
disposable Linux namespaces; the daemon uses production protocol defaults.

`crates/babel-protocol/tests/seqno_recovery.rs` checks that worse/equal infeasible
alternates do not originate unnecessary sequence requests, while preferable
alternates, an infeasible current path and route loss still trigger recovery.
Its three-node triangle delivers encoded packets with virtual time and verifies
loop-free recovery within 20 seconds of a direct-link failure, without relying
on source feasibility garbage collection. It runs in the regular Rust suite.

## Library and release checks

`crates/babel-protocol/tests/ipv4_policy.rs` decodes emitted wire packets to
check default and forced next-hop policies, address/policy changes, retractions,
IPv6 continuity and request responses. `crates/babel-router/tests/lifecycle.rs`
uses public APIs to check ownership cancellation, orderly exporter sequencing,
cleanup errors and deadlines. Configuration tests cover mode parsing and reload.

`tests/release/check-packages.py` builds actual package archives and tests a
consumer outside the workspace using only their extracted contents. It also
checks archive tests/examples/docs and installs the packaged daemon. See
[RELEASING.md](RELEASING.md) for the independent registry upload steps.
Protocol-only Windows/macOS tests and Linux MSRV tests are defined in CI; live
network support and evidence are scoped in [SUPPORT.md](SUPPORT.md).

## Shared LAN and next-hop policies

`tests/e2e/netns-general.py` puts two babel-rs instances and babeld on a common
bridge with IPv4 and IPv6 routes. It checks kernel gateways and bidirectional
forwarding, multiple neighbors on one interface, live `auto`/`ipv4`/`ipv6`
changes, IPv4 address removal/restoration, 100% one-way loss, continued forwarding
between unaffected peers, and recovery after partial loss. It uses production
protocol intervals. Run the VM wrapper with `general`, or include it with `all`.

A peer retaining an old next hop after a live mode change is recorded explicitly;
see [INTEROPERABILITY.md](INTEROPERABILITY.md). Dynamic gateway assertions use
the babel-rs peer. Ordinary numbered-interface exchange still includes babeld.

## Network CI

Every push and pull request runs the eight interoperability/lifecycle/RTT/MTU
regressions and five independent robustness jobs:

| Job | Checks |
| --- | --- |
| `capacity` | Excess announcements remain bounded while healthy neighbors, forwarding and route changes continue; capacity is reusable |
| `state-restart` | Orderly checkpoints, SIGKILL, missing state and a deliberately stale checkpoint recover through a three-node network |
| `shutdown-recovery` | Real stalled netlink/fsync obey cleanup deadlines and startup removes only owned stale state |
| `combined-failures` | Partition/merge with origin replacement, simultaneous primary-link loss and standby-relay crash, third-path recovery, and export revision/table migration |
| `control-clients` | Idle, trickled-request and blocked-response clients expire; their slots can be reused without disrupting a healthy client |

Each robustness job retains its stdout/stderr as a GitHub Actions artifact,
including diagnostics printed by failed tests. The shutdown job builds the
existing test-only netlink preload locally. None of these tests changes a
production fault-injection interface.

The state-restart job allows 20 minutes including build time because random
restart sequences can fall behind surviving feasibility history. An individual
unclean restart has a 240-second recovery deadline. Ordinary CI does not impose
machine-specific performance thresholds on the capacity benchmark.

## Combined failures

`tests/e2e/netns-combined-failures.py` uses three paths between A and D:
A--B--D (preferred), A--C--D (standby), and A--E--F--D (fallback). All daemons
use production Hello/Update intervals; the standby links have a higher wired
cost so path expectations are deterministic. Before faults begin, the fixture
waits for the intended A/E/F paths to be selected and exported; merely having
a route through some path is not sufficient.

- Isolate D by cutting all three incident links. Require the old routes to
  disappear from the other nodes' selected RIBs and active kernel routes.
  Replace an origin on D while partitioned, then reconnect. The new origin must
  propagate and forward; the withdrawn origin must not return.
- SIGKILL standby relay C and cut primary A--B without a convergence wait
  between faults. A must recover through E/F. Restart C with its existing
  identity and restore A--B; the preferred path must return.

Every recovery poll checks that A/E/F remain alive, their healthy adjacencies
remain reachable, exports remain healthy, and A<->F forwarding still works.
After recovery, the test traces selected next hops to reject a forwarding
cycle, checks the expected path and real bidirectional pings. It allows
transient A<->D loss while topology changes converge. Partition/merge phases
have a 90-second budget; combined crash recovery has 240 seconds. These are
regression deadlines, not an operational convergence SLA.

The same fixture changes B's export table without changing its RIB. It checks
both successful generation fields, unchanged-config reloads, actual table
migration and restoration. Shutdown must remove owned routes. Logs include
phase timings and failure diagnostics. Run with:

```sh
BABEL_RS_E2E_HOST=router-test-vm tests/e2e/run-on-linux-vm.sh combined-failures
```

## Steady-state scenario

`tests/e2e/netns-steady-state.py` runs four real daemons:

```text
A -- B -- C
     |
     D
```

A and C continuously originate stable IPv6 host routes. Each polling iteration
checks their learned routes, healthy adjacencies and actual forwarding in both
directions. The leaf D is allowed to disappear while the healthy path stays
usable. Repeating operations are:

1. Announce another origin from C and wait for propagation to A.
2. Withdraw it and wait for it to disappear from A's selected RIB.
3. Bring B's leaf interface down and wait for the leaf route withdrawal.
4. Restore the link and its IPv6 link-local addresses, then wait for readmission.
5. Gracefully restart D with its existing identity/checkpoint and a changed
   origin metric. Recovery must carry the new metric through B to A; an old
   retained route cannot satisfy the assertion.

Healthy-path checks continue during convergence waits. The test records
control latency, RSS, descriptor counts, candidate/source/request counts and
output-budget usage. All five operations must complete at least once. The
script uses the default 4-second Hello and 16-second Update intervals. The
reported seed chooses reproducible leaf metrics; event order is fixed.

Operations are separated by a minute in long runs, allowing steady forwarding
between disruptions. Short smoke runs compress that spacing, with a minimum
120-second requested duration. Every transition has a 60-second convergence
budget. Duration is measured after initial convergence; an in-progress phase
and final recovery/cleanup may take additional time.

For runs of at least ten minutes, RSS and FD checks compare the warmup window
(180..300 seconds) to the last 60 samples. Median RSS growth may not exceed
16 MiB by default; the final FD count may not exceed the warmup maximum by
more than eight. These broad thresholds detect sustained resource growth in
this small topology, not every memory leak or maximum production capacity.
Short runs record these measurements but do not assert a long-term plateau.

At the end, the test restores any offline leaf and withdraws the temporary
origin before checking final recovery. Phase counters count scheduled
operations, excluding this final restoration. Final shutdown must succeed
and leave no owned routes in the test tables.
Processes, namespaces and links are cleaned up on success, failure and SIGTERM;
failed runs print control snapshots, interface addresses and daemon log tails.

The `steady-state` workflow runs:

- **Push / pull request:** a two-minute scenario smoke test.
- **Weekly:** one hour, Sunday at 04:00 UTC.
- **Manual:** a selectable 120, 600 or 3600 seconds.

Every run uploads its progress and final JSON report. A completed short smoke
run does not substitute for a completed hour-long run.

## Local reproduction

On a disposable root Linux host:

```sh
cargo build --release --locked -p babel-rs
sudo python3 tests/e2e/netns-steady-state.py "$PWD/target/release/babel-rs" \
  --seconds 3600 --seed 1
```

The VM runner builds locally and copies only binaries/runtime files:

```sh
BABEL_RS_E2E_HOST=router-test-vm \
  tests/e2e/run-on-linux-vm.sh steady-state

BABEL_RS_E2E_HOST=router-test-vm BABEL_RS_STEADY_SECONDS=120 \
  tests/e2e/run-on-linux-vm.sh steady-state
```

`all` runs the twelve regression groups plus the 120-second
steady-state smoke test. The explicitly selected `steady-state` mode defaults
to one hour. `BABEL_RS_STEADY_SECONDS` accepts
120..86400 seconds for VM runs. For direct runs, `--rss-growth-kib` adjusts the
RSS tolerance when comparing a different platform; record overrides with the
result. No fuzz targets are included in this testing scope.

## Optional endless topology testing

A separate [endless netns harness](../tests/endless/README.md) supports bounded
node pools, configurable average degree, mesh/bottleneck/hub graphs and random
node/link lifecycle changes. It has an independent reachability/FIB verifier
and rotating failure records. `--min-nodes` / `--max-nodes` bound the live
network, and `--mix babel-rs=2,bird=1,babeld=1` assigns implementations to node
slots with seeded weighted sampling. All implementations receive FIB and
forwarding checks; babel-rs additionally receives RIB/export-progress checks.
It is deliberately excluded from ordinary CI,
Cargo tests and the network runner's `all` mode; invoke it explicitly.

## Bounded mixed-round replay

`tests/endless/replay.py --replay-round N` accepts the same daemon, topology,
seed, mix and artifact arguments as `netns.py`. It advances the seeded topology
to just before round N, starts fresh processes, verifies the network, applies
that round's mutations and verifies again with the same RIB/FIB/forwarding
oracle. Each phase has a finite settle deadline and cleanup runs on exit.
This replays topology and events, not the sequence/feasibility history of all
previous rounds. Node deletion records a bounded per-slot pre-stop status/RIB
snapshot to improve future restart diagnostics.

The 2026-09-09 mixed soak was operator-interrupted during round 170 after 169
completed rounds. At interruption BIRD node 17 lacked a route in both its RIB
and FIB; that observation alone does not identify a babel-rs or kernel-export
bug. The 2026-09-10 fresh-state replay (32 slots, minimum 16, degree 4,
bottleneck, seed 20260908, mix babel-rs=2,bird=1,babeld=1) passed round 170 and
cleanup. Historical accumulated-state behavior remains unproven; neither run
is reported as a completed mixed endless pass.
