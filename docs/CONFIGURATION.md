# Configuration

Interface rules are evaluated in file order and the first matching rule owns
the interface. Exact names, `*`, and `?` patterns are supported. An unmatched
interface is not enabled; a rule with no current matches remains valid so that
the interface supervisor can attach future devices.

```toml
[[interfaces]]
match = ["vl-special-*"]
link_type = "tunnel"
hello_interval_ms = 1000

[[interfaces]]
match = ["vl-*", "backbone0"]
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

The `interfaces = ["vl-*"]` form and top-level `[metric]` remain available for
v0.2 compatibility. They describe one wired-style rule with split horizon
enabled and the common timing defaults. The top-level metric table cannot be
mixed with structured `[[interfaces]]` rules.

The `interfaces` control command reports the resolved metric, Hello and Update
intervals, and split-horizon value for every attached interface, in addition to
its live MTU and payload budget.
