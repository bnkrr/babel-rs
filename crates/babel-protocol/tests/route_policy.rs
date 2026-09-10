mod common;

use std::{net::IpAddr, sync::Arc};

use babel_protocol::*;
use common::{ConformanceHarness, id, key, sent_tlv};

#[derive(Default)]
struct Rules {
    deny_import: bool,
    neighbor: Option<IpAddr>,
    max_metric: Option<u16>,
    deny_export: Option<&'static str>,
}
impl RoutePolicy for Rules {
    fn accept(&self, r: &ImportContext<'_>) -> bool {
        assert_ne!(r.advertised_metric, INFINITY, "retractions bypass policy");
        !self.deny_import
            && self.neighbor.is_none_or(|n| n == r.neighbor)
            && self.max_metric.is_none_or(|m| r.advertised_metric <= m)
    }
    fn announce(&self, r: &ExportContext<'_>) -> bool {
        self.deny_export
            .is_none_or(|i| i != "*" && i != r.interface)
    }
}

fn replace(h: &mut ConformanceHarness, rules: impl RoutePolicy) -> Vec<Action> {
    h.now_ms += 1;
    h.engine.handle(Event::ReplaceRoutePolicy {
        policy: Arc::new(rules),
        now_ms: h.now_ms,
    })
}

fn relay() -> (ConformanceHarness, RouteKey) {
    let mut h = ConformanceHarness::new(id(1));
    h.interface("in");
    h.interface("out");
    h.establish_neighbour("in", "fe80::2");
    h.establish_neighbour("out", "fe80::3");
    (h, key("2001:db8:42::/64"))
}

fn updates<'a>(actions: &'a [Action], interface: &str) -> Vec<&'a OutboundUpdate> {
    actions
        .iter()
        .filter_map(|a| match a {
            Action::Send {
                interface: i,
                packet,
                ..
            } if i == interface => Some(packet),
            _ => None,
        })
        .flat_map(|p| &p.tlvs)
        .filter_map(|t| match t {
            OutboundTlv::Update(u) => Some(u),
            _ => None,
        })
        .collect()
}

fn assert_retracted(actions: &[Action], interface: &str, route: RouteKey) {
    let updates = updates(actions, interface);
    assert!(
        updates.iter().any(|u| u.key == Some(route)),
        "missing withdrawal on {interface}: {actions:?}"
    );
    assert!(
        updates
            .iter()
            .filter(|u| u.key == Some(route))
            .all(|u| u.metric == INFINITY)
    );
}

#[test]
fn startup_policy_rejects_routes_without_rejecting_neighbor_protocol() {
    let mut config = EngineConfig::recommended(id(1));
    config.route_policy = Arc::new(Rules {
        deny_import: true,
        ..Rules::default()
    });
    let mut h = ConformanceHarness {
        engine: Engine::new(config),
        now_ms: 0,
    };
    h.interface("in");
    h.establish_neighbour("in", "fe80::2");
    h.update("in", "fe80::2", key("2001:db8::/64"), id(2), 1, 0, 1600);
    assert_eq!(h.engine.neighbour_count(), 1);
    assert_eq!(h.engine.resource_status().candidates, 0);
    assert_eq!(h.engine.resource_status().sources, 0);
    let ack = h.receive(
        "in",
        "fe80::2",
        vec![Tlv::AckRequest {
            nonce: 42,
            interval_cs: 100,
        }],
    );
    assert!(sent_tlv(&ack, |t| matches!(
        t,
        OutboundTlv::Ack { nonce: 42 }
    )));
}

#[test]
fn tightening_import_reselects_allowed_alternate_and_frees_capacity() {
    let (mut h, route) = relay();
    h.update("in", "fe80::2", route, id(2), 10, 0, 1600);
    h.update("out", "fe80::3", route, id(3), 10, 100, 1600);
    assert_eq!(h.engine.selected_routes()[0].interface, "in");
    let actions = replace(
        &mut h,
        Rules {
            neighbor: Some("fe80::3".parse().unwrap()),
            ..Rules::default()
        },
    );
    assert_eq!(actions[0], Action::InvalidatePendingSends);
    assert_eq!(h.engine.selected_routes()[0].interface, "out");
    assert_eq!(h.engine.resource_status().candidates, 1);
    assert!(actions.iter().any(
        |a| matches!(a, Action::RoutesChanged { routes, .. } if routes[0].interface == "out")
    ));
    h.update("in", "fe80::2", route, id(2), 11, 0, 1600);
    assert_eq!(h.engine.resource_status().candidates, 1);
    replace(&mut h, AllowAllRoutes);
    h.update("in", "fe80::2", route, id(2), 11, 0, 1600);
    assert_eq!(h.engine.selected_routes()[0].interface, "in");
    assert_eq!(h.engine.resource_status().candidates, 2);
}

