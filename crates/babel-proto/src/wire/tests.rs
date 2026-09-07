use super::*;
use std::str::FromStr;

fn rid() -> RouterId {
    RouterId::new([1, 2, 3, 4, 5, 6, 7, 8]).unwrap()
}

#[test]
fn source_specific_v4_via_v6_round_trip() {
    let key = RouteKey::new(
        IpNet::from_str("192.0.2.0/24").unwrap(),
        Some(IpNet::from_str("10.0.0.0/8").unwrap()),
    )
    .unwrap();
    let packet = OutboundPacket {
        tlvs: vec![OutboundTlv::Update(OutboundUpdate {
            key: Some(key),
            router_id: Some(rid()),
            next_hop: Some("fe80::9".parse().unwrap()),
            interval_cs: 1600,
            seqno: 9,
            metric: 96,
            v4_via_v6: true,
            sub_tlvs: vec![],
        })],
    };
    let encoded = encode_packet(&packet).unwrap();
    let decoded = decode_packet(
        &encoded,
        DecodeContext {
            source: "fe80::1".parse().unwrap(),
        },
    )
    .unwrap();
    assert_eq!(
        decoded,
        Packet {
            tlvs: vec![
                Tlv::RouterId(rid()),
                Tlv::NextHop("fe80::9".parse().unwrap()),
                Tlv::Update(ResolvedUpdate {
                    key: Some(key),
                    router_id: Some(rid()),
                    next_hop: Some("fe80::9".parse().unwrap()),
                    interval_cs: 1600,
                    seqno: 9,
                    metric: 96,
                    v4_via_v6: true,
                    sub_tlvs: vec![],
                }),
            ],
        }
    );
}

#[test]
fn packetizer_repeats_context_and_respects_the_datagram_budget() {
    let next_hop: IpAddr = "fe80::9".parse().unwrap();
    let tlvs = (1..=32)
        .map(|suffix| {
            let destination = IpNet::from_str(&format!("2001:db8::{suffix}/128")).unwrap();
            OutboundTlv::Update(OutboundUpdate {
                key: RouteKey::new(destination, None),
                router_id: Some(rid()),
                next_hop: Some(next_hop),
                interval_cs: 1600,
                seqno: suffix,
                metric: 96,
                v4_via_v6: false,
                sub_tlvs: vec![],
            })
        })
        .collect();
    let encoded = encode_packets(&OutboundPacket { tlvs }, 96).unwrap();
    assert!(encoded.len() > 1);
    let mut updates = 0;
    for datagram in encoded {
        assert!(datagram.len() <= 96);
        let decoded = decode_packet(
            &datagram,
            DecodeContext {
                source: "fe80::1".parse().unwrap(),
            },
        )
        .unwrap();
        for tlv in decoded.tlvs {
            if let Tlv::Update(update) = tlv {
                assert_eq!(update.router_id, Some(rid()));
                assert_eq!(update.next_hop, Some(next_hop));
                updates += 1;
            }
        }
    }
    assert_eq!(updates, 32);
}

#[test]
fn finite_update_requires_router_id_but_retraction_does_not() {
    let key = RouteKey::new(IpNet::from_str("2001:db8::/64").unwrap(), None).unwrap();
    for metric in [96, INFINITY] {
        let datagram = encode_packet(&OutboundPacket {
            tlvs: vec![OutboundTlv::Update(OutboundUpdate {
                key: Some(key),
                router_id: Some(rid()),
                next_hop: None,
                interval_cs: 1600,
                seqno: 7,
                metric,
                v4_via_v6: false,
                sub_tlvs: vec![],
            })],
        })
        .unwrap();
        let decoded = decode_packet(
            &datagram,
            DecodeContext {
                source: "fe80::1".parse().unwrap(),
            },
        )
        .unwrap();
        if metric < INFINITY {
            assert!(matches!(decoded.tlvs[0], Tlv::RouterId(value) if value == rid()));
            assert!(
                matches!(&decoded.tlvs[1], Tlv::Update(update) if update.router_id == Some(rid()) && update.metric == metric)
            );
        } else {
            assert!(
                matches!(&decoded.tlvs[0], Tlv::Update(update) if update.router_id.is_none() && update.metric == metric)
            );
        }
    }
}

