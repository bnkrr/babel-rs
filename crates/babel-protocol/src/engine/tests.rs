use super::rib::selected_from_candidate;
use super::*;
use ipnet::IpNet;
use std::str::FromStr;

fn id(value: u8) -> RouterId {
    RouterId::new([value; 8]).unwrap()
}
fn key() -> RouteKey {
    RouteKey::new(IpNet::from_str("2001:db8::/64").unwrap(), None).unwrap()
}
fn config(router_id: RouterId) -> EngineConfig {
    let mut config = EngineConfig::recommended(router_id);
    config.metric = Arc::new(WiredMetric::new(96, 1, 1).unwrap());
    config
}

fn policy(cost: u16, hello_interval_cs: u16, split_horizon: bool) -> InterfacePolicy {
    InterfacePolicy {
        control_transport: Default::default(),
        ipv4_next_hop: Default::default(),
        metric: Arc::new(WiredMetric::new(cost, 1, 1).unwrap()),
        hello_interval_cs,
        update_interval_cs: hello_interval_cs * 4,
        split_horizon,
    }
}

#[test]
fn configured_interface_policy_controls_wire_intervals_and_metric() {
    let mut engine = Engine::new(config(id(1)));
    let actions = engine.handle(Event::InterfaceUpWithPolicy {
        interface: "mesh0".into(),
        local_addresses: vec![],
        policy: policy(222, 100, false),
        now_ms: 0,
    });
    assert!(actions.iter().any(|action| matches!(
        action,
        Action::Send { packet, .. }
            if packet.tlvs.iter().any(|tlv| matches!(tlv,
                OutboundTlv::Hello { interval_cs: 100, .. }))
    )));
    assert!(actions.iter().any(|action| matches!(
        action,
        Action::Send { interface, destination, packet, .. }
            if interface == "mesh0"
                && *destination == BABEL_MULTICAST_V6
                && packet.tlvs.iter().any(|tlv| matches!(tlv,
                    OutboundTlv::RouteRequest { key: None, .. }))
    )));

    let source = "fe80::2".parse().unwrap();
    engine.handle(Event::PacketReceived {
        interface: "mesh0".into(),
        source,
        now_ms: 10,
        packet: Packet {
            tlvs: vec![
                Tlv::Hello {
                    unicast: false,
                    seqno: 1,
                    interval_cs: 100,
                    sub_tlvs: vec![],
                },
                Tlv::Ihu {
                    address: None,
                    rxcost: 222,
                    interval_cs: 300,
                    sub_tlvs: vec![],
                },
                Tlv::Update(ResolvedUpdate {
                    key: Some(key()),
                    router_id: Some(id(2)),
                    next_hop: Some(source),
                    interval_cs: 400,
                    seqno: 1,
                    metric: 0,
                    v4_via_v6: false,
                    sub_tlvs: vec![],
                }),
            ],
        },
    });
    assert_eq!(engine.selected_routes()[0].metric, 222);

    let actions = engine.handle(Event::InterfacePolicyChanged {
        interface: "mesh0".into(),
        policy: policy(300, 50, true),
        reset_metric: true,
        now_ms: 20,
    });
    assert_eq!(engine.selected_routes()[0].metric, 222);
    assert!(actions.iter().any(|action| matches!(
        action,
        Action::Send { packet, .. }
            if packet.tlvs.iter().any(|tlv| matches!(tlv,
                OutboundTlv::Hello { interval_cs: 50, .. }))
    )));
}

