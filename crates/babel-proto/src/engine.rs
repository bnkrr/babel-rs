use std::collections::{BTreeMap, HashMap, HashSet};
use std::net::{IpAddr, Ipv6Addr};
use std::sync::Arc;

use crate::metric::{
    AdditiveMetric, HelloHistories, HelloHistoryUpdate, MetricAlgebra, MetricProfile,
    NeighborMetric, WiredMetric,
};
use crate::model::{Distance, INFINITY, RouteKey, RouterId, SelectedRoute, seqno_gt};
use crate::wire::{Packet, SubTlv, Tlv, Update};

pub const BABEL_MULTICAST_V6: IpAddr = IpAddr::V6(Ipv6Addr::new(0xff02, 0, 0, 0, 0, 0, 1, 6));

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
    Tick {
        now_ms: u64,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Action {
    Send {
        interface: String,
        destination: IpAddr,
        packet: Packet,
    },
    RoutesChanged {
        generation: u64,
        routes: Vec<SelectedRoute>,
    },
    SequenceNumberChanged(u16),
}

#[derive(Clone, Debug)]
struct InterfaceState {
    local_addresses: Vec<IpAddr>,
    hello_seqno: u16,
    next_hello_ms: u64,
    next_update_ms: u64,
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
    expires_ms: u64,
}

#[derive(Clone, Debug)]
struct Originated {
    metric: u16,
    seqno: u16,
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
    candidates: HashMap<(RouteKey, RouterId, NeighborKey), Candidate>,
    feasible: HashMap<(RouteKey, RouterId), Distance>,
    originated: BTreeMap<RouteKey, Originated>,
    selected: BTreeMap<RouteKey, SelectedRoute>,
    pending_seqno: HashMap<(RouteKey, RouterId), u64>,
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
            Event::Tick { now_ms } => self.tick(now_ms),
        }
    }

    pub fn selected_routes(&self) -> Vec<SelectedRoute> {
        self.selected.values().cloned().collect()
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
                        next_rtt_probe_ms: self
                            .config
                            .metric
                            .rtt_probe_interval_ms()
                            .map(|_| now_ms),
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
                        self.config.metric.rtt_probe_interval_ms().map(|_| now_ms);
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
        self.recompute_candidate_metrics(now_ms, Some(&neighbour_key));
        if send_ihu && let Some(action) = self.ihu_action(&neighbour_key, now_ms) {
            actions.push(action);
        }

        for tlv in packet.tlvs {
            match tlv {
                Tlv::AckRequest { nonce, .. } => actions.push(Action::Send {
                    interface: interface.clone(),
                    destination: source,
                    packet: Packet {
                        tlvs: vec![Tlv::Ack { nonce }],
                    },
                }),
                Tlv::Hello { .. } | Tlv::Ihu { .. } => {}
                Tlv::Update(update) => {
                    actions.extend(self.receive_update(&neighbour_key, update, now_ms))
                }
                Tlv::RouteRequest { key, .. } => {
                    actions.extend(self.send_updates(now_ms, key, Some(interface.clone())))
                }
                Tlv::SeqnoRequest {
                    key,
                    seqno,
                    hop_count: _,
                    router_id,
                    ..
                } if router_id == self.config.router_id && self.originated.contains_key(&key) => {
                    self.advance_sequence_number(seqno);
                    actions.push(Action::SequenceNumberChanged(self.sequence_number));
                    actions.extend(self.send_updates(now_ms, Some(key), Some(interface.clone())));
                }
                Tlv::SeqnoRequest {
                    key,
                    seqno,
                    hop_count,
                    router_id,
                    ..
                } if hop_count > 1 => {
                    if let Some(route) = self
                        .selected
                        .get(&key)
                        .filter(|route| route.router_id == router_id)
                    {
                        if route.seqno == seqno || seqno_gt(route.seqno, seqno) {
                            actions.push(self.learned_update_action(route, interface.clone()));
                        } else if route.interface != interface {
                            actions.push(Action::Send {
                                interface: route.interface.clone(),
                                destination: BABEL_MULTICAST_V6,
                                packet: Packet {
                                    tlvs: vec![Tlv::SeqnoRequest {
                                        key,
                                        seqno,
                                        hop_count: hop_count - 1,
                                        router_id,
                                        sub_tlvs: vec![],
                                    }],
                                },
                            });
                        }
                    }
                }
                _ => {}
            }
        }
        actions.extend(self.reselect(now_ms));
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
            neighbour.next_rtt_probe_ms = self
                .config
                .metric
                .rtt_probe_interval_ms()
                .map(|interval| now_ms.saturating_add(interval));
        }
        let mut tlvs = Vec::new();
        if send_probe {
            neighbour.unicast_hello_seqno = neighbour.unicast_hello_seqno.wrapping_add(1);
            tlvs.push(Tlv::Hello {
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
        tlvs.push(Tlv::Ihu {
            address: None,
            rxcost: receive_cost,
            interval_cs,
            sub_tlvs,
        });
        Some(Action::Send {
            interface: key.interface.clone(),
            destination: key.address,
            packet: Packet { tlvs },
        })
    }

    fn receive_update(
        &mut self,
        neighbour_key: &NeighborKey,
        update: Update,
        now_ms: u64,
    ) -> Vec<Action> {
        let Some(key) = update.key else {
            if update.metric == INFINITY {
                self.candidates
                    .retain(|(_, _, neighbour), _| neighbour != neighbour_key);
            }
            return Vec::new();
        };
        let Some(router_id) = update.router_id else {
            return Vec::new();
        };
        // Multicast loopback varies across kernels and network namespaces.
        // A Router-ID identifies an originating Babel speaker, so accepting our
        // own Update can only manufacture a route back through ourselves.
        if router_id == self.config.router_id {
            return Vec::new();
        }
        let candidate_key = (key, router_id, neighbour_key.clone());
        if update.metric == INFINITY {
            let newer_than_feasible = self
                .feasible
                .get(&(key, router_id))
                .is_none_or(|distance| seqno_gt(update.seqno, distance.seqno));
            if newer_than_feasible {
                self.feasible.insert(
                    (key, router_id),
                    Distance {
                        seqno: update.seqno,
                        metric: INFINITY,
                    },
                );
                self.candidates
                    .retain(|(candidate_key, candidate_router, _), route| {
                        *candidate_key != key
                            || *candidate_router != router_id
                            || !seqno_gt(update.seqno, route.seqno)
                    });
            }
            self.candidates.remove(&candidate_key);
            self.pending_seqno.remove(&(key, router_id));
            if newer_than_feasible {
                return self
                    .interfaces
                    .keys()
                    .filter(|interface| *interface != &neighbour_key.interface)
                    .map(|interface| Action::Send {
                        interface: interface.clone(),
                        destination: BABEL_MULTICAST_V6,
                        packet: Packet {
                            tlvs: vec![
                                Tlv::RouterId(router_id),
                                Tlv::Update(Update {
                                    key: Some(key),
                                    router_id: Some(router_id),
                                    next_hop: None,
                                    interval_cs: self.config.update_interval_cs,
                                    seqno: update.seqno,
                                    metric: INFINITY,
                                    v4_via_v6: key.destination.addr().is_ipv4(),
                                    sub_tlvs: vec![],
                                }),
                            ],
                        },
                    })
                    .collect();
            }
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
        let feasible = self.feasible.get(&(key, router_id)).copied();
        let already_acquired = self.candidates.contains_key(&candidate_key);
        if let Some(fd) = feasible
            && !distance.feasible_against(fd)
            && !already_acquired
        {
            let pending_key = (key, router_id);
            if self
                .pending_seqno
                .get(&pending_key)
                .is_some_and(|expires| *expires > now_ms)
            {
                return Vec::new();
            }
            self.pending_seqno.insert(
                pending_key,
                now_ms.saturating_add(u64::from(self.config.hello_interval_cs) * 10),
            );
            return vec![Action::Send {
                interface: neighbour_key.interface.clone(),
                destination: BABEL_MULTICAST_V6,
                packet: Packet {
                    tlvs: vec![Tlv::SeqnoRequest {
                        key,
                        seqno: fd.seqno.wrapping_add(1),
                        hop_count: 16,
                        router_id,
                        sub_tlvs: vec![],
                    }],
                },
            }];
        }
        self.feasible
            .entry((key, router_id))
            .and_modify(|fd| {
                if seqno_gt(distance.seqno, fd.seqno)
                    || (distance.seqno == fd.seqno && distance.metric < fd.metric)
                {
                    *fd = distance;
                }
            })
            .or_insert(distance);
        self.pending_seqno.remove(&(key, router_id));
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
                expires_ms: now_ms.saturating_add(u64::from(update.interval_cs) * 35),
            },
        );
        Vec::new()
    }

    fn tick(&mut self, now_ms: u64) -> Vec<Action> {
        for neighbour in self.neighbours.values_mut() {
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
        for key in expired {
            self.neighbours.remove(&key);
            self.candidates
                .retain(|(_, _, neighbour), _| neighbour != &key);
        }
        for neighbour in self.neighbours.values_mut() {
            if neighbour.last_ihu_ms.is_some_and(|last| {
                now_ms > last.saturating_add(u64::from(neighbour.ihu_interval_cs) * 35)
            }) {
                neighbour.last_ihu_ms = None;
                neighbour.metric.on_ihu(INFINITY);
            }
        }
        self.candidates
            .retain(|_, route| route.expires_ms >= now_ms);
        self.pending_seqno.retain(|_, expires| *expires >= now_ms);
        self.recompute_candidate_metrics(now_ms, None);
        let mut actions = self.reselect(now_ms);
        let ihu_due: Vec<_> = self
            .neighbours
            .iter()
            .filter_map(|(key, neighbour)| {
                let regular_due = now_ms >= neighbour.next_ihu_ms;
                let probe_due = neighbour
                    .next_rtt_probe_ms
                    .is_some_and(|deadline| now_ms >= deadline);
                (regular_due || probe_due).then(|| key.clone())
            })
            .collect();
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
                    packet: Packet {
                        tlvs: vec![Tlv::Hello {
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
        &self,
        _now_ms: u64,
        only: Option<RouteKey>,
        interface: Option<String>,
    ) -> Vec<Action> {
        let interfaces: Vec<_> =
            interface.map_or_else(|| self.interfaces.keys().cloned().collect(), |v| vec![v]);
        interfaces
            .into_iter()
            .map(|interface| {
                let mut tlvs = vec![Tlv::RouterId(self.config.router_id)];
                for (key, origin) in &self.originated {
                    if only.is_none_or(|wanted| wanted == *key) {
                        tlvs.push(Tlv::Update(Update {
                            key: Some(*key),
                            router_id: Some(self.config.router_id),
                            next_hop: None,
                            interval_cs: self.config.update_interval_cs,
                            seqno: origin.seqno,
                            metric: origin.metric,
                            v4_via_v6: key.destination.addr().is_ipv4(),
                            sub_tlvs: vec![],
                        }));
                    }
                }
                for route in self.selected.values() {
                    // Split horizon: never advertise a selected route back on
                    // the interface from which its next hop was learned.
                    if route.interface != interface && only.is_none_or(|wanted| wanted == route.key)
                    {
                        tlvs.push(Tlv::RouterId(route.router_id));
                        tlvs.push(Tlv::Update(Update {
                            key: Some(route.key),
                            router_id: Some(route.router_id),
                            next_hop: None,
                            interval_cs: self.config.update_interval_cs,
                            seqno: route.seqno,
                            metric: route.metric,
                            v4_via_v6: route.key.destination.addr().is_ipv4(),
                            sub_tlvs: vec![],
                        }));
                    }
                }
                Action::Send {
                    interface,
                    destination: BABEL_MULTICAST_V6,
                    packet: Packet { tlvs },
                }
            })
            .collect()
    }

    fn send_retraction(&self, key: RouteKey, seqno: u16, _now_ms: u64) -> Vec<Action> {
        self.interfaces
            .keys()
            .map(|interface| Action::Send {
                interface: interface.clone(),
                destination: BABEL_MULTICAST_V6,
                packet: Packet {
                    tlvs: vec![Tlv::Update(Update {
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

    fn recompute_candidate_metrics(&mut self, _now_ms: u64, only: Option<&NeighborKey>) {
        let costs: HashMap<_, _> = self
            .neighbours
            .iter()
            .filter(|(key, _)| only.is_none_or(|wanted| wanted == *key))
            .map(|(key, neighbour)| (key.clone(), neighbour.metric.link_cost()))
            .collect();
        for ((_, _, neighbour_key), candidate) in &mut self.candidates {
            let Some(link_cost) = costs.get(neighbour_key) else {
                continue;
            };
            let metric = self
                .config
                .metric_algebra
                .extend(candidate.advertised_metric, *link_cost);
            if metric == INFINITY || metric <= candidate.advertised_metric {
                candidate.metric = INFINITY;
                continue;
            }
            candidate.metric = metric;
        }
    }

    fn reselect(&mut self, now_ms: u64) -> Vec<Action> {
        let before = self.selected.clone();
        let mut next = BTreeMap::new();
        let mut keys: Vec<_> = self
            .candidates
            .values()
            .filter(|route| route.metric < INFINITY)
            .map(|route| route.key)
            .collect();
        keys.sort();
        keys.dedup();
        for key in keys {
            let routes: Vec<_> = self
                .candidates
                .values()
                .filter(|route| route.key == key && route.metric < INFINITY)
                .collect();
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
        self.pending_switches
            .retain(|key, _| self.selected.contains_key(key));
        self.settling_since
            .retain(|key, _| self.selected.contains_key(key));
        self.settled_routes
            .retain(|key| self.selected.contains_key(key));
        if self.selected != before {
            self.generation = self.generation.wrapping_add(1);
            let mut actions = vec![Action::RoutesChanged {
                generation: self.generation,
                routes: self.selected_routes(),
            }];
            actions.extend(self.selected_delta(&before));
            actions
        } else {
            Vec::new()
        }
    }

    fn selected_delta(&self, before: &BTreeMap<RouteKey, SelectedRoute>) -> Vec<Action> {
        let mut actions = Vec::new();
        for (key, previous) in before {
            if !self.selected.contains_key(key) {
                actions.extend(self.advertise_learned(previous, INFINITY, None));
            }
        }
        for (key, selected) in &self.selected {
            if before.get(key) != Some(selected) {
                actions.extend(self.advertise_learned(
                    selected,
                    selected.metric,
                    Some(&selected.interface),
                ));
            }
        }
        actions
    }

    fn advertise_learned(
        &self,
        route: &SelectedRoute,
        metric: u16,
        exclude_interface: Option<&str>,
    ) -> Vec<Action> {
        self.interfaces
            .keys()
            .filter(|interface| exclude_interface != Some(interface.as_str()))
            .map(|interface| Action::Send {
                interface: interface.clone(),
                destination: BABEL_MULTICAST_V6,
                packet: Packet {
                    tlvs: vec![
                        Tlv::RouterId(route.router_id),
                        Tlv::Update(Update {
                            key: Some(route.key),
                            router_id: Some(route.router_id),
                            next_hop: None,
                            interval_cs: self.config.update_interval_cs,
                            seqno: route.seqno,
                            metric,
                            v4_via_v6: route.key.destination.addr().is_ipv4(),
                            sub_tlvs: vec![],
                        }),
                    ],
                },
            })
            .collect()
    }

    fn learned_update_action(&self, route: &SelectedRoute, interface: String) -> Action {
        Action::Send {
            interface,
            destination: BABEL_MULTICAST_V6,
            packet: Packet {
                tlvs: vec![
                    Tlv::RouterId(route.router_id),
                    Tlv::Update(Update {
                        key: Some(route.key),
                        router_id: Some(route.router_id),
                        next_hop: None,
                        interval_cs: self.config.update_interval_cs,
                        seqno: route.seqno,
                        metric: route.metric,
                        v4_via_v6: route.key.destination.addr().is_ipv4(),
                        sub_tlvs: vec![],
                    }),
                ],
            },
        }
    }

    fn bump_sequence_number(&mut self) {
        self.sequence_number = self.sequence_number.wrapping_add(1);
        for origin in self.originated.values_mut() {
            origin.seqno = self.sequence_number;
        }
    }

    fn advance_sequence_number(&mut self, requested: u16) {
        if !seqno_gt(self.sequence_number, requested) {
            self.sequence_number = requested;
        }
        self.bump_sequence_number();
    }
}

fn timestamp_us(now_ms: u64) -> u32 {
    now_ms.wrapping_mul(1_000) as u32
}

fn valid_cost(cost: u16) -> u16 {
    if cost == 0 { INFINITY } else { cost }
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
                tlvs: vec![Tlv::Update(Update {
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
                    Tlv::Update(Update {
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
                    Tlv::Update(Update {
                        key: Some(key()),
                        router_id: Some(id(2)),
                        next_hop: Some(other),
                        interval_cs: 1600,
                        seqno: 5,
                        metric: 20,
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
                if packet.tlvs.iter().any(|tlv| matches!(tlv, Tlv::Update(update) if update.metric == INFINITY && update.seqno == 1))
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
                    Tlv::Update(Update {
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
                if interface == "wg1" && packet.tlvs.iter().any(|tlv| matches!(tlv, Tlv::Update(update) if update.metric == INFINITY))
        )));
    }

    #[test]
    fn newer_retraction_removes_stale_alternate_candidates() {
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
                        Tlv::Update(Update {
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
                tlvs: vec![Tlv::Update(Update {
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
        assert!(engine.selected_routes().is_empty());
        assert!(actions.iter().any(
            |action| matches!(action, Action::RoutesChanged { routes, .. } if routes.is_empty())
        ));
        assert!(actions.iter().any(|action| matches!(
            action,
            Action::Send { interface, packet, .. }
                if interface == "wg1"
                    && packet.tlvs.iter().any(|tlv| matches!(tlv, Tlv::Update(update) if update.metric == INFINITY && update.seqno == 8))
        )));
    }

    #[test]
    fn seqno_request_advances_local_origin_past_requested_value() {
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
                .any(|action| matches!(action, Action::SequenceNumberChanged(101)))
        );
        assert!(actions.iter().any(|action| matches!(
            action,
            Action::Send { packet, .. }
                if packet.tlvs.iter().any(|tlv| matches!(tlv, Tlv::Update(update) if update.seqno == 101))
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
                    Tlv::Update(Update {
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
                if interface == "upstream" && packet.tlvs.iter().any(|tlv| matches!(tlv, Tlv::SeqnoRequest { seqno: 6, hop_count: 6, .. }))
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
                    Tlv::Update(Update {
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
                    Tlv::Update(Update {
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
                if packet.tlvs.iter().any(|tlv| matches!(tlv, Tlv::Hello { sub_tlvs, .. }
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
                    Tlv::Update(Update {
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
        assert!(contains_unicast_timestamp_hello(&actions));
        assert!(!contains_unicast_timestamp_hello(
            &engine.handle(Event::Tick { now_ms: 2_009 })
        ));
        assert!(contains_unicast_timestamp_hello(
            &engine.handle(Event::Tick { now_ms: 2_010 })
        ));
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
            .retain(|(_, _, neighbour), _| neighbour.interface != "eth0");
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
                    Tlv::Hello { unicast: true, sub_tlvs, .. }
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
            (route_key, router_id, neighbour),
            Candidate {
                key: route_key,
                router_id,
                seqno: 1,
                advertised_metric: 0,
                metric,
                next_hop: next_hop.parse().unwrap(),
                interface: interface.into(),
                expires_ms: u64::MAX,
            },
        );
    }
}
