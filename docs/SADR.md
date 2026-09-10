# Source-specific forwarding

RFC 9079 forwarding compares destination prefix length first, then source
prefix length when destinations are equal. Both IPv4 and IPv6 use this order.
Overlapping source prefixes are supported.

The Linux exporter creates a destination-only table for each source view. A
route `(S, D)` is copied into every view whose source prefix is contained in S.
Ordinary routes are inherited by all views of that address family. For exactly
equal destinations in one table, the most-specific original source wins,
including unreachable withdrawal holds. Kernel destination longest-prefix
matching then implements RFC 9079 within the chosen complete source view.

```toml
[export]
protocol = 203
manage_rules = true
automatic_sources = true
source_table_base = 1000000
source_rule_priority = 10000

[[export.views]]
table = 20000

# Optional: pin one source's table. Other learned source prefixes allocate tables.
[[export.views]]
table = 20001
source = "192.168.0.0/16"
```

The settings shown are defaults except the explicit ordinary table. Automatic
views cover every selected source prefix and retained withdrawal. New views
are fully populated before activating their rules. Parent route changes and
withdrawals update all descendant tables. An automatic view is retired only
when neither a selected route nor a withdrawal hold needs that source.
Table IDs stay stable while needed within a process; they are not persistent
identifiers and can change after restart. Stale owned rules are removed before
reusing tables, and orderly shutdown removes owned routes and rules.

Reserve free routing table IDs from `source_table_base` upward for this daemon;
allocation skips explicitly configured tables. The exporter owns routes/rules
by protocol and does not delete another protocol's entries. Do not put foreign
routes in automatic tables: those entries participate in the kernel lookup.

Source rules use priority `source_rule_priority + 128 - source_prefix_length`.
More-specific source views therefore run earlier. Disjoint sources of equal
length may share a priority because they cannot both match one packet. Put this
priority range before ordinary/default-table lookups; the daemon leaves the
host's local and other policy rules alone. The base must be in 1..=32638.
In automatic mode, a per-view `rule_priority` must equal this formula. Adjust
the shared base to integrate with an existing policy-rule arrangement.

Set `automatic_sources = false` to allocate only explicit views.
`manage_rules = false` also disables automatic allocation; an external manager
then owns rules and must preserve the same ordering. In either static mode,
the daemon's import/announcement policy rejects nonzero source prefixes without
an exact configured view. Ancestor coverage alone is insufficient: it would
apply more-specific routes to the wrong sources. Overlapping configured views
remain supported, and explicit priorities are validated for their ordering.
Export-policy reload reevaluates the routing policy as well as kernel state.

These views represent the selected Babel RIB and its withdrawal holds. Kernel
connected/static routes are not automatically redistributed or copied into
views. Hosts must integrate any additional forwarding policy and explicitly
originate/import the routes their domain needs. A custom `RouteExporter` must
implement RFC 9079 itself, or use `RoutePolicy` to reject unsupported sources.

Materialization costs grow with inherited route/view pairs, up to quadratic in
the number of source prefixes and routes. Kernel changes are reconciled through
individual netlink operations, not an atomic FIB transaction; errors are exposed
in export health and retried. Tests compare projected IPv4/IPv6 lookups to a
destination-first oracle and exercise real transit packets, crossing prefixes,
same-destination overrides, withdrawal holds, recovery, and cleanup on Linux.
