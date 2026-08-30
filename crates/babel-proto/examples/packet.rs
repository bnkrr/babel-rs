use std::net::IpAddr;

use babel_proto::{DecodeContext, Packet, Tlv, decode_packet, encode_packet};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let packet = Packet {
        tlvs: vec![Tlv::Hello {
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
    assert_eq!(decoded, packet);
    println!("encoded {} bytes: {wire:02x?}", wire.len());
    Ok(())
}
