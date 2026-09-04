use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use ipnet::{IpNet, Ipv4Net, Ipv6Net};
use thiserror::Error;

use crate::model::{INFINITY, RouteKey, RouterId};

pub const MAGIC: u8 = 42;
pub const VERSION: u8 = 2;
pub const PORT: u16 = 6696;
pub const MAX_PACKET_SIZE: usize = u16::MAX as usize + 4;
/// Conservative Babel UDP payload that fits the IPv6 minimum MTU without
/// fragmentation (1280 byte IPv6 packet minus IPv6 and UDP headers).
pub const DEFAULT_UDP_PAYLOAD_SIZE: usize = 1232;

const TLV_PAD1: u8 = 0;
const TLV_PADN: u8 = 1;
const TLV_ACK_REQ: u8 = 2;
const TLV_ACK: u8 = 3;
const TLV_HELLO: u8 = 4;
const TLV_IHU: u8 = 5;
const TLV_ROUTER_ID: u8 = 6;
const TLV_NEXT_HOP: u8 = 7;
const TLV_UPDATE: u8 = 8;
const TLV_ROUTE_REQUEST: u8 = 9;
const TLV_SEQNO_REQUEST: u8 = 10;

const SUBTLV_TIMESTAMP: u8 = 3;
const SUBTLV_SOURCE_PREFIX: u8 = 128;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum AddressEncoding {
    Wildcard = 0,
    Ipv4 = 1,
    Ipv6 = 2,
    Ipv6LinkLocal = 3,
    Ipv4ViaIpv6 = 4,
}

impl AddressEncoding {
    fn from_u8(value: u8) -> Option<Self> {
        Some(match value {
            0 => Self::Wildcard,
            1 => Self::Ipv4,
            2 => Self::Ipv6,
            3 => Self::Ipv6LinkLocal,
            4 => Self::Ipv4ViaIpv6,
            _ => return None,
        })
    }

    fn address_len(self) -> Option<usize> {
        match self {
            Self::Wildcard => Some(0),
            Self::Ipv4 | Self::Ipv4ViaIpv6 => Some(4),
            Self::Ipv6 => Some(16),
            Self::Ipv6LinkLocal => Some(8),
        }
    }

