mod common;

use std::net::IpAddr;

use babel_proto::{Action, Event, INFINITY, OutboundTlv, Packet, ResolvedUpdate, Tlv};
use common::{ConformanceHarness, id, key};

#[test]
fn rfc8966_3_8_1_2_seqno_request_increments_an_origin_only_once() {
    let mut h = ConformanceHarness::new(id(1));
    h.interface("a");
    let route = key("2001:db8:1::/64");
    h.engine.handle(Event::Originate {
        key: route,
        metric: 0,
        now_ms: 1,
    });
    let actions = h.receive(
        "a",
        "fe80::2",
        vec![Tlv::SeqnoRequest {
            key: route,
            seqno: 1000,
            hop_count: 16,
            router_id: id(1),
            sub_tlvs: vec![],
        }],
    );
    assert!(actions.contains(&Action::SequenceNumberChanged(8)));
    assert!(!actions.contains(&Action::SequenceNumberChanged(1001)));
}

#[test]
fn rfc8966_3_8_1_2_forwarded_seqno_request_is_unicast_and_decrements_hop_count() {
    let mut h = ConformanceHarness::new(id(1));
    h.interface("in");
    h.interface("out");
    h.establish_neighbour("in", "fe80::10");
    h.establish_neighbour("out", "fe80::20");
    let route = key("2001:db8:2::/64");
    h.update("out", "fe80::20", route, id(2), 5, 0, 1600);
    let actions = h.receive(
        "in",
        "fe80::10",
        vec![Tlv::SeqnoRequest {
            key: route,
            seqno: 6,
            hop_count: 9,
            router_id: id(2),
            sub_tlvs: vec![],
        }],
    );
    assert!(actions.iter().any(|action| matches!(
        action,
        Action::Send { interface, destination, packet }
            if interface == "out"
                && *destination == "fe80::20".parse::<IpAddr>().unwrap()
                && matches!(packet.tlvs.as_slice(), [OutboundTlv::SeqnoRequest { hop_count: 8, .. }])
    )));
}

#[test]
fn rfc8966_3_8_1_2_different_selected_router_id_satisfies_a_request() {
    let mut h = ConformanceHarness::new(id(1));
    h.interface("in");
    h.interface("out");
    h.establish_neighbour("out", "fe80::20");
    let route = key("2001:db8:22::/64");
    h.update("out", "fe80::20", route, id(3), 1, 0, 1600);
    let actions = h.receive(
        "in",
        "fe80::10",
        vec![Tlv::SeqnoRequest {
            key: route,
            seqno: 60000,
            hop_count: 1,
            router_id: id(2),
            sub_tlvs: vec![],
        }],
    );
    assert!(actions.iter().any(|action| matches!(
        action,
        Action::Send { destination, packet, .. }
            if *destination == "fe80::10".parse::<IpAddr>().unwrap()
                && matches!(packet.tlvs.as_slice(), [OutboundTlv::Update(update)]
                    if update.router_id == Some(id(3)))
    )));
}

#[test]
fn rfc8966_3_8_1_1_unknown_specific_route_request_gets_a_retraction() {
    let mut h = ConformanceHarness::new(id(1));
    h.interface("a");
    let route = key("2001:db8:3::/64");
    let actions = h.receive(
        "a",
        "fe80::2",
        vec![Tlv::RouteRequest {
            key: Some(route),
            sub_tlvs: vec![],
        }],
    );
    assert!(actions.iter().any(|action| matches!(
        action,
        Action::Send { destination, packet, .. }
            if *destination == "fe80::2".parse::<IpAddr>().unwrap()
                && matches!(packet.tlvs.as_slice(), [OutboundTlv::Update(update)]
                    if update.key == Some(route) && update.metric == INFINITY && update.router_id.is_none())
    )));
}

#[test]
fn rfc8966_3_5_5_expiry_retracts_then_garbage_collects_the_route() {
    let mut h = ConformanceHarness::new(id(1));
    h.interface("a");
    h.establish_neighbour("a", "fe80::2");
    let route = key("2001:db8:4::/64");
    h.update("a", "fe80::2", route, id(2), 1, 0, 10);
    assert_eq!(h.engine.selected_routes().len(), 1);

    let actions = h.tick(353);
    assert!(h.engine.selected_routes().is_empty());
    assert_eq!(h.engine.unreachable_routes(), vec![route]);
    assert!(actions.iter().any(|action| matches!(
        action,
        Action::RoutesChanged { routes, unreachable, .. }
            if routes.is_empty() && unreachable == &vec![route]
    )));

    h.tick(704);
    assert!(h.engine.unreachable_routes().is_empty());
}

