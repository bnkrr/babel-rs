//! Semantic route advertisements and retractions.

use super::*;

impl Engine {
    fn advertisement(
        &self,
        interface: &str,
        key: RouteKey,
        router_id: RouterId,
        seqno: u16,
        metric: u16,
    ) -> OutboundUpdate {
        let state = self.interfaces.get(interface);
        let policy = state.map_or(Ipv4NextHop::Auto, |s| s.policy.ipv4_next_hop);
        let ipv4 = state.and_then(|s| policy.ipv4_address(&s.local_addresses));
        let transport = state.map_or(ControlTransport::default(), |s| s.policy.control_transport);
        let ipv6 = state.and_then(|s| {
            s.local_addresses.iter().find_map(|address| match address {
                IpAddr::V6(address) if address.is_unicast_link_local() => Some(*address),
                _ => None,
            })
        });
        let is_v4 = key.destination.addr().is_ipv4();
        let needs_v6 = !is_v4 || ipv4.is_none();
        let unavailable = (is_v4 && ipv4.is_none() && !self.config.ipv4_via_ipv6)
            || (is_v4 && policy == Ipv4NextHop::Ipv4 && ipv4.is_none())
            || (transport == ControlTransport::Ipv4 && needs_v6 && ipv6.is_none());
        let metric = if unavailable { INFINITY } else { metric };
        OutboundUpdate {
            key: Some(key),
            router_id: Some(router_id),
            seqno,
            metric,
            interval_cs: self.interface_update_interval(interface),
            next_hop: if metric == INFINITY {
                None
            } else if is_v4 && ipv4.is_some() {
                ipv4.map(IpAddr::V4)
            } else if transport == ControlTransport::Ipv4 {
                ipv6.map(IpAddr::V6)
            } else {
                None
            },
            // IPv4 retractions retain AE 1, including on an IPv6 control link.
            v4_via_v6: is_v4 && metric != INFINITY && ipv4.is_none(),
            sub_tlvs: vec![],
        }
    }

    pub(super) fn update_action_to_candidate(
        &mut self,
        key: RouteKey,
        router_id: RouterId,
        seqno: u16,
        metric: u16,
        destination: NeighborKey,
        now_ms: u64,
    ) -> Action {
        let update = self.advertisement(&destination.interface, key, router_id, seqno, metric);
        if update.metric != INFINITY {
            self.maintain_source(
                key,
                router_id,
                Distance {
                    seqno,
                    metric: update.metric,
                },
                now_ms,
            );
        }
        Action::Send {
            interface: destination.interface,
            destination: destination.address,
            packet: OutboundPacket {
                tlvs: vec![OutboundTlv::Update(update)],
            },
            timing: SendTiming::urgent(now_ms),
        }
    }

    pub(super) fn reply_to_route_request(
        &mut self,
        key: RouteKey,
        destination: NeighborKey,
        now_ms: u64,
    ) -> Action {
        // A specific request requires a reply even on a split-horizon link.
        // Send it to the requester instead of interpreting suppression as loss.
        let selected = self
            .originated
            .get(&key)
            .map(|origin| (self.config.router_id, origin.seqno, origin.metric))
            .or_else(|| {
                self.selected
                    .get(&key)
                    .map(|route| (route.router_id, route.seqno, route.metric))
            });
        match selected {
            Some((router_id, seqno, metric)) => {
                self.update_action_to_candidate(key, router_id, seqno, metric, destination, now_ms)
            }
            None => self.retraction_action(key, self.sequence_number, destination, now_ms),
        }
    }

    pub(super) fn retraction_action(
        &self,
        key: RouteKey,
        seqno: u16,
        destination: NeighborKey,
        now_ms: u64,
    ) -> Action {
        let update_interval_cs = self.interface_update_interval(&destination.interface);
        Action::Send {
            interface: destination.interface,
            destination: destination.address,
            packet: OutboundPacket {
                tlvs: vec![OutboundTlv::Update(OutboundUpdate {
                    key: Some(key),
                    router_id: None,
                    next_hop: None,
                    interval_cs: update_interval_cs,
                    seqno,
                    metric: INFINITY,
                    v4_via_v6: false,
                    sub_tlvs: vec![],
                })],
            },
            timing: SendTiming::urgent(now_ms),
        }
    }

