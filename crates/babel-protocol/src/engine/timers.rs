//! Periodic output, expiration and RTT probe scheduling.

use super::*;

impl Engine {
    pub(super) fn tick(&mut self, now_ms: u64) -> Vec<Action> {
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
                neighbour.last_ihu_cost = None;
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
                if let Some(neighbour) = self.neighbours.get_mut(&key.1) {
                    neighbour.candidates -= 1;
                }
            } else if let Some(route) = self.candidates.get_mut(&key) {
                route.advertised_metric = INFINITY;
                route.metric = INFINITY;
                route.expires_ms =
                    now_ms.saturating_add(u64::from(route.interval_cs).saturating_mul(35));
            }
            routes_may_have_changed = true;
        }
        let selected = self.selected.clone();
        let hello_intervals: HashMap<_, _> = self
            .interfaces
            .iter()
            .map(|(name, state)| (name.clone(), state.policy.hello_interval_cs))
            .collect();
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
                    timing: SendTiming::triggered(
                        now_ms,
                        hello_intervals
                            .get(&neighbour.interface)
                            .copied()
                            .unwrap_or(self.config.hello_interval_cs),
                    ),
                });
            }
        }
        let expired_tombstones = self.tombstones.len();
        self.tombstones.retain(|_, expires| *expires >= now_ms);
        routes_may_have_changed |= expired_tombstones != self.tombstones.len();
        self.advertised_until.retain(|_, until| *until >= now_ms);
        let previous_sources = self.feasible.len();
        self.feasible
            .retain(|_, source| source.expires_ms >= now_ms);
        // Source GC can make an unchanged candidate feasible. It is a route
        // selection input even when no route or neighbor expires this tick.
        routes_may_have_changed |= self.feasible.len() != previous_sources;
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
        if expired_tombstones != self.tombstones.len()
            && !actions
                .iter()
                .any(|a| matches!(a, Action::RoutesChanged { .. }))
        {
            actions.push(self.route_snapshot());
        }
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
                now_ms,
            ));
            if let Some(value) = self.pending_seqno.get_mut(&(key, router_id)) {
                let attempt = REQUEST_RETRIES.saturating_sub(value.retries_left);
                value.retries_left -= 1;
                value.next_retry_ms = now_ms.saturating_add(
                    REQUEST_RETRY_INITIAL_MS.saturating_mul(1u64 << u32::from(attempt + 1)),
                );
            }
        }
        let repeat_keys: Vec<_> = self
            .pending_advertisements
            .iter()
            .filter(|(_, (deadline, _))| now_ms >= *deadline)
            .map(|(key, _)| *key)
            .collect();
        for key in repeat_keys {
            let (_, left) = self
                .pending_advertisements
                .remove(&key)
                .expect("pending repeat");
            actions.extend(self.send_updates(
                now_ms,
                Some(key),
                None,
                Some(SendTiming::urgent(now_ms)),
            ));
            if left > 1 {
                self.pending_advertisements
                    .insert(key, (now_ms.saturating_add(1_000), left - 1));
            }
        }
        let mut ihu_due: Vec<_> = self
            .neighbours
            .iter()
            .filter(|(_, neighbour)| periodic_window_open(now_ms, neighbour.next_ihu_ms))
            .map(|(key, neighbour)| (key.clone(), Some(neighbour.next_ihu_ms)))
            .collect();
        let regular: HashSet<_> = ihu_due.iter().map(|(key, _)| key.clone()).collect();
        ihu_due.extend(
            self.neighbours
                .iter()
                .filter(|(key, neighbour)| {
                    !regular.contains(*key)
                        && neighbour
                            .next_rtt_probe_ms
                            .is_some_and(|deadline| now_ms >= deadline)
                })
                .map(|(key, _)| (key.clone(), None))
                .take(MAX_RTT_PROBES_PER_TICK),
        );
        for (key, deadline) in ihu_due {
            if let Some(action) = self.ihu_action(&key, now_ms, deadline) {
                actions.push(action);
            }
        }
        let interfaces: Vec<String> = self.interfaces.keys().cloned().collect();
        for interface in interfaces {
            let mut hello_deadline = None;
            let mut update_deadline = None;
            let seqno;
            {
                let state = self
                    .interfaces
                    .get_mut(&interface)
                    .expect("interface exists");
                if periodic_window_open(now_ms, state.next_hello_ms) {
                    state.hello_seqno = state.hello_seqno.wrapping_add(1);
                    hello_deadline = Some(state.next_hello_ms);
                    state.next_hello_ms = next_periodic_deadline(
                        state.next_hello_ms,
                        now_ms,
                        state.policy.hello_interval_cs,
                    );
                }
                if periodic_window_open(now_ms, state.next_update_ms) {
                    update_deadline = Some(state.next_update_ms);
                    state.next_update_ms = next_periodic_deadline(
                        state.next_update_ms,
                        now_ms,
                        state.policy.update_interval_cs,
                    );
                }
                seqno = state.hello_seqno;
            }
            if let Some(deadline) = hello_deadline {
                let policy = &self
                    .interfaces
                    .get(&interface)
                    .expect("interface exists")
                    .policy;
                let sub_tlvs = if policy.metric.timestamps_enabled() {
                    vec![SubTlv::TimestampHello(timestamp_us(now_ms))]
                } else {
                    Vec::new()
                };
                actions.push(Action::Send {
                    interface: interface.clone(),
                    destination: self
                        .interfaces
                        .get(&interface)
                        .expect("interface exists")
                        .policy
                        .control_transport
                        .multicast(),
                    packet: OutboundPacket {
                        tlvs: vec![OutboundTlv::Hello {
                            unicast: false,
                            seqno,
                            interval_cs: policy.hello_interval_cs,
                            sub_tlvs,
                        }],
                    },
                    timing: SendTiming::by_deadline(now_ms, deadline),
                });
            }
            if let Some(deadline) = update_deadline {
                actions.extend(self.send_updates(
                    now_ms,
                    None,
                    Some(interface),
                    Some(SendTiming::by_deadline(now_ms, deadline)),
                ));
            }
        }
        actions
    }
}

fn periodic_window_open(now_ms: u64, deadline_ms: u64) -> bool {
    now_ms.saturating_add(MAX_TRIGGERED_JITTER_MS) >= deadline_ms
}

pub(super) fn next_periodic_deadline(deadline_ms: u64, now_ms: u64, interval_cs: u16) -> u64 {
    let interval_ms = u64::from(interval_cs).saturating_mul(10);
    let anchored = deadline_ms.saturating_add(interval_ms);
    if anchored > now_ms {
        anchored
    } else {
        now_ms.saturating_add(interval_ms)
    }
}

pub(super) fn timestamp_us(now_ms: u64) -> u32 {
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

pub(super) fn initial_probe_deadline(now_ms: u64, interval_ms: u64, key: &NeighborKey) -> u64 {
    let spread = interval_ms.clamp(1, 200);
    now_ms.saturating_add(probe_salt(key, 0) % spread)
}

pub(super) fn recurring_probe_deadline(
    now_ms: u64,
    interval_ms: u64,
    key: &NeighborKey,
    seqno: u16,
) -> u64 {
    let spread = (interval_ms / 5).max(1);
    let delay = interval_ms
        .saturating_mul(9)
        .saturating_div(10)
        .saturating_add(probe_salt(key, seqno) % spread);
    now_ms.saturating_add(delay)
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
