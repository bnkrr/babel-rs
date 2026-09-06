use std::net::Ipv6Addr;

use babel_proto::{OutboundPacket, OutboundTlv, SendTiming, WireError, encode_packets};

pub(crate) const DEFAULT_PACING_MS: u64 = 2;
const DEADLINE_MARGIN_MS: u64 = 5;

pub(crate) struct OutboundIntent {
    pub destination: Ipv6Addr,
    pub packet: OutboundPacket,
    pub timing: SendTiming,
}

pub(crate) struct ScheduledDatagram {
    pub destination: Ipv6Addr,
    pub bytes: Vec<u8>,
    pub deadline_ms: u64,
}

struct PendingBatch {
    destination: Ipv6Addr,
    tlvs: Vec<OutboundTlv>,
    release_ms: u64,
    deadline_ms: u64,
}

struct ReadyDatagram {
    destination: Ipv6Addr,
    bytes: Vec<u8>,
    deadline_ms: u64,
}

/// Per-interface Babel output scheduler.
///
/// Semantic TLVs remain unencoded while their random delay is running, so
/// compatible work can be aggregated.  Packet boundaries are chosen only at
/// release time using the interface's current UDP payload budget.
pub(crate) struct OutputScheduler {
    pending: Vec<PendingBatch>,
    ready: Vec<ReadyDatagram>,
    next_send_ms: u64,
    pacing_ms: u64,
    random: JitterRandom,
}

impl OutputScheduler {
    pub(crate) fn new(seed: u64) -> Self {
        Self::with_pacing(seed, DEFAULT_PACING_MS)
    }

    fn with_pacing(seed: u64, pacing_ms: u64) -> Self {
        Self {
            pending: Vec::new(),
            ready: Vec::new(),
            next_send_ms: 0,
            pacing_ms,
            random: JitterRandom::new(seed),
        }
    }

    pub(crate) fn enqueue(&mut self, intent: OutboundIntent, now_ms: u64) {
        let remaining = intent.timing.deadline_ms.saturating_sub(now_ms);
        let latest_delay = remaining
            .saturating_sub(DEADLINE_MARGIN_MS.min(remaining))
            .min(intent.timing.max_jitter_ms);
        let release_ms = now_ms.saturating_add(self.random.bounded(latest_delay));
        if let Some(batch) = self
            .pending
            .iter_mut()
            .find(|batch| batch.destination == intent.destination)
        {
            batch.release_ms = batch.release_ms.min(release_ms);
            batch.deadline_ms = batch.deadline_ms.min(intent.timing.deadline_ms);
            batch.tlvs.extend(intent.packet.tlvs);
            return;
        }
        self.pending.push(PendingBatch {
            destination: intent.destination,
            tlvs: intent.packet.tlvs,
            release_ms,
            deadline_ms: intent.timing.deadline_ms,
        });
    }

    pub(crate) fn next_wake_ms(&self) -> Option<u64> {
        let pending = self
            .pending
            .iter()
            .map(|batch| batch.release_ms.min(batch.deadline_ms))
            .min();
        let ready = self
            .ready
            .iter()
            .map(|datagram| self.next_send_ms.min(datagram.deadline_ms))
            .min();
        pending.into_iter().chain(ready).min()
    }

    pub(crate) fn pop_due(
        &mut self,
        now_ms: u64,
        payload_budget: usize,
    ) -> Result<Option<ScheduledDatagram>, WireError> {
        self.packetize_due(now_ms, payload_budget)?;
        let Some((index, send_at)) = self
            .ready
            .iter()
            .enumerate()
            .map(|(index, datagram)| {
                (
                    index,
                    self.next_send_ms.min(datagram.deadline_ms),
                    datagram.deadline_ms,
                )
            })
            .min_by_key(|(_, send_at, deadline)| (*send_at, *deadline))
            .map(|(index, send_at, _)| (index, send_at))
        else {
            return Ok(None);
        };
        if now_ms < send_at {
            return Ok(None);
        }
        let datagram = self.ready.remove(index);
        self.next_send_ms = now_ms.saturating_add(self.pacing_ms);
        Ok(Some(ScheduledDatagram {
            destination: datagram.destination,
            bytes: datagram.bytes,
            deadline_ms: datagram.deadline_ms,
        }))
    }

