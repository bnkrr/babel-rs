//! Neighbor observations, received packets and IHU output.

use super::*;

impl Engine {
    /// Number of currently retained adjacencies, including unconfirmed links.
    pub fn neighbour_count(&self) -> usize {
        self.neighbours.len()
    }

    /// Clone adjacency observations with ages relative to the supplied engine clock.
    pub fn neighbour_status(&self, now_ms: u64) -> Vec<NeighborStatus> {
        let mut result: Vec<_> = self
            .neighbours
            .iter()
            .map(|(key, neighbour)| {
                let metric = neighbour.metric.status();
                NeighborStatus {
                    interface: key.interface.clone(),
                    address: key.address,
                    candidates: neighbour.candidates,
                    rejected_candidates: neighbour.rejected_candidates,
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

    pub(super) fn receive(
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
        let metric_profile = Arc::clone(&interface_state.policy.metric);
        let hello_interval_cs = interface_state.policy.hello_interval_cs;
        let neighbour_key = NeighborKey {
            interface: interface.clone(),
            address: source,
        };
        if !self.neighbours.contains_key(&neighbour_key)
            && self.neighbours.len() >= self.config.limits.max_neighbors
            && packet
                .tlvs
                .iter()
                .any(|tlv| matches!(tlv, Tlv::Hello { .. }))
        {
            self.resources.rejected_neighbors = self.resources.rejected_neighbors.saturating_add(1);
            return Vec::new();
        }
        let previous_link_cost = self
            .neighbours
            .get(&neighbour_key)
            .map(|n| n.metric.link_cost());
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
                        candidates: 0,
                        rejected_candidates: 0,
                        last_hello_ms: now_ms,
                        histories: HelloHistories::default(),
                        multicast_timer: None,
                        unicast_timer: None,
                        last_ihu_ms: None,
                        last_ihu_cost: None,
                        ihu_interval_cs: hello_interval_cs.saturating_mul(3),
                        next_ihu_ms: now_ms,
                        next_rtt_probe_ms: metric_profile.rtt_probe_interval_ms().map(|interval| {
                            initial_probe_deadline(now_ms, interval, &neighbour_key)
                        }),
                        origin_timestamp: None,
                        receive_timestamp: None,
                        unicast_hello_seqno: 0,
                        metric: metric_profile.new_neighbor(&interface),
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
                    neighbour.last_ihu_cost = None;
                    neighbour.next_ihu_ms = now_ms;
                    neighbour.next_rtt_probe_ms = metric_profile
                        .rtt_probe_interval_ms()
                        .map(|interval| initial_probe_deadline(now_ms, interval, &neighbour_key));
                    neighbour.origin_timestamp = None;
                    neighbour.receive_timestamp = None;
                    neighbour.unicast_hello_seqno = 0;
                    neighbour.metric = metric_profile.new_neighbor(&interface);
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
            && metric_profile.timestamps_enabled()
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
                neighbour.last_ihu_cost = Some(*rxcost);
                neighbour.ihu_interval_cs = *interval_cs;
            }
        }
        let mut routes_changed = false;
        if previous_link_cost
            != self
                .neighbours
                .get(&neighbour_key)
                .map(|n| n.metric.link_cost())
        {
            routes_changed = self.recompute_candidate_metrics(Some(&neighbour_key));
        }
        if send_ihu && let Some(action) = self.ihu_action(&neighbour_key, now_ms, None) {
            actions.push(action);
        }

        for tlv in packet.tlvs {
            match tlv {
                Tlv::AckRequest { nonce, interval_cs } => actions.push(Action::Send {
                    interface: interface.clone(),
                    destination: source,
                    packet: OutboundPacket {
                        tlvs: vec![OutboundTlv::Ack { nonce }],
                    },
                    timing: SendTiming::by_deadline(
                        now_ms,
                        now_ms.saturating_add(u64::from(interval_cs) * 10),
                    ),
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
                    let (changed, updates) = self.receive_update(&neighbour_key, update, now_ms);
                    routes_changed |= changed;
                    actions.extend(updates);
                }
                Tlv::RouteRequest { key, .. } => {
                    // Replies can update feasibility history through send_updates.
                    routes_changed = true;
                    if key.is_some() || self.full_update_request_allowed(&interface, now_ms) {
                        let updates = self.send_updates(
                            now_ms,
                            key,
                            Some(interface.clone()),
                            Some(SendTiming::urgent(now_ms)),
                        );
                        if let Some(key) = key.filter(|_| updates.is_empty()) {
                            actions.push(self.retraction_action(
                                key,
                                self.sequence_number,
                                neighbour_key.clone(),
                                now_ms,
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
                    routes_changed = true;
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
        if routes_changed {
            actions.extend(self.reselect(now_ms));
        }
        // RFC 8966 section 3.5.3 requires a timely triggered update whenever
        // an existing route entry changes Router-ID, even when that entry is
        // not selected and route selection itself therefore did not change.
        for key in changed_router_ids {
            if self.originated.contains_key(&key) || self.selected.contains_key(&key) {
                actions.extend(self.send_updates(
                    now_ms,
                    Some(key),
                    None,
                    Some(SendTiming::urgent(now_ms)),
                ));
            } else {
                actions.extend(self.send_retraction(key, self.sequence_number, now_ms));
            }
        }
        actions
    }

    pub(super) fn ihu_action(
        &mut self,
        key: &NeighborKey,
        now_ms: u64,
        periodic_deadline: Option<u64>,
    ) -> Option<Action> {
        let policy = self.interfaces.get(&key.interface)?.policy.clone();
        let neighbour = self.neighbours.get_mut(key)?;
        let receive_cost = valid_cost(neighbour.metric.receive_cost());
        let echoed = neighbour.origin_timestamp.zip(neighbour.receive_timestamp);
        let interval_cs = policy.hello_interval_cs.saturating_mul(3);
        neighbour.next_ihu_ms = periodic_deadline.map_or_else(
            || now_ms.saturating_add(u64::from(interval_cs) * 10),
            |deadline| next_periodic_deadline(deadline, now_ms, interval_cs),
        );
        let send_probe = neighbour
            .next_rtt_probe_ms
            .is_some_and(|deadline| now_ms >= deadline);
        if send_probe {
            neighbour.unicast_hello_seqno = neighbour.unicast_hello_seqno.wrapping_add(1);
            neighbour.next_rtt_probe_ms = policy.metric.rtt_probe_interval_ms().map(|interval| {
                recurring_probe_deadline(now_ms, interval, key, neighbour.unicast_hello_seqno)
            });
        }
        let mut tlvs = Vec::new();
        if send_probe || (policy.metric.timestamps_enabled() && echoed.is_some()) {
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
        let sub_tlvs = if policy.metric.timestamps_enabled() {
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
            timing: periodic_deadline.map_or_else(
                || SendTiming::triggered(now_ms, policy.hello_interval_cs),
                |deadline| SendTiming::by_deadline(now_ms, deadline),
            ),
        })
    }
}

fn valid_cost(cost: u16) -> u16 {
    if cost == 0 { INFINITY } else { cost }
}

fn ihu_applies(address: Option<IpAddr>, local_addresses: &[IpAddr]) -> bool {
    address.is_none_or(|value| local_addresses.is_empty() || local_addresses.contains(&value))
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