#[test]
fn split_horizon_is_decided_by_each_egress_interface() {
    let mut engine = Engine::new(config(id(1)));
    engine.handle(Event::InterfaceUpWithPolicy {
        interface: "mesh0".into(),
        local_addresses: vec![],
        policy: policy(96, 100, false),
        now_ms: 0,
    });
    engine.handle(Event::InterfaceUpWithPolicy {
        interface: "wired0".into(),
        local_addresses: vec![],
        policy: policy(96, 200, true),
        now_ms: 0,
    });
    engine.selected.insert(
        key(),
        SelectedRoute {
            key: key(),
            router_id: id(2),
            seqno: 1,
            metric: 96,
            next_hop: "fe80::2".parse().unwrap(),
            interface: "mesh0".into(),
        },
    );
    let actions = engine.send_updates(10, Some(key()), None, None);
    assert!(actions.iter().any(|action| matches!(
        action,
        Action::Send { interface, packet, .. }
            if interface == "mesh0" && packet.tlvs.iter().any(|tlv| matches!(tlv,
                OutboundTlv::Update(update) if update.interval_cs == 400))
    )));
    assert!(actions.iter().any(|action| matches!(
        action,
        Action::Send { interface, packet, .. }
            if interface == "wired0" && packet.tlvs.iter().any(|tlv| matches!(tlv,
                OutboundTlv::Update(update) if update.interval_cs == 800))
    )));

    engine.selected.get_mut(&key()).unwrap().interface = "wired0".into();
    let actions = engine.send_updates(20, Some(key()), None, None);
    assert!(
        actions
            .iter()
            .any(|action| matches!(action, Action::Send { interface, .. } if interface == "mesh0"))
    );
    assert!(
        !actions.iter().any(
            |action| matches!(action, Action::Send { interface, .. } if interface == "wired0")
        )
    );
}

#[test]
fn route_requires_neighbour_and_exports_generation() {
    let mut engine = Engine::new(config(id(1)));
    engine.handle(Event::InterfaceUp {
        interface: "wg0".into(),
        local_addresses: vec![],
        now_ms: 0,
    });
    engine.handle(Event::PacketReceived {
        interface: "wg0".into(),
        source: "fe80::2".parse().unwrap(),
        now_ms: 10,
        packet: Packet {
            tlvs: vec![
                Tlv::Hello {
                    unicast: false,
                    seqno: 1,
                    interval_cs: 400,
                    sub_tlvs: vec![],
                },
                Tlv::Ihu {
                    address: None,
                    rxcost: 96,
                    interval_cs: 1200,
                    sub_tlvs: vec![],
                },
            ],
        },
    });
    let actions = engine.handle(Event::PacketReceived {
        interface: "wg0".into(),
        source: "fe80::2".parse().unwrap(),
        now_ms: 20,
        packet: Packet {
            tlvs: vec![Tlv::Update(ResolvedUpdate {
                key: Some(key()),
                router_id: Some(id(2)),
                next_hop: Some("fe80::2".parse().unwrap()),
                interval_cs: 1600,
                seqno: 7,
                metric: 0,
                v4_via_v6: false,
                sub_tlvs: vec![],
            })],
        },
    });
    assert!(actions.iter().any(|action| matches!(action, Action::RoutesChanged { routes, .. } if routes.len() == 1 && routes[0].metric == 96)));
}

#[test]
fn unfeasible_alternate_is_not_acquired() {
    let mut engine = Engine::new(EngineConfig {
        ipv4_via_ipv6: true,
        route_policy: Arc::new(AllowAllRoutes),
        limits: crate::ResourceLimits::default(),
        router_id: id(1),
        metric: Arc::new(WiredMetric::new(96, 1, 1).unwrap()),
        metric_algebra: Arc::new(AdditiveMetric),
        sequence_number: 0,
        hello_interval_cs: 400,
        update_interval_cs: 1600,
        route_selection: RouteSelectionConfig::default(),
    });
    engine.handle(Event::InterfaceUp {
        interface: "wg0".into(),
        local_addresses: vec![],
        now_ms: 0,
    });
    // The first selected route is advertised on this second interface,
    // which establishes the RFC feasibility distance at 10 + 96.
    engine.handle(Event::InterfaceUp {
        interface: "wg-out".into(),
        local_addresses: vec![],
        now_ms: 0,
    });
    let source = "fe80::2".parse().unwrap();
    engine.handle(Event::PacketReceived {
        interface: "wg0".into(),
        source,
        now_ms: 1,
        packet: Packet {
            tlvs: vec![
                Tlv::Hello {
                    unicast: false,
                    seqno: 1,
                    interval_cs: 400,
                    sub_tlvs: vec![],
                },
                Tlv::Ihu {
                    address: None,
                    rxcost: 96,
                    interval_cs: 1200,
                    sub_tlvs: vec![],
                },
                Tlv::Update(ResolvedUpdate {
                    key: Some(key()),
                    router_id: Some(id(2)),
                    next_hop: Some(source),
                    interval_cs: 1600,
                    seqno: 5,
                    metric: 10,
                    v4_via_v6: false,
                    sub_tlvs: vec![],
                }),
            ],
        },
    });
    engine.handle(Event::InterfaceDown {
        interface: "wg0".into(),
        now_ms: 2,
    });
    engine.handle(Event::InterfaceUp {
        interface: "wg1".into(),
        local_addresses: vec![],
        now_ms: 3,
    });
    let other = "fe80::3".parse().unwrap();
    engine.handle(Event::PacketReceived {
        interface: "wg1".into(),
        source: other,
        now_ms: 4,
        packet: Packet {
            tlvs: vec![
                Tlv::Hello {
                    unicast: false,
                    seqno: 1,
                    interval_cs: 400,
                    sub_tlvs: vec![],
                },
                Tlv::Ihu {
                    address: None,
                    rxcost: 96,
                    interval_cs: 1200,
                    sub_tlvs: vec![],
                },
                Tlv::Update(ResolvedUpdate {
                    key: Some(key()),
                    router_id: Some(id(2)),
                    next_hop: Some(other),
                    interval_cs: 1600,
                    seqno: 5,
                    metric: 120,
                    v4_via_v6: false,
                    sub_tlvs: vec![],
                }),
            ],
        },
    });
    assert!(engine.selected_routes().is_empty());
}

