mod common;

use babel_proto::{Action, Event, OutboundTlv, RouteKey, Tlv};
use common::{ConformanceHarness, id, key, sent_tlv};

fn relay(cost: u16) -> (ConformanceHarness, RouteKey) {
    let mut h = ConformanceHarness::new(id(1));
    for (interface, address) in [("good", "fe80::2"), ("alternate", "fe80::3")] {
        h.interface(interface);
        h.establish_neighbour(interface, address);
    }
    let route = key("2001:db8::/64");
    h.update("good", "fe80::2", route, id(2), 100, 0, 1600);
    // Raise the current path's cost without increasing its historical FD=96.
    h.receive(
        "good",
        "fe80::2",
        vec![Tlv::Ihu {
            address: None,
            rxcost: cost,
            interval_cs: 1200,
            sub_tlvs: vec![],
        }],
    );
    assert_eq!(h.engine.selected_routes()[0].metric, cost);
    (h, route)
}

fn requests_new_seqno(actions: &[Action]) -> bool {
    sent_tlv(actions, |tlv| {
        matches!(tlv, OutboundTlv::SeqnoRequest { seqno: 101, .. })
    })
}

#[test]
fn worse_or_equal_unfeasible_alternate_does_not_request_new_sequence() {
    for current_cost in [96, 192] {
        let (mut h, route) = relay(current_cost);
        let actions = h.update("alternate", "fe80::3", route, id(2), 100, 96, 1600);
        assert!(!requests_new_seqno(&actions), "current cost {current_cost}");
        assert_eq!(h.engine.selected_routes()[0].metric, current_cost);
    }
}

#[test]
fn worse_alternate_does_not_suppress_a_later_recovery_request() {
    let (mut h, route) = relay(96);
    h.update("alternate", "fe80::3", route, id(2), 100, 96, 1600);
    let actions = h.receive(
        "alternate",
        "fe80::3",
        vec![Tlv::SeqnoRequest {
            key: route,
            router_id: id(2),
            seqno: 101,
            hop_count: 64,
            sub_tlvs: vec![],
        }],
    );
    assert!(actions.iter().any(|action| matches!(action,
        Action::Send { interface, destination, packet, .. }
        if interface == "good" && *destination == "fe80::2".parse::<std::net::IpAddr>().unwrap()
        && packet.tlvs.iter().any(|tlv| matches!(tlv, OutboundTlv::SeqnoRequest {seqno:101,hop_count:63,..}))
    )));
}

#[test]
fn preferable_unfeasible_alternate_still_requests_new_sequence() {
    let (mut h, route) = relay(288);
    let actions = h.update("alternate", "fe80::3", route, id(2), 100, 96, 1600);
    assert!(requests_new_seqno(&actions));
    assert_eq!(h.engine.selected_routes()[0].metric, 288);
    h.update("alternate", "fe80::3", route, id(2), 101, 96, 1600);
    assert_eq!(h.engine.selected_routes()[0].metric, 192);
}

#[test]
fn infeasible_current_path_still_requests_new_sequence() {
    let (mut h, route) = relay(96);
    let actions = h.update("good", "fe80::2", route, id(2), 100, 96, 1600);
    assert!(requests_new_seqno(&actions));
}

#[test]
fn losing_current_path_still_requests_sequence_from_stored_alternate() {
    let (mut h, route) = relay(96);
    h.update("alternate", "fe80::3", route, id(2), 100, 96, 1600);
    let actions = h.engine.handle(Event::InterfaceDown {
        interface: "good".into(),
        now_ms: h.now_ms + 1,
    });
    assert!(requests_new_seqno(&actions));
    assert!(h.engine.selected_routes().is_empty());
}

