# Testing babel-rs

Run commands from the repository root. This guide maps changes to reproducible
checks. Network fixtures declare interval overrides; many recovery tests use
the normal 4-second Hello / 16-second Update intervals.

## Local source and package checks

[CONTRIBUTING.md](../../CONTRIBUTING.md#local-checks) lists format, Clippy, workspace,
rustdoc, and Python checks. [Protocol coverage](conformance.md#regression-map)
indexes the RFC regressions. Keep new tests with the affected module or public
integration suite.

For distribution changes, run the [archive-consumer check](releasing.md#verify-before-uploading).
It builds actual archives and verifies external consumption, library tests/docs,
and daemon installation without publishing. Local archive checks and actual
crates.io consumption are separate stages.

The [binary release rehearsal](releasing.md#verify-before-uploading) checks static
musl bundles and runs the archived daemon through the three-node network fixture
on native Linux x86_64 and ARM64 runners. The release-tool Python regressions
cover tag/event gates, changelog extraction, archive provenance and checksums,
partial upload recovery, and refusal to overwrite published artifacts. They mock
external publication; they do not upload or establish a hosted workflow result.

## Linux network suite

Use a disposable Linux host with root privileges, iproute2, ping, Python 3, and
babeld/BIRD for interop scenarios. The VM wrapper builds locally, then copies
only binaries and runtime files. It also builds the public route-policy example
and the test-only netlink fault preload, so the build host needs a C compiler.
Set the host and, if needed, SSH configuration:

```sh
BABEL_RS_E2E_HOST=router-test-vm tests/e2e/run-on-linux-vm.sh all
BABEL_RS_E2E_HOST=router-test-vm tests/e2e/run-on-linux-vm.sh mac-sadr
```

`BABEL_RS_SSH_CONFIG`, `BABEL_RS_CARGO_BIN`, and `BABEL_RS_E2E_REMOTE_ROOT`
customize the wrapper; remote assets default to `/tmp/babel-rs-e2e`. Builds
honor the caller's Cargo configuration, cache, target directory and toolchain.
Each scenario creates disposable namespaces and cleans
its owned processes/links on exit. Failed tests preserve or print diagnostics;
inspect cleanup outcomes before reusing the host.

| Wrapper mode | Assertions / fixture |
| --- | --- |
| `babeld`, `bird` | Wire exchange, IPv6 and IPv4-via-IPv6 routes, IPv6 SADR, withdrawal/recovery; `netns-babeld.sh`, `netns-bird.sh` |
| `rfc-boundaries` | IPv4 control with IPv6 disabled and MTU 576; control-family reload, independent RTT and unnumbered ICMPv4; `netns-rfc-boundaries.py` |
| `general` | Shared LAN, live IPv4 next-hop/address changes, asymmetric and partial loss; `netns-general.py` |
| `route-policy` | Live runtime import/export replacement, retained adjacency, withdrawal and recovery; `netns-route-policy.py` |
| `mac-sadr` | Both MAC algorithms, wrong-key isolation, rotation/restart, MTU splitting, IPv4/IPv6 overlapping-source transit and cleanup; `netns-mac-sadr.py` |
| `three-node` | Multi-hop convergence, link failure, withdrawal and recovery; `netns-three-node.sh` |
| `rtt`, `rtt-multipath` | Timestamp exchange and actual delayed-path selection/hysteresis; `netns-rtt.sh`, `netns-rtt-multipath.sh` |
| `lifecycle`, `mtu-output` | Interface glob/rebind/reload, FIB repair and live-MTU packetization; `netns-lifecycle.sh`, `netns-mtu-output.sh` |
| `capacity` | Excess announcements remain bounded while an independent healthy path forwards and updates; `netns-capacity.py` |
| `state-restart` | Orderly checkpoint, SIGKILL, missing/stale state and recovery; `netns-state-restart.py` |
| `shutdown-recovery` | Stalled netlink/fsync obey cleanup deadlines, startup removes only owned stale state; `netns-shutdown-recovery.py` |
| `control-clients` | Idle/slow/blocked client deadlines and connection-slot reuse; `netns-control-clients.py` |
| `combined-failures` | Partition/merge and origin replacement, simultaneous link/relay failure, third-path recovery and export-table migration; `netns-combined-failures.py` |
| `steady-state` | Stable forwarding while a leaf repeatedly changes or restarts; `netns-steady-state.py` |

Fixtures are under [tests/e2e](../../tests/e2e). `all` includes these regression groups
and a 120-second steady-state smoke run; it excludes the optional endless harness.
The [E2E workflow](../../.github/workflows/e2e.yml) runs network/interop checks and
five separate robustness jobs on branches/PRs. Both ordinary version tags and
crate publication tags call the same checks through their independent workflows.
Robustness jobs retain their logs as artifacts.

The stale-checkpoint and combined-crash fixtures allow 240 seconds for recovery;
partition/merge allows 90 seconds. These are regression deadlines, not deployment
SLAs. Assertions check restored forwarding and current route state, not merely
that a process is alive or a timestamp was exchanged.

## Steady-state testing

The four-node fixture uses an A–B–C forwarding path with leaf D attached to B.
It repeatedly adds/withdraws an origin at C, cuts/restores B–D, and restarts D
with retained identity and a changed origin metric. Healthy-path route/export
checks and bidirectional pings continue while D converges.

```sh
cargo build --release --locked -p babel-rs
sudo python3 tests/e2e/netns-steady-state.py "$PWD/target/release/babel-rs" \
  --seconds 3600 --seed 1
```

The VM wrapper's explicit `steady-state` mode defaults to one hour. Override with
`BABEL_RS_STEADY_SECONDS` in 120..86400. Duration starts after initial convergence;
an in-progress phase plus final recovery/cleanup can take additional time. Every
transition has a 60-second convergence budget. At least one full cycle must finish.

Runs shorter than ten minutes record resource usage without asserting a plateau.
Longer runs compare the 180..300-second warmup with the final 60 samples: median
RSS growth is at most 16 MiB by default and final FD count at most warmup maximum
plus eight. `--rss-growth-kib` changes the RSS threshold. Record any override;
these are regression bounds for this fixture, not universal leak/capacity claims.

The [steady-state workflow](../../.github/workflows/steady-state.yml) runs 120 seconds
on pushes/PRs, one hour weekly on Sunday at 04:00 UTC, and 120/600/3600 seconds
on manual dispatch. It uploads progress and final reports. Short smoke results
do not substitute for longer runs.

## Endless topology testing and replay

The opt-in [endless harness](../../tests/endless/README.md) tests seeded node/link
churn with babel-rs, babeld and BIRD. Its README covers graph and mix parameters,
verification, replay, artifacts and cleanup. Network campaigns are excluded from
normal CI; the harness's model and wrapper unit tests run in CI.

## Capacity experiments

The [capacity guide](../guide/capacity.md) defines admission and output budgets. Use these
additional commands for deterministic history churn and synthetic operation cost:

```sh
cargo test --locked -p babel-protocol --test source_churn
cargo run --release --locked -p babel-protocol --example capacity -- 4096
sudo env BABEL_RS_CAPACITY_PER_NEIGHBOR=4096 \
  python3 tests/e2e/netns-capacity.py "$PWD/target/release/babel-rs"
```

The source-churn test rotates 28,800 source/Router-ID pairs through 32 candidate
slots over 15 simulated minutes. The capacity benchmark exercises admission,
refresh, rejection, withdrawal, and reannouncement; it measures operation cost,
not network convergence or packet throughput. Run benchmarks on an idle machine
without concurrent compilation. For direct fixture runs, load duration/rate
can be set with
`BABEL_RS_CAPACITY_SECONDS` and `BABEL_RS_CAPACITY_PPS` (defaults 45 seconds and
250 target packets/second); these are offered-load settings, not received rates.