#[test]
fn withdrawal_keeps_sequence_and_sends_infinity() {
    let mut engine = Engine::new(config(id(1)));
    engine.handle(Event::InterfaceUp {
        interface: "wg0".into(),
        local_addresses: vec![],
        now_ms: 0,
    });
    engine.handle(Event::Originate {
        key: key(),
        metric: 0,
        now_ms: 1,
    });
    let actions = engine.handle(Event::Withdraw {
        key: key(),
        now_ms: 2,
    });
    assert!(
        !actions
            .iter()
            .any(|action| matches!(action, Action::SequenceNumberChanged(_)))
    );
    assert!(actions.iter().any(|action| matches!(
        action,
        Action::Send { packet, .. }
            if packet.tlvs.iter().any(|tlv| matches!(tlv, OutboundTlv::Update(update) if update.metric == INFINITY && update.seqno == 0))
    )));
}

#[test]
fn learned_route_expires_and_is_retracted_from_selected_snapshot() {
    let mut engine = Engine::new(config(id(1)));
    engine.handle(Event::InterfaceUp {
        interface: "wg0".into(),
        local_addresses: vec![],
        now_ms: 0,
    });
    engine.handle(Event::InterfaceUp {
        interface: "wg1".into(),
        local_addresses: vec![],
        now_ms: 0,
    });
    let source = "fe80::2".parse().unwrap();
    engine.handle(Event::PacketReceived {
        interface: "wg0".into(),
        source,
        now_ms: 1,
        packet: Packet {
            tlvs: vec![
                Tlv::Hello {
                    unicast: false,
                    seqno: 1,
                    interval_cs: 400,
                    sub_tlvs: vec![],
                },
                Tlv::Ihu {
                    address: None,
                    rxcost: 96,
                    interval_cs: 1200,
                    sub_tlvs: vec![],
                },
                Tlv::Update(ResolvedUpdate {
                    key: Some(key()),
                    router_id: Some(id(2)),
                    next_hop: Some(source),
                    interval_cs: 100,
                    seqno: 7,
                    metric: 0,
                    v4_via_v6: false,
                    sub_tlvs: vec![],
                }),
            ],
        },
    });
    assert_eq!(engine.selected_routes().len(), 1);
    let actions = engine.handle(Event::Tick { now_ms: 3502 });
    assert!(engine.selected_routes().is_empty());
    assert!(
        actions.iter().any(
            |action| matches!(action, Action::RoutesChanged { routes, .. } if routes.is_empty())
        )
    );
    assert!(actions.iter().any(|action| matches!(
        action,
        Action::Send { interface, packet, .. }
            if interface == "wg1" && packet.tlvs.iter().any(|tlv| matches!(tlv, OutboundTlv::Update(update) if update.metric == INFINITY))
    )));
}