/// Three wired nodes in a triangle. The origin's direct link to node 1 fails,
/// while the path through node 2 stays available. Deliver real encoded packets
/// without loss, using virtual time so the 180-second GC cannot hide starvation.
#[test]
fn triangle_recovers_without_waiting_for_source_gc() {
    use babel_proto::{DecodeContext, Engine, EngineConfig, Packet, decode_packet, encode_packets};
    use std::collections::{BTreeMap, HashSet};
    use std::net::IpAddr;

    const EDGES: [(usize, usize); 3] = [(0, 1), (0, 2), (1, 2)];
    fn address(node: usize) -> IpAddr {
        format!("fe80::{}", node + 1).parse().unwrap()
    }
    struct Network {
        nodes: Vec<Engine>,
        queue: BTreeMap<u64, Vec<(usize, usize, usize, Packet)>>,
        down: bool,
        sent: usize,
    }
    impl Network {
        fn deliver(&mut self, node: usize, event: Event, now: u64) {
            for action in self.nodes[node].handle(event) {
                if let Action::Send {
                    interface,
                    destination,
                    packet,
                    ..
                } = action
                {
                    let edge: usize = interface[1..].parse().unwrap();
                    if self.down && edge == 0 {
                        continue;
                    }
                    let (a, b) = EDGES[edge];
                    let peer = if a == node { b } else { a };
                    assert!(
                        destination == address(peer)
                            || destination == babel_proto::engine::BABEL_MULTICAST_V6
                    );
                    for bytes in encode_packets(&packet, 1232).unwrap() {
                        let packet = decode_packet(
                            &bytes,
                            DecodeContext {
                                source: address(node),
                            },
                        )
                        .unwrap();
                        self.queue
                            .entry(now + 10)
                            .or_default()
                            .push((node, peer, edge, packet));
                        self.sent += 1;
                        assert!(self.sent < 20_000, "unbounded control traffic");
                    }
                }
            }
        }
        fn assert_forwarding(&self, route: RouteKey) {
            for start in [1, 2] {
                let mut node = start;
                let mut seen = HashSet::new();
                while node != 0 {
                    assert!(seen.insert(node), "forwarding loop from {start}");
                    let routes = self.nodes[node].selected_routes();
                    let selected = routes
                        .iter()
                        .find(|r| r.key == route)
                        .expect("route must recover before source GC");
                    let edge: usize = selected.interface[1..].parse().unwrap();
                    assert!(!(self.down && edge == 0));
                    let (a, b) = EDGES[edge];
                    node = if a == node { b } else { a };
                    assert_eq!(selected.next_hop, address(node));
                }
            }
        }
    }
    let route = key("2001:db8::/64");
    let mut net = Network {
        nodes: (0..3)
            .map(|node| Engine::new(EngineConfig::recommended(id(node + 1))))
            .collect(),
        queue: BTreeMap::new(),
        down: false,
        sent: 0,
    };
    net.deliver(
        0,
        Event::Originate {
            key: route,
            metric: 0,
            now_ms: 0,
        },
        0,
    );
    for (edge, (a, b)) in EDGES.into_iter().enumerate() {
        for node in [a, b] {
            net.deliver(
                node,
                Event::InterfaceUp {
                    interface: format!("e{edge}"),
                    local_addresses: vec![address(node)],
                    now_ms: 0,
                },
                0,
            );
        }
    }
    for now in (0..=60_000).step_by(10) {
        if now == 30_000 {
            net.assert_forwarding(route);
            net.down = true;
            net.deliver(
                0,
                Event::InterfaceDown {
                    interface: "e0".into(),
                    now_ms: now,
                },
                now,
            );
        }
        if now % 100 == 0 {
            for node in 0..3 {
                net.deliver(node, Event::Tick { now_ms: now }, now);
            }
        }
        while net
            .queue
            .first_key_value()
            .is_some_and(|(&due, _)| due <= now)
        {
            let (_, packets) = net.queue.pop_first().unwrap();
            for (from, to, edge, packet) in packets {
                if net.down && edge == 0 {
                    continue;
                }
                net.deliver(
                    to,
                    Event::PacketReceived {
                        interface: format!("e{edge}"),
                        source: address(from),
                        packet,
                        now_ms: now,
                    },
                    now,
                );
            }
        }
        if now >= 50_000 && now % 100 == 0 {
            net.assert_forwarding(route);
        }
    }
}
