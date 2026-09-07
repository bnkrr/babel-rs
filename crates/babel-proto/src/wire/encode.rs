//! Encode semantic TLVs while updating outbound packet context.

use super::*;

#[derive(Clone, Default)]
pub(super) struct EncodeContext {
    pub(super) router_id: Option<RouterId>,
    pub(super) next_hop_v4: Option<IpAddr>,
    pub(super) next_hop_v6: Option<IpAddr>,
}

pub(super) fn encode_outbound_tlv(
    tlv: &OutboundTlv,
    context: &mut EncodeContext,
    out: &mut Vec<u8>,
) -> Result<(), WireError> {
    match tlv {
        OutboundTlv::Pad1 => out.push(TLV_PAD1),
        OutboundTlv::PadN(value) => {
            require_zeroes(value, TLV_PADN)?;
            put_tlv(out, TLV_PADN, value)?;
        }
        OutboundTlv::AckRequest { nonce, interval_cs } => {
            require_nonzero(*interval_cs, TLV_ACK_REQ)?;
            let mut v = vec![0, 0];
            v.extend(nonce.to_be_bytes());
            v.extend(interval_cs.to_be_bytes());
            put_tlv(out, TLV_ACK_REQ, &v)?;
        }
        OutboundTlv::Ack { nonce } => put_tlv(out, TLV_ACK, &nonce.to_be_bytes())?,
        OutboundTlv::Hello {
            unicast,
            seqno,
            interval_cs,
            sub_tlvs,
        } => {
            let mut v = Vec::new();
            v.extend(if *unicast { 0x8000u16 } else { 0 }.to_be_bytes());
            v.extend(seqno.to_be_bytes());
            v.extend(interval_cs.to_be_bytes());
            encode_sub_tlvs(sub_tlvs, None, &mut v)?;
            put_tlv(out, TLV_HELLO, &v)?;
        }
        OutboundTlv::Ihu {
            address,
            rxcost,
            interval_cs,
            sub_tlvs,
        } => {
            require_nonzero(*interval_cs, TLV_IHU)?;
            let (ae, bytes) = encode_optional_address(*address);
            let mut v = vec![ae as u8, 0];
            v.extend(rxcost.to_be_bytes());
            v.extend(interval_cs.to_be_bytes());
            v.extend(bytes);
            encode_sub_tlvs(sub_tlvs, None, &mut v)?;
            put_tlv(out, TLV_IHU, &v)?;
        }
        OutboundTlv::Update(update) => {
            require_nonzero(update.interval_cs, TLV_UPDATE)?;
            if update.key.is_some() && update.metric != INFINITY {
                let router_id = update.router_id.ok_or(WireError::MissingRouterId)?;
                if context.router_id != Some(router_id) {
                    encode_router_id(router_id, out)?;
                    context.router_id = Some(router_id);
                }
            }
            if update.metric != INFINITY
                && let Some(next_hop) = update.next_hop
            {
                let active = if update.v4_via_v6 || next_hop.is_ipv6() {
                    &mut context.next_hop_v6
                } else {
                    &mut context.next_hop_v4
                };
                if *active != Some(next_hop) {
                    encode_next_hop(next_hop, out)?;
                    *active = Some(next_hop);
                }
            }
            encode_update(update, out)?;
        }
        OutboundTlv::RouteRequest { key, sub_tlvs } => {
            let mut v = Vec::new();
            if let Some(key) = key {
                let ae = ae_for_key(key, false);
                v.push(ae as u8);
                v.push(key.destination.prefix_len());
                v.extend(prefix_wire_bytes(key.destination, ae));
                encode_source_prefix(key.source, ae, &mut v)?;
            } else {
                v.extend([0, 0]);
            }
            encode_sub_tlvs(sub_tlvs, key.as_ref().map(|k| ae_for_key(k, false)), &mut v)?;
            put_tlv(out, TLV_ROUTE_REQUEST, &v)?;
        }
        OutboundTlv::SeqnoRequest {
            key,
            seqno,
            hop_count,
            router_id,
            sub_tlvs,
        } => {
            require_nonzero(u16::from(*hop_count), TLV_SEQNO_REQUEST)?;
            let ae = ae_for_key(key, false);
            let mut v = vec![ae as u8, key.destination.prefix_len()];
            v.extend(seqno.to_be_bytes());
            v.extend([*hop_count, 0]);
            v.extend(router_id.octets());
            v.extend(prefix_wire_bytes(key.destination, ae));
            encode_source_prefix(key.source, ae, &mut v)?;
            encode_sub_tlvs(sub_tlvs, Some(ae), &mut v)?;
            put_tlv(out, TLV_SEQNO_REQUEST, &v)?;
        }
        OutboundTlv::Unknown { type_, value } => put_tlv(out, *type_, value)?,
    }
    Ok(())
}

