//! Source feasibility history and sequence-number requests.

use super::*;

impl Engine {
    pub(super) fn handle_seqno_request(
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
                now_ms,
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
            actions.extend(self.send_updates(
                now_ms,
                Some(key),
                None,
                Some(SendTiming::urgent(now_ms)),
            ));
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

    pub(super) fn originate_seqno_request(
        &mut self,
        request: SeqnoRequestSpec,
        now_ms: u64,
    ) -> Vec<Action> {
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
            key, router_id, seqno, hop_count, next_hop, now_ms,
        )]
    }

    pub(super) fn maintain_source(
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

    pub(super) fn bump_sequence_number(&mut self) {
        self.sequence_number = self.sequence_number.wrapping_add(1);
        for origin in self.originated.values_mut() {
            origin.seqno = self.sequence_number;
        }
    }
}

pub(super) fn seqno_request_action(
    key: RouteKey,
    router_id: RouterId,
    seqno: u16,
    hop_count: u8,
    next_hop: NeighborKey,
    now_ms: u64,
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
        timing: SendTiming::urgent(now_ms),
    }
}
