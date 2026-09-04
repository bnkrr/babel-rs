#![forbid(unsafe_code)]

pub mod engine;
pub mod metric;
pub mod model;
pub mod wire;

pub use engine::{Action, Engine, EngineConfig, Event, NeighborStatus, RouteSelectionConfig};
pub use metric::{
    AdditiveMetric, EtxMetric, HelloHistories, HelloHistory, HelloHistoryUpdate, MetricAlgebra,
    MetricProfile, MetricStatus, NeighborMetric, RttMetric, WiredMetric,
};
pub use model::*;
pub use wire::{
    DEFAULT_UDP_PAYLOAD_SIZE, DecodeContext, OutboundPacket, OutboundTlv, OutboundUpdate, Packet,
    ResolvedUpdate, SubTlv, Tlv, WireError, decode_packet, encode_packet, encode_packets,
};