#[test]
fn relaxing_import_requests_routes_but_preserves_feasibility_history() {
    let (mut h, route) = relay();
    h.update("in", "fe80::2", route, id(2), 10, 0, 1600); // Advertised FD = 96.
    let sources = h.engine.resource_status().sources;
    let actions = replace(
        &mut h,
        Rules {
            deny_import: true,
            ..Rules::default()
        },
    );
    assert_retracted(&actions, "out", route);
    assert!(h.engine.selected_routes().is_empty());
    assert_eq!(h.engine.resource_status().candidates, 0);
    assert_eq!(h.engine.resource_status().sources, sources);
    let actions = replace(&mut h, AllowAllRoutes);
    for interface in ["in", "out"] {
        assert!(actions.iter().any(|a| matches!(a,
            Action::Send { interface: i, packet, .. } if i == interface
            && packet.tlvs.iter().any(|t| matches!(t, OutboundTlv::RouteRequest { key: None, .. }))
        )));
    }
    let actions = h.update("in", "fe80::2", route, id(2), 10, 100, 1600);
    assert!(
        h.engine.selected_routes().is_empty(),
        "same-sequence infeasible route must stay rejected"
    );
    assert!(sent_tlv(&actions, |t| matches!(
        t,
        OutboundTlv::SeqnoRequest { seqno: 11, .. }
    )));
    h.update("in", "fe80::2", route, id(2), 11, 100, 1600);
    assert_eq!(h.engine.selected_routes()[0].metric, 196);
}

#[test]
fn changed_update_rejected_by_same_policy_removes_old_candidate() {
    let (mut h, route) = relay();
    replace(
        &mut h,
        Rules {
            max_metric: Some(10),
            ..Rules::default()
        },
    );
    h.update("in", "fe80::2", route, id(2), 10, 0, 1600);
    let actions = h.update("in", "fe80::2", route, id(2), 11, 11, 1600);
    assert_eq!(h.engine.resource_status().candidates, 0);
    assert!(h.engine.selected_routes().is_empty());
    assert_retracted(&actions, "out", route);
    h.update("in", "fe80::2", route, id(2), 12, 0, 1600);
    assert_eq!(h.engine.resource_status().candidates, 1);
}

#[test]
fn policy_removal_releases_per_neighbor_admission_slots() {
    let mut h = ConformanceHarness::with_limits(
        id(1),
        ResourceLimits {
            max_candidates_per_neighbor: 1,
            ..ResourceLimits::default()
        },
    );
    h.interface("in");
    h.establish_neighbour("in", "fe80::2");
    let route = key("2001:db8:42::/64");
    let other = key("2001:db8:43::/64");
    h.update("in", "fe80::2", route, id(2), 1, 0, 1600);
    replace(
        &mut h,
        Rules {
            deny_import: true,
            ..Rules::default()
        },
    );
    replace(
        &mut h,
        Rules {
            max_metric: Some(10),
            ..Rules::default()
        },
    );
    h.update("in", "fe80::2", other, id(2), 1, 0, 1600);
    assert_eq!(h.engine.selected_routes()[0].key, other);
    // Rejection caused by a changing Update must free the same slot too.
    h.update("in", "fe80::2", other, id(2), 2, 11, 1600);
    h.update("in", "fe80::2", route, id(2), 2, 0, 1600);
    assert_eq!(h.engine.selected_routes()[0].key, route);
    assert_eq!(
        h.engine.resource_status().rejected_candidates_per_neighbor,
        0
    );
}

#[test]
fn specific_and_wildcard_retractions_bypass_import_callback() {
    for wildcard in [false, true] {
        let (mut h, route) = relay();
        replace(&mut h, Rules::default());
        h.update("in", "fe80::2", route, id(2), 10, 0, 1600);
        let actions = h.receive(
            "in",
            "fe80::2",
            vec![Tlv::Update(ResolvedUpdate {
                key: (!wildcard).then_some(route),
                router_id: None,
                next_hop: None,
                interval_cs: 1600,
                seqno: 10,
                metric: INFINITY,
                v4_via_v6: false,
                sub_tlvs: vec![],
            })],
        );
        assert!(h.engine.selected_routes().is_empty());
        assert_retracted(&actions, "out", route);
    }
}

