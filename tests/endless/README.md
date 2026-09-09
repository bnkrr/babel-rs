# Endless netns E2E (opt-in only)

This is a standalone randomized network-state test for the Linux daemon. It is
**not included in Cargo tests, normal network E2E `all`, or any CI workflow**.
It uses Python 3.11+ standard-library modules, iproute2, ping and a locally built
daemon with export-progress status support. Run on a disposable root Linux host.

## Running

From the repository root:

```sh
cargo build --locked --release -p babel-rs
sudo env PYTHONDONTWRITEBYTECODE=1 python3 tests/endless/netns.py \
  "$PWD/target/release/babel-rs" \
  --nodes 8 --avg-degree 3 --graph mesh --seed 1
```

The default `--rounds 0` runs until failure or interruption. For a finite check:

```sh
sudo env PYTHONDONTWRITEBYTECODE=1 python3 tests/endless/netns.py \
  "$PWD/target/release/babel-rs" \
  --nodes 4 --avg-degree 2 --graph mesh --seed 1 --rounds 4
```

A separate VM entry point builds locally and copies only the binary and four
runtime Python files. It accepts the same test options (without the binary):

```sh
BABEL_RS_E2E_HOST=router-test-vm \
  tests/endless/run-on-linux-vm.sh --nodes 8 --avg-degree 3 --graph bottleneck
```

It also accepts `BABEL_RS_SSH_CONFIG`, `BABEL_RS_CARGO_BIN` and
`BABEL_RS_ENDLESS_REMOTE_ROOT`. The default remote asset directory is
`/tmp/babel-rs-endless`. Use separate asset
roots for concurrent wrapper invocations, so a running binary is not overwritten.
Artifacts are on the VM. Keep the SSH session open; the wrapper allocates a PTY
so terminal interrupt/disconnect can request cleanup. For unattended operation,
run within a persistent session on the test host. SIGINT/SIGTERM/SIGHUP stop the
run, save diagnostics and clean up (exit 130). SIGKILL or host failure cannot
execute Python cleanup; the recorded namespace prefix identifies that run's
resources for manual cleanup. No global namespace deletion is performed.

A finite successful run exits 0; verifier/setup failure or cleanup failure exits
nonzero. The script does not leave an endless background job after a finite run.

For unattended testing across failures, select `--on-failure next-seed`. Each
failure is preserved, its environment is cleaned, and the next child starts
from a fresh full topology with `seed + 1`. For example:

```sh
sudo env PYTHONDONTWRITEBYTECODE=1 python3 tests/endless/netns.py \
  "$PWD/target/release/babel-rs" --nodes 32 --min-nodes 16 --avg-degree 4 \
  --graph bottleneck --seed 20260908 --on-failure next-seed
```

The artifact root then contains `campaign.json` (current child, seed and failure
count) and independent `run-NNNNNN-seed-S/` directories. Snapshots are evidence
for analysis/replay, not process checkpoints or input to the next run. Existing
failure directories are never overwritten or deleted. No seed change occurs on
operator stop or a finite successful run. Earlier failures remain counted and
a finite completion after any failure still exits nonzero.

Cleanup must succeed before changing seeds. The supervisor also stops after
three consecutive failures before initial convergence, unexpected child exit
without confirmed cleanup, or insufficient disk headroom for another run
(`256 + 4 * nodes` MiB). Per-run logs are bounded, but retained failures consume
additional disk over time. Signals are forwarded to the active controller;
the supervisor waits for its cleanup. Keep a background service's stop timeout
above 90 seconds (for example 110 seconds) when using this mode.

## Parameters and graph semantics

| Option | Default | Meaning |
| --- | --- | --- |
| `--nodes` | 8 | Full node pool, initially all present, currently 3..64 |
| `--max-nodes` | same as `--nodes` | Alias for the maximum node pool size |
| `--min-nodes` | 2 | Minimum active nodes, 2..nodes; equal to nodes disables node deletion |
| `--mix` | `babel-rs=1` | Per-slot random weights, e.g. `babel-rs=2,bird=1,babeld=1`; nonnegative integers with a positive total |
| `--bird`, `--babeld` | executable names on PATH | Foreign daemon binaries on the test host; only required when their weight is positive |
| `--avg-degree` | min(3, graph maximum) | Initial average degree `2 * edges / nodes`, rounded to the nearest whole edge |
| `--graph` | mesh | Graph shape below |
| `--seed` | 1 | Independent seeded topology/event generators |
| `--on-failure` | stop | Stop on failure, or archive/clean and start fresh with the next seed |
| `--changes` | 2 | Operations per round, 1..10, followed by a quiet convergence phase |
| `--rounds` | 0 | Number of mutation rounds; 0 is endless; initial convergence is round 0 |
| `--settle-timeout` | 300; 600 for a foreign mix | Deadline for startup, each mutation batch and each convergence phase, including verifier work |
| `--stable-seconds` | 10 | Required continuous successful audit window before further mutations |
| `--probe-pairs` | 16 | Rotating connected ordered pairs probed per audit, in addition to one unreachable pair when available |
| `--rss-growth-mib` | 64 | Allowed rolling-median growth after per-process warmup |
| `--artifacts` | `.local/experiments/endless/<time>-<pid>` | New directory, never an existing run directory |

