# Control protocol

The daemon exposes a local, versioned control protocol over a Unix stream
socket. The socket and a newly-created parent directory are mode `0600` and
`0700`, respectively. Each frame is one UTF-8 JSON object followed by LF and
is limited to one MiB before allocation can grow past that boundary. The
server accepts at most 64 concurrent clients and applies a 30-second idle I/O
timeout.

Immediately after accept, the server sends:

```json
{"type":"hello","api_version":1,"server_version":"0.1.0","capabilities":["status","interfaces","neighbors","routes","reload","shutdown"]}
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

`status.metric` identifies the active metric profile and
`dropped_outbound_datagrams` exposes bounded-output overload. Each `neighbors`
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
