//! Regressions from the RFC audit. These assert protocol behavior, not the old defects.
use babel_protocol::*;
use std::{net::IpAddr, sync::Arc};

fn id(n: u8) -> RouterId {
    RouterId::new([n; 8]).unwrap()
}
fn key() -> RouteKey {
    RouteKey::new("2001:db8:42::/64".parse().unwrap(), None).unwrap()
}
fn engine(split: bool) -> Engine {
    let mut config = EngineConfig::recommended(id(1));
    config.route_selection.better_for_ms = 0;
    let mut e = Engine::new(config);
    e.handle(Event::InterfaceUpWithPolicy {
        interface: "lan".into(),
        local_addresses: vec!["fe80::1".parse().unwrap()],
        now_ms: 0,
        policy: InterfacePolicy {
            control_transport: Default::default(),
            ipv4_next_hop: Ipv4NextHop::Auto,
            metric: Arc::new(WiredMetric::new(96, 1, 1).unwrap()),
            hello_interval_cs: 400,
            update_interval_cs: 1600,
            split_horizon: split,
        },
    });
    e
}
fn receive(e: &mut Engine, source: &str, tlvs: Vec<OutboundTlv>, now_ms: u64) -> Vec<Action> {
    let source: IpAddr = source.parse().unwrap();
    let bytes = encode_packet(&OutboundPacket { tlvs }).unwrap();
    let packet = decode_packet(&bytes, DecodeContext { source }).unwrap();
    e.handle(Event::PacketReceived {
        interface: "lan".into(),
        source,
        packet,
        now_ms,
    })
}
fn hello() -> OutboundTlv {
    OutboundTlv::Hello {
        unicast: false,
        seqno: 1,
        interval_cs: 400,
        sub_tlvs: vec![],
    }
}
fn learn(
    e: &mut Engine,
    source: &str,
    rid: u8,
    metric: u16,
    next_hop: Option<IpAddr>,
    now_ms: u64,
) -> Vec<Action> {
    receive(
        e,
        source,
        vec![
            hello(),
            OutboundTlv::Ihu {
                address: None,
                rxcost: 96,
                interval_cs: 1200,
                sub_tlvs: vec![],
            },
            OutboundTlv::Update(OutboundUpdate {
                key: Some(key()),
                router_id: Some(id(rid)),
                next_hop,
                interval_cs: 1600,
                seqno: 1,
                metric,
                v4_via_v6: false,
                sub_tlvs: vec![],
            }),
        ],
        now_ms,
    )
}
fn updates(actions: &[Action]) -> Vec<&OutboundUpdate> {
    actions
        .iter()
        .filter_map(|a| {
            if let Action::Send { packet, .. } = a {
                Some(packet)
            } else {
                None
            }
        })
        .flat_map(|p| &p.tlvs)
        .filter_map(|t| {
            if let OutboundTlv::Update(u) = t {
                Some(u)
            } else {
                None
            }
        })
        .collect()
}
fn seqreq(rid: u8) -> OutboundTlv {
    OutboundTlv::SeqnoRequest {
        key: key(),
        seqno: 1,
        hop_count: 64,
        router_id: id(rid),
        sub_tlvs: vec![],
    }
}

#[test]
fn specific_request_on_split_horizon_interface_returns_selected_route() {
    let mut e = engine(true);
    learn(&mut e, "fe80::2", 2, 0, None, 10);
    assert_eq!(e.selected_routes().len(), 1);
    let a = receive(
        &mut e,
        "fe80::3",
        vec![
            hello(),
            OutboundTlv::RouteRequest {
                key: Some(key()),
                sub_tlvs: vec![],
            },
        ],
        20,
    );
    assert!(updates(&a).iter().any(|u| u.metric < INFINITY));
}

#[test]
fn seqno_request_for_different_origin_answers_with_local_origin() {
    let mut e = engine(false);
    e.handle(Event::Originate {
        key: key(),
        metric: 0,
        now_ms: 1,
    });
    let a = receive(&mut e, "fe80::3", vec![hello(), seqreq(9)], 20);
    assert!(
        updates(&a)
            .iter()
            .any(|u| u.router_id == Some(id(1)) && u.metric == 0)
    );
}

