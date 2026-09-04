use std::collections::{BTreeMap, HashMap, HashSet};
use std::net::{IpAddr, Ipv6Addr};
use std::sync::Arc;

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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RouteSelectionConfig {
    pub switch_margin_percent: u8,
    pub switch_margin_metric: u16,
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NeighborStatus {
    pub interface: String,
    pub address: IpAddr,
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

#[derive(Clone)]
pub struct EngineConfig {
    pub router_id: RouterId,
    pub metric: Arc<dyn MetricProfile>,
    pub metric_algebra: Arc<dyn MetricAlgebra>,
    pub sequence_number: u16,
    pub hello_interval_cs: u16,
    pub update_interval_cs: u16,
    pub route_selection: RouteSelectionConfig,
}

impl EngineConfig {
    pub fn recommended(router_id: RouterId) -> Self {
        Self {
            router_id,
            metric: Arc::new(WiredMetric::default()),
            metric_algebra: Arc::new(AdditiveMetric),
            sequence_number: 0,
            hello_interval_cs: 400,
            update_interval_cs: 1600,
            route_selection: RouteSelectionConfig::default(),
        }
    }
}

#[derive(Clone, Debug)]
pub enum Event {
    InterfaceUp {
        interface: String,
        local_addresses: Vec<IpAddr>,
        now_ms: u64,
    },
    InterfaceDown {
        interface: String,
        now_ms: u64,
    },
    PacketReceived {
        interface: String,
        source: IpAddr,
        packet: Packet,
        now_ms: u64,
    },
    Originate {
        key: RouteKey,
        metric: u16,
        now_ms: u64,
    },
    Withdraw {
        key: RouteKey,
        now_ms: u64,
    },
    /// Replace the complete locally originated route set as one engine event.
    ReplaceOrigins {
        origins: BTreeMap<RouteKey, u16>,
        now_ms: u64,
    },
    Tick {
        now_ms: u64,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Action {
    Send {
        interface: String,
        destination: IpAddr,
        packet: OutboundPacket,
    },
    RoutesChanged {
        generation: u64,
        routes: Vec<SelectedRoute>,
        unreachable: Vec<RouteKey>,
    },
    SequenceNumberChanged(u16),
}

#[derive(Clone, Debug)]
struct InterfaceState {
    local_addresses: Vec<IpAddr>,
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
    last_hello_ms: u64,
    histories: HelloHistories,
    multicast_timer: Option<HelloTimer>,
    unicast_timer: Option<HelloTimer>,
    last_ihu_ms: Option<u64>,
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

pub struct Engine {
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
    pub fn new(config: EngineConfig) -> Self {
        Self {
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
        }
    }

    pub fn handle(&mut self, event: Event) -> Vec<Action> {
        match event {
            Event::InterfaceUp {
                interface,
                local_addresses,
                now_ms,
            } => {
                self.interfaces
                    .entry(interface.clone())
                    .or_insert(InterfaceState {
                        local_addresses,
                        hello_seqno: 0,
                        next_hello_ms: now_ms,
                        next_update_ms: now_ms,
                        last_full_update_ms: None,
                    });
                self.tick(now_ms)
            }
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
                actions.extend(self.send_updates(now_ms, Some(key), None));
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

    pub fn selected_routes(&self) -> Vec<SelectedRoute> {
        self.selected.values().cloned().collect()
    }

    pub fn unreachable_routes(&self) -> Vec<RouteKey> {
        self.tombstones.keys().copied().collect()
    }

    fn interface_has_ipv4(&self, interface: &str) -> bool {
        self.interfaces
            .get(interface)
            .is_some_and(|state| state.local_addresses.iter().any(IpAddr::is_ipv4))
    }

    pub fn neighbour_count(&self) -> usize {
        self.neighbours.len()
    }

    pub fn metric_name(&self) -> String {
        self.config.metric.name()
    }

    pub fn neighbour_status(&self, now_ms: u64) -> Vec<NeighborStatus> {
        let mut result: Vec<_> = self
            .neighbours
            .iter()
            .map(|(key, neighbour)| {
                let metric = neighbour.metric.status();
                NeighborStatus {
                    interface: key.interface.clone(),
                    address: key.address,
                    algorithm: metric.algorithm,
                    hello_received: u16::from(
                        neighbour.histories.multicast.received(16)
                            + neighbour.histories.unicast.received(16),
                    ),
                    hello_expected: u16::from(
                        neighbour.histories.multicast.observed()
                            + neighbour.histories.unicast.observed(),
                    ),
                    multicast_hello_history: neighbour.histories.multicast.bits(),
                    unicast_hello_history: neighbour.histories.unicast.bits(),
                    receive_cost: metric.receive_cost,
                    transmit_cost: metric.transmit_cost,
                    link_cost: metric.link_cost,
                    last_rtt_us: metric.last_rtt_us,
                    smoothed_rtt_us: metric.smoothed_rtt_us,
                    rtt_penalty: metric.rtt_penalty,
                    last_hello_age_ms: now_ms.saturating_sub(neighbour.last_hello_ms),
                }
            })
            .collect();
        result.sort_by(|left, right| {
            (&left.interface, left.address).cmp(&(&right.interface, right.address))
        });
        result
    }

    fn receive(
        &mut self,
        interface: String,
        source: IpAddr,
        packet: Packet,
        now_ms: u64,
    ) -> Vec<Action> {
        let Some(interface_state) = self.interfaces.get(&interface) else {
            return Vec::new();
        };
        let local_addresses = interface_state.local_addresses.clone();
        let neighbour_key = NeighborKey {
            interface: interface.clone(),
            address: source,
        };
        let mut actions = Vec::new();
        let mut changed_router_ids = HashSet::new();
        let receive_timestamp = timestamp_us(now_ms);
        let hello_timestamp = packet.tlvs.iter().find_map(|tlv| match tlv {
            Tlv::Hello { sub_tlvs, .. } => timestamp_hello(sub_tlvs),
            _ => None,
        });
        let echoed_timestamps = packet.tlvs.iter().find_map(|tlv| match tlv {
            Tlv::Ihu {
                address, sub_tlvs, ..
            } if ihu_applies(*address, &local_addresses) => timestamp_ihu(sub_tlvs),
            _ => None,
        });
        let rtt_sample = hello_timestamp.zip(echoed_timestamps).and_then(
            |(peer_sent, (origin, peer_received))| {
                valid_rtt_sample(receive_timestamp, origin, peer_sent, peer_received)
            },
        );

        let mut send_ihu = false;
        for tlv in &packet.tlvs {
            if let Tlv::Hello {
                unicast,
                seqno,
                interval_cs,
                sub_tlvs,
            } = tlv
            {
                let neighbour = self
                    .neighbours
                    .entry(neighbour_key.clone())
                    .or_insert_with(|| Neighbor {
                        last_hello_ms: now_ms,
                        histories: HelloHistories::default(),
                        multicast_timer: None,
                        unicast_timer: None,
                        last_ihu_ms: None,
                        ihu_interval_cs: self.config.hello_interval_cs.saturating_mul(3),
                        next_ihu_ms: now_ms,
                        next_rtt_probe_ms: self.config.metric.rtt_probe_interval_ms().map(
                            |interval| initial_probe_deadline(now_ms, interval, &neighbour_key),
                        ),
                        origin_timestamp: None,
                        receive_timestamp: None,
                        unicast_hello_seqno: 0,
                        metric: self.config.metric.new_neighbor(&interface),
                    });
                let previous_receive_cost = neighbour.metric.receive_cost();
                let update = (*interval_cs != 0).then(|| {
                    if *unicast {
                        &mut neighbour.histories.unicast
                    } else {
                        &mut neighbour.histories.multicast
                    }
                    .record(*seqno)
                });
                if update == Some(HelloHistoryUpdate::Restarted) {
                    let mut histories = HelloHistories::default();
                    if *interval_cs != 0 {
                        if *unicast {
                            histories.unicast.record(*seqno);
                        } else {
                            histories.multicast.record(*seqno);
                        }
                    }
                    neighbour.histories = histories;
                    neighbour.multicast_timer = None;
                    neighbour.unicast_timer = None;
                    neighbour.last_ihu_ms = None;
                    neighbour.next_ihu_ms = now_ms;
                    neighbour.next_rtt_probe_ms =
                        self.config.metric.rtt_probe_interval_ms().map(|interval| {
                            initial_probe_deadline(now_ms, interval, &neighbour_key)
                        });
                    neighbour.origin_timestamp = None;
                    neighbour.receive_timestamp = None;
                    neighbour.unicast_hello_seqno = 0;
                    neighbour.metric = self.config.metric.new_neighbor(&interface);
                }
                neighbour.metric.on_hello(neighbour.histories);
                neighbour.last_hello_ms = now_ms;
                if *interval_cs != 0 {
                    let timer = HelloTimer {
                        interval_cs: *interval_cs,
                        next_expiry_ms: now_ms
                            .saturating_add(u64::from(*interval_cs).saturating_mul(15)),
                    };
                    if *unicast {
                        neighbour.unicast_timer = Some(timer);
                    } else {
                        neighbour.multicast_timer = Some(timer);
                    }
                }
                if let Some(timestamp) = timestamp_hello(sub_tlvs) {
                    neighbour.origin_timestamp = Some(timestamp);
                    neighbour.receive_timestamp = Some(receive_timestamp);
                }
                if neighbour.metric.receive_cost() != previous_receive_cost
                    || now_ms >= neighbour.next_ihu_ms
                {
                    send_ihu = true;
                }
            }
        }
        if let Some(sample) = rtt_sample
            && self.config.metric.timestamps_enabled()
            && let Some(neighbour) = self.neighbours.get_mut(&neighbour_key)
        {
            neighbour.metric.on_rtt_sample(sample, now_ms);
        }
        for tlv in &packet.tlvs {
            if let Tlv::Ihu {
                address,
                rxcost,
                interval_cs,
                ..
            } = tlv
                && *interval_cs != 0
                && ihu_applies(*address, &local_addresses)
                && let Some(neighbour) = self.neighbours.get_mut(&neighbour_key)
            {
                neighbour.metric.on_ihu(*rxcost);
                neighbour.last_ihu_ms = Some(now_ms);
                neighbour.ihu_interval_cs = *interval_cs;
            }
        }
        self.recompute_candidate_metrics(Some(&neighbour_key));
        if send_ihu && let Some(action) = self.ihu_action(&neighbour_key, now_ms) {
            actions.push(action);
        }

        for tlv in packet.tlvs {
            match tlv {
                Tlv::AckRequest { nonce, .. } => actions.push(Action::Send {
                    interface: interface.clone(),
                    destination: source,
                    packet: OutboundPacket {
                        tlvs: vec![OutboundTlv::Ack { nonce }],
                    },
                }),
                Tlv::Hello { .. } | Tlv::Ihu { .. } => {}
                Tlv::Update(update) => {
                    if update.metric < INFINITY
                        && let (Some(key), Some(router_id)) = (update.key, update.router_id)
                        && self
                            .candidates
                            .get(&(key, neighbour_key.clone()))
                            .is_some_and(|candidate| candidate.router_id != router_id)
                    {
                        changed_router_ids.insert(key);
                    }
                    actions.extend(self.receive_update(&neighbour_key, update, now_ms))
                }
                Tlv::RouteRequest { key, .. } => {
                    if key.is_some() || self.full_update_request_allowed(&interface, now_ms) {
                        let updates = self.send_updates(now_ms, key, Some(interface.clone()));
                        if let Some(key) = key.filter(|_| updates.is_empty()) {
                            actions.push(self.retraction_action(
                                key,
                                self.sequence_number,
                                neighbour_key.clone(),
                            ));
                        } else {
                            actions.extend(updates);
                        }
                    }
                }
                Tlv::SeqnoRequest {
                    key,
                    seqno,
                    hop_count,
                    router_id,
                    ..
                } => {
                    actions.extend(self.handle_seqno_request(
                        neighbour_key.clone(),
                        key,
                        seqno,
                        hop_count,
                        router_id,
                        now_ms,
                    ));
                }
                _ => {}
            }
        }
        actions.extend(self.reselect(now_ms));
        // RFC 8966 section 3.5.3 requires a timely triggered update whenever
        // an existing route entry changes Router-ID, even when that entry is
        // not selected and route selection itself therefore did not change.
        for key in changed_router_ids {
            if self.originated.contains_key(&key) || self.selected.contains_key(&key) {
                actions.extend(self.send_updates(now_ms, Some(key), None));
            } else {
                actions.extend(self.send_retraction(key, self.sequence_number, now_ms));
            }
        }
        actions
    }

    fn ihu_action(&mut self, key: &NeighborKey, now_ms: u64) -> Option<Action> {
        let neighbour = self.neighbours.get_mut(key)?;
        let receive_cost = valid_cost(neighbour.metric.receive_cost());
        let echoed = neighbour.origin_timestamp.zip(neighbour.receive_timestamp);
        let interval_cs = self.config.hello_interval_cs.saturating_mul(3);
        neighbour.next_ihu_ms = now_ms.saturating_add(u64::from(interval_cs) * 10);
        let send_probe = neighbour
            .next_rtt_probe_ms
            .is_some_and(|deadline| now_ms >= deadline);
        if send_probe {
            neighbour.unicast_hello_seqno = neighbour.unicast_hello_seqno.wrapping_add(1);
            neighbour.next_rtt_probe_ms =
                self.config.metric.rtt_probe_interval_ms().map(|interval| {
                    recurring_probe_deadline(now_ms, interval, key, neighbour.unicast_hello_seqno)
                });
        }
        let mut tlvs = Vec::new();
        if send_probe || (self.config.metric.timestamps_enabled() && echoed.is_some()) {
            if !send_probe {
                neighbour.unicast_hello_seqno = neighbour.unicast_hello_seqno.wrapping_add(1);
            }
            tlvs.push(OutboundTlv::Hello {
                unicast: true,
                seqno: neighbour.unicast_hello_seqno,
                interval_cs: 0,
                sub_tlvs: vec![SubTlv::TimestampHello(timestamp_us(now_ms))],
            });
        }
        let sub_tlvs = if self.config.metric.timestamps_enabled() {
            echoed.map_or_else(Vec::new, |(origin, received)| {
                vec![SubTlv::TimestampIhu { origin, received }]
            })
        } else {
            Vec::new()
        };
        tlvs.push(OutboundTlv::Ihu {
            address: None,
            rxcost: receive_cost,
            interval_cs,
            sub_tlvs,
        });
        Some(Action::Send {
            interface: key.interface.clone(),
            destination: key.address,
            packet: OutboundPacket { tlvs },
        })
    }

    fn receive_update(
        &mut self,
        neighbour_key: &NeighborKey,
        update: ResolvedUpdate,
        now_ms: u64,
    ) -> Vec<Action> {
        let Some(key) = update.key else {
            if update.metric == INFINITY {
                for ((_, neighbour), candidate) in &mut self.candidates {
                    if neighbour == neighbour_key {
                        candidate.advertised_metric = INFINITY;
                        candidate.metric = INFINITY;
                    }
                }
            }
            return Vec::new();
        };
        if forbidden_destination(key.destination) {
            return Vec::new();
        }
        let candidate_key = (key, neighbour_key.clone());
        if update.metric == INFINITY {
            if let Some(candidate) = self.candidates.get_mut(&candidate_key) {
                candidate.advertised_metric = INFINITY;
                candidate.metric = INFINITY;
            }
            return Vec::new();
        }
        let Some(router_id) = update.router_id else {
            return Vec::new();
        };
        // Multicast loopback varies across kernels and network namespaces.
        // A Router-ID identifies an originating Babel speaker, so accepting our
        // own Update can only manufacture a route back through ourselves.
        if router_id == self.config.router_id {
            return Vec::new();
        }
        let Some(next_hop) = update.next_hop else {
            return Vec::new();
        };
        let Some(neighbour) = self.neighbours.get(neighbour_key) else {
            return Vec::new();
        };
        let cost = neighbour.metric.link_cost();
        let metric = self.config.metric_algebra.extend(update.metric, cost);
        if metric != INFINITY && metric <= update.metric {
            return Vec::new();
        }
        let distance = Distance {
            seqno: update.seqno,
            metric: update.metric,
        };
        let feasible = self
            .feasible
            .get(&(key, router_id))
            .map(|entry| entry.distance);
        let is_feasible = feasible.is_none_or(|fd| distance.feasible_against(fd));
        if !is_feasible
            && self.selected.get(&key).is_some_and(|selected| {
                selected.router_id == router_id
                    && selected.interface == neighbour_key.interface
                    && selected.next_hop == next_hop
            })
        {
            return feasible.map_or_else(Vec::new, |fd| {
                self.originate_seqno_request(
                    SeqnoRequestSpec {
                        key,
                        router_id,
                        seqno: fd.seqno.wrapping_add(1),
                        hop_count: REQUEST_HOP_COUNT,
                        next_hop: neighbour_key.clone(),
                        requester: None,
                    },
                    now_ms,
                )
            });
        }
        let mut actions = Vec::new();
        if is_feasible {
            let satisfied: Vec<_> = self
                .pending_seqno
                .iter()
                .filter(|((pending_key, pending_router), pending)| {
                    *pending_key == key
                        && (*pending_router != router_id
                            || pending.seqno == update.seqno
                            || seqno_gt(update.seqno, pending.seqno))
                })
                .map(|(pending_key, _)| *pending_key)
                .collect();
            for pending_key in satisfied {
                if let Some(pending) = self.pending_seqno.remove(&pending_key) {
                    if let Some(requester) = pending.requester {
                        actions.push(self.update_action_to_candidate(
                            key,
                            router_id,
                            update.seqno,
                            metric,
                            requester,
                        ));
                    }
                    self.recent_seqno.insert(
                        pending_key,
                        (pending.seqno, now_ms.saturating_add(RECENT_REQUEST_MS)),
                    );
                }
            }
        } else if !self.pending_seqno.contains_key(&(key, router_id))
            && let Some(fd) = feasible
        {
            actions.extend(self.originate_seqno_request(
                SeqnoRequestSpec {
                    key,
                    router_id,
                    seqno: fd.seqno.wrapping_add(1),
                    hop_count: REQUEST_HOP_COUNT,
                    next_hop: neighbour_key.clone(),
                    requester: None,
                },
                now_ms,
            ));
        }
        self.candidates.insert(
            candidate_key,
            Candidate {
                key,
                router_id,
                seqno: update.seqno,
                advertised_metric: update.metric,
                metric,
                next_hop,
                interface: neighbour_key.interface.clone(),
                interval_cs: update.interval_cs,
                expires_ms: now_ms.saturating_add(u64::from(update.interval_cs) * 35),
                refresh_requested: false,
            },
        );
        actions
    }

    fn handle_seqno_request(
        &mut self,
        requester: NeighborKey,
        key: RouteKey,
        seqno: u16,
        hop_count: u8,
        router_id: RouterId,
        now_ms: u64,
    ) -> Vec<Action> {
        if let Some(route) = self.selected.get(&key).cloned()
            && (route.router_id != router_id
                || route.seqno == seqno
                || seqno_gt(route.seqno, seqno))
        {
            return vec![self.update_action_to_candidate(
                route.key,
                route.router_id,
                route.seqno,
                route.metric,
                requester,
            )];
        }

        if router_id == self.config.router_id
            && let Some(origin) = self.originated.get(&key).cloned()
        {
            let mut actions = Vec::new();
            if seqno_gt(seqno, origin.seqno) {
                // RFC 8966 section 3.8.1.2 permits at most one increment in
                // reaction to a single request, even if it asks far ahead.
                self.bump_sequence_number();
                actions.push(Action::SequenceNumberChanged(self.sequence_number));
            }
            // Propagate the new source sequence on every interface. Limiting
            // this reply to the requesting interface lets parallel paths stay
            // one sequence behind and can perpetuate starvation.
            actions.extend(self.send_updates(now_ms, Some(key), None));
            return actions;
        }

        if hop_count <= 1 {
            return Vec::new();
        }

        let pending_key = (key, router_id);
        if self
            .pending_seqno
            .get(&pending_key)
            .is_some_and(|pending| pending.seqno == seqno || seqno_gt(pending.seqno, seqno))
        {
            return Vec::new();
        }
        if self
            .recent_seqno
            .get(&pending_key)
            .is_some_and(|(recent, expires)| {
                *expires >= now_ms && (*recent == seqno || seqno_gt(*recent, seqno))
            })
        {
            return Vec::new();
        }
        let Some((next_hop, _)) = self
            .candidates
            .iter()
            .filter(|((candidate_key, neighbour), candidate)| {
                *candidate_key == key && candidate.router_id == router_id && *neighbour != requester
            })
            .min_by_key(|(_, candidate)| {
                let feasible = self.candidate_is_feasible(candidate);
                (!feasible, candidate.metric, candidate.seqno)
            })
        else {
            return Vec::new();
        };
        self.originate_seqno_request(
            SeqnoRequestSpec {
                key,
                router_id,
                seqno,
                hop_count: hop_count - 1,
                next_hop: next_hop.1.clone(),
                requester: Some(requester),
            },
            now_ms,
        )
    }

    fn originate_seqno_request(&mut self, request: SeqnoRequestSpec, now_ms: u64) -> Vec<Action> {
        let SeqnoRequestSpec {
            key,
            router_id,
            seqno,
            hop_count,
            next_hop,
            requester,
        } = request;
        self.pending_seqno.insert(
            (key, router_id),
            PendingSeqnoRequest {
                seqno,
                hop_count,
                next_hop: next_hop.clone(),
                requester,
                retries_left: REQUEST_RETRIES,
                next_retry_ms: now_ms.saturating_add(REQUEST_RETRY_INITIAL_MS),
            },
        );
        vec![seqno_request_action(
            key, router_id, seqno, hop_count, next_hop,
        )]
    }

    fn update_action_to_candidate(
        &self,
        key: RouteKey,
        router_id: RouterId,
        seqno: u16,
        metric: u16,
        destination: NeighborKey,
    ) -> Action {
        let v4_via_v6 =
            key.destination.addr().is_ipv4() && !self.interface_has_ipv4(&destination.interface);
        Action::Send {
            interface: destination.interface,
            destination: destination.address,
            packet: OutboundPacket {
                tlvs: vec![OutboundTlv::Update(OutboundUpdate {
                    key: Some(key),
                    router_id: Some(router_id),
                    next_hop: None,
                    interval_cs: self.config.update_interval_cs,
                    seqno,
                    metric,
                    v4_via_v6,
                    sub_tlvs: vec![],
                })],
            },
        }
    }

    fn retraction_action(&self, key: RouteKey, seqno: u16, destination: NeighborKey) -> Action {
        Action::Send {
            interface: destination.interface,
            destination: destination.address,
            packet: OutboundPacket {
                tlvs: vec![OutboundTlv::Update(OutboundUpdate {
                    key: Some(key),
                    router_id: None,
                    next_hop: None,
                    interval_cs: self.config.update_interval_cs,
                    seqno,
                    metric: INFINITY,
                    v4_via_v6: key.destination.addr().is_ipv4(),
                    sub_tlvs: vec![],
                })],
            },
        }
    }

    fn candidate_is_feasible(&self, candidate: &Candidate) -> bool {
        let retained_selected = self.selected.get(&candidate.key).is_some_and(|selected| {
            selected.router_id == candidate.router_id
                && selected.interface == candidate.interface
                && selected.next_hop == candidate.next_hop
        });
        retained_selected
            || self
                .feasible
                .get(&(candidate.key, candidate.router_id))
                .is_none_or(|source| {
                    Distance {
                        seqno: candidate.seqno,
                        metric: candidate.advertised_metric,
                    }
                    .feasible_against(source.distance)
                })
    }

    fn tick(&mut self, now_ms: u64) -> Vec<Action> {
        let mut changed_neighbours = HashSet::new();
        for (key, neighbour) in &mut self.neighbours {
            let previous_receive_cost = neighbour.metric.receive_cost();
            advance_hello_timer(
                &mut neighbour.multicast_timer,
                &mut neighbour.histories.multicast,
                now_ms,
            );
            advance_hello_timer(
                &mut neighbour.unicast_timer,
                &mut neighbour.histories.unicast,
                now_ms,
            );
            neighbour.metric.on_hello(neighbour.histories);
            if neighbour.metric.receive_cost() != previous_receive_cost {
                neighbour.next_ihu_ms = now_ms;
                changed_neighbours.insert(key.clone());
            }
        }
        let expired: Vec<_> = self
            .neighbours
            .iter()
            .filter(|(_, neighbour)| {
                neighbour.histories.multicast.is_empty() && neighbour.histories.unicast.is_empty()
            })
            .map(|(k, _)| k.clone())
            .collect();
        let mut routes_may_have_changed = !expired.is_empty();
        for key in expired {
            self.neighbours.remove(&key);
            self.candidates
                .retain(|(_, neighbour), _| neighbour != &key);
        }
        for (key, neighbour) in &mut self.neighbours {
            if neighbour.last_ihu_ms.is_some_and(|last| {
                now_ms > last.saturating_add(u64::from(neighbour.ihu_interval_cs) * 35)
            }) {
                neighbour.last_ihu_ms = None;
                neighbour.metric.on_ihu(INFINITY);
                changed_neighbours.insert(key.clone());
            }
        }
        let expired_candidates: Vec<_> = self
            .candidates
            .iter()
            .filter(|(_, route)| route.expires_ms < now_ms)
            .map(|(key, route)| (key.clone(), route.metric == INFINITY))
            .collect();
        for (key, was_retracted) in expired_candidates {
            if was_retracted {
                self.candidates.remove(&key);
            } else if let Some(route) = self.candidates.get_mut(&key) {
                route.advertised_metric = INFINITY;
                route.metric = INFINITY;
                route.expires_ms =
                    now_ms.saturating_add(u64::from(route.interval_cs).saturating_mul(35));
            }
            routes_may_have_changed = true;
        }
        let selected = self.selected.clone();
        let mut refresh_actions = Vec::new();
        for ((key, neighbour), candidate) in &mut self.candidates {
            let refresh_margin = u64::from(candidate.interval_cs).saturating_mul(10);
            let is_selected = selected.get(key).is_some_and(|route| {
                route.router_id == candidate.router_id
                    && route.interface == candidate.interface
                    && route.next_hop == candidate.next_hop
            });
            if is_selected
                && candidate.metric < INFINITY
                && !candidate.refresh_requested
                && now_ms.saturating_add(refresh_margin) >= candidate.expires_ms
            {
                candidate.refresh_requested = true;
                refresh_actions.push(Action::Send {
                    interface: neighbour.interface.clone(),
                    destination: neighbour.address,
                    packet: OutboundPacket {
                        tlvs: vec![OutboundTlv::RouteRequest {
                            key: Some(*key),
                            sub_tlvs: vec![],
                        }],
                    },
                });
            }
        }
        let expired_tombstones = self.tombstones.len();
        self.tombstones.retain(|_, expires| *expires >= now_ms);
        routes_may_have_changed |= expired_tombstones != self.tombstones.len();
        self.feasible
            .retain(|_, source| source.expires_ms >= now_ms);
        self.recent_seqno
            .retain(|_, (_, expires_ms)| *expires_ms >= now_ms);
        for key in &changed_neighbours {
            routes_may_have_changed |= self.recompute_candidate_metrics(Some(key));
        }
        let mut actions = if routes_may_have_changed || !self.pending_switches.is_empty() {
            self.reselect(now_ms)
        } else {
            Vec::new()
        };
        actions.extend(refresh_actions);
        let due_requests: Vec<_> = self
            .pending_seqno
            .iter()
            .filter(|(_, pending)| now_ms >= pending.next_retry_ms)
            .map(|(key, pending)| (*key, pending.clone()))
            .collect();
        for ((key, router_id), pending) in due_requests {
            if pending.retries_left == 0 {
                self.pending_seqno.remove(&(key, router_id));
                self.recent_seqno.insert(
                    (key, router_id),
                    (pending.seqno, now_ms.saturating_add(RECENT_REQUEST_MS)),
                );
                continue;
            }
            actions.push(seqno_request_action(
                key,
                router_id,
                pending.seqno,
                pending.hop_count,
                pending.next_hop.clone(),
            ));
            if let Some(value) = self.pending_seqno.get_mut(&(key, router_id)) {
                let attempt = REQUEST_RETRIES.saturating_sub(value.retries_left);
                value.retries_left -= 1;
                value.next_retry_ms = now_ms.saturating_add(
                    REQUEST_RETRY_INITIAL_MS.saturating_mul(1u64 << u32::from(attempt + 1)),
                );
            }
        }
        let mut ihu_due: Vec<_> = self
            .neighbours
            .iter()
            .filter_map(|(key, neighbour)| {
                let regular_due = now_ms >= neighbour.next_ihu_ms;
                regular_due.then(|| key.clone())
            })
            .collect();
        let regular: HashSet<_> = ihu_due.iter().cloned().collect();
        ihu_due.extend(
            self.neighbours
                .iter()
                .filter(|(key, neighbour)| {
                    !regular.contains(*key)
                        && neighbour
                            .next_rtt_probe_ms
                            .is_some_and(|deadline| now_ms >= deadline)
                })
                .map(|(key, _)| key.clone())
                .take(MAX_RTT_PROBES_PER_TICK),
        );
        for key in ihu_due {
            if let Some(action) = self.ihu_action(&key, now_ms) {
                actions.push(action);
            }
        }
        let interfaces: Vec<String> = self.interfaces.keys().cloned().collect();
        for interface in interfaces {
            let mut send_hello = false;
            let mut send_update = false;
            let seqno;
            {
                let state = self
                    .interfaces
                    .get_mut(&interface)
                    .expect("interface exists");
                if now_ms >= state.next_hello_ms {
                    state.hello_seqno = state.hello_seqno.wrapping_add(1);
                    state.next_hello_ms = now_ms + u64::from(self.config.hello_interval_cs) * 10;
                    send_hello = true;
                }
                if now_ms >= state.next_update_ms {
                    state.next_update_ms = now_ms + u64::from(self.config.update_interval_cs) * 10;
                    send_update = true;
                }
                seqno = state.hello_seqno;
            }
            if send_hello {
                let sub_tlvs = if self.config.metric.timestamps_enabled() {
                    vec![SubTlv::TimestampHello(timestamp_us(now_ms))]
                } else {
                    Vec::new()
                };
                actions.push(Action::Send {
                    interface: interface.clone(),
                    destination: BABEL_MULTICAST_V6,
                    packet: OutboundPacket {
                        tlvs: vec![OutboundTlv::Hello {
                            unicast: false,
                            seqno,
                            interval_cs: self.config.hello_interval_cs,
                            sub_tlvs,
                        }],
                    },
                });
            }
            if send_update {
                actions.extend(self.send_updates(now_ms, None, Some(interface)));
            }
        }
        actions
    }

    fn send_updates(
        &mut self,
        now_ms: u64,
        only: Option<RouteKey>,
        interface: Option<String>,
    ) -> Vec<Action> {
        let interfaces: Vec<_> =
            interface.map_or_else(|| self.interfaces.keys().cloned().collect(), |v| vec![v]);
        let origins: Vec<_> = self
            .originated
            .iter()
            .map(|(key, origin)| (*key, origin.clone()))
            .collect();
        let selected: Vec<_> = self.selected.values().cloned().collect();
        interfaces
            .into_iter()
            .filter_map(|interface| {
                let mut tlvs = Vec::new();
                for (key, origin) in &origins {
                    if only.is_none_or(|wanted| wanted == *key) {
                        self.maintain_source(
                            *key,
                            self.config.router_id,
                            Distance {
                                seqno: origin.seqno,
                                metric: origin.metric,
                            },
                            now_ms,
                        );
                        tlvs.push(OutboundTlv::Update(OutboundUpdate {
                            key: Some(*key),
                            router_id: Some(self.config.router_id),
                            next_hop: None,
                            interval_cs: self.config.update_interval_cs,
                            seqno: origin.seqno,
                            metric: origin.metric,
                            v4_via_v6: key.destination.addr().is_ipv4()
                                && !self.interface_has_ipv4(&interface),
                            sub_tlvs: vec![],
                        }));
                    }
                }
                for route in &selected {
                    // Split horizon: never advertise a selected route back on
                    // the interface from which its next hop was learned.
                    if route.interface != interface && only.is_none_or(|wanted| wanted == route.key)
                    {
                        self.maintain_source(
                            route.key,
                            route.router_id,
                            Distance {
                                seqno: route.seqno,
                                metric: route.metric,
                            },
                            now_ms,
                        );
                        tlvs.push(OutboundTlv::Update(OutboundUpdate {
                            key: Some(route.key),
                            router_id: Some(route.router_id),
                            next_hop: None,
                            interval_cs: self.config.update_interval_cs,
                            seqno: route.seqno,
                            metric: route.metric,
                            v4_via_v6: route.key.destination.addr().is_ipv4()
                                && !self.interface_has_ipv4(&interface),
                            sub_tlvs: vec![],
                        }));
                    }
                }
                if only.is_none()
                    && !tlvs.is_empty()
                    && let Some(state) = self.interfaces.get_mut(&interface)
                {
                    state.last_full_update_ms = Some(now_ms);
                }
                (!tlvs.is_empty()).then_some(Action::Send {
                    interface,
                    destination: BABEL_MULTICAST_V6,
                    packet: OutboundPacket { tlvs },
                })
            })
            .collect()
    }

    fn full_update_request_allowed(&self, interface: &str, now_ms: u64) -> bool {
        let suppression_ms = u64::from(self.config.hello_interval_cs) * 10;
        self.interfaces
            .get(interface)
            .and_then(|state| state.last_full_update_ms)
            .is_none_or(|last| now_ms.saturating_sub(last) >= suppression_ms)
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
        actions.extend(self.send_updates(now_ms, None, None));
        actions
    }

    fn send_retraction(&self, key: RouteKey, seqno: u16, _now_ms: u64) -> Vec<Action> {
        self.interfaces
            .keys()
            .map(|interface| Action::Send {
                interface: interface.clone(),
                destination: BABEL_MULTICAST_V6,
                packet: OutboundPacket {
                    tlvs: vec![OutboundTlv::Update(OutboundUpdate {
                        key: Some(key),
                        router_id: Some(self.config.router_id),
                        next_hop: None,
                        interval_cs: self.config.update_interval_cs,
                        seqno,
                        metric: INFINITY,
                        v4_via_v6: key.destination.addr().is_ipv4(),
                        sub_tlvs: vec![],
                    })],
                },
            })
            .collect()
    }

    fn recompute_candidate_metrics(&mut self, only: Option<&NeighborKey>) -> bool {
        let costs: HashMap<_, _> = self
            .neighbours
            .iter()
            .filter(|(key, _)| only.is_none_or(|wanted| wanted == *key))
            .map(|(key, neighbour)| (key.clone(), neighbour.metric.link_cost()))
            .collect();
        let mut changed = false;
        for ((_, neighbour_key), candidate) in &mut self.candidates {
            let Some(link_cost) = costs.get(neighbour_key) else {
                continue;
            };
            let metric = self
                .config
                .metric_algebra
                .extend(candidate.advertised_metric, *link_cost);
            let metric = if metric == INFINITY || metric <= candidate.advertised_metric {
                INFINITY
            } else {
                metric
            };
            changed |= candidate.metric != metric;
            candidate.metric = metric;
        }
        changed
    }

    fn reselect(&mut self, now_ms: u64) -> Vec<Action> {
        let before = self.selected.clone();
        let before_tombstones: Vec<_> = self.tombstones.keys().copied().collect();
        let mut next = BTreeMap::new();
        let mut grouped: BTreeMap<RouteKey, Vec<&Candidate>> = BTreeMap::new();
        for candidate in self
            .candidates
            .values()
            .filter(|route| route.metric < INFINITY && self.candidate_is_feasible(route))
        {
            grouped.entry(candidate.key).or_default().push(candidate);
        }
        for (key, routes) in grouped {
            let current = before.get(&key).and_then(|selected| {
                routes.iter().copied().find(|route| {
                    route.router_id == selected.router_id
                        && route.next_hop == selected.next_hop
                        && route.interface == selected.interface
                })
            });
            let settled = if self.settled_routes.contains(&key) {
                true
            } else {
                let since = self.settling_since.entry(key).or_insert(now_ms);
                if now_ms.saturating_sub(*since) >= self.config.route_selection.better_for_ms {
                    self.settled_routes.insert(key);
                    true
                } else {
                    false
                }
            };
            let best = routes
                .iter()
                .copied()
                .min_by(candidate_order)
                .expect("non-empty");
            let chosen = if let Some(current) = current {
                if same_candidate(best, current)
                    || !sufficiently_better(best, current, self.config.route_selection)
                {
                    self.pending_switches.remove(&key);
                    current
                } else if !settled {
                    self.pending_switches.remove(&key);
                    self.settling_since.insert(key, now_ms);
                    best
                } else {
                    let pending =
                        self.pending_switches
                            .entry(key)
                            .or_insert_with(|| PendingSwitch {
                                router_id: best.router_id,
                                next_hop: best.next_hop,
                                interface: best.interface.clone(),
                                since_ms: now_ms,
                                current_peak_metric: current.metric,
                            });
                    if pending.router_id != best.router_id
                        || pending.next_hop != best.next_hop
                        || pending.interface != best.interface
                    {
                        *pending = PendingSwitch {
                            router_id: best.router_id,
                            next_hop: best.next_hop,
                            interface: best.interface.clone(),
                            since_ms: now_ms,
                            current_peak_metric: current.metric,
                        };
                    }
                    pending.current_peak_metric = pending.current_peak_metric.max(current.metric);
                    let recovered = metric_improvement_is_significant(
                        current.metric,
                        pending.current_peak_metric,
                        self.config.route_selection,
                    );
                    let ready = now_ms.saturating_sub(pending.since_ms)
                        >= self.config.route_selection.better_for_ms;
                    if recovered {
                        self.pending_switches.remove(&key);
                        current
                    } else if ready {
                        self.pending_switches.remove(&key);
                        best
                    } else {
                        current
                    }
                }
            } else {
                self.pending_switches.remove(&key);
                if !settled {
                    self.settling_since.insert(key, now_ms);
                }
                best
            };
            next.insert(key, selected_from_candidate(chosen));
        }
        self.selected = next;
        for key in self.selected.keys() {
            self.tombstones.remove(key);
        }
        for key in before.keys().filter(|key| !self.selected.contains_key(key)) {
            let expires_ms = self
                .candidates
                .iter()
                .filter(|((candidate_key, _), _)| *candidate_key == *key)
                .map(|(_, candidate)| candidate.expires_ms)
                .max()
                .unwrap_or_else(|| {
                    now_ms.saturating_add(u64::from(self.config.update_interval_cs) * 35)
                });
            self.tombstones.insert(*key, expires_ms);
        }
        self.pending_switches
            .retain(|key, _| self.selected.contains_key(key));
        self.settling_since
            .retain(|key, _| self.selected.contains_key(key));
        self.settled_routes
            .retain(|key| self.selected.contains_key(key));
        let tombstones_changed =
            self.tombstones.keys().copied().collect::<Vec<_>>() != before_tombstones;
        if self.selected != before || tombstones_changed {
            self.generation = self.generation.wrapping_add(1);
            let mut actions = vec![Action::RoutesChanged {
                generation: self.generation,
                routes: self.selected_routes(),
                unreachable: self.unreachable_routes(),
            }];
            actions.extend(self.selected_delta(&before, now_ms));
            actions
        } else {
            Vec::new()
        }
    }

    fn selected_delta(
        &mut self,
        before: &BTreeMap<RouteKey, SelectedRoute>,
        now_ms: u64,
    ) -> Vec<Action> {
        let mut actions = Vec::new();
        for (key, previous) in before {
            if !self.selected.contains_key(key) {
                actions.extend(self.advertise_learned(previous, INFINITY, None));
                let pending_key = (*key, previous.router_id);
                let requested_seqno = self
                    .feasible
                    .get(&pending_key)
                    .map_or(previous.seqno.wrapping_add(1), |source| {
                        source.distance.seqno.wrapping_add(1)
                    });
                let next_hop = self
                    .candidates
                    .iter()
                    .filter(|((candidate_key, _), candidate)| {
                        candidate_key == key
                            && candidate.metric < INFINITY
                            && !self.candidate_is_feasible(candidate)
                    })
                    .map(|((_, neighbour), _)| neighbour.clone())
                    .next();
                if !self.pending_seqno.contains_key(&pending_key)
                    && let Some(next_hop) = next_hop
                {
                    actions.extend(self.originate_seqno_request(
                        SeqnoRequestSpec {
                            key: *key,
                            router_id: previous.router_id,
                            seqno: requested_seqno,
                            hop_count: REQUEST_HOP_COUNT,
                            next_hop,
                            requester: None,
                        },
                        now_ms,
                    ));
                }
            }
        }
        let changed: Vec<_> = self
            .selected
            .iter()
            .filter(|(key, selected)| before.get(key) != Some(*selected))
            .map(|(_, selected)| selected.clone())
            .collect();
        for selected in &changed {
            self.maintain_source(
                selected.key,
                selected.router_id,
                Distance {
                    seqno: selected.seqno,
                    metric: selected.metric,
                },
                now_ms,
            );
            actions.extend(self.advertise_learned(
                selected,
                selected.metric,
                Some(&selected.interface),
            ));
        }
        actions
    }

    fn advertise_learned(
        &self,
        route: &SelectedRoute,
        metric: u16,
        exclude_interface: Option<&str>,
    ) -> Vec<Action> {
        let interfaces: Vec<_> = self
            .interfaces
            .keys()
            .filter(|interface| exclude_interface != Some(interface.as_str()))
            .cloned()
            .collect();
        interfaces
            .into_iter()
            .map(|interface| {
                let v4_via_v6 =
                    route.key.destination.addr().is_ipv4() && !self.interface_has_ipv4(&interface);
                Action::Send {
                    interface,
                    destination: BABEL_MULTICAST_V6,
                    packet: OutboundPacket {
                        tlvs: vec![OutboundTlv::Update(OutboundUpdate {
                            key: Some(route.key),
                            router_id: Some(route.router_id),
                            next_hop: None,
                            interval_cs: self.config.update_interval_cs,
                            seqno: route.seqno,
                            metric,
                            v4_via_v6,
                            sub_tlvs: vec![],
                        })],
                    },
                }
            })
            .collect()
    }

    fn maintain_source(
        &mut self,
        key: RouteKey,
        router_id: RouterId,
        distance: Distance,
        now_ms: u64,
    ) {
        let source = self
            .feasible
            .entry((key, router_id))
            .or_insert(SourceEntry {
                distance,
                expires_ms: now_ms.saturating_add(SOURCE_GC_TIME_MS),
            });
        if seqno_gt(distance.seqno, source.distance.seqno)
            || (distance.seqno == source.distance.seqno && distance.metric < source.distance.metric)
        {
            source.distance = distance;
        }
        source.expires_ms = now_ms.saturating_add(SOURCE_GC_TIME_MS);
    }

    fn bump_sequence_number(&mut self) {
        self.sequence_number = self.sequence_number.wrapping_add(1);
        for origin in self.originated.values_mut() {
            origin.seqno = self.sequence_number;
        }
    }
}

fn seqno_request_action(
    key: RouteKey,
    router_id: RouterId,
    seqno: u16,
    hop_count: u8,
    next_hop: NeighborKey,
) -> Action {
    Action::Send {
        interface: next_hop.interface,
        destination: next_hop.address,
        packet: OutboundPacket {
            tlvs: vec![OutboundTlv::SeqnoRequest {
                key,
                seqno,
                hop_count,
                router_id,
                sub_tlvs: vec![],
            }],
        },
    }
}

fn timestamp_us(now_ms: u64) -> u32 {
    now_ms.wrapping_mul(1_000) as u32
}

fn probe_salt(key: &NeighborKey, seqno: u16) -> u64 {
    // Stable FNV-1a is sufficient here: jitter is not a security boundary.
    let mut value = 0xcbf2_9ce4_8422_2325u64;
    for byte in key.interface.as_bytes() {
        value = (value ^ u64::from(*byte)).wrapping_mul(0x100_0000_01b3);
    }
    for byte in key.address.to_string().as_bytes() {
        value = (value ^ u64::from(*byte)).wrapping_mul(0x100_0000_01b3);
    }
    for byte in seqno.to_be_bytes() {
        value = (value ^ u64::from(byte)).wrapping_mul(0x100_0000_01b3);
    }
    value
}

fn initial_probe_deadline(now_ms: u64, interval_ms: u64, key: &NeighborKey) -> u64 {
    let spread = interval_ms.clamp(1, 200);
    now_ms.saturating_add(probe_salt(key, 0) % spread)
}

fn recurring_probe_deadline(now_ms: u64, interval_ms: u64, key: &NeighborKey, seqno: u16) -> u64 {
    let spread = (interval_ms / 5).max(1);
    let delay = interval_ms
        .saturating_mul(9)
        .saturating_div(10)
        .saturating_add(probe_salt(key, seqno) % spread);
    now_ms.saturating_add(delay)
}

fn valid_cost(cost: u16) -> u16 {
    if cost == 0 { INFINITY } else { cost }
}

fn forbidden_destination(destination: ipnet::IpNet) -> bool {
    match destination {
        ipnet::IpNet::V4(prefix) => {
            let octets = prefix.network().octets();
            (prefix.prefix_len() == 32
                && (prefix.network() == std::net::Ipv4Addr::UNSPECIFIED
                    || prefix.network() == std::net::Ipv4Addr::LOCALHOST))
                || (prefix.prefix_len() >= 8 && octets[0] == 224)
        }
        ipnet::IpNet::V6(prefix) => {
            let octets = prefix.network().octets();
            (prefix.prefix_len() >= 8 && octets[0] == 0xff)
                || (prefix.prefix_len() >= 64 && octets[..8] == [0xfe, 0x80, 0, 0, 0, 0, 0, 0])
        }
    }
}

fn ihu_applies(address: Option<IpAddr>, local_addresses: &[IpAddr]) -> bool {
    address.is_none_or(|value| local_addresses.is_empty() || local_addresses.contains(&value))
}

fn advance_hello_timer(
    timer: &mut Option<HelloTimer>,
    history: &mut crate::metric::HelloHistory,
    now_ms: u64,
) {
    let Some(value) = timer.as_mut() else {
        return;
    };
    if now_ms < value.next_expiry_ms {
        return;
    }
    let interval_ms = u64::from(value.interval_cs).saturating_mul(10).max(1);
    let missed = 1 + now_ms.saturating_sub(value.next_expiry_ms) / interval_ms;
    history.missed_many(missed);
    value.next_expiry_ms = value
        .next_expiry_ms
        .saturating_add(missed.saturating_mul(interval_ms));
}

fn timestamp_hello(sub_tlvs: &[SubTlv]) -> Option<u32> {
    sub_tlvs.iter().find_map(|value| match value {
        SubTlv::TimestampHello(timestamp) => Some(*timestamp),
        _ => None,
    })
}

fn timestamp_ihu(sub_tlvs: &[SubTlv]) -> Option<(u32, u32)> {
    sub_tlvs.iter().find_map(|value| match value {
        SubTlv::TimestampIhu { origin, received } => Some((*origin, *received)),
        _ => None,
    })
}

fn valid_rtt_sample(now: u32, origin: u32, peer_sent: u32, peer_received: u32) -> Option<u32> {
    const MAX_SAMPLE_AGE_US: u32 = 180_000_000;
    let elapsed = now.wrapping_sub(origin);
    let peer_delay = peer_sent.wrapping_sub(peer_received);
    (elapsed <= MAX_SAMPLE_AGE_US && peer_delay <= MAX_SAMPLE_AGE_US && elapsed >= peer_delay)
        .then_some(elapsed - peer_delay)
}

fn candidate_order(left: &&Candidate, right: &&Candidate) -> std::cmp::Ordering {
    (left.metric, left.router_id, &left.interface, left.next_hop).cmp(&(
        right.metric,
        right.router_id,
        &right.interface,
        right.next_hop,
    ))
}

fn same_candidate(left: &Candidate, right: &Candidate) -> bool {
    left.router_id == right.router_id
        && left.next_hop == right.next_hop
        && left.interface == right.interface
}

fn sufficiently_better(
    candidate: &Candidate,
    current: &Candidate,
    policy: RouteSelectionConfig,
) -> bool {
    metric_improvement_is_significant(candidate.metric, current.metric, policy)
}

fn metric_improvement_is_significant(
    better_metric: u16,
    worse_metric: u16,
    policy: RouteSelectionConfig,
) -> bool {
    let improvement = worse_metric.saturating_sub(better_metric);
    let percentage = (u32::from(worse_metric) * u32::from(policy.switch_margin_percent))
        .div_ceil(100)
        .min(u32::from(u16::MAX)) as u16;
    improvement >= policy.switch_margin_metric.max(percentage) && improvement > 0
}

fn selected_from_candidate(route: &Candidate) -> SelectedRoute {
    SelectedRoute {
        key: route.key,
        router_id: route.router_id,
        seqno: route.seqno,
        metric: route.metric,
        next_hop: route.next_hop,
        interface: route.interface.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ipnet::IpNet;
    use std::str::FromStr;

    fn id(value: u8) -> RouterId {
        RouterId::new([value; 8]).unwrap()
    }
    fn key() -> RouteKey {
        RouteKey::new(IpNet::from_str("2001:db8::/64").unwrap(), None).unwrap()
    }
    fn config(router_id: RouterId) -> EngineConfig {
        let mut config = EngineConfig::recommended(router_id);
        config.metric = Arc::new(WiredMetric::new(96, 1, 1).unwrap());
        config
    }

    #[test]
    fn route_requires_neighbour_and_exports_generation() {
        let mut engine = Engine::new(config(id(1)));
        engine.handle(Event::InterfaceUp {
            interface: "wg0".into(),
            local_addresses: vec![],
            now_ms: 0,
        });
        engine.handle(Event::PacketReceived {
            interface: "wg0".into(),
            source: "fe80::2".parse().unwrap(),
            now_ms: 10,
            packet: Packet {
                tlvs: vec![
                    Tlv::Hello {
                        unicast: false,
                        seqno: 1,
                        interval_cs: 400,
                        sub_tlvs: vec![],
                    },
                    Tlv::Ihu {
                        address: None,
                        rxcost: 96,
                        interval_cs: 1200,
                        sub_tlvs: vec![],
                    },
                ],
            },
        });
        let actions = engine.handle(Event::PacketReceived {
            interface: "wg0".into(),
            source: "fe80::2".parse().unwrap(),
            now_ms: 20,
            packet: Packet {
                tlvs: vec![Tlv::Update(ResolvedUpdate {
                    key: Some(key()),
                    router_id: Some(id(2)),
                    next_hop: Some("fe80::2".parse().unwrap()),
                    interval_cs: 1600,
                    seqno: 7,
                    metric: 0,
                    v4_via_v6: false,
                    sub_tlvs: vec![],
                })],
            },
        });
        assert!(actions.iter().any(|action| matches!(action, Action::RoutesChanged { routes, .. } if routes.len() == 1 && routes[0].metric == 96)));
    }

    #[test]
    fn unfeasible_alternate_is_not_acquired() {
        let mut engine = Engine::new(EngineConfig {
            router_id: id(1),
            metric: Arc::new(WiredMetric::new(96, 1, 1).unwrap()),
            metric_algebra: Arc::new(AdditiveMetric),
            sequence_number: 0,
            hello_interval_cs: 400,
            update_interval_cs: 1600,
            route_selection: RouteSelectionConfig::default(),
        });
        engine.handle(Event::InterfaceUp {
            interface: "wg0".into(),
            local_addresses: vec![],
            now_ms: 0,
        });
        // The first selected route is advertised on this second interface,
        // which establishes the RFC feasibility distance at 10 + 96.
        engine.handle(Event::InterfaceUp {
            interface: "wg-out".into(),
            local_addresses: vec![],
            now_ms: 0,
        });
        let source = "fe80::2".parse().unwrap();
        engine.handle(Event::PacketReceived {
            interface: "wg0".into(),
            source,
            now_ms: 1,
            packet: Packet {
                tlvs: vec![
                    Tlv::Hello {
                        unicast: false,
                        seqno: 1,
                        interval_cs: 400,
                        sub_tlvs: vec![],
                    },
                    Tlv::Ihu {
                        address: None,
                        rxcost: 96,
                        interval_cs: 1200,
                        sub_tlvs: vec![],
                    },
                    Tlv::Update(ResolvedUpdate {
                        key: Some(key()),
                        router_id: Some(id(2)),
                        next_hop: Some(source),
                        interval_cs: 1600,
                        seqno: 5,
                        metric: 10,
                        v4_via_v6: false,
                        sub_tlvs: vec![],
                    }),
                ],
            },
        });
        engine.handle(Event::InterfaceDown {
            interface: "wg0".into(),
            now_ms: 2,
        });
        engine.handle(Event::InterfaceUp {
            interface: "wg1".into(),
            local_addresses: vec![],
            now_ms: 3,
        });
        let other = "fe80::3".parse().unwrap();
        engine.handle(Event::PacketReceived {
            interface: "wg1".into(),
            source: other,
            now_ms: 4,
            packet: Packet {
                tlvs: vec![
                    Tlv::Hello {
                        unicast: false,
                        seqno: 1,
                        interval_cs: 400,
                        sub_tlvs: vec![],
                    },
                    Tlv::Ihu {
                        address: None,
                        rxcost: 96,
                        interval_cs: 1200,
                        sub_tlvs: vec![],
                    },
                    Tlv::Update(ResolvedUpdate {
                        key: Some(key()),
                        router_id: Some(id(2)),
                        next_hop: Some(other),
                        interval_cs: 1600,
                        seqno: 5,
                        metric: 120,
                        v4_via_v6: false,
                        sub_tlvs: vec![],
                    }),
                ],
            },
        });
        assert!(engine.selected_routes().is_empty());
    }

    #[test]
    fn withdrawal_advances_sequence_and_sends_infinity() {
        let mut engine = Engine::new(config(id(1)));
        engine.handle(Event::InterfaceUp {
            interface: "wg0".into(),
            local_addresses: vec![],
            now_ms: 0,
        });
        engine.handle(Event::Originate {
            key: key(),
            metric: 0,
            now_ms: 1,
        });
        let actions = engine.handle(Event::Withdraw {
            key: key(),
            now_ms: 2,
        });
        assert!(
            actions
                .iter()
                .any(|action| matches!(action, Action::SequenceNumberChanged(1)))
        );
        assert!(actions.iter().any(|action| matches!(
            action,
            Action::Send { packet, .. }
                if packet.tlvs.iter().any(|tlv| matches!(tlv, OutboundTlv::Update(update) if update.metric == INFINITY && update.seqno == 1))
        )));
    }

    #[test]
    fn learned_route_expires_and_is_retracted_from_selected_snapshot() {
        let mut engine = Engine::new(config(id(1)));
        engine.handle(Event::InterfaceUp {
            interface: "wg0".into(),
            local_addresses: vec![],
            now_ms: 0,
        });
        engine.handle(Event::InterfaceUp {
            interface: "wg1".into(),
            local_addresses: vec![],
            now_ms: 0,
        });
        let source = "fe80::2".parse().unwrap();
        engine.handle(Event::PacketReceived {
            interface: "wg0".into(),
            source,
            now_ms: 1,
            packet: Packet {
                tlvs: vec![
                    Tlv::Hello {
                        unicast: false,
                        seqno: 1,
                        interval_cs: 400,
                        sub_tlvs: vec![],
                    },
                    Tlv::Ihu {
                        address: None,
                        rxcost: 96,
                        interval_cs: 1200,
                        sub_tlvs: vec![],
                    },
                    Tlv::Update(ResolvedUpdate {
                        key: Some(key()),
                        router_id: Some(id(2)),
                        next_hop: Some(source),
                        interval_cs: 100,
                        seqno: 7,
                        metric: 0,
                        v4_via_v6: false,
                        sub_tlvs: vec![],
                    }),
                ],
            },
        });
        assert_eq!(engine.selected_routes().len(), 1);
        let actions = engine.handle(Event::Tick { now_ms: 3502 });
        assert!(engine.selected_routes().is_empty());
        assert!(actions.iter().any(
            |action| matches!(action, Action::RoutesChanged { routes, .. } if routes.is_empty())
        ));
        assert!(actions.iter().any(|action| matches!(
            action,
            Action::Send { interface, packet, .. }
                if interface == "wg1" && packet.tlvs.iter().any(|tlv| matches!(tlv, OutboundTlv::Update(update) if update.metric == INFINITY))
        )));
    }

    #[test]
    fn retraction_only_retracts_the_route_from_its_neighbour() {
        let mut engine = Engine::new(config(id(1)));
        for (interface, source, metric) in [("wg0", "fe80::2", 0), ("wg1", "fe80::3", 10)] {
            engine.handle(Event::InterfaceUp {
                interface: interface.into(),
                local_addresses: vec![],
                now_ms: 0,
            });
            let source = source.parse().unwrap();
            engine.handle(Event::PacketReceived {
                interface: interface.into(),
                source,
                now_ms: 1,
                packet: Packet {
                    tlvs: vec![
                        Tlv::Hello {
                            unicast: false,
                            seqno: 1,
                            interval_cs: 400,
                            sub_tlvs: vec![],
                        },
                        Tlv::Ihu {
                            address: None,
                            rxcost: 96,
                            interval_cs: 1200,
                            sub_tlvs: vec![],
                        },
                        Tlv::Update(ResolvedUpdate {
                            key: Some(key()),
                            router_id: Some(id(2)),
                            next_hop: Some(source),
                            interval_cs: 1600,
                            seqno: 7,
                            metric,
                            v4_via_v6: false,
                            sub_tlvs: vec![],
                        }),
                    ],
                },
            });
        }
        assert_eq!(engine.selected_routes().len(), 1);
        let actions = engine.handle(Event::PacketReceived {
            interface: "wg0".into(),
            source: "fe80::2".parse().unwrap(),
            now_ms: 2,
            packet: Packet {
                tlvs: vec![Tlv::Update(ResolvedUpdate {
                    key: Some(key()),
                    router_id: Some(id(2)),
                    next_hop: Some("fe80::2".parse().unwrap()),
                    interval_cs: 1600,
                    seqno: 8,
                    metric: INFINITY,
                    v4_via_v6: false,
                    sub_tlvs: vec![],
                })],
            },
        });
        assert_eq!(engine.selected_routes().len(), 1);
        assert_eq!(engine.selected_routes()[0].interface, "wg1");
        assert!(actions.iter().any(|action| matches!(
            action,
            Action::RoutesChanged { routes, .. }
                if routes.len() == 1 && routes[0].interface == "wg1"
        )));
    }

    #[test]
    fn seqno_request_increments_local_origin_at_most_once() {
        let mut config = config(id(1));
        config.sequence_number = 7;
        let mut engine = Engine::new(config);
        engine.handle(Event::InterfaceUp {
            interface: "wg0".into(),
            local_addresses: vec![],
            now_ms: 0,
        });
        engine.handle(Event::Originate {
            key: key(),
            metric: 0,
            now_ms: 1,
        });
        let actions = engine.handle(Event::PacketReceived {
            interface: "wg0".into(),
            source: "fe80::2".parse().unwrap(),
            now_ms: 2,
            packet: Packet {
                tlvs: vec![Tlv::SeqnoRequest {
                    key: key(),
                    seqno: 100,
                    hop_count: 16,
                    router_id: id(1),
                    sub_tlvs: vec![],
                }],
            },
        });
        assert!(
            actions
                .iter()
                .any(|action| matches!(action, Action::SequenceNumberChanged(8)))
        );
        assert!(actions.iter().any(|action| matches!(
            action,
            Action::Send { packet, .. }
                if packet.tlvs.iter().any(|tlv| matches!(tlv, OutboundTlv::Update(update) if update.seqno == 8))
        )));
    }

    #[test]
    fn seqno_request_is_forwarded_towards_remote_origin() {
        let mut engine = Engine::new(config(id(1)));
        for interface in ["upstream", "downstream"] {
            engine.handle(Event::InterfaceUp {
                interface: interface.into(),
                local_addresses: vec![],
                now_ms: 0,
            });
        }
        let source = "fe80::2".parse().unwrap();
        engine.handle(Event::PacketReceived {
            interface: "upstream".into(),
            source,
            now_ms: 1,
            packet: Packet {
                tlvs: vec![
                    Tlv::Hello {
                        unicast: false,
                        seqno: 1,
                        interval_cs: 400,
                        sub_tlvs: vec![],
                    },
                    Tlv::Ihu {
                        address: None,
                        rxcost: 96,
                        interval_cs: 1200,
                        sub_tlvs: vec![],
                    },
                    Tlv::Update(ResolvedUpdate {
                        key: Some(key()),
                        router_id: Some(id(3)),
                        next_hop: Some(source),
                        interval_cs: 1600,
                        seqno: 5,
                        metric: 0,
                        v4_via_v6: false,
                        sub_tlvs: vec![],
                    }),
                ],
            },
        });
        let actions = engine.handle(Event::PacketReceived {
            interface: "downstream".into(),
            source: "fe80::4".parse().unwrap(),
            now_ms: 2,
            packet: Packet {
                tlvs: vec![Tlv::SeqnoRequest {
                    key: key(),
                    seqno: 6,
                    hop_count: 7,
                    router_id: id(3),
                    sub_tlvs: vec![],
                }],
            },
        });
        assert!(actions.iter().any(|action| matches!(
            action,
            Action::Send { interface, packet, .. }
                if interface == "upstream" && packet.tlvs.iter().any(|tlv| matches!(tlv, OutboundTlv::SeqnoRequest { seqno: 6, hop_count: 6, .. }))
        )));
    }

    #[test]
    fn default_wired_metric_activates_a_stored_update_after_second_hello() {
        let mut engine = Engine::new(EngineConfig::recommended(id(1)));
        engine.handle(Event::InterfaceUp {
            interface: "eth0".into(),
            local_addresses: vec![],
            now_ms: 0,
        });
        let source = "fe80::2".parse().unwrap();
        engine.handle(Event::PacketReceived {
            interface: "eth0".into(),
            source,
            now_ms: 10,
            packet: Packet {
                tlvs: vec![
                    Tlv::Hello {
                        unicast: false,
                        seqno: 1,
                        interval_cs: 400,
                        sub_tlvs: vec![],
                    },
                    Tlv::Ihu {
                        address: None,
                        rxcost: 96,
                        interval_cs: 1200,
                        sub_tlvs: vec![],
                    },
                    Tlv::Update(ResolvedUpdate {
                        key: Some(key()),
                        router_id: Some(id(2)),
                        next_hop: Some(source),
                        interval_cs: 1600,
                        seqno: 1,
                        metric: 0,
                        v4_via_v6: false,
                        sub_tlvs: vec![],
                    }),
                ],
            },
        });
        assert!(engine.selected_routes().is_empty());
        engine.handle(Event::PacketReceived {
            interface: "eth0".into(),
            source,
            now_ms: 4_000,
            packet: Packet {
                tlvs: vec![Tlv::Hello {
                    unicast: false,
                    seqno: 2,
                    interval_cs: 400,
                    sub_tlvs: vec![],
                }],
            },
        });
        assert_eq!(engine.selected_routes()[0].metric, 96);
    }

    #[test]
    fn ihu_change_recomputes_an_existing_candidate_without_an_update() {
        let mut engine = Engine::new(config(id(1)));
        engine.handle(Event::InterfaceUp {
            interface: "eth0".into(),
            local_addresses: vec![],
            now_ms: 0,
        });
        let source = "fe80::2".parse().unwrap();
        engine.handle(Event::PacketReceived {
            interface: "eth0".into(),
            source,
            now_ms: 10,
            packet: Packet {
                tlvs: vec![
                    Tlv::Hello {
                        unicast: false,
                        seqno: 1,
                        interval_cs: 400,
                        sub_tlvs: vec![],
                    },
                    Tlv::Ihu {
                        address: None,
                        rxcost: 96,
                        interval_cs: 1200,
                        sub_tlvs: vec![],
                    },
                    Tlv::Update(ResolvedUpdate {
                        key: Some(key()),
                        router_id: Some(id(2)),
                        next_hop: Some(source),
                        interval_cs: 1600,
                        seqno: 1,
                        metric: 0,
                        v4_via_v6: false,
                        sub_tlvs: vec![],
                    }),
                ],
            },
        });
        assert_eq!(engine.selected_routes()[0].metric, 96);
        engine.handle(Event::PacketReceived {
            interface: "eth0".into(),
            source,
            now_ms: 20,
            packet: Packet {
                tlvs: vec![Tlv::Ihu {
                    address: None,
                    rxcost: 200,
                    interval_cs: 1200,
                    sub_tlvs: vec![],
                }],
            },
        });
        assert_eq!(engine.selected_routes()[0].metric, 200);
    }

    #[test]
    fn rfc9616_timestamp_exchange_feeds_the_rtt_metric() {
        let mut config = config(id(1));
        config.metric = Arc::new(crate::metric::RttMetric::recommended(Arc::new(
            WiredMetric::new(96, 1, 1).unwrap(),
        )));
        let mut engine = Engine::new(config);
        let actions = engine.handle(Event::InterfaceUp {
            interface: "eth0".into(),
            local_addresses: vec![],
            now_ms: 0,
        });
        assert!(actions.iter().any(|action| matches!(
            action,
            Action::Send { packet, .. }
                if packet.tlvs.iter().any(|tlv| matches!(tlv, OutboundTlv::Hello { sub_tlvs, .. }
                    if sub_tlvs.contains(&SubTlv::TimestampHello(0))))
        )));
        let source = "fe80::2".parse().unwrap();
        engine.handle(Event::PacketReceived {
            interface: "eth0".into(),
            source,
            now_ms: 50,
            packet: Packet {
                tlvs: vec![
                    Tlv::Hello {
                        unicast: false,
                        seqno: 1,
                        interval_cs: 400,
                        sub_tlvs: vec![SubTlv::TimestampHello(40_000)],
                    },
                    Tlv::Ihu {
                        address: None,
                        rxcost: 96,
                        interval_cs: 1200,
                        sub_tlvs: vec![SubTlv::TimestampIhu {
                            origin: 0,
                            received: 10_000,
                        }],
                    },
                    Tlv::Update(ResolvedUpdate {
                        key: Some(key()),
                        router_id: Some(id(2)),
                        next_hop: Some(source),
                        interval_cs: 1600,
                        seqno: 1,
                        metric: 0,
                        v4_via_v6: false,
                        sub_tlvs: vec![],
                    }),
                ],
            },
        });
        let status = engine.neighbour_status(50).pop().unwrap();
        assert_eq!(status.last_rtt_us, Some(20_000));
        assert_eq!(status.smoothed_rtt_us, Some(20_000));
        assert_eq!(status.rtt_penalty, 14);
        assert_eq!(status.link_cost, 110);
        assert_eq!(engine.selected_routes()[0].metric, 110);
    }

    #[test]
    fn rtt_probe_runs_independently_of_the_regular_ihu_interval() {
        let mut config = config(id(1));
        config.metric = Arc::new(crate::metric::RttMetric::recommended(Arc::new(
            WiredMetric::new(96, 1, 1).unwrap(),
        )));
        let mut engine = Engine::new(config);
        engine.handle(Event::InterfaceUp {
            interface: "eth0".into(),
            local_addresses: vec![],
            now_ms: 0,
        });
        let actions = engine.handle(Event::PacketReceived {
            interface: "eth0".into(),
            source: "fe80::2".parse().unwrap(),
            now_ms: 10,
            packet: Packet {
                tlvs: vec![Tlv::Hello {
                    unicast: false,
                    seqno: 1,
                    interval_cs: 400,
                    sub_tlvs: vec![SubTlv::TimestampHello(0)],
                }],
            },
        });
        let deadline = engine
            .neighbours
            .values()
            .next()
            .and_then(|neighbour| neighbour.next_rtt_probe_ms)
            .unwrap();
        if deadline > 10 {
            // RFC 9616 requires an echoed Timestamp IHU to share a packet
            // with a timestamped Hello, independently of probe scheduling.
            assert!(contains_unicast_timestamp_hello(&actions));
            assert!(contains_unicast_timestamp_hello(
                &engine.handle(Event::Tick { now_ms: deadline })
            ));
        }
        let next_deadline = engine
            .neighbours
            .values()
            .next()
            .and_then(|neighbour| neighbour.next_rtt_probe_ms)
            .unwrap();
        assert!(!contains_unicast_timestamp_hello(&engine.handle(
            Event::Tick {
                now_ms: next_deadline - 1
            }
        )));
        assert!(contains_unicast_timestamp_hello(&engine.handle(
            Event::Tick {
                now_ms: next_deadline
            }
        )));
    }

    #[test]
    fn source_entry_is_maintained_on_advertisement_and_garbage_collected() {
        let mut engine = Engine::new(config(id(1)));
        for interface in ["in", "out"] {
            engine.handle(Event::InterfaceUp {
                interface: interface.into(),
                local_addresses: vec![],
                now_ms: 0,
            });
        }
        let source = "fe80::2".parse().unwrap();
        engine.handle(Event::PacketReceived {
            interface: "in".into(),
            source,
            now_ms: 10,
            packet: Packet {
                tlvs: vec![
                    Tlv::Hello {
                        unicast: false,
                        seqno: 1,
                        interval_cs: 400,
                        sub_tlvs: vec![],
                    },
                    Tlv::Ihu {
                        address: None,
                        rxcost: 96,
                        interval_cs: 1200,
                        sub_tlvs: vec![],
                    },
                    Tlv::Update(ResolvedUpdate {
                        key: Some(key()),
                        router_id: Some(id(2)),
                        next_hop: Some(source),
                        interval_cs: 1600,
                        seqno: 5,
                        metric: 10,
                        v4_via_v6: false,
                        sub_tlvs: vec![],
                    }),
                ],
            },
        });
        let source_key = (key(), id(2));
        assert_eq!(
            engine.feasible.get(&source_key).map(|entry| entry.distance),
            Some(Distance {
                seqno: 5,
                metric: 106
            })
        );
        engine.handle(Event::PacketReceived {
            interface: "in".into(),
            source,
            now_ms: 100,
            packet: Packet {
                tlvs: vec![Tlv::Update(ResolvedUpdate {
                    key: Some(key()),
                    router_id: Some(id(2)),
                    next_hop: None,
                    interval_cs: 1600,
                    seqno: 6,
                    metric: INFINITY,
                    v4_via_v6: false,
                    sub_tlvs: vec![],
                })],
            },
        });
        assert_eq!(
            engine.feasible.get(&source_key).unwrap().expires_ms,
            10 + SOURCE_GC_TIME_MS
        );
        engine.handle(Event::Tick {
            now_ms: 11 + SOURCE_GC_TIME_MS,
        });
        assert!(!engine.feasible.contains_key(&source_key));
    }

    #[test]
    fn replacing_origins_is_a_single_sequence_transition() {
        let mut engine = Engine::new(config(id(1)));
        engine.handle(Event::InterfaceUp {
            interface: "wg0".into(),
            local_addresses: vec![],
            now_ms: 0,
        });
        let old = key();
        engine.handle(Event::Originate {
            key: old,
            metric: 0,
            now_ms: 1,
        });
        let new = RouteKey::new("2001:db8:1::/64".parse().unwrap(), None).unwrap();
        let actions = engine.handle(Event::ReplaceOrigins {
            origins: BTreeMap::from([(new, 7)]),
            now_ms: 2,
        });
        assert_eq!(
            actions
                .iter()
                .filter(|action| matches!(action, Action::SequenceNumberChanged(_)))
                .count(),
            1
        );
        assert!(actions.iter().any(|action| matches!(
            action,
            Action::Send { packet, .. }
                if packet.tlvs.iter().any(|tlv| matches!(tlv, OutboundTlv::Update(update)
                    if update.key == Some(old) && update.metric == INFINITY))
        )));
        assert_eq!(
            engine.originated.keys().copied().collect::<Vec<_>>(),
            vec![new]
        );
    }

    #[test]
    fn better_route_must_clear_margin_for_the_full_dwell_time() {
        let mut engine = Engine::new(config(id(1)));
        let route_key = key();
        insert_candidate(&mut engine, route_key, id(2), "eth0", "fe80::2", 200);
        insert_candidate(&mut engine, route_key, id(3), "eth1", "fe80::3", 180);
        let current = engine
            .candidates
            .values()
            .find(|candidate| candidate.interface == "eth0")
            .map(selected_from_candidate)
            .unwrap();
        engine.selected.insert(route_key, current);
        engine.settled_routes.insert(route_key);

        engine.reselect(0);
        assert_eq!(engine.selected_routes()[0].interface, "eth0");
        engine.reselect(7_999);
        assert_eq!(engine.selected_routes()[0].interface, "eth0");
        engine.reselect(8_000);
        assert_eq!(engine.selected_routes()[0].interface, "eth1");
    }

    #[test]
    fn margin_loss_resets_dwell_but_current_route_loss_switches_immediately() {
        let mut engine = Engine::new(config(id(1)));
        let route_key = key();
        insert_candidate(&mut engine, route_key, id(2), "eth0", "fe80::2", 200);
        insert_candidate(&mut engine, route_key, id(3), "eth1", "fe80::3", 180);
        let current = engine
            .candidates
            .values()
            .find(|candidate| candidate.interface == "eth0")
            .map(selected_from_candidate)
            .unwrap();
        engine.selected.insert(route_key, current);
        engine.settled_routes.insert(route_key);

        engine.reselect(0);
        engine
            .candidates
            .values_mut()
            .find(|candidate| candidate.interface == "eth1")
            .unwrap()
            .metric = 195;
        engine.reselect(4_000);
        engine
            .candidates
            .values_mut()
            .find(|candidate| candidate.interface == "eth1")
            .unwrap()
            .metric = 180;
        engine.reselect(5_000);
        engine.reselect(12_999);
        assert_eq!(engine.selected_routes()[0].interface, "eth0");

        engine
            .candidates
            .retain(|(_, neighbour), _| neighbour.interface != "eth0");
        engine.reselect(13_000);
        assert_eq!(engine.selected_routes()[0].interface, "eth1");
    }

    #[test]
    fn initial_candidate_discovery_is_not_delayed_by_hysteresis() {
        let mut engine = Engine::new(config(id(1)));
        let route_key = key();
        insert_candidate(&mut engine, route_key, id(2), "eth0", "fe80::2", 200);
        let current = engine
            .candidates
            .values()
            .find(|candidate| candidate.interface == "eth0")
            .map(selected_from_candidate)
            .unwrap();
        engine.selected.insert(route_key, current);
        engine.settling_since.insert(route_key, 0);
        insert_candidate(&mut engine, route_key, id(3), "eth1", "fe80::3", 180);

        engine.reselect(1_000);
        assert_eq!(engine.selected_routes()[0].interface, "eth1");
        assert_eq!(engine.settling_since[&route_key], 1_000);
        assert!(!engine.settled_routes.contains(&route_key));
    }

    #[test]
    fn meaningful_current_route_recovery_cancels_a_stale_switch() {
        let mut engine = Engine::new(config(id(1)));
        let route_key = key();
        insert_candidate(&mut engine, route_key, id(2), "eth0", "fe80::2", 200);
        insert_candidate(&mut engine, route_key, id(3), "eth1", "fe80::3", 180);
        let current = engine
            .candidates
            .values()
            .find(|candidate| candidate.interface == "eth0")
            .map(selected_from_candidate)
            .unwrap();
        engine.selected.insert(route_key, current);
        engine.settled_routes.insert(route_key);

        engine.reselect(0);
        engine
            .candidates
            .values_mut()
            .find(|candidate| candidate.interface == "eth0")
            .unwrap()
            .metric = 240;
        engine.reselect(3_000);
        engine
            .candidates
            .values_mut()
            .find(|candidate| candidate.interface == "eth0")
            .unwrap()
            .metric = 220;
        engine.reselect(7_000);
        assert_eq!(engine.selected_routes()[0].interface, "eth0");

        engine.reselect(8_000);
        engine.reselect(15_999);
        assert_eq!(engine.selected_routes()[0].interface, "eth0");
        engine.reselect(16_000);
        assert_eq!(engine.selected_routes()[0].interface, "eth1");
    }

    fn contains_unicast_timestamp_hello(actions: &[Action]) -> bool {
        actions.iter().any(|action| matches!(
            action,
            Action::Send { packet, .. }
                if packet.tlvs.iter().any(|tlv| matches!(tlv,
                    OutboundTlv::Hello { unicast: true, sub_tlvs, .. }
                        if sub_tlvs.iter().any(|sub_tlv| matches!(sub_tlv, SubTlv::TimestampHello(_)))
                ))
        ))
    }

    fn insert_candidate(
        engine: &mut Engine,
        route_key: RouteKey,
        router_id: RouterId,
        interface: &str,
        next_hop: &str,
        metric: u16,
    ) {
        let neighbour = NeighborKey {
            interface: interface.into(),
            address: next_hop.parse().unwrap(),
        };
        engine.candidates.insert(
            (route_key, neighbour),
            Candidate {
                key: route_key,
                router_id,
                seqno: 1,
                advertised_metric: 0,
                metric,
                next_hop: next_hop.parse().unwrap(),
                interface: interface.into(),
                interval_cs: 400,
                expires_ms: u64::MAX,
                refresh_requested: false,
            },
        );
    }
}
