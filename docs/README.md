# babel-rs documentation

Choose the task you need to perform. The [project README](../README.md) provides
the introduction and a first-run example.

## Use and integrate

| Task | Guide | What it covers |
| --- | --- | --- |
| Install a precompiled Linux daemon | [Binary bundles](../packaging/README.md) | Architecture selection, checksums, installation and upgrades |
| Install or configure the daemon | [Configuration](guide/configuration.md) | Host networking, route-table ownership, systemd, interfaces, metrics, reload and restart |
| Inspect or administer an instance | [Control](guide/control.md) | CLI commands, local protocol, health fields, counters and completion limits |
| Embed a library | [Embedding](guide/embedding.md) | Validation, owner/handle lifetime, route policy, exporters and persistence |
| Authenticate Babel links | [MAC](guide/mac.md) | Key files, strict/migration modes, rotation and host responsibilities |
| Route by destination and source | [SADR](guide/sadr.md) | Overlapping prefixes, Linux source views and external policy integration |
| Size and monitor routing workloads | [Capacity](guide/capacity.md) | Admission, queue budgets, overload behavior and sizing limits |
| Check compatibility and deployment scope | [Support](guide/support.md) | Platforms, versions, tested peers, known observations and operational limits |

## Develop and maintain

| Task | Guide |
| --- | --- |
| Understand ownership and data flow | [Architecture](development/architecture.md) |
| Check implemented RFC behavior and alternatives | [Protocol coverage](development/conformance.md) |
| Reproduce local, network, steady-state or endless checks | [Testing](development/testing.md) |
| Freeze source, release binaries or publish crates | [Releasing](development/releasing.md) |

See [CONTRIBUTING.md](../CONTRIBUTING.md) for contribution expectations and basic
checks, and [CHANGELOG.md](../CHANGELOG.md) for version changes and migration.
