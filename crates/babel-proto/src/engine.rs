use std::collections::{BTreeMap, HashMap};
use std::net::{IpAddr, Ipv6Addr};

use crate::metric::{LinkMetric, LinkSample};
use crate::model::{Distance, INFINITY, RouteKey, RouterId, SelectedRoute, seqno_gt};
use crate::wire::{Packet, Tlv, Update};

pub const BABEL_MULTICAST_V6: IpAddr = IpAddr::V6(Ipv6Addr::new(0xff02, 0, 0, 0, 0, 0, 1, 6));

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NeighborStatus {
    pub interface: String,
    pub address: IpAddr,
    pub hello_received: u16,
    pub hello_expected: u16,
    pub remote_rxcost: u16,
    pub cost: u16,
    pub last_hello_age_ms: u64,
}

#[derive(Clone, Debug)]
pub struct EngineConfig<M> {
    pub router_id: RouterId,
    pub metric: M,
    pub sequence_number: u16,
    pub hello_interval_cs: u16,
    pub update_interval_cs: u16,
}

impl<M: Default> EngineConfig<M> {
    pub fn recommended(router_id: RouterId) -> Self {
        Self {
            router_id,
            metric: M::default(),
            sequence_number: 0,
            hello_interval_cs: 400,
            update_interval_cs: 1600,
        }
    }
}

