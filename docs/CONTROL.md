# Control protocol

The daemon exposes a local, versioned control protocol over a Unix stream
socket. The socket and a newly-created parent directory are mode `0600` and
`0700`, respectively. Each frame is one UTF-8 JSON object followed by LF and
is limited to one MiB before allocation can grow past that boundary. The
server accepts at most 64 concurrent clients. Each frame read and greeting or
response write has its own 30-second deadline. Partial progress does not extend
that deadline; a completed operation gets a new budget for the next operation.
An idle, incomplete-request or blocked-response connection is closed on timeout
and releases its client slot. This bounds client I/O, not command execution.

Immediately after accept, the server sends:

```json
{"type":"hello","api_version":1,"server_version":"0.4.0","capabilities":["status","interfaces","neighbors","routes","reload","shutdown"]}
```

A client then sends requests of this form:

```json
{"api_version":1,"id":1,"command":"status","params":{}}
```

The response repeats `api_version` and `id`, sets `ok`, and contains exactly
one of `result` or `error`. Errors have stable machine-readable `code` and a
diagnostic `message`. An unsupported API version or command is rejected without
closing an otherwise well-framed session. Malformed or oversized framing ends
only that client session.

The read-only commands are `status`, `interfaces`, `neighbors`, and `routes`.
`routes` reports selected routes and accepts exact `destination`, `source`, and
`interface` filters. `reload` parses a complete config candidate, keeps the
prior active configuration on rejection, and returns its committed generation
and SHA-256 digest. `shutdown` acknowledges and flushes its response before
initiating the same graceful path used by SIGINT and SIGTERM.

`status.route_generation` identifies the currently selected RIB snapshot. The
`status.export` object reports Linux export progress:

| Field | Meaning |
| --- | --- |
| `config_generation` | Exporter-local configuration revision, initially 0; advances only when the effective `Export` configuration changes |
| `last_success_route_generation` | RIB generation captured by the last fully successful reconciliation; null before the first success |
| `last_success_config_generation` | Export configuration revision captured by that same reconciliation; null before the first success |
| `last_success_age_seconds` | Time since that successful reconciliation; null before the first success |
| `last_error` | Most recent reconciliation failure; cleared by a subsequent successful reconciliation |

The two successful generations describe the same completed attempt. A config
reload during netlink I/O cannot make an old attempt acknowledge the new
configuration. Failures, including partial application, preserve both previous
successful generations and the success timestamp. Periodic successful checks
refresh the timestamp even if neither generation changes. Reloading identical
export settings does not advance the export revision; changing unrelated daemon
settings does not advance it either. This revision is separate from the
top-level daemon `config_generation`.

Compare the successful route generation with `status.route_generation` and the
successful export config generation with `export.config_generation`, alongside
`last_error` and success age. A lag indicates work has not yet been confirmed,
not necessarily failure. These are diagnostic observations, not an atomic
completion barrier: router and exporter status are sampled separately. Matching
generations mean those inputs were successfully applied previously; they do not
prove the kernel has remained unchanged or end-to-end forwarding is working.
An external deletion is still repaired by periodic reconciliation. Counters
restart with the daemon and are not persistent identifiers. `ready` indicates
control availability, not successful export of the current RIB.

`status.metric` identifies the common active metric profile, or is
`per-interface` when attached interfaces differ.
`status.shutdown_timeout_ms` reports the currently committed daemon-wide
shutdown budget (default 5000 ms); a successful reload can change it.
`status.sequence_number` is the current in-memory sequence number for local
origins (not an interface's Hello sequence). It is checkpointed only on orderly
shutdown; the running state file contains no sequence number.
`status` also reports effective admission `limits`, current `candidates`,
`sources`, `pending_requests`, `unreachable_routes`, and cumulative rejection
counters. Each neighbor reports its candidate occupancy and rejected Update
count. See [CAPACITY.md](CAPACITY.md) for exact counting and recovery semantics.
`dropped_outbound_datagrams` counts encoding failures and datagrams discarded
on send error, timeout or expiry, while
`missed_outbound_deadlines` detects runtime stalls that violate a protocol
deadline. Each `interfaces` result includes its resolved metric, Hello and
Update intervals, split-horizon setting, live MTU, and derived UDP payload
budget. Its `output` object reports:

- `budget_bytes`, `used_bytes`: per-interface accounted output budget and
  occupancy, including channel, scheduler and in-flight work; not RSS.
- `rejected_batches`, `rejected_tlvs`: new work refused by channel or byte
  admission (including an oversized batch or a stopped sender).
- `expired_batches`: admitted semantic batches discarded before encoding;
  several original batches may have been merged by the scheduler.
- `dropped_datagrams`: encoding failures or datagrams dropped on error,
  expiry or timeout. An encoding failure counts once for the failing batch
  because its eventual datagram count is unknown.
- `expired_datagrams`, `send_timeouts`: subsets of dropped datagrams; a send
  can count in both if it times out at the work's expiry.
- `missed_deadlines`: send attempts started after their scheduling deadline.

Output counters in `interfaces` reset on detach/reattach; the top-level drop
and deadline counters remain cumulative for the running router. Refused and
expired semantic batches are separate from the datagram counter.

Each `neighbors`
entry reports its concrete algorithm, separate `receive_cost`, `transmit_cost`,
and `link_cost`, both 16-bit Hello histories, and (when RFC 9616 is active) the
last and smoothed RTT in microseconds plus the current RTT penalty. `reachable`
is derived from the final link cost rather than from receipt of a Hello alone.

There are intentionally no imperative add/delete route, origin, neighbour, or
interface commands. Those resources remain owned by the configuration and the
protocol engine, so restart and reconciliation have one source of truth.

The bundled client is the daemon binary itself:

```sh
babel-rs status --socket /run/babel-rs/babel-rs.ctl
babel-rs interfaces --socket /run/babel-rs/babel-rs.ctl
babel-rs neighbors --socket /run/babel-rs/babel-rs.ctl
babel-rs routes --socket /run/babel-rs/babel-rs.ctl --interface wg0
babel-rs reload --socket /run/babel-rs/babel-rs.ctl
babel-rs shutdown --socket /run/babel-rs/babel-rs.ctl
```

The protocol is local administration, not a Babel wire extension. File-system
permissions are its authorization boundary.

`cargo test -p babel-rs control::tests` covers idle reads, trickled requests,
blocked response writes and fresh budgets after complete frames with a
controlled clock and bounded duplex transport. The VM runner's `control-clients`
mode exercises the real daemon with one active client plus 21 idle, 21 slow
request writers and 21 blocked response readers. It fills all 64 slots, checks
that extra clients are refused, and reuses all 63 timed-out slots before closing
the old client sockets. It also checks disconnect reuse and shutdown while
clients remain connected. The `all` suite includes this mode.
