use std::collections::{BTreeMap, HashMap, HashSet};
use std::net::{IpAddr, Ipv6Addr};
use std::sync::Arc;

use crate::{ResourceLimits, ResourceStatus};

use crate::metric::{
    AdditiveMetric, HelloHistories, HelloHistoryUpdate, MetricAlgebra, MetricProfile,
    NeighborMetric, WiredMetric,
};
use crate::model::{Distance, INFINITY, RouteKey, RouterId, SelectedRoute, seqno_gt};
use crate::wire::{
    OutboundPacket, OutboundTlv, OutboundUpdate, Packet, ResolvedUpdate, SubTlv, Tlv,
};

pub const BABEL_MULTICAST_V6: IpAddr = IpAddr::V6(Ipv6Addr::new(0xff02, 0, 0, 0, 0, 0, 1, 6));
const SOURCE_GC_TIME_MS: u64 = 180_000;
const MAX_RTT_PROBES_PER_TICK: usize = 32;
const REQUEST_RETRY_INITIAL_MS: u64 = 2_000;
const REQUEST_RETRIES: u8 = 3;
const REQUEST_HOP_COUNT: u8 = 64;
const RECENT_REQUEST_MS: u64 = 16_000;
const URGENT_TIMEOUT_MS: u64 = 20;
const MAX_TRIGGERED_JITTER_MS: u64 = 100;

mod api;
mod interfaces;
mod neighbors;
mod output;
mod rib;
mod sources;
mod timers;

pub use api::{
    Action, EngineConfig, Event, InterfacePolicy, NeighborStatus, RouteSelectionConfig, SendTiming,
};
use sources::seqno_request_action;
use timers::{
    initial_probe_deadline, next_periodic_deadline, recurring_probe_deadline, timestamp_us,
};

#[derive(Clone, Debug)]
struct InterfaceState {
    local_addresses: Vec<IpAddr>,
    policy: InterfacePolicy,
    hello_seqno: u16,
    next_hello_ms: u64,
    next_update_ms: u64,
    last_full_update_ms: Option<u64>,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct NeighborKey {
    interface: String,
    address: IpAddr,
}

struct Neighbor {
    candidates: usize,
    rejected_candidates: u64,
    last_hello_ms: u64,
    histories: HelloHistories,
    multicast_timer: Option<HelloTimer>,
    unicast_timer: Option<HelloTimer>,
    last_ihu_ms: Option<u64>,
    last_ihu_cost: Option<u16>,
    ihu_interval_cs: u16,
    next_ihu_ms: u64,
    next_rtt_probe_ms: Option<u64>,
    origin_timestamp: Option<u32>,
    receive_timestamp: Option<u32>,
    unicast_hello_seqno: u16,
    metric: Box<dyn NeighborMetric>,
}

#[derive(Clone, Copy, Debug)]
struct HelloTimer {
    interval_cs: u16,
    next_expiry_ms: u64,
}

#[derive(Clone, Debug)]
struct Candidate {
    key: RouteKey,
    router_id: RouterId,
    seqno: u16,
    advertised_metric: u16,
    metric: u16,
    next_hop: IpAddr,
    interface: String,
    interval_cs: u16,
    expires_ms: u64,
    refresh_requested: bool,
}

#[derive(Clone, Debug)]
struct Originated {
    metric: u16,
    seqno: u16,
}

#[derive(Clone, Copy, Debug)]
struct SourceEntry {
    distance: Distance,
    expires_ms: u64,
}

#[derive(Clone, Debug)]
struct PendingSeqnoRequest {
    seqno: u16,
    hop_count: u8,
    next_hop: NeighborKey,
    requester: Option<NeighborKey>,
    retries_left: u8,
    next_retry_ms: u64,
}

struct SeqnoRequestSpec {
    key: RouteKey,
    router_id: RouterId,
    seqno: u16,
    hop_count: u8,
    next_hop: NeighborKey,
    requester: Option<NeighborKey>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PendingSwitch {
    router_id: RouterId,
    next_hop: IpAddr,
    interface: String,
    since_ms: u64,
    current_peak_metric: u16,
}

/// Synchronous owner of all protocol state. It performs no I/O and reads no clock.
/// Drive it with [`Self::try_handle`] and execute the resulting [`Action`]s in order.
pub struct Engine {
    resources: ResourceStatus,
    config: EngineConfig,
    interfaces: BTreeMap<String, InterfaceState>,
    neighbours: HashMap<NeighborKey, Neighbor>,
    candidates: HashMap<(RouteKey, NeighborKey), Candidate>,
    feasible: HashMap<(RouteKey, RouterId), SourceEntry>,
    originated: BTreeMap<RouteKey, Originated>,
    selected: BTreeMap<RouteKey, SelectedRoute>,
    pending_seqno: HashMap<(RouteKey, RouterId), PendingSeqnoRequest>,
    recent_seqno: HashMap<(RouteKey, RouterId), (u16, u64)>,
    tombstones: BTreeMap<RouteKey, u64>,
    pending_switches: HashMap<RouteKey, PendingSwitch>,
    settling_since: HashMap<RouteKey, u64>,
    settled_routes: HashSet<RouteKey>,
    generation: u64,
    sequence_number: u16,
}

impl Engine {
    /// Create an engine from known-valid configuration.
    ///
    /// # Panics
    /// Panics on invalid configuration. Use [`Self::try_new`] for user input.
    pub fn new(config: EngineConfig) -> Self {
        Self::try_new(config).expect("invalid Babel engine configuration")
    }