#[test]
fn export_policy_covers_triggered_requested_forwarded_and_repeated_updates() {
    let (mut h, route) = relay();
    replace(
        &mut h,
        Rules {
            deny_export: Some("out"),
            ..Rules::default()
        },
    );
    let actions = h.update("in", "fe80::2", route, id(2), 10, 0, 1600);
    assert_eq!(
        h.engine.selected_routes().len(),
        1,
        "export rules do not reject the local RIB"
    );
    assert_retracted(&actions, "out", route);
    let request = |seqno| Tlv::SeqnoRequest {
        key: route,
        router_id: id(2),
        seqno,
        hop_count: 64,
        sub_tlvs: vec![],
    };
    for tlv in [
        Tlv::RouteRequest {
            key: Some(route),
            sub_tlvs: vec![],
        },
        request(10),
    ] {
        let actions = h.receive("out", "fe80::3", vec![tlv]);
        assert_retracted(&actions, "out", route);
    }
    // A forwarded request may be satisfied by a new Update before reselection.
    let forwarded = h.receive("out", "fe80::3", vec![request(11)]);
    assert!(sent_tlv(&forwarded, |t| matches!(
        t,
        OutboundTlv::SeqnoRequest { seqno: 11, .. }
    )));
    let reply = h.update("in", "fe80::2", route, id(2), 11, 0, 1600);
    assert_retracted(&reply, "out", route);
    let repeated = h.tick(1100);
    assert_retracted(&repeated, "out", route);
    h.now_ms = 4100; // Beyond wildcard-reply suppression, before neighbor expiry.
    let full = h.receive(
        "out",
        "fe80::3",
        vec![Tlv::RouteRequest {
            key: None,
            sub_tlvs: vec![],
        }],
    );
    assert_retracted(&full, "out", route);
}

#[test]
fn tightening_export_retracts_previous_unicast_on_split_horizon_interface() {
    let (mut h, route) = relay();
    h.update("in", "fe80::2", route, id(2), 10, 0, 1600);
    let reply = h.receive(
        "in",
        "fe80::2",
        vec![Tlv::RouteRequest {
            key: Some(route),
            sub_tlvs: vec![],
        }],
    );
    assert!(updates(&reply, "in").iter().any(|u| u.metric < INFINITY));
    let actions = replace(
        &mut h,
        Rules {
            deny_export: Some("*"),
            ..Rules::default()
        },
    );
    for interface in ["in", "out"] {
        assert_retracted(&actions, interface, route);
        let repeated = h.tick(h.now_ms + 1000);
        assert_retracted(&repeated, interface, route);
    }
    assert_eq!(h.engine.selected_routes().len(), 1);
    assert!(
        !actions
            .iter()
            .any(|a| matches!(a, Action::RoutesChanged { .. }))
    );
    let actions = replace(&mut h, AllowAllRoutes);
    assert!(updates(&actions, "out").iter().any(|u| u.metric < INFINITY));
    assert!(
        updates(&actions, "in").is_empty(),
        "normal finite split horizon still applies"
    );
}

#[test]
fn export_context_distinguishes_origins_and_periodic_output_respects_it() {
    struct OriginsOnly;
    impl RoutePolicy for OriginsOnly {
        fn announce(&self, r: &ExportContext<'_>) -> bool {
            r.locally_originated
        }
    }
    let (mut h, learned) = relay();
    replace(&mut h, OriginsOnly);
    h.update("in", "fe80::2", learned, id(2), 10, 0, 1600);
    let local = key("192.0.2.0/24");
    h.engine.handle(Event::Originate {
        key: local,
        metric: 0,
        now_ms: h.now_ms,
    });
    h.now_ms = 15_000;
    h.establish_neighbour("in", "fe80::2");
    let periodic = h.tick(20_000);
    assert!(
        updates(&periodic, "out")
            .iter()
            .any(|u| u.key == Some(local) && u.metric == 0)
    );
    assert_retracted(&periodic, "out", learned);
    let withdrawn = h.engine.handle(Event::Withdraw {
        key: local,
        now_ms: 20_001,
    });
    assert_retracted(&withdrawn, "out", local);
}

#[test]
fn import_context_retains_source_prefix_and_distinguishes_speaker_from_next_hop() {
    struct SourceRule;
    impl RoutePolicy for SourceRule {
        fn accept(&self, r: &ImportContext<'_>) -> bool {
            r.key.source == Some("2001:db8:10::/64".parse().unwrap())
                && r.interface == "in"
                && r.router_id == id(2)
                && r.seqno == 7
                && r.neighbor == "fe80::2".parse::<IpAddr>().unwrap()
                && r.next_hop == "fe80::4".parse::<IpAddr>().unwrap()
        }
    }
    let (mut h, plain) = relay();
    replace(&mut h, SourceRule);
    let source_key =
        RouteKey::new(plain.destination, Some("2001:db8:10::/64".parse().unwrap())).unwrap();
    for key in [plain, source_key] {
        h.receive(
            "in",
            "fe80::2",
            vec![Tlv::Update(ResolvedUpdate {
                key: Some(key),
                router_id: Some(id(2)),
                next_hop: Some("fe80::4".parse().unwrap()),
                interval_cs: 1600,
                seqno: 7,
                metric: 0,
                v4_via_v6: false,
                sub_tlvs: vec![],
            })],
        );
    }
    assert_eq!(h.engine.selected_routes().len(), 1);
    assert_eq!(h.engine.selected_routes()[0].key, source_key);
}
