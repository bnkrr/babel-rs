//! Packet framing, TLV validation and inbound context resolution.

use super::*;

#[derive(Default)]
struct ParserState {
    router_id: Option<RouterId>,
    prefixes: HashMap<AddressEncoding, Vec<u8>>,
    next_hop_v4: Option<IpAddr>,
    next_hop_v6: Option<IpAddr>,
}

impl ParserState {
    fn new(source: IpAddr) -> Self {
        let mut state = Self::default();
        match source {
            IpAddr::V4(_) => state.next_hop_v4 = Some(source),
            IpAddr::V6(_) => state.next_hop_v6 = Some(source),
        }
        state
    }
}

/// Decode one datagram, validating framing and resolving packet-local context.
/// Unknown mandatory sub-TLVs discard their enclosing TLV, not the whole packet.
/// The caller supplies the real packet source and enforces transport admission.
pub fn decode_packet(data: &[u8], context: DecodeContext) -> Result<Packet, WireError> {
    if data.len() < 4 {
        return Err(WireError::ShortHeader);
    }
    if data[0] != MAGIC {
        return Err(WireError::BadMagic);
    }
    if data[1] != VERSION {
        return Err(WireError::UnsupportedVersion(data[1]));
    }
    let body_len = u16::from_be_bytes([data[2], data[3]]) as usize;
    if data.len() < 4 + body_len {
        return Err(WireError::TruncatedBody);
    }
    let body = &data[4..4 + body_len];
    let mut state = ParserState::new(context.source);
    let mut tlvs = Vec::new();
    let mut offset = 0;
    while offset < body.len() {
        let type_ = body[offset];
        offset += 1;
        if type_ == TLV_PAD1 {
            tlvs.push(Tlv::Pad1);
            continue;
        }
        if offset >= body.len() {
            return Err(WireError::TruncatedTlv { type_ });
        }
        let len = body[offset] as usize;
        offset += 1;
        if body.len() - offset < len {
            return Err(WireError::TruncatedTlv { type_ });
        }
        let value = &body[offset..offset + len];
        offset += len;
        if let Some(tlv) = decode_tlv(type_, value, &mut state)? {
            tlvs.push(tlv);
        }
    }
    Ok(Packet { tlvs })
}

