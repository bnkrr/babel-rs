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

/// Babel address encoding (AE), including RFC 9229 IPv4-with-IPv6-next-hop encoding.
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

/// Recognized sub-TLV payloads and retained unknown values. Timestamp values are microseconds.
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

/// Decoded inbound TLVs. Updates already contain their resolved packet context.
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

/// One decoded inbound packet. Obtain untrusted input through [`decode_packet`].
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Packet {
    pub tlvs: Vec<Tlv>,
}

/// A semantic outbound Update.  The packetizer emits the separate Router-ID
/// and Next-Hop context TLVs required by the Babel wire format.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OutboundUpdate {
    /// Canonical route key; `None` represents a wildcard retraction.
    pub key: Option<RouteKey>,
    /// Origin identity; required for finite advertisements, optional for retractions.
    pub router_id: Option<RouterId>,
    /// Explicit next hop, or `None` to use the packet sender.
    pub next_hop: Option<IpAddr>,
    /// Advertised Update interval in centiseconds.
    pub interval_cs: u16,
    /// Origin sequence number.
    pub seqno: u16,
    /// Advertised metric; [`INFINITY`] retracts the route.
    pub metric: u16,
    /// Encode IPv4 destinations with an IPv6 next hop using RFC 9229 AE 4.
    pub v4_via_v6: bool,
    /// Additional sub-TLVs; source prefixes are supplied from `key` by the encoder.
    pub sub_tlvs: Vec<SubTlv>,
}

/// Semantic outbound TLVs; the packetizer supplies Router-ID and Next-Hop context.
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

/// Semantic outbound work that may span several independently decodable datagrams.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct OutboundPacket {
    pub tlvs: Vec<OutboundTlv>,
}

/// Transport context for decoding one received datagram.
#[derive(Clone, Copy, Debug)]
pub struct DecodeContext {
    /// Real sender address, used as the default next hop during context resolution.
    pub source: IpAddr,
}

/// Invalid packet framing, TLV content or outbound packet budget.
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

#[cfg(test)]
mod tests;

mod decode;
mod encode;
mod packetizer;
mod prefix;
pub use decode::decode_packet;
pub use packetizer::{encode_packet, encode_packets, stamp_hello_timestamps};
use prefix::*;
