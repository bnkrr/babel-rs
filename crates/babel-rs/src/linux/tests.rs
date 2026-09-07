use super::*;
use babel_proto::{RouteKey, RouterId};

fn selected(destination: &str, source: Option<&str>, metric: u16) -> SelectedRoute {
    SelectedRoute {
        key: RouteKey::new(
            destination.parse().unwrap(),
            source.map(|value| value.parse().unwrap()),
        )
        .unwrap(),
        router_id: RouterId::new([1; 8]).unwrap(),
        seqno: 1,
        metric,
        next_hop: "fe80::1".parse().unwrap(),
        interface: "wg0".into(),
    }
}

#[test]
fn ordinary_routes_are_materialized_into_every_matching_view() {
    let snapshot = RouteSnapshot {
        generation: 1,
        routes: vec![selected("192.0.2.0/24", None, 256)],
        unreachable: vec![],
    };
    let views = vec![
        ExportView {
            table: 20000,
            source: None,
            rule_priority: None,
        },
        ExportView {
            table: 20001,
            source: Some("10.0.0.0/8".parse().unwrap()),
            rule_priority: None,
        },
    ];
    let routes = project_routes(&views, &snapshot);
    assert_eq!(routes.len(), 2);
    assert!(routes.iter().all(|route| !route.source_specific));
}

#[test]
fn exact_source_route_overrides_ordinary_route_in_its_view() {
    let snapshot = RouteSnapshot {
        generation: 1,
        routes: vec![
            selected("0.0.0.0/0", None, 128),
            selected("0.0.0.0/0", Some("10.0.0.0/8"), 512),
        ],
        unreachable: vec![],
    };
    let views = vec![ExportView {
        table: 20001,
        source: Some("10.0.0.0/8".parse().unwrap()),
        rule_priority: None,
    }];
    let routes = project_routes(&views, &snapshot);
    assert_eq!(routes.len(), 1);
    assert!(routes[0].source_specific);
    assert_eq!(routes[0].selected.as_ref().unwrap().metric, 512);
}

#[test]
fn explicit_zero_source_is_an_ordinary_route() {
    let snapshot = RouteSnapshot {
        generation: 1,
        routes: vec![selected("203.0.113.0/24", Some("0.0.0.0/0"), 96)],
        unreachable: vec![],
    };
    let views = vec![
        ExportView {
            table: 20000,
            source: None,
            rule_priority: None,
        },
        ExportView {
            table: 20001,
            source: Some("10.0.0.0/8".parse().unwrap()),
            rule_priority: None,
        },
    ];
    let routes = project_routes(&views, &snapshot);
    assert_eq!(routes.len(), 2);
    assert!(routes.iter().all(|route| !route.source_specific));
}

#[test]
fn unreachable_tombstone_is_projected_into_matching_views() {
    let key = RouteKey::new("192.0.2.0/24".parse().unwrap(), None).unwrap();
    let snapshot = RouteSnapshot {
        generation: 2,
        routes: vec![],
        unreachable: vec![key],
    };
    let views = vec![
        ExportView {
            table: 20000,
            source: None,
            rule_priority: None,
        },
        ExportView {
            table: 20001,
            source: Some("10.0.0.0/8".parse().unwrap()),
            rule_priority: None,
        },
    ];
    let routes = project_routes(&views, &snapshot);
    assert_eq!(routes.len(), 2);
    assert!(routes.iter().all(|route| route.selected.is_none()));
}
