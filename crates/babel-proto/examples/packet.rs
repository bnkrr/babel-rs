use std::net::IpAddr;

use babel_proto::{DecodeContext, OutboundPacket, OutboundTlv, Tlv, decode_packet, encode_packet};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let packet = OutboundPacket {
        tlvs: vec![OutboundTlv::Hello {
            unicast: false,
            seqno: 7,
            interval_cs: 400,
            sub_tlvs: vec![],
        }],
    };
    let wire = encode_packet(&packet)?;
    let decoded = decode_packet(
        &wire,
        DecodeContext {
            source: IpAddr::V6("fe80::1".parse()?),
        },
    )?;
    assert!(matches!(
        decoded.tlvs.as_slice(),
        [Tlv::Hello { seqno: 7, .. }]
    ));
    println!("encoded {} bytes: {wire:02x?}", wire.len());
    Ok(())
}