fn decode_tlv(type_: u8, value: &[u8], state: &mut ParserState) -> Result<Option<Tlv>, WireError> {
    let invalid = || WireError::InvalidTlv { type_ };
    Ok(match type_ {
        TLV_PADN => {
            // RFC 8966 section 4.6.2 requires zeroes on transmission, but the
            // complete PadN TLV (including its MBZ body) is silently ignored
            // on reception.
            Some(Tlv::PadN(value.to_vec()))
        }
        TLV_ACK_REQ => {
            if value.len() < 6 {
                return Err(invalid());
            }
            let interval_cs = be16(&value[4..6]);
            if interval_cs == 0 {
                return Err(invalid());
            }
            if has_unknown_mandatory(&value[6..])? {
                None
            } else {
                Some(Tlv::AckRequest {
                    nonce: be16(&value[2..4]),
                    interval_cs,
                })
            }
        }
        TLV_ACK => {
            if value.len() < 2 {
                return Err(invalid());
            }
            if has_unknown_mandatory(&value[2..])? {
                None
            } else {
                Some(Tlv::Ack { nonce: be16(value) })
            }
        }
        TLV_HELLO => {
            if value.len() < 6 {
                return Err(invalid());
            }
            let sub_tlvs = decode_sub_tlvs(&value[6..], SubContext::Hello, None)?;
            if sub_tlvs.mandatory_unknown {
                None
            } else {
                Some(Tlv::Hello {
                    unicast: be16(value) & 0x8000 != 0,
                    seqno: be16(&value[2..4]),
                    interval_cs: be16(&value[4..6]),
                    sub_tlvs: sub_tlvs.values,
                })
            }
        }
        TLV_IHU => {
            if value.len() < 6 {
                return Err(invalid());
            }
            let Some(ae) = AddressEncoding::from_u8(value[0]) else {
                return Ok(None);
            };
            if ae == AddressEncoding::Ipv4ViaIpv6 {
                return Ok(None);
            }
            let addr_len = ae.address_len().ok_or_else(invalid)?;
            if value.len() < 6 + addr_len || be16(&value[4..6]) == 0 {
                return Err(invalid());
            }
            let address = if ae == AddressEncoding::Wildcard {
                None
            } else {
                Some(decode_address(ae, &value[6..6 + addr_len]).ok_or_else(invalid)?)
            };
            let sub_tlvs = decode_sub_tlvs(&value[6 + addr_len..], SubContext::Ihu, None)?;
            if sub_tlvs.mandatory_unknown {
                None
            } else {
                Some(Tlv::Ihu {
                    address,
                    rxcost: be16(&value[2..4]),
                    interval_cs: be16(&value[4..6]),
                    sub_tlvs: sub_tlvs.values,
                })
            }
        }
        TLV_ROUTER_ID => {
            if value.len() < 10 {
                return Err(invalid());
            }
            let raw: [u8; 8] = value[2..10].try_into().map_err(|_| invalid())?;
            let id = RouterId::new(raw).ok_or_else(invalid)?;
            state.router_id = Some(id);
            if has_unknown_mandatory(&value[10..])? {
                None
            } else {
                Some(Tlv::RouterId(id))
            }
        }
        TLV_NEXT_HOP => {
            if value.len() < 2 {
                return Err(invalid());
            }
            let Some(ae) = AddressEncoding::from_u8(value[0]) else {
                return Ok(None);
            };
            if matches!(ae, AddressEncoding::Wildcard | AddressEncoding::Ipv4ViaIpv6) {
                return Ok(None);
            }
            let addr_len = ae.address_len().ok_or_else(invalid)?;
            if value.len() < 2 + addr_len {
                return Err(invalid());
            }
            let address = decode_address(ae, &value[2..2 + addr_len]).ok_or_else(invalid)?;
            if address.is_ipv4() {
                state.next_hop_v4 = Some(address);
            } else {
                state.next_hop_v6 = Some(address);
            }
            if has_unknown_mandatory(&value[2 + addr_len..])? {
                None
            } else {
                Some(Tlv::NextHop(address))
            }
        }
        TLV_UPDATE => decode_update(value, state)?,
        TLV_ROUTE_REQUEST => decode_route_request(value)?,
        TLV_SEQNO_REQUEST => decode_seqno_request(value)?,
        _ => Some(Tlv::Unknown {
            type_,
            value: value.to_vec(),
        }),
    })
}