#[test]
fn retraction_only_retracts_the_route_from_its_neighbour() {
    let mut engine = Engine::new(config(id(1)));
    for (interface, source, metric) in [("wg0", "fe80::2", 0), ("wg1", "fe80::3", 10)] {
        engine.handle(Event::InterfaceUp {
            interface: interface.into(),
            local_addresses: vec![],
            now_ms: 0,
        });
        let source = source.parse().unwrap();
        engine.handle(Event::PacketReceived {
            interface: interface.into(),
            source,
            now_ms: 1,
            packet: Packet {
                tlvs: vec![
                    Tlv::Hello {
                        unicast: false,
                        seqno: 1,
                        interval_cs: 400,
                        sub_tlvs: vec![],
                    },
                    Tlv::Ihu {
                        address: None,
                        rxcost: 96,
                        interval_cs: 1200,
                        sub_tlvs: vec![],
                    },
                    Tlv::Update(ResolvedUpdate {
                        key: Some(key()),
                        router_id: Some(id(2)),
                        next_hop: Some(source),
                        interval_cs: 1600,
                        seqno: 7,
                        metric,
                        v4_via_v6: false,
                        sub_tlvs: vec![],
                    }),
                ],
            },
        });
    }
    assert_eq!(engine.selected_routes().len(), 1);
    let actions = engine.handle(Event::PacketReceived {
        interface: "wg0".into(),
        source: "fe80::2".parse().unwrap(),
        now_ms: 2,
        packet: Packet {
            tlvs: vec![Tlv::Update(ResolvedUpdate {
                key: Some(key()),
                router_id: Some(id(2)),
                next_hop: Some("fe80::2".parse().unwrap()),
                interval_cs: 1600,
                seqno: 8,
                metric: INFINITY,
                v4_via_v6: false,
                sub_tlvs: vec![],
            })],
        },
    });
    assert_eq!(engine.selected_routes().len(), 1);
    assert_eq!(engine.selected_routes()[0].interface, "wg1");
    assert!(actions.iter().any(|action| matches!(
        action,
        Action::RoutesChanged { routes, .. }
            if routes.len() == 1 && routes[0].interface == "wg1"
    )));
}

#[test]
fn seqno_request_increments_local_origin_at_most_once() {
    let mut config = config(id(1));
    config.sequence_number = 7;
    let mut engine = Engine::new(config);
    engine.handle(Event::InterfaceUp {
        interface: "wg0".into(),
        local_addresses: vec![],
        now_ms: 0,
    });
    engine.handle(Event::Originate {
        key: key(),
        metric: 0,
        now_ms: 1,
    });
    let actions = engine.handle(Event::PacketReceived {
        interface: "wg0".into(),
        source: "fe80::2".parse().unwrap(),
        now_ms: 2,
        packet: Packet {
            tlvs: vec![Tlv::SeqnoRequest {
                key: key(),
                seqno: 100,
                hop_count: 16,
                router_id: id(1),
                sub_tlvs: vec![],
            }],
        },
    });
    assert!(
        actions
            .iter()
            .any(|action| matches!(action, Action::SequenceNumberChanged(8)))
    );
    assert!(actions.iter().any(|action| matches!(
        action,
        Action::Send { packet, .. }
            if packet.tlvs.iter().any(|tlv| matches!(tlv, OutboundTlv::Update(update) if update.seqno == 8))
    )));
}

#[test]
fn seqno_request_is_forwarded_towards_remote_origin() {
    let mut engine = Engine::new(config(id(1)));
    for interface in ["upstream", "downstream"] {
        engine.handle(Event::InterfaceUp {
            interface: interface.into(),
            local_addresses: vec![],
            now_ms: 0,
        });
    }
    let source = "fe80::2".parse().unwrap();
    engine.handle(Event::PacketReceived {
        interface: "upstream".into(),
        source,
        now_ms: 1,
        packet: Packet {
            tlvs: vec![
                Tlv::Hello {
                    unicast: false,
                    seqno: 1,
                    interval_cs: 400,
                    sub_tlvs: vec![],
                },
                Tlv::Ihu {
                    address: None,
                    rxcost: 96,
                    interval_cs: 1200,
                    sub_tlvs: vec![],
                },
                Tlv::Update(ResolvedUpdate {
                    key: Some(key()),
                    router_id: Some(id(3)),
                    next_hop: Some(source),
                    interval_cs: 1600,
                    seqno: 5,
                    metric: 0,
                    v4_via_v6: false,
                    sub_tlvs: vec![],
                }),
            ],
        },
    });
    let actions = engine.handle(Event::PacketReceived {
        interface: "downstream".into(),
        source: "fe80::4".parse().unwrap(),
        now_ms: 2,
        packet: Packet {
            tlvs: vec![Tlv::SeqnoRequest {
                key: key(),
                seqno: 6,
                hop_count: 7,
                router_id: id(3),
                sub_tlvs: vec![],
            }],
        },
    });
    assert!(actions.iter().any(|action| matches!(
        action,
        Action::Send { interface, packet, .. }
            if interface == "upstream" && packet.tlvs.iter().any(|tlv| matches!(tlv, OutboundTlv::SeqnoRequest { seqno: 6, hop_count: 6, .. }))
    )));
}

