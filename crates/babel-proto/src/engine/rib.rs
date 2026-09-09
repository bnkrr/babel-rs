//! Candidate admission, feasibility and route selection.

use super::*;

impl Engine {
    /// Return whether route-selection inputs changed, alongside protocol output.
    /// Timer refreshes and rejected admissions alone do not require reselection.
    pub(super) fn receive_update(
        &mut self,
        neighbour_key: &NeighborKey,
        update: ResolvedUpdate,
        now_ms: u64,
    ) -> (bool, Vec<Action>) {
        let Some(key) = update.key else {
            let mut changed = false;
            if update.metric == INFINITY {
                for ((_, neighbour), candidate) in &mut self.candidates {
                    if neighbour == neighbour_key {
                        changed |=
                            candidate.advertised_metric != INFINITY || candidate.metric != INFINITY;
                        candidate.advertised_metric = INFINITY;
                        candidate.metric = INFINITY;
                    }
                }
            }
            return (changed, Vec::new());
        };
        if forbidden_destination(key.destination) {
            return (false, Vec::new());
        }
        let candidate_key = (key, neighbour_key.clone());
        if update.metric == INFINITY {
            let mut changed = false;
            if let Some(candidate) = self.candidates.get_mut(&candidate_key) {
                changed = candidate.advertised_metric != INFINITY || candidate.metric != INFINITY;
                candidate.advertised_metric = INFINITY;
                candidate.metric = INFINITY;
            }
            return (changed, Vec::new());
        }
        let Some(router_id) = update.router_id else {
            return (false, Vec::new());
        };
        // Multicast loopback varies across kernels and network namespaces.
        // A Router-ID identifies an originating Babel speaker, so accepting our
        // own Update can only manufacture a route back through ourselves.
        if router_id == self.config.router_id {
            return (false, Vec::new());
        }
        let Some(next_hop) = update.next_hop else {
            return (false, Vec::new());
        };
        let Some(neighbour) = self.neighbours.get(neighbour_key) else {
            return (false, Vec::new());
        };
        let is_new = !self.candidates.contains_key(&candidate_key);
        if is_new {
            let per_neighbor =
                neighbour.candidates >= self.config.limits.max_candidates_per_neighbor;
            if per_neighbor || self.candidates.len() >= self.config.limits.max_candidates {
                let rejected = if per_neighbor {
                    &mut self.resources.rejected_candidates_per_neighbor
                } else {
                    &mut self.resources.rejected_candidates_global
                };
                *rejected = rejected.saturating_add(1);
                let neighbour = self
                    .neighbours
                    .get_mut(neighbour_key)
                    .expect("known neighbor");
                neighbour.rejected_candidates = neighbour.rejected_candidates.saturating_add(1);
                return (false, Vec::new());
            }
        }
        let cost = neighbour.metric.link_cost();
        let metric = self.config.metric_algebra.extend(update.metric, cost);
        if metric != INFINITY && metric <= update.metric {
            return (false, Vec::new());
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
            return (
                false,
                feasible.map_or_else(Vec::new, |fd| {
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
                }),
            );
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
                            now_ms,
                        ));
                    }
                    self.recent_seqno.insert(
                        pending_key,
                        (pending.seqno, now_ms.saturating_add(RECENT_REQUEST_MS)),
                    );
                }
            }
        } else if !self.pending_seqno.contains_key(&(key, router_id))
            // RFC 8966 3.8.2.2: an unselected infeasible route warrants a
            // request when it could improve our current path. Requests through
            // worse alternates can suppress real recovery requests as duplicates.
            && self.selected.get(&key).is_none_or(|selected| metric < selected.metric)
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
        if is_new {
            self.neighbours
                .get_mut(neighbour_key)
                .expect("known neighbor")
                .candidates += 1;
        }
        let changed = self.candidates.get(&candidate_key).is_none_or(|old| {
            old.router_id != router_id
                || old.seqno != update.seqno
                || old.metric != metric
                || old.advertised_metric != update.metric
                || old.next_hop != next_hop
        });
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
        (changed, actions)
    }

    pub(super) fn candidate_is_feasible(&self, candidate: &Candidate) -> bool {
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

    pub(super) fn recompute_candidate_metrics(&mut self, only: Option<&NeighborKey>) -> bool {
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

    pub(super) fn reselect(&mut self, now_ms: u64) -> Vec<Action> {
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
        let removed: HashSet<_> = before
            .keys()
            .filter(|key| !self.selected.contains_key(key))
            .copied()
            .collect();
        if !removed.is_empty() {
            // Batch withdrawals must not scan the complete candidate table for
            // every lost prefix. Collect each hold deadline in one pass.
            let mut expiries: HashMap<RouteKey, u64> = HashMap::new();
            for candidate in self.candidates.values() {
                if removed.contains(&candidate.key) {
                    expiries
                        .entry(candidate.key)
                        .and_modify(|expiry| *expiry = (*expiry).max(candidate.expires_ms))
                        .or_insert(candidate.expires_ms);
                }
            }
            let longest = self
                .interfaces
                .values()
                .map(|state| state.policy.update_interval_cs)
                .max()
                .unwrap_or(self.config.update_interval_cs);
            for key in removed {
                self.tombstones.insert(
                    key,
                    expiries
                        .get(&key)
                        .copied()
                        .unwrap_or_else(|| now_ms.saturating_add(u64::from(longest) * 35)),
                );
            }
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

    pub(super) fn selected_delta(
        &mut self,
        before: &BTreeMap<RouteKey, SelectedRoute>,
        now_ms: u64,
    ) -> Vec<Action> {
        let mut actions = Vec::new();
        let mut request_targets = HashMap::new();
        if before.keys().any(|key| !self.selected.contains_key(key)) {
            for ((key, neighbor), candidate) in &self.candidates {
                if before.contains_key(key)
                    && !self.selected.contains_key(key)
                    && candidate.metric < INFINITY
                    && !self.candidate_is_feasible(candidate)
                {
                    request_targets
                        .entry(*key)
                        .or_insert_with(|| neighbor.clone());
                }
            }
        }
        for (key, previous) in before {
            if !self.selected.contains_key(key) {
                actions.extend(self.advertise_learned(previous, INFINITY, None, now_ms));
                let pending_key = (*key, previous.router_id);
                let requested_seqno = self
                    .feasible
                    .get(&pending_key)
                    .map_or(previous.seqno.wrapping_add(1), |source| {
                        source.distance.seqno.wrapping_add(1)
                    });
                let next_hop = request_targets.remove(key);
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
                now_ms,
            ));
        }
        actions
    }
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

pub(super) fn selected_from_candidate(route: &Candidate) -> SelectedRoute {
    SelectedRoute {
        key: route.key,
        router_id: route.router_id,
        seqno: route.seqno,
        metric: route.metric,
        next_hop: route.next_hop,
        interface: route.interface.clone(),
    }
}
