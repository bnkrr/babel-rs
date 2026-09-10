# babel-router

An embeddable Tokio runtime for the independent `babel-protocol` routing engine.
The live interface/socket backend currently supports **Linux**. It does not
depend on the standalone daemon's configuration or Linux netlink exporter.

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

The host owns stable identity and initial sequence state. `SequenceStore` is an
orderly-exit checkpoint, not crash-safe runtime persistence. The initial sequence
defaults to zero; do not repeatedly reuse it for a persistent identity. Abrupt
restarts can require minute-scale recovery. See the packaged `embedded` example.

Interfaces need IPv6 link-local addresses and Linux socket privileges. IPv4
announcements use `auto`, `ipv4`, or `ipv6` next-hop policy independently of
the IPv6 control transport. Authentication is not implemented; use protected links.

[API](https://docs.rs/babel-router) ·
[Embedding contracts](https://github.com/bnkrr/babel-rs/blob/main/docs/EMBEDDING.md) ·
[Support](https://github.com/bnkrr/babel-rs/blob/main/docs/SUPPORT.md)

MIT licensed; see LICENSE.