#[test]
fn default_wired_metric_activates_a_stored_update_after_second_hello() {
    let mut engine = Engine::new(EngineConfig::recommended(id(1)));
    engine.handle(Event::InterfaceUp {
        interface: "eth0".into(),
        local_addresses: vec![],
        now_ms: 0,
    });
    let source = "fe80::2".parse().unwrap();
    engine.handle(Event::PacketReceived {
        interface: "eth0".into(),
        source,
        now_ms: 10,
        packet: Packet {
            tlvs: vec![
                Tlv::Hello {
                    unicast: false,
                    seqno: 1,
                    interval_cs: 400,
                    sub_tlvs: vec![],
                },
                Tlv::Ihu {
                    address: None,
                    rxcost: 96,
                    interval_cs: 1200,
                    sub_tlvs: vec![],
                },
                Tlv::Update(ResolvedUpdate {
                    key: Some(key()),
                    router_id: Some(id(2)),
                    next_hop: Some(source),
                    interval_cs: 1600,
                    seqno: 1,
                    metric: 0,
                    v4_via_v6: false,
                    sub_tlvs: vec![],
                }),
            ],
        },
    });
    assert!(engine.selected_routes().is_empty());
    engine.handle(Event::PacketReceived {
        interface: "eth0".into(),
        source,
        now_ms: 4_000,
        packet: Packet {
            tlvs: vec![Tlv::Hello {
                unicast: false,
                seqno: 2,
                interval_cs: 400,
                sub_tlvs: vec![],
            }],
        },
    });
    assert_eq!(engine.selected_routes()[0].metric, 96);
}

#[test]
fn ihu_change_recomputes_an_existing_candidate_without_an_update() {
    let mut engine = Engine::new(config(id(1)));
    engine.handle(Event::InterfaceUp {
        interface: "eth0".into(),
        local_addresses: vec![],
        now_ms: 0,
    });
    let source = "fe80::2".parse().unwrap();
    engine.handle(Event::PacketReceived {
        interface: "eth0".into(),
        source,
        now_ms: 10,
        packet: Packet {
            tlvs: vec![
                Tlv::Hello {
                    unicast: false,
                    seqno: 1,
                    interval_cs: 400,
                    sub_tlvs: vec![],
                },
                Tlv::Ihu {
                    address: None,
                    rxcost: 96,
                    interval_cs: 1200,
                    sub_tlvs: vec![],
                },
                Tlv::Update(ResolvedUpdate {
                    key: Some(key()),
                    router_id: Some(id(2)),
                    next_hop: Some(source),
                    interval_cs: 1600,
                    seqno: 1,
                    metric: 0,
                    v4_via_v6: false,
                    sub_tlvs: vec![],
                }),
            ],
        },
    });
    assert_eq!(engine.selected_routes()[0].metric, 96);
    engine.handle(Event::PacketReceived {
        interface: "eth0".into(),
        source,
        now_ms: 20,
        packet: Packet {
            tlvs: vec![Tlv::Ihu {
                address: None,
                rxcost: 200,
                interval_cs: 1200,
                sub_tlvs: vec![],
            }],
        },
    });
    assert_eq!(engine.selected_routes()[0].metric, 200);
}