#[test]
fn unicast_reply_records_source_feasibility() {
    let mut e = engine(true);
    learn(&mut e, "fe80::2", 2, 0, None, 10);
    assert_eq!(e.resource_status().sources, 0);
    let a = receive(&mut e, "fe80::3", vec![hello(), seqreq(2)], 20);
    assert!(updates(&a).iter().any(|u| u.metric < INFINITY));
    assert_eq!(e.resource_status().sources, 1);
}

#[test]
fn selected_origin_switch_is_urgent() {
    let mut e = engine(false);
    learn(&mut e, "fe80::2", 2, 200, None, 10);
    let a = learn(&mut e, "fe80::3", 3, 0, None, 20);
    assert_eq!(e.selected_routes()[0].router_id, id(3));
    let deadlines: Vec<_> =
        a.iter()
            .filter_map(|a| match a {
                Action::Send { packet, timing, .. }
                    if packet.tlvs.iter().any(
                        |t| matches!(t, OutboundTlv::Update(u) if u.router_id == Some(id(3))),
                    ) =>
                {
                    Some(timing.deadline_ms - 20)
                }
                _ => None,
            })
            .collect();
    assert_eq!(deadlines, vec![SendTiming::urgent(20).deadline_ms - 20]);
}

#[test]
fn timestamped_ihu_keeps_hello_at_mtu_boundary() {
    let mut tlvs: Vec<_> = (0..300).map(|nonce| OutboundTlv::Ack { nonce }).collect();
    tlvs.push(OutboundTlv::Hello {
        unicast: true,
        seqno: 1,
        interval_cs: 0,
        sub_tlvs: vec![SubTlv::TimestampHello(1000)],
    });
    tlvs.push(OutboundTlv::Ihu {
        address: None,
        rxcost: 96,
        interval_cs: 1200,
        sub_tlvs: vec![SubTlv::TimestampIhu {
            origin: 1,
            received: 2,
        }],
    });
    let packets = encode_packets(&OutboundPacket { tlvs }, 1232).unwrap();
    let mut split = false;
    for bytes in &packets {
        let p = decode_packet(
            bytes,
            DecodeContext {
                source: "fe80::2".parse().unwrap(),
            },
        )
        .unwrap();
        let has_ihu = p.tlvs.iter().any(|t| matches!(t, Tlv::Ihu { sub_tlvs, .. } if sub_tlvs.iter().any(|s| matches!(s, SubTlv::TimestampIhu { .. }))));
        let has_hello = p.tlvs.iter().any(|t| matches!(t, Tlv::Hello { .. }));
        split |= has_ihu && !has_hello;
    }
    assert!(!split);
}

#[test]
fn missing_compression_context_ignores_only_update() {
    let mut body = vec![8, 17, 2, 0, 64, 1, 6, 64, 0, 1, 0, 96];
    body.extend([0; 7]);
    let good = encode_packet(&OutboundPacket {
        tlvs: vec![hello()],
    })
    .unwrap();
    body.extend_from_slice(&good[4..]);
    let mut bytes = vec![42, 2];
    bytes.extend((body.len() as u16).to_be_bytes());
    bytes.extend(body);
    let decoded = decode_packet(
        &bytes,
        DecodeContext {
            source: "fe80::2".parse().unwrap(),
        },
    );
    assert!(
        decoded
            .unwrap()
            .tlvs
            .iter()
            .any(|t| matches!(t, Tlv::Hello { .. }))
    );
}