    fn packetize_due(&mut self, now_ms: u64, payload_budget: usize) -> Result<(), WireError> {
        let mut index = 0;
        while index < self.pending.len() {
            let due = now_ms >= self.pending[index].release_ms
                || now_ms >= self.pending[index].deadline_ms;
            if !due {
                index += 1;
                continue;
            }
            let batch = self.pending.remove(index);
            let packets = encode_packets(&OutboundPacket { tlvs: batch.tlvs }, payload_budget)?;
            self.ready
                .extend(packets.into_iter().map(|bytes| ReadyDatagram {
                    destination: batch.destination,
                    bytes,
                    deadline_ms: batch.deadline_ms,
                }));
        }
        Ok(())
    }
}

struct JitterRandom(u64);

impl JitterRandom {
    fn new(seed: u64) -> Self {
        Self(if seed == 0 {
            0x9e37_79b9_7f4a_7c15
        } else {
            seed
        })
    }

    fn bounded(&mut self, inclusive_max: u64) -> u64 {
        if inclusive_max == 0 {
            return 0;
        }
        // xorshift64*: deterministic, small, and sufficient for protocol
        // desynchronisation.  This randomness is not a security boundary.
        let mut value = self.0;
        value ^= value >> 12;
        value ^= value << 25;
        value ^= value >> 27;
        self.0 = value;
        value.wrapping_mul(0x2545_f491_4f6c_dd1d) % inclusive_max.saturating_add(1)
    }
}

#[cfg(test)]
mod tests {
    use std::net::Ipv6Addr;

    use babel_proto::{OutboundPacket, OutboundTlv, SendTiming};

    use super::*;

    fn intent(destination: Ipv6Addr, nonce: u16, timing: SendTiming) -> OutboundIntent {
        OutboundIntent {
            destination,
            packet: OutboundPacket {
                tlvs: vec![OutboundTlv::Ack { nonce }],
            },
            timing,
        }
    }

    #[test]
    fn deterministic_jitter_stays_inside_window() {
        let mut scheduler = OutputScheduler::new(7);
        scheduler.enqueue(
            intent(
                Ipv6Addr::LOCALHOST,
                1,
                SendTiming {
                    deadline_ms: 1100,
                    max_jitter_ms: 50,
                },
            ),
            1000,
        );
        let wake = scheduler.next_wake_ms().unwrap();
        assert!((1000..=1050).contains(&wake));
        assert!(scheduler.pop_due(wake - 1, 1280).unwrap().is_none());
        assert!(scheduler.pop_due(wake, 1280).unwrap().is_some());
    }

    #[test]
    fn jitter_keeps_a_transport_scheduling_margin() {
        for seed in 1..100 {
            let mut scheduler = OutputScheduler::new(seed);
            scheduler.enqueue(
                intent(
                    Ipv6Addr::LOCALHOST,
                    1,
                    SendTiming {
                        deadline_ms: 1100,
                        max_jitter_ms: 100,
                    },
                ),
                1000,
            );
            assert!(scheduler.next_wake_ms().unwrap() <= 1095);
        }
    }

    #[test]
    fn aggregates_only_matching_destinations() {
        let mut scheduler = OutputScheduler::new(1);
        let now = 10;
        let timing = SendTiming::immediate(now);
        scheduler.enqueue(intent(Ipv6Addr::LOCALHOST, 1, timing), now);
        scheduler.enqueue(intent(Ipv6Addr::LOCALHOST, 2, timing), now);
        scheduler.enqueue(intent(Ipv6Addr::UNSPECIFIED, 3, timing), now);

        let first = scheduler.pop_due(now, 1280).unwrap().unwrap();
        let second = scheduler.pop_due(now, 1280).unwrap().unwrap();
        assert_ne!(first.destination, second.destination);
        assert!(scheduler.pop_due(now, 1280).unwrap().is_none());
        assert_eq!(first.bytes.len().max(second.bytes.len()), 12);
    }

    #[test]
    fn packetizes_to_budget_and_paces_without_missing_deadline() {
        let mut scheduler = OutputScheduler::with_pacing(1, 5);
        let now = 100;
        let packet = OutboundPacket {
            tlvs: (0..20).map(|nonce| OutboundTlv::Ack { nonce }).collect(),
        };
        scheduler.enqueue(
            OutboundIntent {
                destination: Ipv6Addr::LOCALHOST,
                packet,
                timing: SendTiming {
                    deadline_ms: 103,
                    max_jitter_ms: 0,
                },
            },
            now,
        );
        let mut sent = Vec::new();
        for tick in now..=110 {
            while let Some(datagram) = scheduler.pop_due(tick, 24).unwrap() {
                assert!(datagram.bytes.len() <= 24);
                sent.push(tick);
            }
        }
        assert!(sent.len() > 1);
        assert_eq!(sent[0], 100);
        assert_eq!(sent[1], 103, "deadline overrides the 5ms pacing gap");
    }
}