fn decode_update(value: &[u8], state: &mut ParserState) -> Result<Option<Tlv>, WireError> {
    let invalid = || WireError::InvalidTlv { type_: TLV_UPDATE };
    if value.len() < 10 {
        return Err(invalid());
    }
    let Some(ae) = AddressEncoding::from_u8(value[0]) else {
        return Ok(None);
    };
    let flags = value[1];
    let plen = value[2];
    let omitted = value[3] as usize;
    let interval_cs = be16(&value[4..6]);
    let seqno = be16(&value[6..8]);
    let metric = be16(&value[8..10]);
    if interval_cs == 0 {
        return Err(invalid());
    }
    if metric != INFINITY && ae == AddressEncoding::Wildcard {
        return Ok(None);
    }
    if ae == AddressEncoding::Wildcard {
        if plen != 0 || omitted != 0 {
            return Ok(None);
        }
        let sub = decode_sub_tlvs(&value[10..], SubContext::Prefix(ae), None)?;
        if sub.mandatory_unknown || sub.source_prefix.is_some() {
            return Ok(None);
        }
        return Ok(Some(Tlv::Update(ResolvedUpdate {
            key: None,
            router_id: None,
            next_hop: None,
            interval_cs,
            seqno,
            metric,
            v4_via_v6: false,
            sub_tlvs: sub.values,
        })));
    }
    if ae == AddressEncoding::Ipv6LinkLocal && omitted != 0 {
        return Ok(None);
    }
    let encoded_len = prefix_encoded_len(ae, plen).ok_or_else(invalid)?;
    if omitted > encoded_len || value.len() < 10 + encoded_len - omitted {
        return Err(invalid());
    }
    let prefix_bytes = &value[10..10 + encoded_len - omitted];
    let prefix = decode_prefix(ae, plen, omitted, prefix_bytes, state.prefixes.get(&ae))
        .ok_or_else(invalid)?;

    // Parser state changes happen before mandatory sub-TLV handling.
    if flags & 0x80 != 0 {
        state.prefixes.insert(ae, prefix_wire_bytes(prefix, ae));
    }
    if flags & 0x40 != 0 {
        let raw = router_id_from_prefix(prefix);
        state.router_id = RouterId::new(raw);
    }

    let natural = 10 + encoded_len - omitted;
    let sub = decode_sub_tlvs(&value[natural..], SubContext::Prefix(ae), Some(prefix))?;
    if sub.mandatory_unknown {
        return Ok(None);
    }
    let key = RouteKey::new(prefix, sub.source_prefix).ok_or_else(invalid)?;
    let router_id = if metric == INFINITY {
        state.router_id
    } else {
        Some(state.router_id.ok_or_else(invalid)?)
    };
    let next_hop = if metric == INFINITY {
        None
    } else if ae == AddressEncoding::Ipv4ViaIpv6 {
        state.next_hop_v6
    } else if prefix.addr().is_ipv4() {
        state.next_hop_v4
    } else {
        state.next_hop_v6
    };
    if metric != INFINITY && next_hop.is_none() {
        return Ok(None);
    }
    Ok(Some(Tlv::Update(ResolvedUpdate {
        key: Some(key),
        router_id,
        next_hop,
        interval_cs,
        seqno,
        metric,
        v4_via_v6: ae == AddressEncoding::Ipv4ViaIpv6,
        sub_tlvs: sub.values,
    })))
}

fn decode_route_request(value: &[u8]) -> Result<Option<Tlv>, WireError> {
    let invalid = || WireError::InvalidTlv {
        type_: TLV_ROUTE_REQUEST,
    };
    if value.len() < 2 {
        return Err(invalid());
    }
    let Some(ae) = AddressEncoding::from_u8(value[0]) else {
        return Ok(None);
    };
    let plen = value[1];
    if ae == AddressEncoding::Wildcard {
        if plen != 0 {
            return Ok(None);
        }
        let sub = decode_sub_tlvs(&value[2..], SubContext::Prefix(ae), None)?;
        if sub.mandatory_unknown || sub.source_prefix.is_some() {
            return Ok(None);
        }
        return Ok(Some(Tlv::RouteRequest {
            key: None,
            sub_tlvs: sub.values,
        }));
    }
    let size = prefix_encoded_len(ae, plen).ok_or_else(invalid)?;
    if value.len() < 2 + size {
        return Err(invalid());
    }
    let prefix = decode_prefix(ae, plen, 0, &value[2..2 + size], None).ok_or_else(invalid)?;
    let sub = decode_sub_tlvs(&value[2 + size..], SubContext::Prefix(ae), Some(prefix))?;
    if sub.mandatory_unknown {
        return Ok(None);
    }
    let key = RouteKey::new(prefix, sub.source_prefix).ok_or_else(invalid)?;
    Ok(Some(Tlv::RouteRequest {
        key: Some(key),
        sub_tlvs: sub.values,
    }))
}

