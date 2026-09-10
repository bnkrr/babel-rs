mod common;
use babel_protocol::{
    Action, DecodeContext, Event, INFINITY, InterfacePolicy, Ipv4NextHop, ResolvedUpdate, Tlv,
    WiredMetric, decode_packet, encode_packets,
};
use common::{ConformanceHarness, id, key};
use std::sync::Arc;

fn policy(mode: Ipv4NextHop) -> InterfacePolicy {
    InterfacePolicy {
        control_transport: Default::default(),
        ipv4_next_hop: mode,
        metric: Arc::new(WiredMetric::new(96, 1, 1).unwrap()),
        hello_interval_cs: 400,
        update_interval_cs: 1600,
        split_horizon: false,
    }
}

fn updates(actions: Vec<Action>) -> Vec<ResolvedUpdate> {
    actions
        .into_iter()
        .filter_map(|a| match a {
            Action::Send { packet, .. } => Some(packet),
            _ => None,
        })
        .flat_map(|packet| encode_packets(&packet, 1232).unwrap())
        .flat_map(|bytes| {
            decode_packet(
                &bytes,
                DecodeContext {
                    source: "fe80::1".parse().unwrap(),
                },
            )
            .unwrap()
            .tlvs
        })
        .filter_map(|tlv| match tlv {
            Tlv::Update(update) => Some(update),
            _ => None,
        })
        .collect()
}

#[test]
fn ipv4_policy_matrix_encodes_decodable_next_hops_and_retractions() {
    for mode in [Ipv4NextHop::Auto, Ipv4NextHop::Ipv4, Ipv4NextHop::Ipv6] {
        for has_v4 in [false, true] {
            let mut h = ConformanceHarness::new(id(1));
            let mut addresses = vec!["fe80::1".parse().unwrap()];
            if has_v4 {
                addresses.extend([
                    "192.0.2.2".parse::<std::net::IpAddr>().unwrap(),
                    "192.0.2.1".parse().unwrap(),
                ]);
            }
            h.engine.handle(Event::InterfaceUpWithPolicy {
                interface: "lan".into(),
                local_addresses: addresses,
                policy: policy(mode),
                now_ms: 0,
            });
            let route = key("198.51.100.0/24");
            let announced = updates(h.engine.handle(Event::Originate {
                key: route,
                metric: 0,
                now_ms: 1,
            }));
            assert_eq!(announced.len(), 1);
            let update = &announced[0];
            if mode == Ipv4NextHop::Ipv4 && !has_v4 {
                assert_eq!(update.metric, INFINITY);
                assert_eq!(h.engine.resource_status().sources, 0);
            } else {
                assert_eq!(update.metric, 0);
                let ordinary = has_v4 && mode != Ipv4NextHop::Ipv6;
                assert_eq!(update.v4_via_v6, !ordinary);
                assert_eq!(
                    update.next_hop,
                    Some(
                        if ordinary { "192.0.2.1" } else { "fe80::1" }
                            .parse()
                            .unwrap()
                    )
                );
            }
            let withdrawn = updates(h.engine.handle(Event::Withdraw {
                key: route,
                now_ms: 2,
            }));
            assert!(
                withdrawn
                    .iter()
                    .any(|u| u.metric == INFINITY && !u.v4_via_v6)
            );
        }
    }
}

#[test]
fn address_and_policy_changes_refresh_announcements_without_losing_neighbors() {
    let mut h = ConformanceHarness::new(id(1));
    h.engine.handle(Event::InterfaceUpWithPolicy {
        interface: "lan".into(),
        local_addresses: vec!["fe80::1".parse().unwrap()],
        policy: policy(Ipv4NextHop::Auto),
        now_ms: 0,
    });
    h.establish_neighbour("lan", "fe80::2");
    let route = key("198.51.100.0/24");
    h.engine.handle(Event::Originate {
        key: route,
        metric: 0,
        now_ms: 2,
    });
    let announced = updates(h.engine.handle(Event::InterfaceAddressesChanged {
        interface: "lan".into(),
        local_addresses: vec!["192.0.2.1".parse().unwrap(), "fe80::1".parse().unwrap()],
        now_ms: 3,
    }));
    assert!(announced.iter().any(|u| u.key == Some(route)
        && u.next_hop == Some("192.0.2.1".parse().unwrap())
        && !u.v4_via_v6));
    assert_eq!(h.engine.neighbour_count(), 1);
    let changed = updates(h.engine.handle(Event::InterfacePolicyChanged {
        interface: "lan".into(),
        policy: policy(Ipv4NextHop::Ipv6),
        reset_metric: false,
        now_ms: 4,
    }));
    assert!(changed.iter().any(|u| u.key == Some(route) && u.v4_via_v6));
    h.now_ms = 4;
    let response = updates(h.receive(
        "lan",
        "fe80::2",
        vec![Tlv::RouteRequest {
            key: Some(route),
            sub_tlvs: vec![],
        }],
    ));
    assert!(response.iter().any(|u| u.key == Some(route) && u.v4_via_v6));
    h.engine.handle(Event::InterfacePolicyChanged {
        interface: "lan".into(),
        policy: policy(Ipv4NextHop::Ipv4),
        reset_metric: false,
        now_ms: 6,
    });
    let removed = updates(h.engine.handle(Event::InterfaceAddressesChanged {
        interface: "lan".into(),
        local_addresses: vec!["fe80::1".parse().unwrap()],
        now_ms: 7,
    }));
    assert!(
        removed
            .iter()
            .any(|u| u.key == Some(route) && u.metric == INFINITY)
    );
    let v6 = updates(h.engine.handle(Event::Originate {
        key: key("2001:db8::/64"),
        metric: 0,
        now_ms: 8,
    }));
    assert!(v6.iter().any(|u| u.metric == 0));
    assert_eq!(h.engine.neighbour_count(), 1);
}