#[test]
fn rfc9616_timestamp_exchange_feeds_the_rtt_metric() {
    let mut config = config(id(1));
    config.metric = Arc::new(crate::metric::RttMetric::recommended(Arc::new(
        WiredMetric::new(96, 1, 1).unwrap(),
    )));
    let mut engine = Engine::new(config);
    let actions = engine.handle(Event::InterfaceUp {
        interface: "eth0".into(),
        local_addresses: vec![],
        now_ms: 0,
    });
    assert!(actions.iter().any(|action| matches!(
        action,
        Action::Send { packet, .. }
            if packet.tlvs.iter().any(|tlv| matches!(tlv, OutboundTlv::Hello { sub_tlvs, .. }
                if sub_tlvs.contains(&SubTlv::TimestampHello(0))))
    )));
    let source = "fe80::2".parse().unwrap();
    engine.handle(Event::PacketReceived {
        interface: "eth0".into(),
        source,
        now_ms: 50,
        packet: Packet {
            tlvs: vec![
                Tlv::Hello {
                    unicast: false,
                    seqno: 1,
                    interval_cs: 400,
                    sub_tlvs: vec![SubTlv::TimestampHello(40_000)],
                },
                Tlv::Ihu {
                    address: None,
                    rxcost: 96,
                    interval_cs: 1200,
                    sub_tlvs: vec![SubTlv::TimestampIhu {
                        origin: 0,
                        received: 10_000,
                    }],
                },
                Tlv::Update(ResolvedUpdate {
                    key: Some(key()),
                    router_id: Some(id(2)),
                    next_hop: Some(source),
                    interval_cs: 1600,
                    seqno: 1,
                    metric: 0,
                    v4_via_v6: false,
                    sub_tlvs: vec![],
                }),
            ],
        },
    });
    let status = engine.neighbour_status(50).pop().unwrap();
    assert_eq!(status.last_rtt_us, Some(20_000));
    assert_eq!(status.smoothed_rtt_us, Some(20_000));
    assert_eq!(status.rtt_penalty, 14);
    assert_eq!(status.link_cost, 110);
    assert_eq!(engine.selected_routes()[0].metric, 110);
}

#[test]
fn rtt_probe_runs_independently_of_the_regular_ihu_interval() {
    let mut config = config(id(1));
    config.metric = Arc::new(crate::metric::RttMetric::recommended(Arc::new(
        WiredMetric::new(96, 1, 1).unwrap(),
    )));
    let mut engine = Engine::new(config);
    engine.handle(Event::InterfaceUp {
        interface: "eth0".into(),
        local_addresses: vec![],
        now_ms: 0,
    });
    let actions = engine.handle(Event::PacketReceived {
        interface: "eth0".into(),
        source: "fe80::2".parse().unwrap(),
        now_ms: 10,
        packet: Packet {
            tlvs: vec![Tlv::Hello {
                unicast: false,
                seqno: 1,
                interval_cs: 400,
                sub_tlvs: vec![SubTlv::TimestampHello(0)],
            }],
        },
    });
    let deadline = engine
        .neighbours
        .values()
        .next()
        .and_then(|neighbour| neighbour.next_rtt_probe_ms)
        .unwrap();
    if deadline > 10 {
        // RFC 9616 requires an echoed Timestamp IHU to share a packet
        // with a timestamped Hello, independently of probe scheduling.
        assert!(contains_unicast_timestamp_hello(&actions));
        assert!(contains_unicast_timestamp_hello(
            &engine.handle(Event::Tick { now_ms: deadline })
        ));
    }
    let next_deadline = engine
        .neighbours
        .values()
        .next()
        .and_then(|neighbour| neighbour.next_rtt_probe_ms)
        .unwrap();
    assert!(!contains_unicast_timestamp_hello(&engine.handle(
        Event::Tick {
            now_ms: next_deadline - 1
        }
    )));
    assert!(contains_unicast_timestamp_hello(&engine.handle(
        Event::Tick {
            now_ms: next_deadline
        }
    )));
}

