//! Public events, actions and configuration for the synchronous engine.

use super::*;

/// Delivery constraints for one semantic outbound Babel packet.
///
/// Times use the same monotonic millisecond clock as [`Event`].  The router
/// may aggregate this packet with compatible work and choose any send time up
/// to `max_jitter_ms` after enqueueing, but it must never intentionally send
/// later than `deadline_ms`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SendTiming {
    pub deadline_ms: u64,
    pub max_jitter_ms: u64,
}

impl SendTiming {
    pub const fn immediate(now_ms: u64) -> Self {
        Self {
            deadline_ms: now_ms,
            max_jitter_ms: 0,
        }
    }

    pub fn urgent(now_ms: u64) -> Self {
        Self {
            deadline_ms: now_ms.saturating_add(URGENT_TIMEOUT_MS),
            max_jitter_ms: URGENT_TIMEOUT_MS,
        }
    }

    pub fn triggered(now_ms: u64, hello_interval_cs: u16) -> Self {
        let deadline_delta = u64::from(hello_interval_cs).saturating_mul(5);
        Self {
            deadline_ms: now_ms.saturating_add(deadline_delta),
            max_jitter_ms: deadline_delta.min(MAX_TRIGGERED_JITTER_MS),
        }
    }

    pub fn by_deadline(now_ms: u64, deadline_ms: u64) -> Self {
        Self {
            deadline_ms,
            max_jitter_ms: deadline_ms.saturating_sub(now_ms),
        }
    }
}

/// Hysteresis for switching an established route to a better feasible candidate.
/// Both margins must be met. Initial discovery and loss of the current route
/// bypass this delay. Validate raw values with [`Self::validate`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RouteSelectionConfig {
    /// Required relative improvement, in 0..=100 percent (default 5).
    pub switch_margin_percent: u8,
    /// Required absolute improvement, below infinity (default 8).
    pub switch_margin_metric: u16,
    /// Continuous improvement dwell time in milliseconds; zero disables waiting (default 8000).
    pub better_for_ms: u64,
}

impl Default for RouteSelectionConfig {
    fn default() -> Self {
        Self {
            switch_margin_percent: 5,
            switch_margin_metric: 8,
            better_for_ms: 8_000,
        }
    }
}

/// Point-in-time adjacency observations and per-neighbor capacity counters.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NeighborStatus {
    pub interface: String,
    pub address: IpAddr,
    pub candidates: usize,
    pub rejected_candidates: u64,
    pub algorithm: String,
    pub hello_received: u16,
    pub hello_expected: u16,
    pub multicast_hello_history: u16,
    pub unicast_hello_history: u16,
    pub receive_cost: u16,
    pub transmit_cost: u16,
    pub link_cost: u16,
    pub last_rtt_us: Option<u32>,
    pub smoothed_rtt_us: Option<u32>,
    pub rtt_penalty: u16,
    pub last_hello_age_ms: u64,
}

/// Engine-wide defaults. Start with [`Self::recommended`], then use
/// [`Engine::try_new`] to validate overrides before creating state.
#[derive(Clone)]
pub struct EngineConfig {
    /// Admission limits for learned state; zero refuses all new entries of that kind.
    pub limits: ResourceLimits,
    /// Stable origin identity, validated by [`RouterId::new`].
    pub router_id: RouterId,
    /// Default per-neighbor metric factory (recommended default: wired 2-out-of-3).
    pub metric: Arc<dyn MetricProfile>,
    /// Extends an advertised route metric across a link (recommended default: addition).
    pub metric_algebra: Arc<dyn MetricAlgebra>,
    /// Initial local sequence number; every u16 value is valid. The host owns restart policy.
    pub sequence_number: u16,
    /// Default periodic Hello interval in centiseconds, nonzero (recommended: 400).
    pub hello_interval_cs: u16,
    /// Default periodic Update interval in centiseconds, nonzero (recommended: 1600).
    pub update_interval_cs: u16,
    /// Selection hysteresis, checked as part of [`Self::validate`].
    pub route_selection: RouteSelectionConfig,
}