#[test]
fn packetizer_resets_an_explicit_next_hop_before_source_default() {
    let key = RouteKey::new(IpNet::from_str("2001:db8::/64").unwrap(), None).unwrap();
    let update = |next_hop| {
        OutboundTlv::Update(OutboundUpdate {
            key: Some(key),
            router_id: Some(rid()),
            next_hop,
            interval_cs: 1600,
            seqno: 1,
            metric: 96,
            v4_via_v6: false,
            sub_tlvs: vec![],
        })
    };
    let datagrams = encode_packets(
        &OutboundPacket {
            tlvs: vec![update(Some("fe80::9".parse().unwrap())), update(None)],
        },
        DEFAULT_UDP_PAYLOAD_SIZE,
    )
    .unwrap();
    assert_eq!(datagrams.len(), 2);
    let second = decode_packet(
        &datagrams[1],
        DecodeContext {
            source: "fe80::1".parse().unwrap(),
        },
    )
    .unwrap();
    assert!(matches!(&second.tlvs[1], Tlv::Update(value)
        if value.next_hop == Some("fe80::1".parse().unwrap())));
}

#[test]
fn unknown_tlv_is_preserved_and_unknown_mandatory_subtlv_ignores_enclosing() {
    let packet = [42, 2, 0, 12, 200, 2, 1, 2, 4, 6, 0, 0, 0, 1, 0, 100];
    let decoded = decode_packet(
        &packet,
        DecodeContext {
            source: "fe80::1".parse().unwrap(),
        },
    )
    .unwrap();
    assert!(matches!(decoded.tlvs[0], Tlv::Unknown { type_: 200, .. }));
    assert!(matches!(decoded.tlvs[1], Tlv::Hello { .. }));
}

#[test]
fn malformed_lengths_never_overrun() {
    assert_eq!(
        decode_packet(
            &[42, 2, 0, 2, 8, 255],
            DecodeContext {
                source: "::1".parse().unwrap()
            }
        ),
        Err(WireError::TruncatedTlv { type_: 8 })
    );
}

#[test]
fn timestamp_extension_round_trips() {
    let packet = OutboundPacket {
        tlvs: vec![
            OutboundTlv::Hello {
                unicast: false,
                seqno: 17,
                interval_cs: 400,
                sub_tlvs: vec![SubTlv::TimestampHello(0x1020_3040)],
            },
            OutboundTlv::Ihu {
                address: None,
                rxcost: 96,
                interval_cs: 1200,
                sub_tlvs: vec![SubTlv::TimestampIhu {
                    origin: 0x1020_3040,
                    received: 0x5060_7080,
                }],
            },
        ],
    };
    let encoded = encode_packet(&packet).unwrap();
    let decoded = decode_packet(
        &encoded,
        DecodeContext {
            source: "fe80::1".parse().unwrap(),
        },
    )
    .unwrap();
    assert_eq!(
        decoded,
        Packet {
            tlvs: vec![
                Tlv::Hello {
                    unicast: false,
                    seqno: 17,
                    interval_cs: 400,
                    sub_tlvs: vec![SubTlv::TimestampHello(0x1020_3040)],
                },
                Tlv::Ihu {
                    address: None,
                    rxcost: 96,
                    interval_cs: 1200,
                    sub_tlvs: vec![SubTlv::TimestampIhu {
                        origin: 0x1020_3040,
                        received: 0x5060_7080,
                    }],
                },
            ],
        }
    );
}

#[test]
fn arbitrary_datagrams_never_panic() {
    let source = DecodeContext {
        source: "fe80::1".parse().unwrap(),
    };
    let mut state = 0x9e37_79b9_u32;
    for length in 0..=4096 {
        let mut bytes = vec![0; length];
        for byte in &mut bytes {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            *byte = state as u8;
        }
        let result = std::panic::catch_unwind(|| decode_packet(&bytes, source));
        assert!(result.is_ok(), "decoder panicked for {length} bytes");
    }
}