    fn max_prefix_bits(self) -> Option<u8> {
        match self {
            Self::Wildcard => Some(0),
            Self::Ipv4 | Self::Ipv4ViaIpv6 => Some(32),
            Self::Ipv6 | Self::Ipv6LinkLocal => Some(128),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SubTlv {
    Pad1,
    PadN(Vec<u8>),
    TimestampHello(u32),
    TimestampIhu { origin: u32, received: u32 },
    SourcePrefix(IpNet),
    Unknown { type_: u8, value: Vec<u8> },
}

/// An inbound Update after packet-local Router-ID, Next-Hop and prefix state
/// has been resolved by the decoder.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedUpdate {
    pub key: Option<RouteKey>,
    pub router_id: Option<RouterId>,
    pub next_hop: Option<IpAddr>,
    pub interval_cs: u16,
    pub seqno: u16,
    pub metric: u16,
    pub v4_via_v6: bool,
    pub sub_tlvs: Vec<SubTlv>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Tlv {
    Pad1,
    PadN(Vec<u8>),
    AckRequest {
        nonce: u16,
        interval_cs: u16,
    },
    Ack {
        nonce: u16,
    },
    Hello {
        unicast: bool,
        seqno: u16,
        interval_cs: u16,
        sub_tlvs: Vec<SubTlv>,
    },
    Ihu {
        address: Option<IpAddr>,
        rxcost: u16,
        interval_cs: u16,
        sub_tlvs: Vec<SubTlv>,
    },
    RouterId(RouterId),
    NextHop(IpAddr),
    Update(ResolvedUpdate),
    RouteRequest {
        key: Option<RouteKey>,
        sub_tlvs: Vec<SubTlv>,
    },
    SeqnoRequest {
        key: RouteKey,
        seqno: u16,
        hop_count: u8,
        router_id: RouterId,
        sub_tlvs: Vec<SubTlv>,
    },
    Unknown {
        type_: u8,
        value: Vec<u8>,
    },
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Packet {
    pub tlvs: Vec<Tlv>,
}

/// A semantic outbound Update.  The packetizer emits the separate Router-ID
/// and Next-Hop context TLVs required by the Babel wire format.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OutboundUpdate {
    pub key: Option<RouteKey>,
    pub router_id: Option<RouterId>,
    pub next_hop: Option<IpAddr>,
    pub interval_cs: u16,
    pub seqno: u16,
    pub metric: u16,
    pub v4_via_v6: bool,
    pub sub_tlvs: Vec<SubTlv>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OutboundTlv {
    Pad1,
    PadN(Vec<u8>),
    AckRequest {
        nonce: u16,
        interval_cs: u16,
    },
    Ack {
        nonce: u16,
    },
    Hello {
        unicast: bool,
        seqno: u16,
        interval_cs: u16,
        sub_tlvs: Vec<SubTlv>,
    },
    Ihu {
        address: Option<IpAddr>,
        rxcost: u16,
        interval_cs: u16,
        sub_tlvs: Vec<SubTlv>,
    },
    Update(OutboundUpdate),
    RouteRequest {
        key: Option<RouteKey>,
        sub_tlvs: Vec<SubTlv>,
    },
    SeqnoRequest {
        key: RouteKey,
        seqno: u16,
        hop_count: u8,
        router_id: RouterId,
        sub_tlvs: Vec<SubTlv>,
    },
    Unknown {
        type_: u8,
        value: Vec<u8>,
    },
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct OutboundPacket {
    pub tlvs: Vec<OutboundTlv>,
}

#[derive(Clone, Copy, Debug)]
pub struct DecodeContext {
    pub source: IpAddr,
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum WireError {
    #[error("packet is shorter than the Babel header")]
    ShortHeader,
    #[error("packet has invalid Babel magic")]
    BadMagic,
    #[error("packet has unsupported Babel version {0}")]
    UnsupportedVersion(u8),
    #[error("declared body is truncated")]
    TruncatedBody,
    #[error("TLV {type_} is truncated")]
    TruncatedTlv { type_: u8 },
    #[error("TLV {type_} has invalid body")]
    InvalidTlv { type_: u8 },
    #[error("packet body is too large")]
    BodyTooLarge,
    #[error("outbound route update has no router-id")]
    MissingRouterId,
    #[error("packet size budget is smaller than the Babel header")]
    PacketBudgetTooSmall,
    #[error("semantic TLVs require more than one independently decodable packet")]
    PacketSplitRequired,
}

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
    for tlv in &packet.tlvs {
        if !body.is_empty() && requires_fresh_next_hop_context(tlv, &context) {
            packets.push(finish_packet(std::mem::take(&mut body))?);
            context = EncodeContext::default();
        }
        let mut next_context = context.clone();
        let mut encoded = Vec::new();
        encode_outbound_tlv(tlv, &mut next_context, &mut encoded)?;
        if encoded.len() > body_budget {
            return Err(WireError::BodyTooLarge);
        }
        if !body.is_empty() && body.len() + encoded.len() > body_budget {
            packets.push(finish_packet(std::mem::take(&mut body))?);
            context = EncodeContext::default();
            next_context = context.clone();
            encoded.clear();
            encode_outbound_tlv(tlv, &mut next_context, &mut encoded)?;
            if encoded.len() > body_budget {
                return Err(WireError::BodyTooLarge);
            }
        }
        body.extend(encoded);
        context = next_context;
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

#[derive(Clone, Default)]
struct EncodeContext {
    router_id: Option<RouterId>,
    next_hop_v4: Option<IpAddr>,
    next_hop_v6: Option<IpAddr>,
}

fn encode_outbound_tlv(
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

fn require_nonzero(value: u16, type_: u8) -> Result<(), WireError> {
    if value == 0 {
        Err(WireError::InvalidTlv { type_ })
    } else {
        Ok(())
    }
}

fn require_zeroes(value: &[u8], type_: u8) -> Result<(), WireError> {
    if value.iter().any(|byte| *byte != 0) {
        Err(WireError::InvalidTlv { type_ })
    } else {
        Ok(())
    }
}

fn put_tlv(out: &mut Vec<u8>, type_: u8, value: &[u8]) -> Result<(), WireError> {
    if value.len() > u8::MAX as usize {
        return Err(WireError::BodyTooLarge);
    }
    out.extend([type_, value.len() as u8]);
    out.extend(value);
    Ok(())
}

fn decode_address(ae: AddressEncoding, data: &[u8]) -> Option<IpAddr> {
    match ae {
        AddressEncoding::Ipv4 => Some(IpAddr::V4(Ipv4Addr::from(<[u8; 4]>::try_from(data).ok()?))),
        AddressEncoding::Ipv6 => Some(IpAddr::V6(Ipv6Addr::from(<[u8; 16]>::try_from(data).ok()?))),
        AddressEncoding::Ipv6LinkLocal => {
            let mut raw = [0u8; 16];
            raw[0] = 0xfe;
            raw[1] = 0x80;
            raw[8..].copy_from_slice(data);
            Some(IpAddr::V6(Ipv6Addr::from(raw)))
        }
        _ => None,
    }
}

fn encode_address(address: IpAddr) -> (AddressEncoding, Vec<u8>) {
    match address {
        IpAddr::V4(value) => (AddressEncoding::Ipv4, value.octets().to_vec()),
        IpAddr::V6(value) if value.is_unicast_link_local() => {
            (AddressEncoding::Ipv6LinkLocal, value.octets()[8..].to_vec())
        }
        IpAddr::V6(value) => (AddressEncoding::Ipv6, value.octets().to_vec()),
    }
}

fn encode_optional_address(address: Option<IpAddr>) -> (AddressEncoding, Vec<u8>) {
    address
        .map(encode_address)
        .unwrap_or((AddressEncoding::Wildcard, vec![]))
}

fn prefix_encoded_len(ae: AddressEncoding, plen: u8) -> Option<usize> {
    if plen > ae.max_prefix_bits()? {
        return None;
    }
    let octets = usize::from(plen).div_ceil(8);
    match ae {
        AddressEncoding::Ipv6LinkLocal if plen >= 64 => Some(octets - 8),
        AddressEncoding::Ipv6LinkLocal => None,
        AddressEncoding::Wildcard if plen == 0 => Some(0),
        AddressEncoding::Wildcard => None,
        _ => Some(octets),
    }
}

fn decode_prefix(
    ae: AddressEncoding,
    plen: u8,
    omitted: usize,
    suffix: &[u8],
    previous: Option<&Vec<u8>>,
) -> Option<IpNet> {
    let encoded_len = prefix_encoded_len(ae, plen)?;
    if omitted > encoded_len || suffix.len() < encoded_len - omitted {
        return None;
    }
    let mut encoded = vec![0u8; encoded_len];
    if omitted > 0 {
        encoded[..omitted].copy_from_slice(&previous?[..omitted]);
    }
    encoded[omitted..].copy_from_slice(&suffix[..encoded_len - omitted]);
    if !plen.is_multiple_of(8) && !encoded.is_empty() {
        let mask = 0xff << (8 - plen % 8);
        *encoded.last_mut()? &= mask;
    }
    match ae {
        AddressEncoding::Ipv4 | AddressEncoding::Ipv4ViaIpv6 => {
            let mut raw = [0; 4];
            raw[..encoded.len()].copy_from_slice(&encoded);
            Some(IpNet::V4(Ipv4Net::new(Ipv4Addr::from(raw), plen).ok()?))
        }
        AddressEncoding::Ipv6 => {
            let mut raw = [0; 16];
            raw[..encoded.len()].copy_from_slice(&encoded);
            Some(IpNet::V6(Ipv6Net::new(Ipv6Addr::from(raw), plen).ok()?))
        }
        AddressEncoding::Ipv6LinkLocal => {
            let mut raw = [0; 16];
            raw[0] = 0xfe;
            raw[1] = 0x80;
            raw[8..8 + encoded.len()].copy_from_slice(&encoded);
            Some(IpNet::V6(Ipv6Net::new(Ipv6Addr::from(raw), plen).ok()?))
        }
        AddressEncoding::Wildcard => None,
    }
}

fn prefix_wire_bytes(prefix: IpNet, ae: AddressEncoding) -> Vec<u8> {
    let size = prefix_encoded_len(ae, prefix.prefix_len()).unwrap_or(0);
    match (prefix, ae) {
        (IpNet::V4(value), _) => value.addr().octets()[..size].to_vec(),
        (IpNet::V6(value), AddressEncoding::Ipv6LinkLocal) => {
            value.addr().octets()[8..8 + size].to_vec()
        }
        (IpNet::V6(value), _) => value.addr().octets()[..size].to_vec(),
    }
}

fn ae_for_key(key: &RouteKey, v4_via_v6: bool) -> AddressEncoding {
    if key.destination.addr().is_ipv4() {
        if v4_via_v6 {
            AddressEncoding::Ipv4ViaIpv6
        } else {
            AddressEncoding::Ipv4
        }
    } else {
        AddressEncoding::Ipv6
    }
}

fn router_id_from_prefix(prefix: IpNet) -> [u8; 8] {
    let bytes: Vec<u8> = match prefix {
        IpNet::V4(v) => v.addr().octets().to_vec(),
        IpNet::V6(v) => v.addr().octets().to_vec(),
    };
    let mut id = [0; 8];
    let count = bytes.len().min(8);
    id[8 - count..].copy_from_slice(&bytes[bytes.len() - count..]);
    id
}

fn be16(data: &[u8]) -> u16 {
    u16::from_be_bytes([data[0], data[1]])
}
fn be32(data: &[u8]) -> u32 {
    u32::from_be_bytes([data[0], data[1], data[2], data[3]])
}

#[cfg(test)]
mod tests {
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
}
