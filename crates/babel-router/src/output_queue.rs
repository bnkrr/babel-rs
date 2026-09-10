use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use babel_protocol::{OutboundPacket, OutboundTlv, SubTlv};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc};
use tokio::time::Instant;
use tracing::warn;

use crate::output::OutboundIntent;

// Internal transport budgets, independent of learned-route admission. The
// byte charge includes semantic storage and room for packetization overhead.
pub(crate) const OUTPUT_BUDGET_BYTES: usize = 16 * 1024 * 1024;
pub(crate) const OUTPUT_QUEUE_CAPACITY: usize = 256;
pub(crate) const OUTPUT_EXPIRY_GRACE_MS: u64 = 1_000;
pub(crate) const SEND_TIMEOUT_MS: u64 = 100;

/// Output usage and losses since this interface was attached. Charged bytes
/// cover the channel, scheduler and in-flight send; they are not measured RSS.
#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct OutputStatus {
    pub budget_bytes: usize,
    pub used_bytes: usize,
    pub rejected_batches: u64,
    pub rejected_tlvs: u64,
    pub expired_batches: u64,
    pub dropped_datagrams: u64,
    pub expired_datagrams: u64,
    pub send_timeouts: u64,
    pub missed_deadlines: u64,
}

#[derive(Default)]
pub(crate) struct OutputCounters {
    pub dropped: AtomicU64,
    pub missed_deadlines: AtomicU64,
}

#[derive(Default)]
struct QueueCounters {
    rejected_batches: AtomicU64,
    rejected_tlvs: AtomicU64,
    expired_batches: AtomicU64,
    dropped_datagrams: AtomicU64,
    expired_datagrams: AtomicU64,
    send_timeouts: AtomicU64,
    missed_deadlines: AtomicU64,
    next_log_ms: AtomicU64,
    logged_events: AtomicU64,
}

pub(crate) struct QueuedIntent {
    pub intent: OutboundIntent,
    pub expires_ms: u64,
    pub reservation: OwnedSemaphorePermit,
}

#[derive(Clone)]
pub(crate) struct OutputQueue {
    send: mpsc::Sender<QueuedIntent>,
    budget: Arc<Semaphore>,
    budget_bytes: usize,
    counters: Arc<QueueCounters>,
    totals: Arc<OutputCounters>,
    started: Arc<Instant>,
}

impl OutputQueue {
    pub(crate) fn new(
        capacity: usize,
        budget_bytes: usize,
        totals: Arc<OutputCounters>,
        started: Arc<Instant>,
    ) -> (Self, mpsc::Receiver<QueuedIntent>) {
        let (send, receive) = mpsc::channel(capacity);
        (
            Self {
                send,
                budget: Arc::new(Semaphore::new(budget_bytes)),
                budget_bytes,
                counters: Arc::new(QueueCounters::default()),
                totals,
                started,
            },
            receive,
        )
    }

    /// Admission never waits, including when an oversized single batch arrives.
    pub(crate) fn submit(&self, intent: OutboundIntent) {
        let tlvs = intent.packet.tlvs.len() as u64;
        let reservation = u32::try_from(packet_charge(&intent.packet))
            .ok()
            .and_then(|charge| Arc::clone(&self.budget).try_acquire_many_owned(charge).ok());
        if let Some(reservation) = reservation {
            // A late engine tick can generate a fresh periodic message with
            // an already-passed scheduling deadline. Give that new work the
            // same bounded transport grace, rather than dropping it on arrival.
            let now_ms = self.started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
            let expires_ms = intent
                .timing
                .deadline_ms
                .max(now_ms)
                .saturating_add(OUTPUT_EXPIRY_GRACE_MS);
            let queued = QueuedIntent {
                intent,
                expires_ms,
                reservation,
            };
            if self.send.try_send(queued).is_ok() {
                return;
            }
        }
        self.counters
            .rejected_batches
            .fetch_add(1, Ordering::Relaxed);
        self.counters
            .rejected_tlvs
            .fetch_add(tlvs, Ordering::Relaxed);
    }

    pub(crate) fn expired_batches(&self, count: u64) {
        self.counters
            .expired_batches
            .fetch_add(count, Ordering::Relaxed);
    }