fn decode_seqno_request(value: &[u8]) -> Result<Option<Tlv>, WireError> {
    let invalid = || WireError::InvalidTlv {
        type_: TLV_SEQNO_REQUEST,
    };
    if value.len() < 14 {
        return Err(invalid());
    }
    let Some(ae) = AddressEncoding::from_u8(value[0]) else {
        return Ok(None);
    };
    if ae == AddressEncoding::Wildcard {
        return Ok(None);
    }
    let plen = value[1];
    let size = prefix_encoded_len(ae, plen).ok_or_else(invalid)?;
    if value.len() < 14 + size || value[4] == 0 {
        return Err(invalid());
    }
    let router_id =
        RouterId::new(value[6..14].try_into().map_err(|_| invalid())?).ok_or_else(invalid)?;
    let prefix = decode_prefix(ae, plen, 0, &value[14..14 + size], None).ok_or_else(invalid)?;
    let sub = decode_sub_tlvs(&value[14 + size..], SubContext::Prefix(ae), Some(prefix))?;
    if sub.mandatory_unknown {
        return Ok(None);
    }
    let key = RouteKey::new(prefix, sub.source_prefix).ok_or_else(invalid)?;
    Ok(Some(Tlv::SeqnoRequest {
        key,
        seqno: be16(&value[2..4]),
        hop_count: value[4],
        router_id,
        sub_tlvs: sub.values,
    }))
}

#[derive(Clone, Copy)]
enum SubContext {
    Hello,
    Ihu,
    Prefix(AddressEncoding),
}

struct DecodedSubTlvs {
    values: Vec<SubTlv>,
    source_prefix: Option<IpNet>,
    mandatory_unknown: bool,
}

fn decode_sub_tlvs(
    data: &[u8],
    context: SubContext,
    destination: Option<IpNet>,
) -> Result<DecodedSubTlvs, WireError> {
    let mut result = DecodedSubTlvs {
        values: Vec::new(),
        source_prefix: None,
        mandatory_unknown: false,
    };
    let mut offset = 0;
    while offset < data.len() {
        let type_ = data[offset];
        offset += 1;
        if type_ == 0 {
            result.values.push(SubTlv::Pad1);
            continue;
        }
        if offset >= data.len() {
            return Err(WireError::InvalidTlv { type_ });
        }
        let len = data[offset] as usize;
        offset += 1;
        if data.len() - offset < len {
            return Err(WireError::InvalidTlv { type_ });
        }
        let body = &data[offset..offset + len];
        offset += len;
        match type_ {
            1 => {
                // As with top-level PadN, section 4.7.2 says to ignore the
                // complete sub-TLV on reception; MBZ is an encoder rule.
                result.values.push(SubTlv::PadN(body.to_vec()));
            }
            SUBTLV_TIMESTAMP => match context {
                SubContext::Hello if body.len() >= 4 => {
                    result.values.push(SubTlv::TimestampHello(be32(body)))
                }
                SubContext::Ihu if body.len() >= 8 => result.values.push(SubTlv::TimestampIhu {
                    origin: be32(body),
                    received: be32(&body[4..]),
                }),
                _ => result.values.push(SubTlv::Unknown {
                    type_,
                    value: body.to_vec(),
                }),
            },
            SUBTLV_SOURCE_PREFIX => {
                let SubContext::Prefix(ae) = context else {
                    result.mandatory_unknown = true;
                    continue;
                };
                if result.source_prefix.is_some()
                    || ae == AddressEncoding::Wildcard
                    || body.is_empty()
                    || body[0] == 0
                {
                    result.mandatory_unknown = true;
                    continue;
                }
                let plen = body[0];
                let Some(size) = prefix_encoded_len(ae, plen) else {
                    result.mandatory_unknown = true;
                    continue;
                };
                if body.len() < 1 + size {
                    result.mandatory_unknown = true;
                    continue;
                }
                let Some(prefix) = decode_prefix(ae, plen, 0, &body[1..1 + size], None) else {
                    result.mandatory_unknown = true;
                    continue;
                };
                if destination.is_some_and(|dst| dst.addr().is_ipv4() != prefix.addr().is_ipv4()) {
                    result.mandatory_unknown = true;
                    continue;
                }
                result.source_prefix = Some(prefix);
            }
            _ => {
                if type_ & 0x80 != 0 {
                    result.mandatory_unknown = true;
                }
                result.values.push(SubTlv::Unknown {
                    type_,
                    value: body.to_vec(),
                });
            }
        }
    }
    Ok(result)
}

fn has_unknown_mandatory(data: &[u8]) -> Result<bool, WireError> {
    Ok(decode_sub_tlvs(data, SubContext::Hello, None)?.mandatory_unknown)
}