#[test]
fn short_default_prefix_can_supply_zero_extended_compression() {
    // A /64 default prefix is eight stored bytes; the following /128 asks
    // to reuse nine bytes. A decoder must handle this without slicing panic.
    let mut body = vec![8, 18, 2, 128, 64, 0, 6, 64, 0, 1, 255, 255];
    body.extend([0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0]);
    body.extend([8, 17, 2, 0, 128, 9, 6, 64, 0, 1, 255, 255]);
    body.extend([0; 7]);
    let mut bytes = vec![42, 2];
    bytes.extend((body.len() as u16).to_be_bytes());
    bytes.extend(body);
    let decoded = decode_packet(
        &bytes,
        DecodeContext {
            source: "fe80::2".parse().unwrap(),
        },
    )
    .unwrap();
    let updates: Vec<_> = decoded
        .tlvs
        .iter()
        .filter_map(|t| {
            if let Tlv::Update(u) = t {
                Some(u)
            } else {
                None
            }
        })
        .collect();
    assert_eq!(updates.len(), 2);
    assert_eq!(
        updates[1].key.unwrap().destination.to_string(),
        "2001:db8::/128"
    );
}

#[test]
fn ipv4_control_without_ipv6_announces_ordinary_ipv4() {
    let mut e = engine(false);
    e.handle(Event::InterfaceDown {
        interface: "lan".into(),
        now_ms: 1,
    });
    let initial = e.handle(Event::InterfaceUpWithPolicy {
        interface: "lan".into(),
        local_addresses: vec!["192.0.2.1".parse().unwrap()],
        now_ms: 2,
        policy: InterfacePolicy {
            control_transport: ControlTransport::Ipv4,
            ipv4_next_hop: Ipv4NextHop::Auto,
            metric: Arc::new(WiredMetric::default()),
            hello_interval_cs: 400,
            update_interval_cs: 1600,
            split_horizon: true,
        },
    });
    assert!(initial.iter().all(|a| !matches!(a, Action::Send { destination, .. } if *destination != "224.0.0.111".parse::<IpAddr>().unwrap())));
    receive(&mut e, "fe80::2", vec![hello()], 2);
    assert_eq!(e.neighbour_count(), 0);
    receive(&mut e, "192.0.2.2", vec![hello()], 2);
    assert_eq!(e.neighbour_count(), 1);
    let v4 = RouteKey::new("198.51.100.0/24".parse().unwrap(), None).unwrap();
    let actions = e.handle(Event::Originate {
        key: v4,
        metric: 0,
        now_ms: 3,
    });
    assert!(updates(&actions).iter().any(|u| u.metric == 0
        && !u.v4_via_v6
        && u.next_hop == Some("192.0.2.1".parse().unwrap())));
    let v6 = e.handle(Event::Originate {
        key: key(),
        metric: 0,
        now_ms: 4,
    });
    assert!(updates(&v6).iter().all(|u| u.metric == INFINITY));
    let changed = e.handle(Event::InterfaceAddressesChanged {
        interface: "lan".into(),
        local_addresses: vec!["192.0.2.1".parse().unwrap(), "fe80::1".parse().unwrap()],
        now_ms: 5,
    });
    assert!(updates(&changed).iter().any(|u| u.key == Some(key())
        && u.metric == 0
        && u.next_hop == Some("fe80::1".parse().unwrap())));
}

#[test]
fn withdrawal_hold_uses_outbound_interval_and_repeats_current_state() {
    let mut e = engine(false);
    e.handle(Event::Originate {
        key: key(),
        metric: 0,
        now_ms: 1,
    });
    let withdrawn = e.handle(Event::Withdraw {
        key: key(),
        now_ms: 2,
    });
    assert!(updates(&withdrawn).iter().any(|u| u.metric == INFINITY));
    assert_eq!(e.unreachable_routes(), vec![key()]);
    let repeated = e.handle(Event::Tick { now_ms: 1002 });
    assert!(updates(&repeated).iter().any(|u| u.metric == INFINITY));
    e.handle(Event::Originate {
        key: key(),
        metric: 0,
        now_ms: 1003,
    });
    let replaced = e.handle(Event::Tick { now_ms: 2003 });
    assert!(updates(&replaced).iter().all(|u| u.metric != INFINITY));
    assert!(e.unreachable_routes().is_empty());
    e.handle(Event::Withdraw {
        key: key(),
        now_ms: 2004,
    });
    e.handle(Event::Tick { now_ms: 59003 });
    assert_eq!(e.unreachable_routes(), vec![key()]);
    let expired = e.handle(Event::Tick { now_ms: 59004 });
    assert!(
        expired.iter().any(
            |a| matches!(a, Action::RoutesChanged { unreachable, .. } if unreachable.is_empty())
        )
    );
    assert!(e.unreachable_routes().is_empty());
}