#[test]
fn rfc8966_3_8_2_3_selected_route_is_refreshed_before_expiry() {
    let mut h = ConformanceHarness::new(id(1));
    h.interface("a");
    h.establish_neighbour("a", "fe80::2");
    let route = key("2001:db8:44::/64");
    h.update("a", "fe80::2", route, id(2), 1, 0, 100);
    let actions = h.tick(2502);
    assert!(actions.iter().any(|action| matches!(
        action,
        Action::Send { destination, packet, .. }
            if *destination == "fe80::2".parse::<IpAddr>().unwrap()
                && matches!(packet.tlvs.as_slice(), [OutboundTlv::RouteRequest { key: Some(value), .. }] if *value == route)
    )));
}

#[test]
fn rfc8966_appendix_b_pending_request_uses_bounded_exponential_retries() {
    let mut h = ConformanceHarness::new(id(1));
    h.interface("in");
    h.interface("out");
    h.establish_neighbour("in", "fe80::10");
    h.establish_neighbour("out", "fe80::20");
    let route = key("2001:db8:55::/64");
    h.update("out", "fe80::20", route, id(2), 5, 0, 1600);
    h.receive(
        "in",
        "fe80::10",
        vec![Tlv::SeqnoRequest {
            key: route,
            seqno: 6,
            hop_count: 9,
            router_id: id(2),
            sub_tlvs: vec![],
        }],
    );
    for deadline in [2004, 6004, 14004] {
        let actions = h.tick(deadline);
        assert!(actions.iter().any(|action| matches!(
            action,
            Action::Send { destination, packet, .. }
                if *destination == "fe80::20".parse::<IpAddr>().unwrap()
                    && matches!(packet.tlvs.as_slice(), [OutboundTlv::SeqnoRequest { .. }])
        )));
    }
    assert!(!h.tick(30004).iter().any(|action| matches!(
        action,
        Action::Send { packet, .. }
            if matches!(packet.tlvs.as_slice(), [OutboundTlv::SeqnoRequest { .. }])
    )));

    // The recently-forwarded cache remains after the pending request expires,
    // so a duplicate from the original requester is not forwarded again.
    let duplicate = h.receive(
        "in",
        "fe80::10",
        vec![Tlv::SeqnoRequest {
            key: route,
            seqno: 6,
            hop_count: 9,
            router_id: id(2),
            sub_tlvs: vec![],
        }],
    );
    assert!(!duplicate.iter().any(|action| matches!(
        action,
        Action::Send { destination, packet, .. }
            if *destination == "fe80::20".parse::<IpAddr>().unwrap()
                && matches!(packet.tlvs.as_slice(), [OutboundTlv::SeqnoRequest { .. }])
    )));
}

#[test]
fn rfc8966_3_5_3_router_id_change_triggers_an_update_even_if_unselected() {
    let mut h = ConformanceHarness::new(id(1));
    h.interface("a");
    h.interface("b");
    h.establish_neighbour("a", "fe80::2");
    h.establish_neighbour("b", "fe80::3");
    let route = key("2001:db8:57::/64");
    h.update("a", "fe80::2", route, id(2), 1, 100, 1600);
    h.update("b", "fe80::3", route, id(3), 1, 0, 1600);
    assert_eq!(h.engine.selected_routes()[0].router_id, id(3));

    let actions = h.update("a", "fe80::2", route, id(4), 1, 100, 1600);
    assert!(actions.iter().any(|action| matches!(
        action,
        Action::Send { interface, packet, .. }
            if interface == "a"
                && matches!(packet.tlvs.as_slice(), [OutboundTlv::Update(update)]
                    if update.key == Some(route) && update.router_id == Some(id(3)))
    )));
}