#[test]
fn source_entry_is_maintained_on_advertisement_and_garbage_collected() {
    let mut engine = Engine::new(config(id(1)));
    for interface in ["in", "out"] {
        engine.handle(Event::InterfaceUp {
            interface: interface.into(),
            local_addresses: vec![],
            now_ms: 0,
        });
    }
    let source = "fe80::2".parse().unwrap();
    engine.handle(Event::PacketReceived {
        interface: "in".into(),
        source,
        now_ms: 10,
        packet: Packet {
            tlvs: vec![
                Tlv::Hello {
                    unicast: false,
                    seqno: 1,
                    interval_cs: 400,
                    sub_tlvs: vec![],
                },
                Tlv::Ihu {
                    address: None,
                    rxcost: 96,
                    interval_cs: 1200,
                    sub_tlvs: vec![],
                },
                Tlv::Update(ResolvedUpdate {
                    key: Some(key()),
                    router_id: Some(id(2)),
                    next_hop: Some(source),
                    interval_cs: 1600,
                    seqno: 5,
                    metric: 10,
                    v4_via_v6: false,
                    sub_tlvs: vec![],
                }),
            ],
        },
    });
    let source_key = (key(), id(2));
    assert_eq!(
        engine.feasible.get(&source_key).map(|entry| entry.distance),
        Some(Distance {
            seqno: 5,
            metric: 106
        })
    );
    engine.handle(Event::PacketReceived {
        interface: "in".into(),
        source,
        now_ms: 100,
        packet: Packet {
            tlvs: vec![Tlv::Update(ResolvedUpdate {
                key: Some(key()),
                router_id: Some(id(2)),
                next_hop: None,
                interval_cs: 1600,
                seqno: 6,
                metric: INFINITY,
                v4_via_v6: false,
                sub_tlvs: vec![],
            })],
        },
    });
    assert_eq!(
        engine.feasible.get(&source_key).unwrap().expires_ms,
        10 + SOURCE_GC_TIME_MS
    );
    engine.handle(Event::Tick {
        now_ms: 11 + SOURCE_GC_TIME_MS,
    });
    assert!(!engine.feasible.contains_key(&source_key));
}

#[test]
fn replacing_origins_without_metric_increase_keeps_sequence() {
    let mut engine = Engine::new(config(id(1)));
    engine.handle(Event::InterfaceUp {
        interface: "wg0".into(),
        local_addresses: vec![],
        now_ms: 0,
    });
    let old = key();
    engine.handle(Event::Originate {
        key: old,
        metric: 0,
        now_ms: 1,
    });
    let new = RouteKey::new("2001:db8:1::/64".parse().unwrap(), None).unwrap();
    let actions = engine.handle(Event::ReplaceOrigins {
        origins: BTreeMap::from([(new, 7)]),
        now_ms: 2,
    });
    assert_eq!(
        actions
            .iter()
            .filter(|action| matches!(action, Action::SequenceNumberChanged(_)))
            .count(),
        0
    );
    assert!(actions.iter().any(|action| matches!(
        action,
        Action::Send { packet, .. }
            if packet.tlvs.iter().any(|tlv| matches!(tlv, OutboundTlv::Update(update)
                if update.key == Some(old) && update.metric == INFINITY))
    )));
    assert_eq!(
        engine.originated.keys().copied().collect::<Vec<_>>(),
        vec![new]
    );
}

#[test]
fn better_route_must_clear_margin_for_the_full_dwell_time() {
    let mut engine = Engine::new(config(id(1)));
    let route_key = key();
    insert_candidate(&mut engine, route_key, id(2), "eth0", "fe80::2", 200);
    insert_candidate(&mut engine, route_key, id(3), "eth1", "fe80::3", 180);
    let current = engine
        .candidates
        .values()
        .find(|candidate| candidate.interface == "eth0")
        .map(selected_from_candidate)
        .unwrap();
    engine.selected.insert(route_key, current);
    engine.settled_routes.insert(route_key);

    engine.reselect(0);
    assert_eq!(engine.selected_routes()[0].interface, "eth0");
    engine.reselect(7_999);
    assert_eq!(engine.selected_routes()[0].interface, "eth0");
    engine.reselect(8_000);
    assert_eq!(engine.selected_routes()[0].interface, "eth1");
}

