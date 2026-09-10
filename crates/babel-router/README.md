# babel-router

An embeddable Tokio runtime for the independent `babel-protocol` routing engine.
The live interface/socket backend currently supports **Linux**. It does not
depend on the standalone daemon's configuration or Linux netlink exporter.

Most of this project's code was written by **OpenAI Codex**. The project is
**pre-1.0**: public APIs and behavior may change in breaking ways between 0.x
minor releases. Pin the version you deploy, review the
[changelog](https://github.com/bnkrr/babel-rs/blob/main/CHANGELOG.md), and test
upgrades in your own environment before production use. See the
[compatibility policy](https://github.com/bnkrr/babel-rs/blob/main/docs/SUPPORT.md#api-compatibility).

```rust,no_run
use babel_router::{BabelRouter, RouterId};

# async fn example() -> Result<(), babel_router::RouterError> {
let router = BabelRouter::builder()
    .router_id(RouterId::new([1; 8]).unwrap()) // use a stable, domain-unique ID
    .interface("wg0")
    .start().await?;
let handle = router.handle();
// Share cloned handles, subscribe to route snapshots, or supply RouteExporter.
router.shutdown().await?;
# Ok(()) }
```

`start` returns after initial engine configuration. Modification commands
acknowledge engine application, not network delivery or exporter completion.
`wait` observes the running task; `shutdown` requests and awaits orderly cleanup.
The default cleanup budget is five seconds and can be overridden. Drop cancels
owned tasks without promising asynchronous retractions or external-state cleanup.
Control handles do not own the router's lifetime. `build`/`run` remain aliases
for `start`/`wait`; `RouterHandle::shutdown` is a non-waiting compatibility alias
for `request_shutdown`.

Exporters receive coalesced full desired-state snapshots. The runtime waits for
in-flight reconciliation before final cleanup. Exporters must yield during I/O
and be cancellation-safe; detached work remains their responsibility.

Use `RoutePolicy` for read-only import/export allow/deny rules, independently of
`RouteExporter`. Install it with `.route_policy(Arc::new(policy))` and replace it
explicitly using `handle.replace_route_policy(...).await`. Replacement reselects
routes, retracts denied announcements and cancels older queued output. Export
rules apply per interface; callbacks must be deterministic and nonblocking. The
packaged `route_policy` example demonstrates live replacement with a memory RIB.

The host owns stable identity and initial sequence state. `SequenceStore` is an
orderly-exit checkpoint, not crash-safe runtime persistence. The initial sequence
defaults to zero; do not repeatedly reuse it for a persistent identity. Abrupt
restarts can require minute-scale recovery. See the packaged `embedded` example.

Interfaces need Linux socket privileges and an address for the selected control
transport (`ControlTransport::Ipv6` by default, or `Ipv4`). IPv4
announcements use `auto`, `ipv4`, or `ipv6` next-hop policy independently of
the selected control transport. Custom exporters default to rejecting IPv4-via-IPv6
routes; opt in with `supports_ipv4_via_ipv6()` only when the backend supports
that forwarding form and unnumbered ICMPv4. Configure `MacKey`/`MacConfig` with
`interface_with_mac` or `add_interface_with_mac` for strict RFC 8967 authentication
from the first packet. HMAC-SHA256 and BLAKE2s-128 are supported. Remove/reattach
the interface to rotate its key set without restarting the router. DTLS is deferred.

[API](https://docs.rs/babel-router) ·
[Embedding contracts](https://github.com/bnkrr/babel-rs/blob/main/docs/EMBEDDING.md) ·
[RFC audit and open gaps](https://github.com/bnkrr/babel-rs/blob/main/docs/CONFORMANCE.md) ·
[Support](https://github.com/bnkrr/babel-rs/blob/main/docs/SUPPORT.md)

MIT licensed; see LICENSE.
