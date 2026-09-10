# Embedding API

`babel-protocol` owns synchronous protocol state and the packet codec.
`babel-router` adds the Tokio socket runtime. Neither crate depends on the
standalone daemon, its TOML format, state files, control socket or Linux
netlink exporter.

Generate the public API reference with `cargo doc --workspace --no-deps`.
Both library landing pages contain runnable examples; CI checks rustdoc links
and runs the examples with `cargo test --workspace --doc`.

## Validate at the input boundary

Public configuration structs retain editable fields. Their `validate()`
methods are side-effect-free and return `babel_protocol::ConfigError`.
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
previous behavior. Existing public paths, including `babel_protocol::engine::*`
and `babel_protocol::wire::*`, remain available.

For the runtime, builder setters store pending values.
`BabelRouterBuilder::validate()` checks the entire configuration without I/O;
`start()` calls it before opening any socket or spawning any task. It rejects
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

`BabelRouterBuilder::start()` validates configuration, opens Linux sockets and
starts routing; it returns after initial engine configuration is applied.
`BabelRouter::wait()` joins the running instance. `BabelRouter::shutdown().await`
requests and waits for orderly cleanup. `build`/`run` remain compatibility aliases
for `start`/`wait`.

Dropping the owner or its wait future cancels the engine and its owned workers;
it does not perform asynchronous retractions/checkpoints/export cleanup. Cloned
control handles do not own the runtime and cannot keep it running after its owner
is dropped. The live socket backend supports Linux; see [SUPPORT.md](SUPPORT.md).

| Handle operation | Successful completion means |
| --- | --- |
| `originate`, `withdraw` | The validated change was applied by the serialized engine |
| `replace_origins` | The complete replacement was applied by the serialized engine |
| Add/update/remove interface | The socket/engine operation completed |
| `status` | Earlier commands in that queue have been processed and status was sampled |
| `subscribe_routes` | A watch receiver for complete selected learned-route snapshots was created |
| Handle `request_shutdown` (legacy `shutdown`) | A shutdown request was signaled; await owner `wait()` to join |
| Owner `shutdown().await` | Orderly cleanup completed, or an error identifies failure/timeout |
| `set_shutdown_timeout` | The nonzero cleanup budget was applied |

Adding an already active interface is a no-op; use
`update_interface_policy` to change it. Removing/updating an absent interface
returns `InterfaceNotFound`. Duplicate initial interfaces are rejected by the
builder so they cannot silently choose one of multiple policies.

Command completion and route snapshots do not acknowledge packet delivery or
kernel export. Snapshot readers may skip intermediate generations. A snapshot
contains selected **learned** routes and exact unreachable hold state; local
origins are advertised separately.

## Export and shutdown responsibilities

`RouteExporter::supports_ipv4_via_ipv6()` defaults to false. Opt in only when
its forwarding backend supports RFC 9229 routes and ICMPv4 generation on
unnumbered links; unsupported routes are excluded before selection. A direct
protocol host makes the same decision with `EngineConfig::ipv4_via_ipv6`.

`InterfacePolicy::control_transport` defaults to IPv6; select IPv4 for links
without IPv6. Runtime policy updates that change this field require interface
removal and reattachment. `PacketReceivedWithTimestamp` lets a protocol host
preserve microsecond arrival time while supplying current processing time for
timers. All event processing clocks must remain nondecreasing.

`RouteExporter` receives complete desired-state snapshots. Implementations
must be idempotent and tolerate skipped generations. Reconciliation errors are
logged and retried. During orderly shutdown the runtime stops submitting new
snapshots, waits for in-flight reconciliation, and then calls the final shutdown
hook. These runtime callbacks do not overlap. Any exporter-owned background work
still needs synchronization and cancellation handling; the daemon's independent
periodic reconciler uses an apply lock and terminal stopping flag.

The total orderly-cleanup deadline defaults to five seconds and is configured
with `BabelRouterBuilder::shutdown_timeout(Duration)` or the handle setter. It
covers retractions, checkpoint, waiting for the exporter, final cleanup and worker
joins. Expiry cancels owned tasks and returns `RouterError::ShutdownTimeout`;
external state may require repair on restart. Custom callbacks must yield during
I/O; an async deadline cannot preempt arbitrary synchronous blocking code.

`SequenceStore` remains an orderly-exit checkpoint, with no runtime writes and a
one-second cap inside that overall budget. A failed/timed-out checkpoint does not
skip route cleanup, but is returned afterward as `RouterError::SequenceStore`.
Final exporter errors return `RouterError::Cleanup` (taking precedence over a
checkpoint failure); runtime task failures return `RouterError::Task`. Successful
cleanup does not guarantee that a remote peer received UDP retractions.

The host owns stable identity and initial sequence policy. Defaults remain zero
and `NoopSequenceStore`; persistent identities must use an explicit restart policy.
The `embedded` example accepts INTERFACE, an exclusively owned STATE_FILE, and a
stable unique 16-digit ROUTER_ID_HEX. It durably consumes an orderly checkpoint
before advertising, saves once on orderly exit, and uses random fallback after
an unclean restart. This can still need minute-scale convergence. It is an example
of host integration, not a shared state-file service or crash-safe sequence store.

```sh
cargo run -p babel-router --example embedded -- wg0 /var/lib/my-router/state 0102030405060708
```

The host retains responsibility for privileges, domain-wide identity uniqueness,
external-state recovery and any detached I/O its callbacks start.