Graph types:

- **mesh:** a random spanning tree plus random additional undirected edges.
  It is connected initially; setting degree to `N-1` creates a complete graph.
- **bottleneck:** two internally connected random groups with exactly one
  inter-group bridge. Extra edges stay inside groups. Requests exceeding this
  graph's density limit are rejected instead of silently removing the bottleneck.
- **hub:** a star around one randomly selected hub, plus random leaf-to-leaf
  edges up to the requested density. Dense settings can diminish the hub's role.

Requested degree must be possible for a connected graph of the chosen shape.
For example, six bottleneck nodes allow degree 1.66667..2.33333; eight allow
1.75..3.25. The initial edge list and subsequent actual degree are recorded.

The initial graph defines potential links for the whole run. Operations only
add/delete node slots and bring potential links up/down; they do not regenerate
the graph, alter origins independently, change metrics or mutate packets.
Deleting a node kills its daemon and removes its namespace and incident veths.
Adding it recreates the namespace and links to present peers, reusing its fixed
Router-ID, origin and state file. Per-link desired up/down state is retained
while an endpoint is absent. The realized node count, degree and connectivity
therefore fluctuate. Active node count stays between `--min-nodes` and `--nodes`;
this bound does not guarantee that those nodes stay connected. For example,
`--nodes 32 --min-nodes 16 --avg-degree 4 --graph bottleneck` keeps 16–32 nodes
present while still allowing partitions. Every node originates one fixed IPv6 /128; no IPv4,
source-specific or RTT coverage is claimed by this particular harness.

The daemon's production Hello/Update and route-selection defaults are retained.
Admission limits remain default. The 64-node fixture ceiling keeps its one-prefix
per-node workload below those learned-state defaults even for a complete graph;
it is not a production capacity recommendation. Larger-scale capacity testing
needs its own resource assumptions. A slow host may need a larger convergence
budget because the verifier's subprocesses and probes also consume time.

## Mixed implementations

The same topology, node/link operations, failure snapshots and next-seed
supervisor can run babel-rs, BIRD (2.x/3.x with Babel) and babeld together:

```sh
sudo env PYTHONDONTWRITEBYTECODE=1 python3 tests/endless/netns.py \
  "$PWD/target/release/babel-rs" \
  --min-nodes 16 --max-nodes 32 --avg-degree 4 --graph bottleneck \
  --mix babel-rs=2,bird=1,babeld=1 --seed 20260908 --on-failure next-seed
```

Weights are sampling probabilities, not exact counts or per-implementation
minimums. Each slot is assigned once using an independent seeded RNG and keeps
its implementation when deleted/recreated. Thus node churn changes the online
mix, and a small pool or partition can contain none of a requested type. The
manifest and `--plan` show the actual assignment; each verified event includes
online counts. Changing only the mix preserves the graph and mutation sequence.
The default remains all babel-rs. A next-seed campaign generates a fresh graph
and implementation assignment for every child.

All implementations use wired interfaces with 4-second Hellos and 16-second
Updates, one IPv6 /128 origin and fixed unique Router-IDs. BIRD's router-ID
randomization stays off. Each daemon has its own foreground process, config,
Unix control socket, state/PID paths and bounded log; no TCP control service is
enabled. Foreign binaries run from the VM installation and are not installed or
upgraded by the runner. The manifest records paths, versions and SHA256 for all
implementations present in the pool.

All instances export into table 201 inside their own namespace. Using a low
table avoids babeld Linux builds that truncate large table IDs to eight bits.

Deleting a node remains a SIGKILL for every implementation. Startup state
behavior stays native: babel-rs/babeld consume orderly sequence checkpoints;
BIRD starts its sequence afresh. Surviving source history can therefore delay
recovery for minutes. Mixed runs default to a 600-second phase deadline to
include foreign source GC and the stable window; this does not claim that
ordinary link recovery should take that long. Individual timings remain in the
event log. An explicit `--settle-timeout` overrides either default.

## Verifier

The independent host model owns node liveness, potential links, link state and
origin ownership. Graph connected components determine expected reachability.
Babel's metrics, feasibility history and another daemon are not the expected
answer. Each audit checks **every active node and every modeled destination**:

- Every implementation's active kernel FIB routes match graph reachability;
  missing paths and stale unicast routes to removed/disconnected nodes fail.
- Next-hop interfaces and gateways identify live modeled links. Following each
  FIB path across any mixture reaches its origin without a cycle. The fixture
  uses single next hops and does not support ECMP.
- babel-rs also exposes its selected RIB, which must agree with reachability
  and FIB. Foreign internal RIBs are not parsed or fabricated from the FIB;
  their control dumps are retained in failure diagnostics.
