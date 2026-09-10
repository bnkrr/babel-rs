//! Interface lifecycle and policy changes.

use super::*;

impl Engine {
    pub(super) fn default_interface_policy(&self) -> InterfacePolicy {
        InterfacePolicy {
            ipv4_next_hop: Default::default(),
            metric: Arc::clone(&self.config.metric),
            hello_interval_cs: self.config.hello_interval_cs,
            update_interval_cs: self.config.update_interval_cs,
            split_horizon: true,
        }
    }

    pub(super) fn interface_up(
        &mut self,
        interface: String,
        local_addresses: Vec<IpAddr>,
        policy: InterfacePolicy,
        now_ms: u64,
    ) -> Vec<Action> {
        self.interfaces
            .entry(interface.clone())
            .or_insert(InterfaceState {
                local_addresses,
                policy,
                hello_seqno: 0,
                next_hello_ms: now_ms,
                next_update_ms: now_ms,
                last_full_update_ms: None,
            });
        let mut actions = self.tick(now_ms);
        actions.push(Action::Send {
            interface,
            destination: BABEL_MULTICAST_V6,
            packet: OutboundPacket {
                tlvs: vec![OutboundTlv::RouteRequest {
                    key: None,
                    sub_tlvs: vec![],
                }],
            },
            timing: SendTiming::urgent(now_ms),
        });
        actions
    }

    pub(super) fn interface_policy_changed(
        &mut self,
        interface: String,
        policy: InterfacePolicy,
        reset_metric: bool,
        now_ms: u64,
    ) -> Vec<Action> {
        let Some(state) = self.interfaces.get_mut(&interface) else {
            return Vec::new();
        };
        state.policy = policy.clone();
        state.next_hello_ms = now_ms;
        state.next_update_ms = now_ms;
        state.last_full_update_ms = None;

        let mut actions = Vec::new();
        if reset_metric {
            for (key, neighbour) in &mut self.neighbours {
                if key.interface != interface {
                    continue;
                }
                let mut metric = policy.metric.new_neighbor(&interface);
                metric.on_hello(neighbour.histories);
                if let Some(cost) = neighbour.last_ihu_cost {
                    metric.on_ihu(cost);
                }
                neighbour.metric = metric;
                neighbour.next_ihu_ms = now_ms;
                neighbour.next_rtt_probe_ms = policy
                    .metric
                    .rtt_probe_interval_ms()
                    .map(|value| initial_probe_deadline(now_ms, value, key));
                neighbour.origin_timestamp = None;
                neighbour.receive_timestamp = None;
            }
            if self.recompute_candidate_metrics(None) {
                actions.extend(self.reselect(now_ms));
            }
        }
        actions.extend(self.send_updates(
            now_ms,
            None,
            Some(interface),
            Some(SendTiming::urgent(now_ms)),
        ));
        actions.extend(self.tick(now_ms));
        actions
    }

    pub(super) fn interface_hello_interval(&self, interface: &str) -> u16 {
        self.interfaces
            .get(interface)
            .map_or(self.config.hello_interval_cs, |state| {
                state.policy.hello_interval_cs
            })
    }

    pub(super) fn interface_update_interval(&self, interface: &str) -> u16 {
        self.interfaces
            .get(interface)
            .map_or(self.config.update_interval_cs, |state| {
                state.policy.update_interval_cs
            })
    }
}