    pub(crate) fn drop_datagram(&self, expired: bool, timed_out: bool) {
        self.counters
            .dropped_datagrams
            .fetch_add(1, Ordering::Relaxed);
        self.totals.dropped.fetch_add(1, Ordering::Relaxed);
        if expired {
            self.counters
                .expired_datagrams
                .fetch_add(1, Ordering::Relaxed);
        }
        if timed_out {
            self.counters.send_timeouts.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub(crate) fn missed_deadline(&self) {
        self.counters
            .missed_deadlines
            .fetch_add(1, Ordering::Relaxed);
        self.totals.missed_deadlines.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn status(&self) -> OutputStatus {
        let c = &self.counters;
        OutputStatus {
            budget_bytes: self.budget_bytes,
            used_bytes: self.budget_bytes - self.budget.available_permits(),
            rejected_batches: c.rejected_batches.load(Ordering::Relaxed),
            rejected_tlvs: c.rejected_tlvs.load(Ordering::Relaxed),
            expired_batches: c.expired_batches.load(Ordering::Relaxed),
            dropped_datagrams: c.dropped_datagrams.load(Ordering::Relaxed),
            expired_datagrams: c.expired_datagrams.load(Ordering::Relaxed),
            send_timeouts: c.send_timeouts.load(Ordering::Relaxed),
            missed_deadlines: c.missed_deadlines.load(Ordering::Relaxed),
        }
    }

    pub(crate) fn log_losses(&self, interface: &str) {
        let status = self.status();
        let events = status
            .rejected_batches
            .wrapping_add(status.expired_batches)
            .wrapping_add(status.dropped_datagrams)
            .wrapping_add(status.missed_deadlines);
        let now = self.started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
        if events != self.counters.logged_events.load(Ordering::Relaxed)
            && now >= self.counters.next_log_ms.load(Ordering::Relaxed)
        {
            self.counters.logged_events.store(events, Ordering::Relaxed);
            self.counters
                .next_log_ms
                .store(now.saturating_add(30_000), Ordering::Relaxed);
            warn!(
                interface,
                ?status,
                "Babel output dropped work or missed deadlines"
            );
        }
    }
}

// Conservative accounting, not an allocator/RSS measurement. Include vector
// capacities and nested allocations, plus per-TLV space for wire context,
// datagram metadata/headers and reservation references. At least 1024 bytes
// per batch bounds even empty intents. Arithmetic saturates before admission.
pub(crate) fn packet_charge(packet: &OutboundPacket) -> usize {
    let mut charge = 1024usize.saturating_add(
        packet
            .tlvs
            .capacity()
            .saturating_mul(size_of::<OutboundTlv>())
            .saturating_mul(2),
    );
    for tlv in &packet.tlvs {
        charge = charge.saturating_add(512);
        let sub_tlvs = match tlv {
            OutboundTlv::Hello { sub_tlvs, .. }
            | OutboundTlv::Ihu { sub_tlvs, .. }
            | OutboundTlv::RouteRequest { sub_tlvs, .. }
            | OutboundTlv::SeqnoRequest { sub_tlvs, .. } => Some(sub_tlvs),
            OutboundTlv::Update(update) => Some(&update.sub_tlvs),
            OutboundTlv::PadN(value) | OutboundTlv::Unknown { value, .. } => {
                charge = charge.saturating_add(value.capacity().saturating_mul(2));
                None
            }
            OutboundTlv::Pad1 | OutboundTlv::AckRequest { .. } | OutboundTlv::Ack { .. } => None,
        };
        if let Some(sub_tlvs) = sub_tlvs {
            charge = charge.saturating_add(
                sub_tlvs
                    .capacity()
                    .saturating_mul(size_of::<SubTlv>() + 32)
                    .saturating_mul(2),
            );
            for sub_tlv in sub_tlvs {
                if let SubTlv::PadN(value) | SubTlv::Unknown { value, .. } = sub_tlv {
                    charge = charge.saturating_add(value.capacity().saturating_mul(2));
                }
            }
        }
    }
    charge
}

#[cfg(test)]
mod tests {
    use super::*;
    use babel_protocol::SendTiming;

    #[test]
    fn full_closed_and_oversized_admission_release_reservations() {
        let packet = OutboundPacket {
            tlvs: vec![OutboundTlv::Ack { nonce: 1 }],
        };
        let charge = packet_charge(&packet);
        let make = || OutboundIntent {
            destination: "ff02::1:6".parse().unwrap(),
            packet: packet.clone(),
            timing: SendTiming::immediate(0),
        };
        let (queue, mut receive) = OutputQueue::new(
            1,
            charge * 2,
            Arc::new(OutputCounters::default()),
            Arc::new(Instant::now()),
        );
        queue.submit(make());
        queue.submit(make());
        assert_eq!(queue.status().rejected_batches, 1);
        assert_eq!(queue.status().used_bytes, charge);
        drop(receive.try_recv().unwrap());
        assert_eq!(queue.status().used_bytes, 0);
        // A single large nested allocation cannot bypass a batch-count limit.
        queue.submit(OutboundIntent {
            packet: OutboundPacket {
                tlvs: vec![OutboundTlv::Hello {
                    unicast: false,
                    seqno: 1,
                    interval_cs: 400,
                    sub_tlvs: vec![SubTlv::Unknown {
                        type_: 42,
                        value: vec![0; charge * 2],
                    }],
                }],
            },
            ..make()
        });
        assert_eq!(queue.status().rejected_batches, 2);
        assert_eq!(queue.status().used_bytes, 0);
        queue.submit(make());
        drop(receive);
        assert_eq!(queue.status().used_bytes, 0);
        queue.submit(make());
        assert_eq!(queue.status().rejected_batches, 3);
        assert_eq!(queue.status().rejected_tlvs, 3);
        assert_eq!(queue.status().used_bytes, 0);
    }
}
