# Automated regression and steady-state testing

The standard Rust workflow runs unit/integration tests, executable rustdoc
examples, Clippy and documentation checks, plus the Rust 1.90 compatibility
check. The network workflows build the release daemon and exercise it inside
disposable Linux namespaces; the daemon uses production protocol defaults.

## Network CI

Every push and pull request runs the seven interoperability/lifecycle/RTT/MTU
regressions and four independent robustness jobs:

| Job | Checks |
| --- | --- |
| `capacity` | Excess announcements remain bounded while healthy neighbors, forwarding and route changes continue; capacity is reusable |
| `state-restart` | Orderly checkpoints, SIGKILL, missing state and a deliberately stale checkpoint recover through a three-node network |
| `shutdown-recovery` | Real stalled netlink/fsync obey cleanup deadlines and startup removes only owned stale state |
| `control-clients` | Idle, trickled-request and blocked-response clients expire; their slots can be reused without disrupting a healthy client |

Each robustness job retains its stdout/stderr as a GitHub Actions artifact,
including diagnostics printed by failed tests. The shutdown job builds the
existing test-only netlink preload locally. None of these tests changes a
production fault-injection interface.

The state-restart job allows 20 minutes including build time because random
restart sequences can fall behind surviving feasibility history. An individual
unclean restart has a 240-second recovery deadline. Ordinary CI does not impose
machine-specific performance thresholds on the capacity benchmark.

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

`all` runs the eleven existing regression groups plus the 120-second
steady-state smoke test. The explicitly selected `steady-state` mode defaults
to one hour. `BABEL_RS_STEADY_SECONDS` accepts
120..86400 seconds for VM runs. For direct runs, `--rss-growth-kib` adjusts the
RSS tolerance when comparing a different platform; record overrides with the
result. No fuzz targets are included in this testing scope.