#[derive(Clone, Debug)]
pub enum Event {
    InterfaceUp {
        interface: String,
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
    hello_seqno: u16,
    next_hello_ms: u64,
    next_update_ms: u64,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct NeighborKey {
    interface: String,
    address: IpAddr,
}

#[derive(Clone, Debug)]
struct Neighbor {
    last_hello_ms: u64,
    hello_interval_cs: u16,
    last_seqno: u16,
    hello_received: u16,
    hello_expected: u16,
    remote_rxcost: u16,
}

#[derive(Clone, Debug)]
struct Candidate {
    key: RouteKey,
    router_id: RouterId,
    seqno: u16,
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

pub struct Engine<M> {
    config: EngineConfig<M>,
    interfaces: BTreeMap<String, InterfaceState>,
    neighbours: HashMap<NeighborKey, Neighbor>,
    candidates: HashMap<(RouteKey, RouterId, NeighborKey), Candidate>,
    feasible: HashMap<(RouteKey, RouterId), Distance>,
    originated: BTreeMap<RouteKey, Originated>,
    selected: BTreeMap<RouteKey, SelectedRoute>,
    pending_seqno: HashMap<(RouteKey, RouterId), u64>,
    generation: u64,
    sequence_number: u16,
}

impl<M: LinkMetric> Engine<M> {
    pub fn new(config: EngineConfig<M>) -> Self {
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
            generation: 0,
        }
    }

    pub fn handle(&mut self, event: Event) -> Vec<Action> {
        match event {
            Event::InterfaceUp { interface, now_ms } => {
                self.interfaces
                    .entry(interface.clone())
                    .or_insert(InterfaceState {
                        hello_seqno: 0,
                        next_hello_ms: now_ms,
                        next_update_ms: now_ms,
                    });
                self.tick(now_ms)
            }
            Event::InterfaceDown {
                interface,
                now_ms: _,
            } => {
                self.interfaces.remove(&interface);
                self.neighbours.retain(|key, _| key.interface != interface);
                self.candidates
                    .retain(|_, value| value.interface != interface);
                self.reselect()
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
                actions.extend(self.reselect());
                actions.extend(self.send_updates(now_ms, Some(key), None));
                actions
            }
            Event::Withdraw { key, now_ms } => {
                let existed = self.originated.remove(&key).is_some();
                let mut actions = self.reselect();
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

    pub fn neighbour_status(&self, now_ms: u64) -> Vec<NeighborStatus> {
        let mut result: Vec<_> = self
            .neighbours
            .iter()
            .map(|(key, neighbour)| NeighborStatus {
                interface: key.interface.clone(),
                address: key.address,
                hello_received: neighbour.hello_received,
                hello_expected: neighbour.hello_expected,
                remote_rxcost: neighbour.remote_rxcost,
                cost: self.config.metric.cost(LinkSample {
                    hello_received: neighbour.hello_received,
                    hello_expected: neighbour.hello_expected,
                    remote_rxcost: neighbour.remote_rxcost,
                }),
                last_hello_age_ms: now_ms.saturating_sub(neighbour.last_hello_ms),
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
        if !self.interfaces.contains_key(&interface) {
            return Vec::new();
        }
        let neighbour_key = NeighborKey {
            interface: interface.clone(),
            address: source,
        };
        let mut actions = Vec::new();
        for tlv in packet.tlvs {
            match tlv {
                Tlv::AckRequest { nonce, .. } => actions.push(Action::Send {
                    interface: interface.clone(),
                    destination: source,
                    packet: Packet {
                        tlvs: vec![Tlv::Ack { nonce }],
                    },
                }),
                Tlv::Hello {
                    seqno, interval_cs, ..
                } if interval_cs != 0 => {
                    let neighbour =
                        self.neighbours
                            .entry(neighbour_key.clone())
                            .or_insert(Neighbor {
                                last_hello_ms: now_ms,
                                hello_interval_cs: interval_cs,
                                last_seqno: seqno.wrapping_sub(1),
                                hello_received: 0,
                                hello_expected: 0,
                                remote_rxcost: INFINITY,
                            });
                    let delta = seqno.wrapping_sub(neighbour.last_seqno);
                    if delta != 0 && delta < 0x8000 {
                        neighbour.hello_expected =
                            neighbour.hello_expected.saturating_add(delta.min(16));
                        neighbour.hello_received = neighbour.hello_received.saturating_add(1);
                        neighbour.last_seqno = seqno;
                    }
                    neighbour.last_hello_ms = now_ms;
                    neighbour.hello_interval_cs = interval_cs;
                    actions.push(Action::Send {
                        interface: interface.clone(),
                        destination: source,
                        packet: Packet {
                            tlvs: vec![Tlv::Ihu {
                                address: None,
                                rxcost: 96,
                                interval_cs: self.config.hello_interval_cs.saturating_mul(3),
                                sub_tlvs: vec![],
                            }],
                        },
                    });
                }
                Tlv::Ihu { rxcost, .. } => {
                    if let Some(neighbour) = self.neighbours.get_mut(&neighbour_key) {
                        neighbour.remote_rxcost = rxcost;
                    }
                }
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
        actions.extend(self.reselect());
        actions
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
        let cost = self.config.metric.cost(LinkSample {
            hello_received: neighbour.hello_received,
            hello_expected: neighbour.hello_expected,
            remote_rxcost: neighbour.remote_rxcost,
        });
        if cost == INFINITY {
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
                metric: update.metric.saturating_add(cost),
                next_hop,
                interface: neighbour_key.interface.clone(),
                expires_ms: now_ms.saturating_add(u64::from(update.interval_cs) * 35),
            },
        );
        Vec::new()
    }

    fn tick(&mut self, now_ms: u64) -> Vec<Action> {
        let expired: Vec<_> = self
            .neighbours
            .iter()
            .filter(|(_, n)| {
                now_ms
                    > n.last_hello_ms
                        .saturating_add(u64::from(n.hello_interval_cs) * 35)
            })
            .map(|(k, _)| k.clone())
            .collect();
        for key in expired {
            self.neighbours.remove(&key);
            self.candidates
                .retain(|(_, _, neighbour), _| neighbour != &key);
        }
        self.candidates
            .retain(|_, route| route.expires_ms >= now_ms);
        self.pending_seqno.retain(|_, expires| *expires >= now_ms);
        let mut actions = self.reselect();
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
                actions.push(Action::Send {
                    interface: interface.clone(),
                    destination: BABEL_MULTICAST_V6,
                    packet: Packet {
                        tlvs: vec![Tlv::Hello {
                            unicast: false,
                            seqno,
                            interval_cs: self.config.hello_interval_cs,
                            sub_tlvs: vec![],
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

    fn reselect(&mut self) -> Vec<Action> {
        let before = self.selected.clone();
        let mut next = BTreeMap::new();
        for route in self
            .candidates
            .values()
            .filter(|route| route.metric < INFINITY)
        {
            let selected = SelectedRoute {
                key: route.key,
                router_id: route.router_id,
                seqno: route.seqno,
                metric: route.metric,
                next_hop: route.next_hop,
                interface: route.interface.clone(),
            };
            next.entry(route.key)
                .and_modify(|current: &mut SelectedRoute| {
                    if (selected.metric, selected.router_id) < (current.metric, current.router_id) {
                        *current = selected.clone();
                    }
                })
                .or_insert(selected);
        }
        self.selected = next;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metric::FixedMetric;
    use ipnet::IpNet;
    use std::str::FromStr;

    fn id(value: u8) -> RouterId {
        RouterId::new([value; 8]).unwrap()
    }
    fn key() -> RouteKey {
        RouteKey::new(IpNet::from_str("2001:db8::/64").unwrap(), None).unwrap()
    }

    #[test]
    fn route_requires_neighbour_and_exports_generation() {
        let mut engine = Engine::new(EngineConfig::<FixedMetric>::recommended(id(1)));
        engine.handle(Event::InterfaceUp {
            interface: "wg0".into(),
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
            metric: FixedMetric::default(),
            sequence_number: 0,
            hello_interval_cs: 400,
            update_interval_cs: 1600,
        });
        engine.handle(Event::InterfaceUp {
            interface: "wg0".into(),
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
        let mut engine = Engine::new(EngineConfig::<FixedMetric>::recommended(id(1)));
        engine.handle(Event::InterfaceUp {
            interface: "wg0".into(),
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
        let mut engine = Engine::new(EngineConfig::<FixedMetric>::recommended(id(1)));
        engine.handle(Event::InterfaceUp {
            interface: "wg0".into(),
            now_ms: 0,
        });
        engine.handle(Event::InterfaceUp {
            interface: "wg1".into(),
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
        let mut engine = Engine::new(EngineConfig::<FixedMetric>::recommended(id(1)));
        for (interface, source, metric) in [("wg0", "fe80::2", 0), ("wg1", "fe80::3", 10)] {
            engine.handle(Event::InterfaceUp {
                interface: interface.into(),
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
        let mut config = EngineConfig::<FixedMetric>::recommended(id(1));
        config.sequence_number = 7;
        let mut engine = Engine::new(config);
        engine.handle(Event::InterfaceUp {
            interface: "wg0".into(),
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
        let mut engine = Engine::new(EngineConfig::<FixedMetric>::recommended(id(1)));
        for interface in ["upstream", "downstream"] {
            engine.handle(Event::InterfaceUp {
                interface: interface.into(),
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
}