#[test]
fn margin_loss_resets_dwell_but_current_route_loss_switches_immediately() {
    let mut engine = Engine::new(config(id(1)));
    let route_key = key();
    insert_candidate(&mut engine, route_key, id(2), "eth0", "fe80::2", 200);
    insert_candidate(&mut engine, route_key, id(3), "eth1", "fe80::3", 180);
    let current = engine
        .candidates
        .values()
        .find(|candidate| candidate.interface == "eth0")
        .map(selected_from_candidate)
        .unwrap();
    engine.selected.insert(route_key, current);
    engine.settled_routes.insert(route_key);

    engine.reselect(0);
    engine
        .candidates
        .values_mut()
        .find(|candidate| candidate.interface == "eth1")
        .unwrap()
        .metric = 195;
    engine.reselect(4_000);
    engine
        .candidates
        .values_mut()
        .find(|candidate| candidate.interface == "eth1")
        .unwrap()
        .metric = 180;
    engine.reselect(5_000);
    engine.reselect(12_999);
    assert_eq!(engine.selected_routes()[0].interface, "eth0");

    engine
        .candidates
        .retain(|(_, neighbour), _| neighbour.interface != "eth0");
    engine.reselect(13_000);
    assert_eq!(engine.selected_routes()[0].interface, "eth1");
}

#[test]
fn initial_candidate_discovery_is_not_delayed_by_hysteresis() {
    let mut engine = Engine::new(config(id(1)));
    let route_key = key();
    insert_candidate(&mut engine, route_key, id(2), "eth0", "fe80::2", 200);
    let current = engine
        .candidates
        .values()
        .find(|candidate| candidate.interface == "eth0")
        .map(selected_from_candidate)
        .unwrap();
    engine.selected.insert(route_key, current);
    engine.settling_since.insert(route_key, 0);
    insert_candidate(&mut engine, route_key, id(3), "eth1", "fe80::3", 180);

    engine.reselect(1_000);
    assert_eq!(engine.selected_routes()[0].interface, "eth1");
    assert_eq!(engine.settling_since[&route_key], 1_000);
    assert!(!engine.settled_routes.contains(&route_key));
}

#[test]
fn meaningful_current_route_recovery_cancels_a_stale_switch() {
    let mut engine = Engine::new(config(id(1)));
    let route_key = key();
    insert_candidate(&mut engine, route_key, id(2), "eth0", "fe80::2", 200);
    insert_candidate(&mut engine, route_key, id(3), "eth1", "fe80::3", 180);
    let current = engine
        .candidates
        .values()
        .find(|candidate| candidate.interface == "eth0")
        .map(selected_from_candidate)
        .unwrap();
    engine.selected.insert(route_key, current);
    engine.settled_routes.insert(route_key);

    engine.reselect(0);
    engine
        .candidates
        .values_mut()
        .find(|candidate| candidate.interface == "eth0")
        .unwrap()
        .metric = 240;
    engine.reselect(3_000);
    engine
        .candidates
        .values_mut()
        .find(|candidate| candidate.interface == "eth0")
        .unwrap()
        .metric = 220;
    engine.reselect(7_000);
    assert_eq!(engine.selected_routes()[0].interface, "eth0");

    engine.reselect(8_000);
    engine.reselect(15_999);
    assert_eq!(engine.selected_routes()[0].interface, "eth0");
    engine.reselect(16_000);
    assert_eq!(engine.selected_routes()[0].interface, "eth1");
}

fn contains_unicast_timestamp_hello(actions: &[Action]) -> bool {
    actions.iter().any(|action| matches!(
        action,
        Action::Send { packet, .. }
            if packet.tlvs.iter().any(|tlv| matches!(tlv,
                OutboundTlv::Hello { unicast: true, sub_tlvs, .. }
                    if sub_tlvs.iter().any(|sub_tlv| matches!(sub_tlv, SubTlv::TimestampHello(_)))
            ))
    ))
}

fn insert_candidate(
    engine: &mut Engine,
    route_key: RouteKey,
    router_id: RouterId,
    interface: &str,
    next_hop: &str,
    metric: u16,
) {
    let neighbour = NeighborKey {
        interface: interface.into(),
        address: next_hop.parse().unwrap(),
    };
    engine.candidates.insert(
        (route_key, neighbour),
        Candidate {
            key: route_key,
            router_id,
            seqno: 1,
            advertised_metric: 0,
            metric,
            next_hop: next_hop.parse().unwrap(),
            interface: interface.into(),
            interval_cs: 400,
            expires_ms: u64::MAX,
            refresh_requested: false,
        },
    );
}