    pub(super) fn send_updates(
        &mut self,
        now_ms: u64,
        only: Option<RouteKey>,
        interface: Option<String>,
        timing: Option<SendTiming>,
    ) -> Vec<Action> {
        let interfaces: Vec<_> =
            interface.map_or_else(|| self.interfaces.keys().cloned().collect(), |v| vec![v]);
        let origins: Vec<_> = match only {
            Some(key) => self
                .originated
                .get(&key)
                .map(|origin| (key, origin.clone()))
                .into_iter()
                .collect(),
            None => self
                .originated
                .iter()
                .map(|(key, origin)| (*key, origin.clone()))
                .collect(),
        };
        let selected: Vec<_> = match only {
            Some(key) => self.selected.get(&key).cloned().into_iter().collect(),
            None => self.selected.values().cloned().collect(),
        };
        let tombstones: Vec<_> = match only {
            Some(key) => self
                .tombstones
                .contains_key(&key)
                .then_some(key)
                .into_iter()
                .collect(),
            None => self.tombstones.keys().copied().collect(),
        };
        interfaces
            .into_iter()
            .filter_map(|interface| {
                let policy = &self.interfaces.get(&interface)?.policy;
                let split_horizon = policy.split_horizon;
                let hello_interval_cs = policy.hello_interval_cs;
                let send_timing =
                    timing.unwrap_or_else(|| SendTiming::triggered(now_ms, hello_interval_cs));
                let mut tlvs = Vec::new();
                for (key, origin) in &origins {
                    if only.is_none_or(|wanted| wanted == *key) {
                        let update = self.advertisement(
                            &interface,
                            *key,
                            self.config.router_id,
                            origin.seqno,
                            origin.metric,
                        );
                        if update.metric != INFINITY {
                            self.maintain_source(
                                *key,
                                self.config.router_id,
                                Distance {
                                    seqno: origin.seqno,
                                    metric: origin.metric,
                                },
                                now_ms,
                            );
                        }
                        tlvs.push(OutboundTlv::Update(update));
                    }
                }
                for route in &selected {
                    // Split horizon: never advertise a selected route back on
                    // the interface from which its next hop was learned.
                    if (!split_horizon || route.interface != interface)
                        && only.is_none_or(|wanted| wanted == route.key)
                    {
                        let update = self.advertisement(
                            &interface,
                            route.key,
                            route.router_id,
                            route.seqno,
                            route.metric,
                        );
                        if update.metric != INFINITY {
                            self.maintain_source(
                                route.key,
                                route.router_id,
                                Distance {
                                    seqno: route.seqno,
                                    metric: route.metric,
                                },
                                now_ms,
                            );
                        }
                        tlvs.push(OutboundTlv::Update(update));
                    }
                }
                for key in &tombstones {
                    if !self.originated.contains_key(key)
                        && !self.selected.contains_key(key)
                        && only.is_none_or(|wanted| wanted == *key)
                    {
                        tlvs.push(OutboundTlv::Update(self.advertisement(
                            &interface,
                            *key,
                            self.config.router_id,
                            self.sequence_number,
                            INFINITY,
                        )));
                    }
                }
                if only.is_none()
                    && !tlvs.is_empty()
                    && let Some(state) = self.interfaces.get_mut(&interface)
                {
                    state.last_full_update_ms = Some(now_ms);
                }
                (!tlvs.is_empty()).then_some(Action::Send {
                    destination: self
                        .interfaces
                        .get(interface.as_str())
                        .expect("interface exists")
                        .policy
                        .control_transport
                        .multicast(),

                    interface,
                    packet: OutboundPacket { tlvs },
                    timing: send_timing,
                })
            })
            .collect()
    }

    pub(super) fn full_update_request_allowed(&self, interface: &str, now_ms: u64) -> bool {
        let suppression_ms = u64::from(self.interface_hello_interval(interface)) * 10;
        self.interfaces
            .get(interface)
            .and_then(|state| state.last_full_update_ms)
            .is_none_or(|last| now_ms.saturating_sub(last) >= suppression_ms)
    }

    pub(super) fn send_retraction(&self, key: RouteKey, seqno: u16, now_ms: u64) -> Vec<Action> {
        self.interfaces
            .iter()
            .map(|(interface, state)| Action::Send {
                interface: interface.clone(),
                destination: self
                    .interfaces
                    .get(interface.as_str())
                    .expect("interface exists")
                    .policy
                    .control_transport
                    .multicast(),
                packet: OutboundPacket {
                    tlvs: vec![OutboundTlv::Update(OutboundUpdate {
                        key: Some(key),
                        router_id: Some(self.config.router_id),
                        next_hop: None,
                        interval_cs: state.policy.update_interval_cs,
                        seqno,
                        metric: INFINITY,
                        v4_via_v6: false,
                        sub_tlvs: vec![],
                    })],
                },
                timing: SendTiming::urgent(now_ms),
            })
            .collect()
    }

    pub(super) fn advertise_learned(
        &self,
        route: &SelectedRoute,
        metric: u16,
        learned_interface: Option<&str>,
        now_ms: u64,
    ) -> Vec<Action> {
        let interfaces: Vec<_> = self
            .interfaces
            .iter()
            .filter(|(interface, state)| {
                !state.policy.split_horizon || learned_interface != Some(interface.as_str())
            })
            .map(|(interface, _)| interface.clone())
            .collect();
        interfaces
            .into_iter()
            .map(|interface| {
                let update =
                    self.advertisement(&interface, route.key, route.router_id, route.seqno, metric);
                Action::Send {
                    destination: self
                        .interfaces
                        .get(interface.as_str())
                        .expect("interface exists")
                        .policy
                        .control_transport
                        .multicast(),

                    interface,
                    packet: OutboundPacket {
                        tlvs: vec![OutboundTlv::Update(update)],
                    },
                    timing: SendTiming::urgent(now_ms),
                }
            })
            .collect()
    }
}
