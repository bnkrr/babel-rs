use super::*;
use babel_protocol::{RouteKey, RouterId};

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

#[test]
fn export_progress_preserves_attempted_revision_across_reload_and_failure() {
    let mut state = ExportState {
        export: Export {
            protocol: 203,
            device_only: false,
            manage_rules: false,
            views: vec![],
        },
        snapshot: RouteSnapshot::default(),
        retain_rules: true,
        stopping: false,
        retired: vec![],
        config_generation: 0,
        last_success_revision: None,
        last_success: None,
        last_error: None,
    };
    let error = Err(LinuxError::Interface("missing0".into()));
    state.record_reconcile(state.revision(), &error);
    assert!(state.last_success_revision.is_none());
    assert!(state.last_success.is_none());

    state.snapshot.generation = 7;
    let attempted = state.revision();
    // Reload changes only export settings while netlink applies the old input.
    // Equal RIB generations alone must not acknowledge this reload.
    state.config_generation = 1;
    state.record_reconcile(attempted, &Ok(()));
    assert_eq!(state.last_success_revision, Some(attempted));
    assert_ne!(state.last_success_revision, Some(state.revision()));
    assert!(state.last_error.is_none());

    state.snapshot.generation = 8;
    let last_success = state.last_success;
    state.record_reconcile(state.revision(), &error);
    assert_eq!(state.last_success_revision, Some(attempted));
    assert_eq!(state.last_success, last_success);
    assert!(state.last_error.as_ref().unwrap().contains("missing0"));

    state.record_reconcile(state.revision(), &Ok(()));
    assert_eq!(state.last_success_revision, Some(state.revision()));
    assert!(state.last_error.is_none());
}

#[test]
fn source_specificity_orders_finite_routes_and_tombstones_equally() {
    for specific_is_unreachable in [false, true] {
        let source = "2001:db8:99::/64";
        let finite = selected(
            "2001:db8:42::/64",
            (!specific_is_unreachable).then_some(source),
            96,
        );
        let unreachable = RouteKey::new(
            finite.key.destination,
            specific_is_unreachable.then(|| source.parse().unwrap()),
        )
        .unwrap();
        let snapshot = RouteSnapshot {
            generation: 1,
            routes: vec![finite],
            unreachable: vec![unreachable],
        };
        let routes = project_routes(
            &[ExportView {
                table: 100,
                source: Some(source.parse().unwrap()),
                rule_priority: None,
            }],
            &snapshot,
        );
        assert_eq!(routes.len(), 1);
        assert!(routes[0].source_specific);
        assert_eq!(routes[0].selected.is_none(), specific_is_unreachable);
    }
}
