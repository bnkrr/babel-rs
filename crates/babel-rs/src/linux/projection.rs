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
                .is_none_or(|current| source_specific && !current.source_specific);
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
            // Apply the same source precedence to unreachable and finite
            // entries. An ordinary tombstone cannot shadow an exact-source route.
            if projected
                .get(&(view.table, key.destination))
                .is_some_and(|current| current.source_specific && key.source.is_none())
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
        (Some(view_source), Some(route_source)) => view_source == route_source,
        (None, Some(_)) => false,
    }
}
