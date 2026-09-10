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

fn automatic_export() -> Export {
    Export {
        protocol: 203,
        device_only: false,
        manage_rules: true,
        automatic_sources: true,
        source_table_base: 1_000_000,
        source_rule_priority: 10_000,
        views: vec![ExportView {
            table: 201,
            source: None,
            rule_priority: None,
        }],
    }
}

#[test]
fn source_views_match_destination_first_oracle_for_ipv4_and_ipv6() {
    for (sources, destinations, packets) in [
        (
            vec!["10.0.0.0/8", "10.1.0.0/16", "10.1.2.0/24"],
            vec!["0.0.0.0/0", "192.0.0.0/16", "192.0.2.0/24"],
            vec![
                ("10.1.2.1", "192.0.2.1"),
                ("10.1.3.1", "192.0.3.1"),
                ("10.2.0.1", "192.0.2.1"),
                ("11.0.0.1", "193.0.0.1"),
            ],
        ),
        (
            vec!["2001:db8::/32", "2001:db8:1::/48", "2001:db8:1:2::/64"],
            vec!["::/0", "fd00::/16", "fd00:2::/32"],
            vec![
                ("2001:db8:1:2::1", "fd00:2::1"),
                ("2001:db8:1:3::1", "fd00:3::1"),
                ("2001:db8:2::1", "fd00:2::1"),
                ("2002::1", "fe00::1"),
            ],
        ),
    ] {
        // Vary presence, withdrawal and input iteration order independently.
        for variant in 0..64u32 {
            let mut snapshot = RouteSnapshot::default();
            for (si, source) in std::iter::once(None)
                .chain(sources.iter().map(|s| Some(*s)))
                .enumerate()
            {
                for (di, destination) in destinations.iter().enumerate() {
                    let route = selected(destination, source, (1000 - si * 100 + di) as u16);
                    match (variant.rotate_left((si * 3 + di) as u32) ^ ((si * 7 + di) as u32)) % 4 {
                        0 => {}
                        1 => snapshot.unreachable.push(route.key),
                        _ => snapshot.routes.push(route),
                    }
                }
            }
            if variant % 2 == 0 {
                snapshot.routes.reverse();
                snapshot.unreachable.reverse();
            }
            let views =
                projection::source_views(&automatic_export(), &snapshot, &mut Default::default())
                    .unwrap();
            let projected = project_routes(&views, &snapshot);
            for (src, dst) in &packets {
                let src: IpAddr = src.parse().unwrap();
                let dst: IpAddr = dst.parse().unwrap();
                let expected = snapshot
                    .routes
                    .iter()
                    .map(|r| (r.key, false))
                    .chain(snapshot.unreachable.iter().map(|k| (*k, true)))
                    .filter(|(key, _)| {
                        key.destination.contains(&dst)
                            && key.source.is_none_or(|s| s.contains(&src))
                    })
                    .max_by_key(|(k, _)| {
                        (
                            k.destination.prefix_len(),
                            k.source.map_or(0, |s| s.prefix_len()),
                        )
                    });
                let view = views
                    .iter()
                    .filter(|v| v.source.is_none_or(|s| s.contains(&src)))
                    .min_by_key(|v| {
                        if v.source.is_some() {
                            automatic_export().rule_priority(**v)
                        } else {
                            u32::MAX
                        }
                    })
                    .unwrap();
                let actual = projected
                    .iter()
                    .filter(|r| r.table == view.table && r.key.destination.contains(&dst))
                    .max_by_key(|r| r.key.destination.prefix_len())
                    .map(|r| (r.key, r.selected.is_none()));
                assert_eq!(actual, expected, "variant={variant} src={src} dst={dst}");
            }
        }
    }
}

#[test]
fn inherited_routes_refresh_withdraw_and_keep_tables_until_hold_expires() {
    let mut export = automatic_export();
    export.views.push(ExportView {
        table: 202,
        source: Some("10.1.2.0/24".parse().unwrap()),
        rule_priority: None,
    });
    let parent = selected("192.0.2.0/24", Some("10.0.0.0/8"), 500);
    let child = selected("0.0.0.0/0", Some("10.1.0.0/16"), 100);
    let mut snapshot = RouteSnapshot {
        routes: vec![parent.clone(), child.clone()],
        ..Default::default()
    };
    let mut allocated = Default::default();
    let first = projection::source_views(&export, &snapshot, &mut allocated).unwrap();
    let child_table = allocated[&child.key.source.unwrap()];
    assert!(
        project_routes(&first, &snapshot)
            .iter()
            .any(|r| r.table == child_table && r.key == parent.key)
    );
    assert!(
        project_routes(&first, &snapshot)
            .iter()
            .any(|r| r.table == 202 && r.key == parent.key)
    );
    snapshot.routes[0].metric = 42;
    let views = projection::source_views(&export, &snapshot, &mut allocated).unwrap();
    assert_eq!(allocated[&child.key.source.unwrap()], child_table);
    assert!(
        project_routes(&views, &snapshot)
            .iter()
            .filter(|r| r.key == parent.key)
            .all(|r| r.selected.as_ref().unwrap().metric == 42)
    );
    snapshot.routes.remove(1);
    snapshot.unreachable.push(child.key);
    let views = projection::source_views(&export, &snapshot, &mut allocated).unwrap();
    assert!(
        project_routes(&views, &snapshot)
            .iter()
            .any(|r| r.table == child_table && r.key == child.key && r.selected.is_none())
    );
    snapshot.unreachable.clear();
    let views = projection::source_views(&export, &snapshot, &mut allocated).unwrap();
    assert!(!views.iter().any(|v| v.source == child.key.source));
    assert!(!allocated.contains_key(&child.key.source.unwrap()));
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
            automatic_sources: true,
            source_table_base: 1_000_000,
            source_rule_priority: 10_000,
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
