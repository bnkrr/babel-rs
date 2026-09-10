#![forbid(unsafe_code)]

//! Babel protocol state and packet encoding without sockets, tasks or clocks.
//!
//! Start from [`EngineConfig::recommended`]. Use [`Engine::try_new`] and
//! [`Engine::try_handle`] at fallible input boundaries; invalid local changes
//! return [`ConfigError`] before mutating state. Public struct fields remain
//! editable, and their `validate()` methods provide side-effect-free checks.
//!
//! ```
//! use babel_protocol::{ConfigError, Engine, EngineConfig, Event, RouteKey, RouterId};
//! # fn main() -> Result<(), ConfigError> {
//! let id = RouterId::new([1; 8]).expect("valid router-id");
//! let mut engine = Engine::try_new(EngineConfig::recommended(id))?;
//! let key = RouteKey::new("2001:db8::1/64".parse().unwrap(), None).unwrap();
//! // The constructor normalizes the destination to 2001:db8::/64.
//! assert_eq!(key.destination.to_string(), "2001:db8::/64");
//! let actions = engine.try_handle(Event::Originate { key, metric: 0, now_ms: 0 })?;
//! # let _ = actions;
//! // Attach interfaces, deliver decoded packets and advance Tick on the same clock.
//! # Ok(())
//! # }
//! ```
//!
//! Decode datagrams with [`decode_packet`] before delivering [`Event::PacketReceived`].
//! Apply returned [`Action`]s in order. Use [`encode_packets`] for [`Action::Send`]
//! with the interface's UDP payload budget; stamp RFC 9616 Hello timestamps
//! immediately before transmission. The host owns transport admission, scheduling,
//! route export, identity/checkpoint storage and shutdown policy.

/// Engine configuration, events, actions and synchronous protocol state.
pub mod engine;
mod limits;
/// RFC 8967 authentication and RFC 9467 replay protection, before normal decoding.
pub mod mac;
pub mod policy;
pub use policy::{AllowAllRoutes, ExportContext, ImportContext, RoutePolicy};
mod validation;
pub use validation::ConfigError;
/// Link-quality profiles, Hello histories and metric composition.
pub mod metric;
/// Canonical routing keys, identities, distances and selected routes.
pub mod model;
/// Validated inbound codec and semantic outbound packetizer.
pub mod wire;

pub use engine::{
    Action, ControlTransport, Engine, EngineConfig, Event, InterfacePolicy, Ipv4NextHop,
    NeighborStatus, RouteSelectionConfig, SendTiming,
};
pub use limits::{ResourceLimits, ResourceStatus};
pub use metric::{
    AdditiveMetric, EtxMetric, HelloHistories, HelloHistory, HelloHistoryUpdate, MetricAlgebra,
    MetricProfile, MetricStatus, NeighborMetric, RttMetric, WiredMetric,
};
pub use model::*;
pub use wire::{
    DEFAULT_UDP_PAYLOAD_SIZE, DecodeContext, OutboundPacket, OutboundTlv, OutboundUpdate, Packet,
    ResolvedUpdate, SubTlv, Tlv, WireError, decode_packet, encode_packet, encode_packets,
    stamp_hello_timestamps,
};