    /// Validate configuration and create an engine without performing I/O.
    pub fn try_new(config: EngineConfig) -> Result<Self, crate::ConfigError> {
        config.validate()?;
        Ok(Self {
            resources: ResourceStatus {
                limits: config.limits,
                ..ResourceStatus::default()
            },
            sequence_number: config.sequence_number,
            config,
            interfaces: BTreeMap::new(),
            neighbours: HashMap::new(),
            candidates: HashMap::new(),
            feasible: HashMap::new(),
            originated: BTreeMap::new(),
            selected: BTreeMap::new(),
            pending_seqno: HashMap::new(),
            recent_seqno: HashMap::new(),
            tombstones: BTreeMap::new(),
            pending_switches: HashMap::new(),
            settling_since: HashMap::new(),
            settled_routes: HashSet::new(),
            generation: 0,
        })
    }

    /// Current local sequence number, including changes made by the last event.
    pub fn sequence_number(&self) -> u16 {
        self.sequence_number
    }

    /// Apply a known-valid event and return its ordered side effects.
    ///
    /// # Panics
    /// Panics on invalid local policy/origin input. Use [`Self::try_handle`]
    /// for user input. Decode received packets with [`crate::decode_packet`].
    pub fn handle(&mut self, event: Event) -> Vec<Action> {
        self.try_handle(event).expect("invalid local Babel event")
    }

    /// Validate an event before applying it. An error leaves all state unchanged.
    /// Supply nondecreasing monotonic milliseconds from one clock for all events.
    pub fn try_handle(&mut self, event: Event) -> Result<Vec<Action>, crate::ConfigError> {
        event.validate()?;
        Ok(self.handle_validated(event))
    }

