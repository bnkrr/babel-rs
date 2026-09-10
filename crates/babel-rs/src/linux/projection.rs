//! Pure projection of the selected Babel RIB into Linux policy views.

use super::*;

pub(super) fn project_routes(
    views: &[ExportView],
    snapshot: &RouteSnapshot,
) -> Vec<ProjectedRoute> {
    let mut projected: HashMap<(u32, IpNet), ProjectedRoute> = HashMap::new();
    for view in views {
        for route in &snapshot.routes {
            if !route_matches_view(view, route.key) {
                continue;
            }
            let source_specific = route.key.source.is_some();
            let key = (view.table, route.key.destination);
            let replace = projected
                .get(&key)
                .is_none_or(|current| specificity(route.key) > specificity(current.key));
            if replace {
                projected.insert(
                    key,
                    ProjectedRoute {
                        table: view.table,
                        key: route.key,
                        selected: Some(route.clone()),
                        source_specific,
                    },
                );
            }
        }
        for key in &snapshot.unreachable {
            if !route_matches_view(view, *key) {
                continue;
            }
            // Retained withdrawals obey the same source precedence as finite
            // routes, so an ancestor tombstone cannot shadow a child route.
            if projected
                .get(&(view.table, key.destination))
                .is_some_and(|current| specificity(current.key) > specificity(*key))
            {
                continue;
            }
            projected.insert(
                (view.table, key.destination),
                ProjectedRoute {
                    table: view.table,
                    key: *key,
                    selected: None,
                    source_specific: key.source.is_some(),
                },
            );
        }
    }
    let mut result: Vec<_> = projected.into_values().collect();
    result.sort_by_key(|route| (route.table, route.key.destination));
    result
}

fn route_matches_view(view: &ExportView, key: RouteKey) -> bool {
    match (view.source, key.source) {
        (None, None) => true,
        (Some(view_source), None) => {
            view_source.addr().is_ipv4() == key.destination.addr().is_ipv4()
        }
        (Some(view_source), Some(route_source)) => route_source.contains(&view_source),
        (None, Some(_)) => false,
    }
}

fn specificity(key: RouteKey) -> u8 {
    key.source.map_or(0, |source| source.prefix_len())
}

/// Complete source views. Explicit views pin table IDs; other active source
/// prefixes (including unreachable holds) receive stable, process-local IDs.
pub(super) fn source_views(
    export: &Export,
    snapshot: &RouteSnapshot,
    allocated: &mut std::collections::BTreeMap<IpNet, u32>,
) -> Result<Vec<ExportView>, LinuxError> {
    let mut views = export.views.clone();
    if !export.automatic_sources() {
        allocated.clear();
        return Ok(views);
    }
    let explicit: HashSet<_> = views.iter().filter_map(|v| v.source).collect();
    let sources: std::collections::BTreeSet<_> = snapshot
        .routes
        .iter()
        .map(|r| r.key)
        .chain(snapshot.unreachable.iter().copied())
        .filter_map(|key| key.source)
        .filter(|s| !explicit.contains(s))
        .collect();
    let mut used: HashSet<_> = views.iter().map(|v| v.table).collect();
    allocated.retain(|source, table| {
        sources.contains(source) && *table >= export.source_table_base && !used.contains(table)
    });
    used.extend(allocated.values().copied());
    let mut next = export.source_table_base;
    for source in sources {
        let table = if let Some(table) = allocated.get(&source) {
            *table
        } else {
            while used.contains(&next) {
                next = next
                    .checked_add(1)
                    .ok_or_else(|| LinuxError::InvalidRoute("source table IDs exhausted".into()))?;
            }
            allocated.insert(source, next);
            used.insert(next);
            next
        };
        views.push(ExportView {
            table,
            source: Some(source),
            rule_priority: None,
        });
    }
    Ok(views)
}

/// An externally managed/static configuration must not admit source prefixes
/// without their own complete view. Automatic allocation supports every source.
pub(crate) struct SourcePolicy(pub Export);

impl SourcePolicy {
    fn supported(&self, key: RouteKey) -> bool {
        key.source.is_none()
            || self.0.automatic_sources()
            || self.0.views.iter().any(|v| v.source == key.source)
    }
}

impl babel_protocol::RoutePolicy for SourcePolicy {
    fn accept(&self, route: &babel_protocol::ImportContext<'_>) -> bool {
        self.supported(route.key)
    }
    fn announce(&self, route: &babel_protocol::ExportContext<'_>) -> bool {
        self.supported(route.key)
    }
}