fn encode_router_id(id: RouterId, out: &mut Vec<u8>) -> Result<(), WireError> {
    let mut value = vec![0, 0];
    value.extend(id.octets());
    put_tlv(out, TLV_ROUTER_ID, &value)
}

fn encode_next_hop(address: IpAddr, out: &mut Vec<u8>) -> Result<(), WireError> {
    let (ae, bytes) = encode_address(address);
    let mut value = vec![ae as u8, 0];
    value.extend(bytes);
    put_tlv(out, TLV_NEXT_HOP, &value)
}

fn encode_update(update: &OutboundUpdate, out: &mut Vec<u8>) -> Result<(), WireError> {
    let Some(key) = update.key else {
        let mut v = vec![0, 0, 0, 0];
        v.extend(update.interval_cs.to_be_bytes());
        v.extend(update.seqno.to_be_bytes());
        v.extend(update.metric.to_be_bytes());
        return put_tlv(out, TLV_UPDATE, &v);
    };
    let ae = ae_for_key(&key, update.v4_via_v6);
    let mut v = vec![ae as u8, 0, key.destination.prefix_len(), 0];
    v.extend(update.interval_cs.to_be_bytes());
    v.extend(update.seqno.to_be_bytes());
    v.extend(update.metric.to_be_bytes());
    v.extend(prefix_wire_bytes(key.destination, ae));
    encode_source_prefix(key.source, ae, &mut v)?;
    encode_sub_tlvs(&update.sub_tlvs, Some(ae), &mut v)?;
    put_tlv(out, TLV_UPDATE, &v)
}

fn encode_source_prefix(
    source: Option<IpNet>,
    ae: AddressEncoding,
    out: &mut Vec<u8>,
) -> Result<(), WireError> {
    if let Some(source) = source {
        let mut body = vec![source.prefix_len()];
        body.extend(prefix_wire_bytes(source, ae));
        put_tlv(out, SUBTLV_SOURCE_PREFIX, &body)?;
    }
    Ok(())
}

fn encode_sub_tlvs(
    values: &[SubTlv],
    _ae: Option<AddressEncoding>,
    out: &mut Vec<u8>,
) -> Result<(), WireError> {
    for value in values {
        match value {
            SubTlv::SourcePrefix(_) => {} // emitted from RouteKey exactly once
            SubTlv::Pad1 => out.push(0),
            SubTlv::PadN(v) => {
                require_zeroes(v, 1)?;
                put_tlv(out, 1, v)?;
            }
            SubTlv::TimestampHello(v) => put_tlv(out, SUBTLV_TIMESTAMP, &v.to_be_bytes())?,
            SubTlv::TimestampIhu { origin, received } => {
                let mut v = Vec::new();
                v.extend(origin.to_be_bytes());
                v.extend(received.to_be_bytes());
                put_tlv(out, SUBTLV_TIMESTAMP, &v)?;
            }
            SubTlv::Unknown { type_, value } => put_tlv(out, *type_, value)?,
        }
    }
    Ok(())
}
