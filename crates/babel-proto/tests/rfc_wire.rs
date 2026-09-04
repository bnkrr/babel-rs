use std::str::FromStr;

use babel_proto::{
    DecodeContext, INFINITY, OutboundPacket, OutboundTlv, OutboundUpdate, RouteKey, RouterId,
    SubTlv, WireError, decode_packet, encode_packet, stamp_hello_timestamps,
};
use ipnet::IpNet;

fn context() -> DecodeContext {
    DecodeContext {
        source: "fe80::1".parse().unwrap(),
    }
}

#[test]
fn rfc8966_padn_mbz_is_ignored_on_receive_and_enforced_on_send() {
    let raw = [42, 2, 0, 3, 1, 1, 1];
    assert_eq!(
        decode_packet(&raw, context()).unwrap().tlvs,
        vec![babel_proto::Tlv::PadN(vec![1])]
    );
    assert_eq!(
        encode_packet(&OutboundPacket {
            tlvs: vec![OutboundTlv::PadN(vec![1])],
        }),
        Err(WireError::InvalidTlv { type_: 1 })
    );
}

#[test]
fn rfc8966_encoder_rejects_zero_required_control_values() {
    let route = RouteKey::new(IpNet::from_str("2001:db8::/64").unwrap(), None).unwrap();
    let rid = RouterId::new([1; 8]).unwrap();
    let cases = [
        OutboundTlv::AckRequest {
            nonce: 1,
            interval_cs: 0,
        },
        OutboundTlv::Ihu {
            address: None,
            rxcost: 96,
            interval_cs: 0,
            sub_tlvs: vec![],
        },
        OutboundTlv::Update(OutboundUpdate {
            key: Some(route),
            router_id: Some(rid),
            next_hop: None,
            interval_cs: 0,
            seqno: 1,
            metric: INFINITY,
            v4_via_v6: false,
            sub_tlvs: vec![],
        }),
        OutboundTlv::SeqnoRequest {
            key: route,
            seqno: 1,
            hop_count: 0,
            router_id: rid,
            sub_tlvs: vec![],
        },
    ];
    for value in cases {
        assert!(matches!(
            encode_packet(&OutboundPacket { tlvs: vec![value] }),
            Err(WireError::InvalidTlv { .. })
        ));
    }
}

#[test]
fn rfc9079_zero_source_prefix_is_the_ordinary_sadr_domain() {
    let key = RouteKey::new(
        IpNet::from_str("192.0.2.0/24").unwrap(),
        Some(IpNet::from_str("0.0.0.0/0").unwrap()),
    )
    .unwrap();
    assert_eq!(key.source, None);
}

#[test]
fn rfc9079_wildcard_retraction_with_source_prefix_is_ignored() {
    // Update AE=0, metric=infinity, followed by mandatory Source Prefix.
    let raw = [
        42, 2, 0, 15, 8, 13, 0, 0, 0, 0, 0, 1, 0, 1, 0xff, 0xff, 128, 1, 8,
    ];
    let decoded = decode_packet(&raw, context()).unwrap();
    assert!(decoded.tlvs.is_empty());
}

#[test]
fn rfc9616_timestamp_is_stamped_at_the_transport_boundary() {
    let mut data = encode_packet(&OutboundPacket {
        tlvs: vec![OutboundTlv::Hello {
            unicast: true,
            seqno: 1,
            interval_cs: 0,
            sub_tlvs: vec![SubTlv::TimestampHello(1)],
        }],
    })
    .unwrap();
    stamp_hello_timestamps(&mut data, 0x0102_0304).unwrap();
    let decoded = decode_packet(&data, context()).unwrap();
    assert!(matches!(
        &decoded.tlvs[0],
        babel_proto::Tlv::Hello { sub_tlvs, .. }
            if sub_tlvs.contains(&SubTlv::TimestampHello(0x0102_0304))
    ));
}