#[test]
fn sequence_only_and_small_metric_changes_do_not_trigger_updates() {
    let mut e = engine(false);
    learn(&mut e, "fe80::2", 2, 100, None, 1);
    for (seqno, metric) in [(2, 100), (2, 101)] {
        let actions = receive(
            &mut e,
            "fe80::2",
            vec![OutboundTlv::Update(OutboundUpdate {
                key: Some(key()),
                router_id: Some(id(2)),
                next_hop: None,
                interval_cs: 1600,
                seqno,
                metric,
                v4_via_v6: false,
                sub_tlvs: vec![],
            })],
            2 + u64::from(metric),
        );
        assert!(updates(&actions).is_empty());
    }
    // An explicit request still receives the latest sequence and metric.
    let reply = receive(
        &mut e,
        "fe80::3",
        vec![OutboundTlv::RouteRequest {
            key: Some(key()),
            sub_tlvs: vec![],
        }],
        200,
    );
    assert!(
        updates(&reply)
            .iter()
            .any(|u| u.seqno == 2 && u.metric == 197)
    );
}

#[test]
fn structured_compression_matrix_never_panics_and_preserves_following_hello() {
    for (ae, width) in [(1u8, 32u8), (2, 128), (4, 32)] {
        for plen in 0..=width {
            for omitted in 0..=width / 8 + 1 {
                // First a zero default prefix, then a compressed retraction.
                let mut body = vec![8, 10, ae, 128, 0, 0, 6, 64, 0, 1, 255, 255];
                let bytes = plen.div_ceil(8).saturating_sub(omitted);
                body.extend([8, 10 + bytes, ae, 0, plen, omitted, 6, 64, 0, 1, 255, 255]);
                body.extend(vec![0; usize::from(bytes)]);
                body.extend_from_slice(
                    &encode_packet(&OutboundPacket {
                        tlvs: vec![hello()],
                    })
                    .unwrap()[4..],
                );
                let mut packet = vec![42, 2];
                packet.extend((body.len() as u16).to_be_bytes());
                packet.extend(body);
                let decoded = decode_packet(
                    &packet,
                    DecodeContext {
                        source: "fe80::2".parse().unwrap(),
                    },
                );
                // Invalid omitted counts may reject the Update/packet, but must not panic.
                if omitted <= plen.div_ceil(8) {
                    assert!(
                        decoded
                            .unwrap()
                            .tlvs
                            .iter()
                            .any(|t| matches!(t, Tlv::Hello { .. }))
                    );
                }
            }
        }
    }
}

#[test]
fn rtt_default_is_per_sample_but_time_based_override_remains_available() {
    let base: Arc<dyn MetricProfile> = Arc::new(WiredMetric::default());
    for gap in [1, 2000, 6000] {
        let mut metric = RttMetric::recommended(base.clone()).new_neighbor("lan");
        metric.on_rtt_sample(10_000, 0);
        metric.on_rtt_sample(120_000, gap);
        assert_eq!(metric.status().smoothed_rtt_us, Some(28_040));
    }
    let mut metric = RttMetric::new(base, 2000, 6000, 10000, 120000, 150)
        .unwrap()
        .new_neighbor("lan");
    metric.on_rtt_sample(10000, 0);
    metric.on_rtt_sample(120000, 6000);
    assert_eq!(metric.status().smoothed_rtt_us, Some(65000));
}

