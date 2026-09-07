# Configuration

The top-level `shutdown_timeout_ms` sets the daemon's total cleanup budget in
milliseconds (default `5000`, nonzero 32-bit unsigned integer). It is reloadable
and appears in `status`. Place it before any TOML table headers:

```toml
shutdown_timeout_ms = 5000
```

The timer begins when the daemon handles a stop signal, accepts a control
shutdown request, or starts cleanup after a critical task failure. Response
flush, route retractions, the sequence checkpoint, Linux route/rule cleanup and
service completion share that single deadline. Expiry logs the unfinished
stage, cancels pending router/service tasks and exits nonzero. A supervisor's
forced-stop timeout should leave room beyond this budget.

Optional `[limits]` controls global neighbors, global candidates, and candidates
per neighbor. It uses built-in defaults when omitted and requires a restart to
change. See [CAPACITY.md](CAPACITY.md) for all defaults and overload behavior.

Interface rules are evaluated in file order and the first matching rule owns
the interface. Exact names, `*`, and `?` patterns are supported. An unmatched
interface is not enabled; a rule with no current matches remains valid so that
the interface supervisor can attach future devices.

Only structured `[[interfaces]]` rules are accepted. Metric overrides belong
to the matching rule under `[interfaces.metric]`.

```toml
[[interfaces]]
match = ["test-special-*"]
link_type = "tunnel"
hello_interval_ms = 1000

[[interfaces]]
match = ["test-*", "backbone0"]
link_type = "tunnel"
```

There is no cross-rule merge or inheritance. Resolution is:

```text
explicit interface value > link_type preset > common built-in default
```

`link_type` affects only the default metric and split-horizon behaviour:

| `link_type` | Metric preset | Split horizon |
|---|---|---|
| `wired` (default) | RFC 8966 wired: cost 96, 2 of 3 Hellos | enabled |
| `wireless` | RFC 8966 ETX, window 6 | disabled |
| `tunnel` | RFC 9616 RTT over the wired preset | enabled |

The RTT preset probes every 2000 ms, uses a 6000 ms half-life, maps 10–120 ms
to a maximum penalty of 150, and uses the wired preset as its base. An explicit
`[interfaces.metric]` table replaces the complete metric preset; it is never
deep-merged. `split_horizon` may also be set explicitly.

The common Hello interval is 4000 ms. The Update interval defaults to four
times the effective Hello interval, so it is 16000 ms unless Hello is
overridden. `hello_interval_ms` and `update_interval_ms` accept nonzero
multiples of 10 up to 655350 ms. IHU uses three times the effective Hello
interval and is not separately configurable.

The `interfaces` control command reports the resolved metric, Hello and Update
intervals, and split-horizon value for every attached interface, in addition to
its live MTU and payload budget.

## Router identity and restart state

`state_file` stores the stable Router-ID and, after orderly shutdown, an
optional sequence-number checkpoint. An explicit `router_id` overrides the
stored identity; a checkpoint for a different identity is discarded. Both
settings require a restart to change. Give each daemon its own state file.

Startup must be able to write the file and its parent directory: it consumes
any checkpoint before advertising. While running, sequence changes cause no
disk writes. SIGINT, SIGTERM and the control `shutdown` command attempt one
final checkpoint, waiting at most one second. Save failures are logged and do
not prevent cleanup. The checkpoint's one-second limit also fits within the
remaining global shutdown budget; a shorter `shutdown_timeout_ms` can interrupt
it earlier. There is no separate checkpoint timeout or persistence interval.

The daemon migrates old state files automatically. After a crash or a missing
checkpoint it uses a random sequence number, so recovery may take several
minutes while peers expire older feasibility history. If the entire file is
lost, a configured `router_id` still preserves identity; otherwise a new ID is
generated. See [ARCHITECTURE.md](ARCHITECTURE.md#persistence-and-failure).

## Cleanup after an interrupted exit

Startup reconciles from an empty RIB and continues retrying export failures
periodically. It enumerates IPv4/IPv6 routes and policy rules carrying the
configured `export.protocol` across all tables in the current network namespace,
including tables no longer present in `export.views`. Obsolete entries are
removed; currently desired source rules are retained or recreated. Changing
`manage_rules` to false also removes leftover rules owned by this protocol.

The ownership token is the namespace plus `export.protocol`. Other protocols
and namespaces are untouched. Changing either token on restart does not clean
the previous ownership scope automatically. Every manager must use its own
protocol number within a namespace, including an external policy-rule manager.
