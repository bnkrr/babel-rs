//! MTU-bounded datagrams, packet context resets and send-time timestamps.

use super::encode::{EncodeContext, encode_outbound_tlv};
use super::*;

/// Encode exactly one datagram using the maximum Babel body size.
/// Returns [`WireError::PacketSplitRequired`] when context needs multiple packets.
/// Use [`encode_packets`] with the actual UDP payload budget for transmission.
pub fn encode_packet(packet: &OutboundPacket) -> Result<Vec<u8>, WireError> {
    let mut packets = encode_packets(packet, MAX_PACKET_SIZE)?;
    if packets.len() != 1 {
        return Err(WireError::PacketSplitRequired);
    }
    Ok(packets.pop().expect("exactly one packet was checked"))
}

/// Rewrite RFC 9616 Timestamp sub-TLVs carried by Hello TLVs immediately
/// before a datagram is sent. This keeps queueing time out of RTT samples.
pub fn stamp_hello_timestamps(data: &mut [u8], timestamp: u32) -> Result<(), WireError> {
    if data.len() < 4 {
        return Err(WireError::ShortHeader);
    }
    if data[0] != MAGIC {
        return Err(WireError::BadMagic);
    }
    if data[1] != VERSION {
        return Err(WireError::UnsupportedVersion(data[1]));
    }
    let body_len = usize::from(be16(&data[2..4]));
    if data.len() < 4 + body_len {
        return Err(WireError::TruncatedBody);
    }
    let mut offset = 4;
    let end = 4 + body_len;
    while offset < end {
        let type_ = data[offset];
        offset += 1;
        if type_ == TLV_PAD1 {
            continue;
        }
        if offset >= end {
            return Err(WireError::TruncatedTlv { type_ });
        }
        let length = usize::from(data[offset]);
        offset += 1;
        if end - offset < length {
            return Err(WireError::TruncatedTlv { type_ });
        }
        if type_ == TLV_HELLO && length >= 6 {
            let mut sub = offset + 6;
            let tlv_end = offset + length;
            while sub < tlv_end {
                let sub_type = data[sub];
                sub += 1;
                if sub_type == 0 {
                    continue;
                }
                if sub >= tlv_end {
                    return Err(WireError::InvalidTlv { type_: TLV_HELLO });
                }
                let sub_length = usize::from(data[sub]);
                sub += 1;
                if tlv_end - sub < sub_length {
                    return Err(WireError::InvalidTlv { type_: TLV_HELLO });
                }
                if sub_type == SUBTLV_TIMESTAMP && sub_length >= 4 {
                    data[sub..sub + 4].copy_from_slice(&timestamp.to_be_bytes());
                }
                sub += sub_length;
            }
        }
        offset += length;
    }
    Ok(())
}

/// Encode semantic outbound TLVs into independently decodable Babel packets.
/// Router-ID and Next-Hop context is repeated after every packet boundary.
/// `max_packet_size` includes the Babel header but excludes UDP/IP headers.
/// Timestamped IHUs must follow their timestamped Hello; each such group stays
/// in one datagram. A group or TLV that cannot fit the budget is rejected.
pub fn encode_packets(
    packet: &OutboundPacket,
    max_packet_size: usize,
) -> Result<Vec<Vec<u8>>, WireError> {
    if max_packet_size < 4 {
        return Err(WireError::PacketBudgetTooSmall);
    }
    let max_packet_size = max_packet_size.min(MAX_PACKET_SIZE);
    let body_budget = max_packet_size - 4;
    let mut packets = Vec::new();
    let mut body = Vec::new();
    let mut context = EncodeContext::default();
    let mut offset = 0;
    let mut has_hello = false;
    while offset < packet.tlvs.len() {
        let tlv = &packet.tlvs[offset];
        let is_hello = matches!(tlv, OutboundTlv::Hello { .. });
        let timestamped = matches!(tlv, OutboundTlv::Hello { sub_tlvs, .. }
            if sub_tlvs.iter().any(|s| matches!(s, SubTlv::TimestampHello(_))));
        let mut end = offset + 1;
        if timestamped {
            while end < packet.tlvs.len() && matches!(&packet.tlvs[end], OutboundTlv::Ihu { .. }) {
                end += 1;
            }
        } else if matches!(tlv, OutboundTlv::Ihu { sub_tlvs, .. }
            if sub_tlvs.iter().any(|s| matches!(s, SubTlv::TimestampIhu { .. })))
        {
            return Err(WireError::InvalidTlv { type_: TLV_IHU });
        }
        let group = &packet.tlvs[offset..end];
        if !body.is_empty()
            && (requires_fresh_next_hop_context(tlv, &context) || (is_hello && has_hello))
        {
            packets.push(finish_packet(std::mem::take(&mut body))?);
            context = EncodeContext::default();
            has_hello = false;
        }
        let mut next_context = context.clone();
        let mut encoded = Vec::new();
        for value in group {
            encode_outbound_tlv(value, &mut next_context, &mut encoded)?;
        }
        if encoded.len() > body_budget {
            return Err(WireError::BodyTooLarge);
        }
        if !body.is_empty() && body.len() + encoded.len() > body_budget {
            packets.push(finish_packet(std::mem::take(&mut body))?);
            context = EncodeContext::default();
            has_hello = false;
            next_context = context.clone();
            encoded.clear();
            for value in group {
                encode_outbound_tlv(value, &mut next_context, &mut encoded)?;
            }
            if encoded.len() > body_budget {
                return Err(WireError::BodyTooLarge);
            }
        }
        body.extend(encoded);
        context = next_context;
        has_hello |= is_hello;
        offset = end;
    }
    packets.push(finish_packet(body)?);
    Ok(packets)
}

fn requires_fresh_next_hop_context(tlv: &OutboundTlv, context: &EncodeContext) -> bool {
    let OutboundTlv::Update(update) = tlv else {
        return false;
    };
    if update.metric == INFINITY || update.next_hop.is_some() {
        return false;
    }
    if update.v4_via_v6
        || update
            .key
            .is_some_and(|key| key.destination.addr().is_ipv6())
    {
        context.next_hop_v6.is_some()
    } else {
        context.next_hop_v4.is_some()
    }
}

fn finish_packet(body: Vec<u8>) -> Result<Vec<u8>, WireError> {
    if body.len() > u16::MAX as usize {
        return Err(WireError::BodyTooLarge);
    }
    let mut result = Vec::with_capacity(4 + body.len());
    result.extend_from_slice(&[MAGIC, VERSION]);
    result.extend_from_slice(&(body.len() as u16).to_be_bytes());
    result.extend_from_slice(&body);
    Ok(result)
}
