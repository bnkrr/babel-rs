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

The babeld test also injects a stale route in the owned table/protocol and
requires startup reconciliation to remove it. Restart must preserve Router-ID,
advance sequence state, remove routes on SIGTERM and reconverge. The BIRD test
uses separate IPv4 and IPv6 SADR channels. The three-node test requires a
triggered withdrawal to cross the remaining adjacency before the advertised
route hold time expires.

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
