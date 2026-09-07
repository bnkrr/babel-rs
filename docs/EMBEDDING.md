# Embedding API

`babel-proto` owns synchronous protocol state and the packet codec.
`babel-router` adds the Tokio socket runtime. Neither crate depends on the
standalone daemon, its TOML format, state files, control socket or Linux
netlink exporter.

Generate the public API reference with `cargo doc --workspace --no-deps`.
Both library landing pages contain runnable examples; CI checks rustdoc links
and runs the examples with `cargo test --workspace --doc`.

## Validate at the input boundary

Public configuration structs retain editable fields. Their `validate()`
methods are side-effect-free and return `babel_proto::ConfigError`.
`babel-router` reexports that error and reports shared configuration failures
through `RouterError::InvalidConfig`. Its existing `InvalidInterfacePolicy`
and `InvalidOriginMetric` variants remain available for those specific errors.

| Input | Constraint |
| --- | --- |
| `RouteSelectionConfig` | Percentage in `0..=100`; absolute margin below 65535; zero dwell is valid |
| `EngineConfig`, `InterfacePolicy` | Periodic Hello/Update intervals in `1..=65535` centiseconds |
| `ResourceLimits` | Every `usize` is valid, including zero to refuse new admissions; per-neighbor limits need not be below the global limit |
| `RouterId` | Construct with `RouterId::new`; all-zero and all-one octets are invalid |
| `RouteKey` | Construct with `RouteKey::new` to normalize host bits and zero-length sources; mixed address families are invalid |
| Local origin metric | `0..65535` (zero is valid; infinity requires a withdrawal operation) |

The daemon additionally validates its millisecond representation, interface
patterns and export policy. It uses the same route-selection validation as
both library layers. Metric constructors already return `Option` for invalid
wired, ETX or RTT parameters; their rustdoc describes the accepted ranges.
Custom metric callbacks run synchronously and must obey the trait contracts,
including yielding control back to the engine promptly rather than doing I/O.

For direct protocol use, prefer `Engine::try_new(config)` and
`engine.try_handle(event)`. They validate configuration or a complete local
change before allocating engine state or mutating live state, respectively.
A failed origin replacement leaves the previous set intact. Local events
reject noncanonical `RouteKey` struct literals; construct the canonical key
before sending an event.

`Engine::new` and `Engine::handle` retain their existing signatures as
convenience methods for known-valid inputs. They now call the same validation
and panic on invalid local configuration. Applications accepting user input
should use the fallible methods. Valid configuration and events keep their
previous behavior. Existing public paths, including `babel_proto::engine::*`
and `babel_proto::wire::*`, remain available.

For the runtime, builder setters store pending values.
`BabelRouterBuilder::validate()` checks the entire configuration without I/O;
`build()` calls it before opening any socket or spawning any task. It rejects
missing Router-ID, duplicate interface names, duplicate origins, invalid
origin keys/metrics, intervals and route-selection parameters. Zero interfaces
is allowed so an application can attach them later. Dynamic handle methods
validate policy/origin changes before submitting commands.

## Event and command completion

A direct engine host supplies one monotonic millisecond clock, drives `Tick`
even while idle, and executes returned actions in order. The host decodes
received datagrams with `decode_packet` and applies its own transport admission
rules; local event validation does not revalidate arbitrary manually assembled
inbound TLVs. `encode_packets` accepts a UDP payload budget including the Babel
header and excluding UDP/IP headers. The host handles send timing and stamps
Hello timestamps immediately before sending.

The runtime starts during `build()`; `BabelRouter::run()` joins its task.
Dropping the router or the join future detaches that task and does not request
shutdown.

| Handle operation | Successful completion means |
| --- | --- |
| `originate`, `withdraw` | The validated command was queued |
| `replace_origins` | The complete replacement was applied by the serialized engine |
| Add/update/remove interface | The socket/engine operation completed |
| `status` | Earlier commands in that queue have been processed and status was sampled |
| `subscribe_routes` | A watch receiver for complete selected learned-route snapshots was created |
| `shutdown` | A shutdown request was signaled; await `run()` to join |

Adding an already active interface is a no-op; use
`update_interface_policy` to change it. Removing/updating an absent interface
returns `InterfaceNotFound`. Duplicate initial interfaces are rejected by the
builder so they cannot silently choose one of multiple policies.

Command completion and route snapshots do not acknowledge packet delivery or
kernel export. Snapshot readers may skip intermediate generations. A snapshot
contains selected **learned** routes and exact unreachable hold state; local
origins are advertised separately.

## Export and shutdown responsibilities

`RouteExporter` receives complete desired-state snapshots. Implementations
must be idempotent, tolerate skipped generations and serialize external
writes. Reconciliation errors are logged and retried. An in-flight
reconciliation can overlap the final shutdown hook, so an exporter must make
shutdown terminal and prevent an old snapshot from reinstalling routes after
cleanup. The daemon's Linux exporter implements this with an apply lock and a
stopping flag.

`SequenceStore` is a best-effort orderly-exit checkpoint. The runtime keeps
sequence changes in memory, attempts one checkpoint on exit and drops a pending
checkpoint future after one second. Dropping a future does not stop a detached
task or an already running blocking operation.

The embedding host owns stable Router-ID storage and restart sequence policy.
The runtime defaults to sequence zero and `NoopSequenceStore`; it does not
implement the standalone daemon's checkpoint consumption or random restart
fallback. It also does not impose the daemon's global five-second shutdown
budget. Hosts requiring bounded cleanup should signal `shutdown()`, await
`run()` within their own deadline and use `abort_handle()` when necessary,
while arranging cleanup/recovery for external state and detached I/O.