#[test]
fn exporter_capability_prevents_selecting_ipv4_via_ipv6() {
    let mut config = EngineConfig::recommended(id(1));
    config.ipv4_via_ipv6 = false;
    let mut e = Engine::new(config);
    e.handle(Event::InterfaceUpWithPolicy {
        interface: "lan".into(),
        now_ms: 0,
        local_addresses: vec!["fe80::1".parse().unwrap()],
        policy: InterfacePolicy {
            control_transport: ControlTransport::Ipv6,
            ipv4_next_hop: Ipv4NextHop::Auto,
            metric: Arc::new(WiredMetric::new(96, 1, 1).unwrap()),
            hello_interval_cs: 400,
            update_interval_cs: 1600,
            split_horizon: false,
        },
    });
    let route = RouteKey::new("198.51.100.0/24".parse().unwrap(), None).unwrap();
    receive(
        &mut e,
        "fe80::2",
        vec![
            hello(),
            OutboundTlv::Ihu {
                address: None,
                rxcost: 96,
                interval_cs: 1200,
                sub_tlvs: vec![],
            },
            OutboundTlv::Update(OutboundUpdate {
                key: Some(route),
                router_id: Some(id(2)),
                next_hop: None,
                interval_cs: 1600,
                seqno: 1,
                metric: 0,
                v4_via_v6: true,
                sub_tlvs: vec![],
            }),
        ],
        1,
    );
    assert!(e.selected_routes().is_empty());
}

#[test]
fn custom_algebra_cannot_resurrect_an_infinite_link() {
    struct BrokenAlgebra;
    impl MetricAlgebra for BrokenAlgebra {
        fn extend(&self, _: u16, _: u16) -> u16 {
            1
        }
    }
    let mut config = EngineConfig::recommended(id(1));
    config.metric_algebra = Arc::new(BrokenAlgebra);
    let mut e = Engine::new(config);
    e.handle(Event::InterfaceUp {
        interface: "lan".into(),
        local_addresses: vec![],
        now_ms: 0,
    });
    receive(
        &mut e,
        "fe80::2",
        vec![
            hello(),
            OutboundTlv::Update(OutboundUpdate {
                key: Some(key()),
                router_id: Some(id(2)),
                next_hop: None,
                interval_cs: 1600,
                seqno: 1,
                metric: 0,
                v4_via_v6: false,
                sub_tlvs: vec![],
            }),
        ],
        1,
    );
    assert!(e.selected_routes().is_empty());
}

#[test]
fn receive_timestamp_is_independent_of_the_processing_clock() {
    let mut e = engine(false);
    e.handle(Event::InterfacePolicyChanged {
        interface: "lan".into(),
        now_ms: 1,
        reset_metric: true,
        policy: InterfacePolicy {
            control_transport: ControlTransport::Ipv6,
            ipv4_next_hop: Ipv4NextHop::Auto,
            metric: Arc::new(RttMetric::recommended(Arc::new(
                WiredMetric::new(96, 1, 1).unwrap(),
            ))),
            hello_interval_cs: 400,
            update_interval_cs: 1600,
            split_horizon: false,
        },
    });
    e.handle(Event::Tick { now_ms: 4000 });
    let source = "fe80::2".parse().unwrap();
    let bytes = encode_packet(&OutboundPacket {
        tlvs: vec![
            OutboundTlv::Hello {
                unicast: false,
                seqno: 1,
                interval_cs: 400,
                sub_tlvs: vec![SubTlv::TimestampHello(30000)],
            },
            OutboundTlv::Ihu {
                address: None,
                rxcost: 96,
                interval_cs: 1200,
                sub_tlvs: vec![SubTlv::TimestampIhu {
                    origin: 10000,
                    received: 20000,
                }],
            },
        ],
    })
    .unwrap();
    e.handle(Event::PacketReceivedWithTimestamp {
        interface: "lan".into(),
        source,
        packet: decode_packet(&bytes, DecodeContext { source }).unwrap(),
        now_ms: 5000,
        received_timestamp_us: 60000,
    });
    let status = e.neighbour_status(5000);
    assert_eq!(status[0].last_rtt_us, Some(40000));
    assert_eq!(status[0].last_hello_age_ms, 0);
}