    fn handle_validated(&mut self, event: Event) -> Vec<Action> {
        match event {
            Event::InterfaceUp {
                interface,
                local_addresses,
                now_ms,
            } => {
                let policy = self.default_interface_policy();
                self.interface_up(interface, local_addresses, policy, now_ms)
            }
            Event::InterfaceUpWithPolicy {
                interface,
                local_addresses,
                policy,
                now_ms,
            } => self.interface_up(interface, local_addresses, policy, now_ms),
            Event::InterfacePolicyChanged {
                interface,
                policy,
                reset_metric,
                now_ms,
            } => self.interface_policy_changed(interface, policy, reset_metric, now_ms),
            Event::InterfaceDown { interface, now_ms } => {
                self.interfaces.remove(&interface);
                self.neighbours.retain(|key, _| key.interface != interface);
                self.candidates
                    .retain(|_, value| value.interface != interface);
                self.reselect(now_ms)
            }
            Event::PacketReceived {
                interface,
                source,
                packet,
                now_ms,
            } => self.receive(interface, source, packet, now_ms),
            Event::Originate {
                key,
                metric,
                now_ms,
            } => {
                let mut actions = Vec::new();
                if let Some(entry) = self.originated.get(&key)
                    && entry.metric != metric
                {
                    self.bump_sequence_number();
                    actions.push(Action::SequenceNumberChanged(self.sequence_number));
                }
                self.originated.insert(
                    key,
                    Originated {
                        metric,
                        seqno: self.sequence_number,
                    },
                );
                actions.extend(self.reselect(now_ms));
                actions.extend(self.send_updates(now_ms, Some(key), None, None));
                actions
            }
            Event::Withdraw { key, now_ms } => {
                let existed = self.originated.remove(&key).is_some();
                let mut actions = self.reselect(now_ms);
                if existed {
                    self.bump_sequence_number();
                    actions.insert(0, Action::SequenceNumberChanged(self.sequence_number));
                    actions.extend(self.send_retraction(key, self.sequence_number, now_ms));
                }
                actions
            }
            Event::ReplaceOrigins { origins, now_ms } => self.replace_origins(origins, now_ms),
            Event::Tick { now_ms } => self.tick(now_ms),
        }
    }

    /// Clone the complete selected learned RIB; local origins are advertised separately.
    pub fn selected_routes(&self) -> Vec<SelectedRoute> {
        self.selected.values().cloned().collect()
    }

    /// Exact destinations retained as unreachable during the withdrawal hold time.
    pub fn unreachable_routes(&self) -> Vec<RouteKey> {
        self.tombstones.keys().copied().collect()
    }

    /// Current learned-state occupancy and cumulative admission rejections.
    pub fn resource_status(&self) -> ResourceStatus {
        ResourceStatus {
            candidates: self.candidates.len(),
            sources: self.feasible.len(),
            pending_requests: self.pending_seqno.len(),
            unreachable: self.tombstones.len(),
            ..self.resources.clone()
        }
    }

    /// Name of the default metric profile; individual interfaces may override it.
    pub fn metric_name(&self) -> String {
        self.config.metric.name()
    }

    fn replace_origins(&mut self, origins: BTreeMap<RouteKey, u16>, now_ms: u64) -> Vec<Action> {
        let unchanged = self.originated.len() == origins.len()
            && origins.iter().all(|(key, metric)| {
                self.originated
                    .get(key)
                    .is_some_and(|origin| origin.metric == *metric)
            });
        if unchanged {
            return Vec::new();
        }

        let removed: Vec<_> = self
            .originated
            .keys()
            .filter(|key| !origins.contains_key(key))
            .copied()
            .collect();
        let changes_existing = self.originated.iter().any(|(key, origin)| {
            origins
                .get(key)
                .is_none_or(|metric| *metric != origin.metric)
        });
        let mut actions = Vec::new();
        if changes_existing {
            self.bump_sequence_number();
            actions.push(Action::SequenceNumberChanged(self.sequence_number));
        }
        self.originated = origins
            .into_iter()
            .map(|(key, metric)| {
                (
                    key,
                    Originated {
                        metric,
                        seqno: self.sequence_number,
                    },
                )
            })
            .collect();
        for key in removed {
            actions.extend(self.send_retraction(key, self.sequence_number, now_ms));
        }
        actions.extend(self.reselect(now_ms));
        actions.extend(self.send_updates(now_ms, None, None, None));
        actions
    }
}

#[cfg(test)]
mod tests;
