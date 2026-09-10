//! Explicit policy replacement keeps retained state and output consistent.

use super::*;

impl Engine {
    pub(super) fn replace_route_policy(
        &mut self,
        policy: Arc<dyn RoutePolicy>,
        now_ms: u64,
    ) -> Vec<Action> {
        self.config.route_policy = policy;
        self.candidates.retain(|(_, neighbor), candidate| {
            candidate.advertised_metric != INFINITY
                && self.config.route_policy.accept(&ImportContext {
                    key: candidate.key,
                    interface: &neighbor.interface,
                    neighbor: neighbor.address,
                    next_hop: candidate.next_hop,
                    router_id: candidate.router_id,
                    seqno: candidate.seqno,
                    advertised_metric: candidate.advertised_metric,
                })
        });
        for neighbor in self.neighbours.values_mut() {
            neighbor.candidates = 0;
        }
        for (_, neighbor) in self.candidates.keys() {
            if let Some(state) = self.neighbours.get_mut(neighbor) {
                state.candidates += 1;
            }
        }
        // Retain recovery through allowed candidates, including requests whose
        // eventual replies will be checked against the new export rules.
        self.pending_seqno.retain(|(key, _), pending| {
            self.candidates
                .contains_key(&(*key, pending.next_hop.clone()))
        });
        let mut actions = vec![Action::InvalidatePendingSends];
        actions.extend(self.reselect(now_ms));
        let keys: Vec<_> = self
            .originated
            .keys()
            .chain(self.selected.keys())
            .chain(self.tombstones.keys())
            .copied()
            .collect();
        for key in keys {
            self.repeat_advertisement(key, now_ms);
        }
        actions.extend(self.send_updates(now_ms, None, None, Some(SendTiming::urgent(now_ms))));
        // We do not retain rejected candidates just in case policy is relaxed.
        // Normal periodic updates also recover if this request is lost/suppressed.
        actions.extend(
            self.interfaces
                .iter()
                .map(|(interface, state)| Action::Send {
                    interface: interface.clone(),
                    destination: state.policy.control_transport.multicast(),
                    packet: OutboundPacket {
                        tlvs: vec![OutboundTlv::RouteRequest {
                            key: None,
                            sub_tlvs: vec![],
                        }],
                    },
                    timing: SendTiming::urgent(now_ms),
                }),
        );
        actions
    }
}
