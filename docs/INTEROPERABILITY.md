# Interoperability

The executable suite is `tests/e2e/run-on-linux-vm.sh`. It builds locally, copies
only the binary and scripts to the Debian VM, and creates disposable network
namespaces. The reference versions currently installed there are babeld
1.13.1 and BIRD 3.1.7, on Linux 6.12.96+deb13-amd64.

The original scenarios below use IPv6 control transport; columns describe
payload routes. The added RFC boundary suite also tests pure IPv4 control with
IPv6 disabled and MTU 576, live control-family reload, independent babeld RTT,
and ICMPv4 over unnumbered links. MAC and overlapping SADR have a dedicated `netns-mac-sadr.py` fixture. DTLS remains deferred; see
[CONFORMANCE.md](CONFORMANCE.md). An RTT sample is evidence of timestamp exchange,
not proof of route selection or arbitrary-load timing.

| Peer/topology | IPv6 routes | IPv4 routes via IPv6 next hop | SADR | Lifecycle |
|---|---:|---:|---:|---|
| babel-rs ↔ babeld 1.13.1 | pass | pass | pass | retract, reannounce, restart |
| babel-rs ↔ BIRD 3.1.7 | pass | pass | pass | bidirectional exchange |
| shared LAN: two babel-rs + babeld | pass | pass between babel-rs | n/a | numbered IPv4, live policies/addresses, one-way and partial loss |
| three babel-rs nodes, line | pass | n/a | n/a | two-hop, failure, recovery |
| two babel-rs nodes, RFC 9616 | pass | n/a | n/a | Timestamp exchange and non-null RTT status |
| babel-rs lifecycle | pass | n/a | n/a | glob attach, rebind, reload, FIB repair |
| three babel-rs nodes, restart | pass | n/a | n/a | graceful checkpoint, SIGKILL, missing state, stale seqno |
| shutdown/startup cleanup | n/a | n/a | n/a | total deadline, stalled netlink/fsync, old tables and rules |

The babeld test also injects a stale route in the owned table/protocol and
requires startup reconciliation to remove it. Restart must preserve Router-ID,
advance sequence state, remove routes on SIGTERM and reconverge. The BIRD test
uses separate IPv4 and IPv6 SADR channels. The three-node test requires a
triggered withdrawal to cross the remaining adjacency before the advertised
route hold time expires.

`netns-state-restart.py` uses default 4-second Hellos and 16-second Updates on
an A–B–C line. It verifies that runtime origin changes leave the identity file
untouched, orderly exit saves the final sequence, and startup consumes it.
Crash and lost-state recovery must propagate the new instance's sequence and
a new origin to C before forwarding counts as restored. A deliberately old
checkpoint also tests recovery after peer feasibility history expires; this
case takes about three minutes and has a 240-second test deadline.

`netns-shutdown-recovery.py` uses a locally compiled test preload to withhold
netlink route-dump replies. It verifies the default five-second budget, a
reloaded 700 ms budget, critical-service failure cleanup, and that a stalled
checkpoint consumes part of the same total budget. A restart with different
export tables and disabled rule management must remove old owned state while
preserving a different protocol and another namespace. `netlink-stall.c` is a
test fixture only; the daemon contains no fault-injection switches.

The RFC 9616 test enables `rtt(wired)` on both nodes and requires both route
convergence and non-null raw and smoothed RTT observations from the control
socket. This distinguishes a working Timestamp/IHU exchange from silent
fallback to the base wired metric.

The lifecycle test starts the daemon with no matching interface, creates a
glob match later, commits a valid origin and export-table reload, rejects
malformed TOML without losing the active state, deletes and recreates the link
under the same name, and removes an owned kernel route out of band. The daemon
must attach the new ifindex and restore the FIB route without restarting.

Network namespaces disable automatic IPv6 address generation and assign one
stable link-local address per interface, avoiding accidental ambiguity in the
test topology.

The shared-LAN regression passed on 2026-09-10 with babeld 1.13.1. In that
fixture, babeld retained its existing IPv4 gateway after the neighboring
babel-rs changed its advertisement to an IPv6 next hop; the second babel-rs
updated its gateway. The test records that observation and does not claim
that babeld supports a live next-hop-family switch. For such peers, choose a
compatible mode before establishing routes; live changes need separate peer
validation. Initial ordinary IPv4 exchange and subsequent withdrawal/recovery
are verified against both implementations.

The interrupted mixed soak and fresh-state round replay are described in
[TESTING.md](TESTING.md). The replay passing does not establish the cause of
the historical missing BIRD route or reproduce its prior sequence history.

## 2026-09-10 audit-fix verification

All 15 VM scenarios passed across the full run and targeted continuation: the
original eight through capacity isolation, restart recovery, shutdown recovery,
RFC boundaries, shared LAN, control clients, combined failures, and 120-second
steady state. The restart fixture was updated to assert the new withdrawal
policy (unchanged sequence and remote route disappearance); stale-checkpoint
recovery took about 180 seconds. The final queued-old-family guard additionally
has a protocol regression and a rerun of the affected RFC boundary network.

The pure IPv4 test disables IPv6, checks MTU 576 packet sizing, forwarding,
wrong-port/off-subnet source admission and live family switching. babeld RTT
samples match an injected 40 ms round-trip delay with a reachable adjacency.
The ICMP test observes both TTL exceeded and fragmentation needed at MTU 1280,
using the router's loopback IPv4 address or kernel fallback 192.0.0.8.
The 120-second steady run passed 132 samples with maximum control latency
1.262 ms; its duration is too short to make long-run leak claims.
The historical mixed-soak round 170 observation remains unattributed.

## 0.6.0 MAC/SADR verification

The VM MAC suite passed HMAC-SHA256 and BLAKE2s-128 exchange against babeld
1.13.1. IPv4 and IPv6 babel-rs peers also passed key rotation, wrong-key
isolation, restart recovery and multi-datagram route dumps at MTU 576 and 1280,
respectively, using the daemon installed from the verified crate archive.

The overlapping-source tests passed actual IPv4/IPv6 transit forwarding,
destination-first lookup, equal-destination source overrides, withdrawal and
reannouncement, static-source filtering/re-enable and owned-state cleanup.
The final SADR run waited for all child defaults before testing destination
precedence; an earlier run failed because that readiness condition was missing.
The existing BIRD SADR, RFC boundary and dynamic-MTU regressions also passed.
These are scoped VM checks; hosted tests for the 0.6.0 commit and a long-duration
mixed run remain outstanding. No Babel-DTLS interoperability is claimed.