/// Behaviour selected independently for one Babel interface.
#[derive(Clone)]
pub struct InterfacePolicy {
    /// Creates independent metric state for each neighbor on this interface.
    pub metric: Arc<dyn MetricProfile>,
    /// Periodic Hello interval in centiseconds, nonzero.
    pub hello_interval_cs: u16,
    /// Periodic Update interval in centiseconds, nonzero.
    pub update_interval_cs: u16,
    /// Suppress learned routes on their ingress interface when true.
    pub split_horizon: bool,
}

impl std::fmt::Debug for InterfacePolicy {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("InterfacePolicy")
            .field("metric", &self.metric.name())
            .field("hello_interval_cs", &self.hello_interval_cs)
            .field("update_interval_cs", &self.update_interval_cs)
            .field("split_horizon", &self.split_horizon)
            .finish()
    }
}

impl EngineConfig {
    /// Wired metric, additive algebra, 4 s Hellos, 16 s Updates and default limits/hysteresis.
    pub fn recommended(router_id: RouterId) -> Self {
        Self {
            router_id,
            limits: ResourceLimits::default(),
            metric: Arc::new(WiredMetric::default()),
            metric_algebra: Arc::new(AdditiveMetric),
            sequence_number: 0,
            hello_interval_cs: 400,
            update_interval_cs: 1600,
            route_selection: RouteSelectionConfig::default(),
        }
    }
}

/// One atomic engine input. All `now_ms` values share a nondecreasing monotonic
/// millisecond clock. The host supplies periodic [`Self::Tick`] events, even
/// while idle, and decodes received datagrams before delivering them.
#[derive(Clone, Debug)]
pub enum Event {
    /// Attach or replace an interface using engine-wide defaults.
    InterfaceUp {
        interface: String,
        local_addresses: Vec<IpAddr>,
        now_ms: u64,
    },
    /// Attach or replace an interface using an explicit, validated policy.
    InterfaceUpWithPolicy {
        interface: String,
        local_addresses: Vec<IpAddr>,
        policy: InterfacePolicy,
        now_ms: u64,
    },
    /// Update an active interface; `reset_metric` rebuilds metric state from observations.
    InterfacePolicyChanged {
        interface: String,
        policy: InterfacePolicy,
        reset_metric: bool,
        now_ms: u64,
    },
    /// Remove an interface and its neighbors/candidates, then reselect routes.
    InterfaceDown { interface: String, now_ms: u64 },
    /// A decoded packet on an active interface; hosts enforce transport admission.
    PacketReceived {
        interface: String,
        source: IpAddr,
        packet: Packet,
        now_ms: u64,
    },
    /// Advertise or replace a canonical local route with a finite metric (zero is valid).
    Originate {
        key: RouteKey,
        metric: u16,
        now_ms: u64,
    },
    /// Withdraw a local route; an absent key is a no-op.
    Withdraw { key: RouteKey, now_ms: u64 },
    /// Replace the complete locally originated route set as one engine event.
    ReplaceOrigins {
        origins: BTreeMap<RouteKey, u16>,
        now_ms: u64,
    },
    /// Advance periodic output and expiry; missed ticks may be coalesced.
    Tick { now_ms: u64 },
}

/// Ordered side effects for the host. Engine state has already changed when these return.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Action {
    /// Queue semantic output, preserving timing and repeating context at packet boundaries.
    Send {
        interface: String,
        destination: IpAddr,
        packet: OutboundPacket,
        timing: SendTiming,
    },
    /// Replace the complete learned RIB and exact unreachable set with this generation.
    RoutesChanged {
        generation: u64,
        routes: Vec<SelectedRoute>,
        unreachable: Vec<RouteKey>,
    },
    /// Local in-memory sequence changed; this is not a request for synchronous disk I/O.
    SequenceNumberChanged(u16),
}
