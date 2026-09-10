# babel-protocol

An independent, sans-I/O Babel routing protocol engine and packet codec.
No sockets, background tasks, operating-system APIs, or clock reads. The host
supplies monotonic time and executes ordered actions.

Most of this project's code was written by **OpenAI Codex**. The project is
**pre-1.0**: public APIs and behavior may change in breaking ways between 0.x
minor releases. Pin the version you deploy, review the
[changelog](https://github.com/bnkrr/babel-rs/blob/main/CHANGELOG.md), and test
upgrades in your own environment before production use. See the
[compatibility policy](https://github.com/bnkrr/babel-rs/blob/main/docs/SUPPORT.md#api-compatibility).

```rust
use babel_protocol::{Engine, EngineConfig, Event, RouteKey, RouterId};

let id = RouterId::new([1; 8]).unwrap(); // use your own stable, unique identity
let mut engine = Engine::try_new(EngineConfig::recommended(id)).unwrap();
let key = RouteKey::new("2001:db8::/64".parse().unwrap(), None).unwrap();
let actions = engine.try_handle(Event::Originate { key, metric: 0, now_ms: 0 }).unwrap();
```

Attach interfaces, feed decoded packets, and drive `Event::Tick` even when idle.
Actions describe packet timing, complete selected-route snapshots, and local
sequence changes. Use `encode_packets` with the actual UDP payload budget;
stamp Hello timestamps at transmission. `InterfacePolicy::ipv4_next_hop`
defaults to `Auto`: ordinary IPv4 when the host supplies a usable IPv4 address,
otherwise RFC 9229. Address changes are explicit engine events.

`EngineConfig::route_policy` defaults to `AllowAllRoutes`. Implement `RoutePolicy`
to accept/reject learned routes and allow/retract announcements per interface,
without changing protocol fields. Replace immutable rules with
`Event::ReplaceRoutePolicy`; hosts must execute its first
`Action::InvalidatePendingSends` before queueing subsequent output. Feasibility
history is retained, and retractions bypass policy. Callbacks are synchronous
and must not block or silently change behavior between explicit replacements.

Implements RFC 8966, source-specific routes (RFC 9079), IPv4 routes with IPv6
next hops (RFC 9229), and RTT metrics (RFC 9616). `mac::MacSession` implements
RFC 8967 authentication and RFC 9467 split replay counters. Hosts supply entropy,
actual UDP endpoints and monotonic time; verify before decoding and sign after
stamping timestamps. DTLS is not implemented.
The core is OS-independent but uses Rust's standard library; it is not `no_std`.

[API](https://docs.rs/babel-protocol) ·
[Conformance and limitations](https://github.com/bnkrr/babel-rs/blob/main/docs/CONFORMANCE.md) ·
[Support and compatibility](https://github.com/bnkrr/babel-rs/blob/main/docs/SUPPORT.md)

MIT licensed; see LICENSE.