- Each babel-rs exporter acknowledges a fixed route/config generation sampled at the
  start of the stable window (or a newer generation) within ten seconds. New
  route generations do not move that checkpoint or reset the window. Exporters
  must still report no current error and a successful reconciliation younger
  than ten seconds; all checkpoints must be acknowledged before a phase passes.
- Real IPv6 probes rotate over connected ordered pairs. One disconnected or
  removed destination is also probed when available and must not answer.

Temporary unreachable hold routes are allowed. The verifier accepts valid
non-shortest paths. Intermediate route/FIB mismatches and probe failures reset
the stable window and are allowed only until the phase deadline. Process exits,
established control-socket failures and resource violations fail immediately.
New processes get the convergence budget to expose their initial control socket.
An actual export error, stale health or overdue checkpoint also resets the
window. Checkpoints are resampled after a reset. A frozen exporter cannot pass
by merely reporting successful reconciliation of an obsolete generation.

For connected components untouched by *any* operation in the round, up to two
ordered pairs are continuously probed during convergence and may not lose
forwarding. There may be no such component in a connected mesh: the test does
not falsely label a physically surviving path as unaffected by all routing
changes elsewhere in its component. The existing deterministic combined-failure
suite supplies a stricter deliberately unaffected-path scenario.

## Bounded state and failure records

Each node slot has at most one process and current log-drain thread. The host
retains only its latest observation and 60 RSS samples per current process.
FD use has a broad `64 + 8 * potential_degree` ceiling. After five minutes and
60 samples, the current median RSS becomes that process's baseline; subsequent
60-sample medians must stay within `--rss-growth-mib`. Recreating a process resets
its warmup. This is a gross-growth check, not proof of leak freedom, and no RSS
plateau is claimed for a process that has not completed warmup. Protocol occupancy,
rejection counts and export progress are sampled alongside RSS/FDs for babel-rs;
foreign processes receive the same RSS/FD checks and control-liveness checks.

Artifacts include:

- `manifest.json`: arguments, initial graph, origin map, Python/kernel versions
  implementation assignments, executable versions/SHA256, runner PID and
  namespace prefix. The exact three Python runtime
  files are copied beside it.
- `events.jsonl` and three rotated backups: operations, node implementations,
  babel-rs startup sequence numbers, phase timings and outcomes; up to about
  16 MiB total. Each unsuccessful
  audit records its stage/reason and interrupted stable-window duration;
  export mismatches include requested/acknowledged generations, age and error.
- `samples.jsonl` and one backup: resource samples; up to about 2 MiB total.
- `node-N.log` and one backup per slot: daemon output; about 2 MiB per slot.
- `latest.json`: the last verified topology and complete observations, replaced
  each round.
- `last-reset.json`: the latest interrupted stable-window observation and reason,
  overwritten on each reset. Samples also include the observed route generation.
- On failure/interruption: `failure.json`, bounded-time per-node diagnostics,
  and copies of current configs/state files. Failure metadata retains audit
  attempts, reset count and last rejection even during a new stable window.
  Disturbance stops before capture.
- `outcome.json`: exit code, last round, initial-convergence result and cleanup
  result, written after cleanup. A node already found dead is a test failure,
  not itself a cleanup failure; failure to stop remaining nodes or remove
  namespaces still prevents advancing to the next seed.

Files may exceed a rotation boundary by one bounded record. Diagnostic size is
bounded by the configured node pool and daemon control limits. Standard output
is a live progress stream; if externally redirected for an endless run, arrange
rotation for that destination too. Cleanup targets only this run's recorded
processes, namespaces and veths.

## Reproduction and verifier checks

Print a plan without root or network access:

```sh
PYTHONDONTWRITEBYTECODE=1 python3 tests/endless/netns.py \
  --plan --nodes 4 --avg-degree 2 --graph mesh --seed 1 --rounds 4
```

Topology/event randomness is isolated from timing-dependent probes and samples.
With the same parameters and harness version, the seed reconstructs the event
sequence even when old log segments rotate away; choose the failed round as
`--rounds` to replay up to that point. Real daemon sequence initialization,
scheduling and network timing remain nondeterministic, so this reproduces the
scenario, not an identical execution. Preserve the manifest, recent events,
failed observations and tested binary when investigating a failure.

The small model/oracle test suite deliberately supplies missing routes, FIB
mismatches, stale destinations and forwarding loops, and accepts a valid
non-shortest path. Run it explicitly; it is not registered with regular CI:

```sh
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s tests/endless -v
```

The separate root-only fixture regression holds an open reference to each
deleted namespace while immediately recreating its node, twenty times. This
checks that incident veth pairs are explicitly removed before names are reused,
even if namespace destruction is delayed. It also checks partial-link cleanup:

```sh
sudo env PYTHONDONTWRITEBYTECODE=1 python3 tests/endless/check_lifecycle.py \
  "$PWD/target/release/babel-rs" --nodes 3 --avg-degree 2 \
  --artifacts .local/experiments/endless-lifecycle/run-1
```

Like the endless runner, this check is opt-in and its artifact directory must
not already exist. It tests fixture lifecycle, not routing convergence.
