//! Semantic route advertisements and retractions.

use super::*;

impl Engine {
    pub(super) fn update_action_to_candidate(
        &self,
        key: RouteKey,
        router_id: RouterId,
        seqno: u16,
        metric: u16,
        destination: NeighborKey,
        now_ms: u64,
    ) -> Action {
        let update_interval_cs = self.interface_update_interval(&destination.interface);
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
                    interval_cs: update_interval_cs,
                    seqno,
                    metric,
                    v4_via_v6,
                    sub_tlvs: vec![],
                })],
            },
            timing: SendTiming::urgent(now_ms),
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
                    v4_via_v6: key.destination.addr().is_ipv4(),
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
        let origins: Vec<_> = self
            .originated
            .iter()
            .map(|(key, origin)| (*key, origin.clone()))
            .collect();
        let selected: Vec<_> = self.selected.values().cloned().collect();
        interfaces
            .into_iter()
            .filter_map(|interface| {
                let policy = &self.interfaces.get(&interface)?.policy;
                let update_interval_cs = policy.update_interval_cs;
                let split_horizon = policy.split_horizon;
                let hello_interval_cs = policy.hello_interval_cs;
                let send_timing =
                    timing.unwrap_or_else(|| SendTiming::triggered(now_ms, hello_interval_cs));
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
                            interval_cs: update_interval_cs,
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
                    if (!split_horizon || route.interface != interface)
                        && only.is_none_or(|wanted| wanted == route.key)
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
                            interval_cs: update_interval_cs,
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
                destination: BABEL_MULTICAST_V6,
                packet: OutboundPacket {
                    tlvs: vec![OutboundTlv::Update(OutboundUpdate {
                        key: Some(key),
                        router_id: Some(self.config.router_id),
                        next_hop: None,
                        interval_cs: state.policy.update_interval_cs,
                        seqno,
                        metric: INFINITY,
                        v4_via_v6: key.destination.addr().is_ipv4(),
                        sub_tlvs: vec![],
                    })],
                },
                timing: SendTiming::triggered(now_ms, state.policy.hello_interval_cs),
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
                let state = self.interfaces.get(&interface).expect("interface exists");
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
                            interval_cs: state.policy.update_interval_cs,
                            seqno: route.seqno,
                            metric,
                            v4_via_v6,
                            sub_tlvs: vec![],
                        })],
                    },
                    timing: SendTiming::triggered(now_ms, state.policy.hello_interval_cs),
                }
            })
            .collect()
    }
}
