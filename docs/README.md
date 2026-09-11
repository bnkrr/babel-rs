# babel-rs documentation

Choose the task you need to perform. The [project README](../README.md) provides
the introduction and a first-run example. Guides describe the current software;
records preserve evidence from a particular version and date.

## Use and integrate

| Task | Guide | What it covers |
| --- | --- | --- |
| Install or configure the daemon | [Configuration](guide/configuration.md) | Host networking, route-table ownership, systemd, interfaces, metrics, reload and restart |
| Inspect or administer an instance | [Control](guide/control.md) | CLI commands, local protocol, health fields, counters and completion limits |
| Embed a library | [Embedding](guide/embedding.md) | Validation, owner/handle lifetime, route policy, exporters and persistence |
| Authenticate Babel links | [MAC](guide/mac.md) | Key files, strict/migration modes, rotation and host responsibilities |
| Route by destination and source | [SADR](guide/sadr.md) | Overlapping prefixes, Linux source views and external policy integration |
| Size and monitor routing workloads | [Capacity](guide/capacity.md) | Admission, queue budgets, overload behavior and sizing limits |
| Check compatibility and deployment scope | [Support](guide/support.md) | Platforms, versions, tested peers, known observations and operational limits |

For daemon deployment, read Configuration and Support first; consult Control
when diagnosing a running instance. Embedders should start with Embedding and
Support. MAC, SADR, and Capacity are focused references when those features matter.

## Develop and maintain

| Task | Guide |
| --- | --- |
| Understand ownership and data flow | [Architecture](development/architecture.md) |
| Check implemented RFC behavior and alternatives | [Protocol coverage](development/conformance.md) |
| Reproduce local, network, steady-state or endless checks | [Testing](development/testing.md) |
| Freeze source, verify archives or publish crates | [Releasing](development/releasing.md) |

See [CONTRIBUTING.md](../CONTRIBUTING.md) for contribution expectations and basic
checks, and [CHANGELOG.md](../CHANGELOG.md) for version changes and migration.

## Historical evidence

- [0.6.0 validation](history/0.6.0-validation.md): source commits, scoped Linux
  results, the mixed campaign, earlier replay, and the dated publication snapshot.
- [September 2026 RFC audit](history/rfc-audit-2026-09.md): repaired defects,
  original reproduction, historical decision IDs, and regression inventory.

Historical records are not live status pages or active feature checklists.
Keep instructions and contracts in the relevant guide; add dated results to a
record instead of copying them into several guides. Current peer observations
belong in Support, RFC scope in Protocol coverage, and release steps in Releasing.
