# babel-router

An embeddable Tokio runtime for the Babel routing protocol, with Linux UDP
transport and pluggable route export. Part of [babel-rs](https://github.com/bnkrr/babel-rs).

Use this crate to run Babel in an application and consume route snapshots or
implement `RouteExporter`. It does not depend on the standalone daemon's TOML
configuration, control socket, or netlink exporter. The live backend supports
Linux; Rust 1.90 or newer is required.

## Status and compatibility

Version 0.6.0 is usable within the documented support scope. Most project code
was written by **OpenAI Codex**. Automated tests and RFC review do not guarantee
correctness or replace an independent security audit. Pin deployed versions,
validate upgrades on your topology, and review the
[compatibility policy](https://github.com/bnkrr/babel-rs/blob/main/docs/guide/support.md#api-compatibility):
breaking API or behavior changes use a new 0.x minor version.

## Example

```rust,no_run
use babel_router::{BabelRouter, RouterId};

# async fn example() -> Result<(), babel_router::RouterError> {
let router = BabelRouter::builder()
    .router_id(RouterId::new([1; 8]).unwrap()) // replace with a domain-unique identity
    .interface("eth0")
    .start().await?;
let handle = router.handle();
// Keep the owner alive while the application uses handles and route snapshots.
let _status = handle.status().await?;
router.shutdown().await?;
# Ok(()) }
```

The interface must exist, be up, have an address for the selected control
transport, and be accessible with the required Linux socket privileges. IPv6
control is the default; `ControlTransport::Ipv4` supports IPv4-only links.
This example starts and stops a runtime; a real application keeps the owner
alive and supplies origins and a forwarding backend as needed.

## Lifecycle and host responsibilities

`start()` acknowledges initial engine configuration. Handle commands acknowledge
engine application, not packet delivery or completed route export. `wait()`
observes the running instance; `shutdown().await` requests and waits for orderly
cleanup. Dropping the owner cancels its tasks without asynchronous cleanup.
Cloned control handles do not own the router's lifetime.

Exporters receive complete desired-state snapshots and may skip obsolete
generations. Reconciliation must be idempotent, yield during I/O, and tolerate
cancellation. The runtime waits for in-flight reconciliation before final
cleanup, within a configurable budget of five seconds by default.

The host owns stable identity and initial sequence state. `SequenceStore` saves
an orderly-exit checkpoint, not crash-safe runtime state. Do not repeatedly
reuse the default zero sequence for a persistent identity. The packaged
`embedded` example demonstrates checkpoint consumption and restart policy;
abrupt restarts can still require minute-scale convergence.

## Routing policy and authentication

Install read-only import/export rules with `.route_policy(Arc::new(policy))` and
replace them with `handle.replace_route_policy(...).await`. Callbacks must be
deterministic and nonblocking. The packaged `route_policy` example demonstrates
live changes. Custom exporters must implement source-specific forwarding or
reject unsupported sources, and explicitly opt in to IPv4-via-IPv6 support.

`interface_with_mac` and `add_interface_with_mac` enable HMAC-SHA256 or
BLAKE2s-128 authentication before the first packet. Replace a key set by removing
and reattaching the interface. DTLS is not implemented.

## Documentation

- [Embedding API and completion contracts](https://github.com/bnkrr/babel-rs/blob/main/docs/guide/embedding.md)
- [MAC authentication](https://github.com/bnkrr/babel-rs/blob/main/docs/guide/mac.md)
- [Source-specific forwarding](https://github.com/bnkrr/babel-rs/blob/main/docs/guide/sadr.md)
- [Validation and known limits](https://github.com/bnkrr/babel-rs/blob/main/docs/development/conformance.md)
- [Changelog](https://github.com/bnkrr/babel-rs/blob/main/CHANGELOG.md)
- [Issues](https://github.com/bnkrr/babel-rs/issues)

Generate the API reference with `cargo doc -p babel-router --no-deps`.
Publication instructions are in the
[release guide](https://github.com/bnkrr/babel-rs/blob/main/docs/development/releasing.md).

## License

MIT; see the packaged LICENSE.
