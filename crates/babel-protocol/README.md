# babel-protocol

A synchronous, sans-I/O Babel routing engine and packet codec for Rust.
Part of [babel-rs](https://github.com/bnkrr/babel-rs).

Use this crate when your application owns transport, scheduling, and forwarding.
The engine performs no socket I/O, reads no clock, and starts no background tasks.
It uses Rust's standard library and requires Rust 1.90 or newer.

## Status and compatibility

Version 0.6.0 is usable within the documented support scope. Most project code
was written by **OpenAI Codex**. Automated tests and RFC review do not guarantee
correctness or replace an independent security audit. Pin deployed versions,
validate upgrades on your topology, and review the
[compatibility policy](https://github.com/bnkrr/babel-rs/blob/main/docs/SUPPORT.md#api-compatibility):
breaking API or behavior changes use a new 0.x minor version.

## Example

```rust
use babel_protocol::{Engine, EngineConfig, Event, RouteKey, RouterId};

// Choose a stable, unique Router-ID for a real routing domain.
let id = RouterId::new([1; 8]).unwrap();
let mut engine = Engine::try_new(EngineConfig::recommended(id)).unwrap();
let key = RouteKey::new("2001:db8::/64".parse().unwrap(), None).unwrap();
let actions = engine.try_handle(Event::Originate { key, metric: 0, now_ms: 0 }).unwrap();
```

This creates engine state and originates a route; it does not send packets or
install routes. A host attaches interfaces, supplies decoded packets and
monotonic time, drives `Event::Tick` while idle, and executes returned actions
in order. Use the fallible methods for user-provided configuration and events.
The packaged `packet` example demonstrates wire encoding and decoding.

## Integration contract

- `encode_packets` takes the actual UDP payload budget. The host schedules
  output and stamps Hello timestamps immediately before transmission.
- `InterfacePolicy::ipv4_next_hop` defaults to `Auto`: ordinary IPv4 when the
  host supplies a usable IPv4 address, otherwise RFC 9229. Address changes are
  explicit events. Declare forwarding capability in `EngineConfig`.
- `RoutePolicy` accepts/rejects learned routes and controls announcements per
  interface. Callbacks are synchronous and must not block. Replace rules with
  `Event::ReplaceRoutePolicy`; execute `Action::InvalidatePendingSends` before
  queueing replacement output. Feasibility history is preserved.
- Source-specific route storage and selection do not implement the host's
  forwarding plane. Apply destination-before-source matching in the exporter.
- `mac::MacSession` implements optional RFC 8967 authentication and RFC 9467
  replay counters. Supply fresh cryptographic entropy, real UDP endpoints, and
  monotonic time; verify before decoding and sign after timestamp stamping.

The engine supports base Babel, source-specific routes, IPv4 routes with IPv6
next hops, and wired/ETX/RTT metrics. DTLS is not implemented. Exact coverage and
validation evidence are in the [RFC audit](https://github.com/bnkrr/babel-rs/blob/main/docs/CONFORMANCE.md).

## Documentation

- [Embedding and host responsibilities](https://github.com/bnkrr/babel-rs/blob/main/docs/EMBEDDING.md)
- [MAC authentication](https://github.com/bnkrr/babel-rs/blob/main/docs/MAC.md)
- [Platform support and known limits](https://github.com/bnkrr/babel-rs/blob/main/docs/SUPPORT.md)
- [Changelog](https://github.com/bnkrr/babel-rs/blob/main/CHANGELOG.md)
- [Issues](https://github.com/bnkrr/babel-rs/issues)

Generate the API reference with `cargo doc -p babel-protocol --no-deps`.
Registry availability is recorded in the
[release guide](https://github.com/bnkrr/babel-rs/blob/main/docs/RELEASING.md).

## License

MIT; see the packaged LICENSE.