#[test]
fn rfc8966_3_5_4_unfeasible_update_cannot_replace_the_selected_route() {
    let mut h = ConformanceHarness::new(id(1));
    h.interface("a");
    h.interface("b");
    h.establish_neighbour("a", "fe80::2");
    h.establish_neighbour("b", "fe80::3");
    let route = key("2001:db8:5::/64");
    h.update("a", "fe80::2", route, id(2), 7, 10, 1600);
    assert_eq!(h.engine.selected_routes()[0].interface, "a");

    h.receive(
        "b",
        "fe80::3",
        vec![Tlv::Update(ResolvedUpdate {
            key: Some(route),
            router_id: Some(id(2)),
            next_hop: Some("fe80::3".parse().unwrap()),
            interval_cs: 1600,
            seqno: 7,
            metric: 20,
            v4_via_v6: false,
            sub_tlvs: vec![],
        })],
    );
    assert_eq!(h.engine.selected_routes()[0].interface, "a");
}

#[test]
fn rfc8966_3_8_2_1_starvation_keeps_a_seqno_request_active() {
    let mut h = ConformanceHarness::new(id(1));
    h.interface("a");
    h.interface("b");
    h.establish_neighbour("a", "fe80::2");
    h.establish_neighbour("b", "fe80::3");
    let route = key("2001:db8:56::/64");
    h.update("a", "fe80::2", route, id(2), 7, 10, 1600);
    let request = h.update("b", "fe80::3", route, id(2), 7, 120, 1600);
    assert!(request.iter().any(|action| matches!(
        action,
        Action::Send { destination, packet, .. }
            if *destination == "fe80::3".parse::<IpAddr>().unwrap()
                && matches!(packet.tlvs.as_slice(), [OutboundTlv::SeqnoRequest { seqno: 8, .. }])
    )));

    h.update("a", "fe80::2", route, id(2), 7, INFINITY, 1600);
    assert!(h.engine.selected_routes().is_empty());
    let retry = h.tick(2005);
    assert!(retry.iter().any(|action| matches!(
        action,
        Action::Send { destination, packet, .. }
            if *destination == "fe80::3".parse::<IpAddr>().unwrap()
                && matches!(packet.tlvs.as_slice(), [OutboundTlv::SeqnoRequest { seqno: 8, .. }])
    )));
}

#[test]
fn arbitrary_event_order_preserves_the_no_infinite_selected_route_invariant() {
    let route = key("2001:db8:6::/64");
    for metric in [0, 1, 96, 1024, INFINITY] {
        let mut h = ConformanceHarness::new(id(1));
        h.interface("a");
        h.establish_neighbour("a", "fe80::2");
        h.update("a", "fe80::2", route, id(2), 1, metric, 100);
        h.receive("a", "fe80::2", Packet { tlvs: vec![] }.tlvs);
        assert!(
            h.engine
                .selected_routes()
                .iter()
                .all(|value| value.metric < INFINITY)
        );
    }
}

#[test]
fn rfc9229_prefers_ordinary_ipv4_ae_when_interface_has_ipv4() {
    let mut h = ConformanceHarness::new(id(1));
    h.engine.handle(Event::InterfaceUp {
        interface: "dual".into(),
        local_addresses: vec!["192.0.2.1".parse().unwrap(), "fe80::1".parse().unwrap()],
        now_ms: 0,
    });
    let route = key("198.51.100.0/24");
    let actions = h.engine.handle(Event::Originate {
        key: route,
        metric: 0,
        now_ms: 1,
    });
    assert!(actions.iter().any(|action| matches!(
        action,
        Action::Send { packet, .. }
            if matches!(packet.tlvs.as_slice(), [OutboundTlv::Update(update)] if !update.v4_via_v6)
    )));
}

#[test]
fn rfc8966_appendix_c_minimum_dangerous_destinations_are_filtered() {
    for prefix in [
        "fe80::/64",
        "ff00::/8",
        "127.0.0.1/32",
        "0.0.0.0/32",
        "224.0.0.0/8",
    ] {
        let mut h = ConformanceHarness::new(id(1));
        h.interface("a");
        h.establish_neighbour("a", "fe80::2");
        h.update("a", "fe80::2", key(prefix), id(2), 1, 0, 1600);
        assert!(h.engine.selected_routes().is_empty(), "accepted {prefix}");
    }
}
