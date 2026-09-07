# Interoperability

The executable suite is `tests/e2e/run-on-linux-vm.sh`. It builds locally, copies
only the binary and scripts to the Debian VM, and creates disposable network
namespaces. The reference versions currently installed there are babeld
1.13.1 and BIRD 3.1.7.

| Peer/topology | IPv6 | IPv4 via IPv6 | SADR | Lifecycle |
|---|---:|---:|---:|---|
| babel-rs ↔ babeld 1.13.1 | pass | pass | pass | retract, reannounce, restart |
| babel-rs ↔ BIRD 3.1.7 | pass | pass | pass | bidirectional exchange |
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
