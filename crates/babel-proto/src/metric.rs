use std::sync::Arc;

use crate::model::INFINITY;

/// The two independent Hello histories maintained by RFC 8966.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct HelloHistories {
    pub multicast: HelloHistory,
    pub unicast: HelloHistory,
}

/// A bounded, allocation-free record of the latest 16 Hello sequence numbers.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct HelloHistory {
    bits: u16,
    samples: u8,
    expected_seqno: Option<u16>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HelloHistoryUpdate {
    Recorded,
    Restarted,
}

impl HelloHistory {
    /// Applies RFC 8966 Appendix A.1 sequence arithmetic.
    pub fn record(&mut self, seqno: u16) -> HelloHistoryUpdate {
        let Some(expected) = self.expected_seqno else {
            self.bits = 1;
            self.samples = 1;
            self.expected_seqno = Some(seqno.wrapping_add(1));
            return HelloHistoryUpdate::Recorded;
        };
        let forward = seqno.wrapping_sub(expected);
        let backward = expected.wrapping_sub(seqno);
        if forward <= 16 {
            self.append_missed(forward as u8);
        } else if backward <= 16 {
            self.bits >>= backward;
            self.samples = self.samples.saturating_sub(backward as u8);
        } else {
            self.bits = 0;
            self.samples = 0;
            self.expected_seqno = None;
            self.record(seqno);
            return HelloHistoryUpdate::Restarted;
        }
        self.bits = (self.bits << 1) | 1;
        self.samples = self.samples.saturating_add(1).min(16);
        self.expected_seqno = Some(seqno.wrapping_add(1));
        HelloHistoryUpdate::Recorded
    }

    pub fn missed(&mut self) {
        self.missed_many(1);
    }

    pub fn missed_many(&mut self, count: u64) {
        if self.expected_seqno.is_some() {
            self.append_missed(count.min(16) as u8);
            self.expected_seqno = self
                .expected_seqno
                .map(|value| value.wrapping_add(count as u16));
        }
    }

    pub fn is_empty(self) -> bool {
        self.bits == 0
    }

    fn append_missed(&mut self, count: u8) {
        if count >= 16 {
            self.bits = 0;
            self.samples = 16;
        } else {
            self.bits <<= count;
            self.samples = self.samples.saturating_add(count).min(16);
        }
    }

    pub fn received(self, window: u8) -> u8 {
        debug_assert!((1..=16).contains(&window));
        let mask = if window == 16 {
            u16::MAX
        } else {
            (1u16 << window) - 1
        };
        (self.bits & mask).count_ones() as u8
    }

    pub fn observed(self) -> u8 {
        self.samples
    }

    pub fn bits(self) -> u16 {
        self.bits
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct MetricStatus {
    pub algorithm: String,
    pub receive_cost: u16,
    pub transmit_cost: u16,
    pub link_cost: u16,
    pub last_rtt_us: Option<u32>,
    pub smoothed_rtt_us: Option<u32>,
    pub rtt_penalty: u16,
}

/// Per-neighbour metric state. The engine supplies protocol observations; the
/// implementation owns the policy used to turn them into Babel costs.
pub trait NeighborMetric: Send + 'static {
    fn on_hello(&mut self, histories: HelloHistories);
    fn on_ihu(&mut self, receive_cost: u16);
    fn on_rtt_sample(&mut self, sample_us: u32, now_ms: u64);
    fn receive_cost(&self) -> u16;
    fn transmit_cost(&self) -> u16;
    fn link_cost(&self) -> u16;
    fn status(&self) -> MetricStatus;
}

/// Factory for independent per-neighbour metric state.
pub trait MetricProfile: Send + Sync + 'static {
    fn name(&self) -> String;
    fn new_neighbor(&self, interface: &str) -> Box<dyn NeighborMetric>;
    fn timestamps_enabled(&self) -> bool {
        false
    }
    fn rtt_probe_interval_ms(&self) -> Option<u64> {
        None
    }
}

/// Metric algebra is separate from link-quality estimation so embedders can
/// replace either policy independently.
pub trait MetricAlgebra: Send + Sync + 'static {
    fn extend(&self, advertised_metric: u16, link_cost: u16) -> u16;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct AdditiveMetric;

impl MetricAlgebra for AdditiveMetric {
    fn extend(&self, advertised_metric: u16, link_cost: u16) -> u16 {
        if advertised_metric == INFINITY || link_cost == 0 || link_cost == INFINITY {
            return INFINITY;
        }
        let result = u32::from(advertised_metric) + u32::from(link_cost);
        if result >= u32::from(INFINITY) {
            INFINITY
        } else {
            result as u16
        }
    }
}

/// RFC 8966 Appendix A.2.1 k-out-of-j link sensing.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WiredMetric {
    nominal_cost: u16,
    received: u8,
    window: u8,
}

impl WiredMetric {
    pub const DEFAULT_NOMINAL_COST: u16 = 96;
    pub const DEFAULT_RECEIVED: u8 = 2;
    pub const DEFAULT_WINDOW: u8 = 3;

    pub fn new(nominal_cost: u16, received: u8, window: u8) -> Option<Self> {
        (nominal_cost > 0
            && nominal_cost < INFINITY
            && received > 0
            && received <= window
            && window <= 16)
            .then_some(Self {
                nominal_cost,
                received,
                window,
            })
    }
}

impl Default for WiredMetric {
    fn default() -> Self {
        Self::new(
            Self::DEFAULT_NOMINAL_COST,
            Self::DEFAULT_RECEIVED,
            Self::DEFAULT_WINDOW,
        )
        .expect("RFC defaults are valid")
    }
}

impl MetricProfile for WiredMetric {
    fn name(&self) -> String {
        "wired".into()
    }

    fn new_neighbor(&self, _interface: &str) -> Box<dyn NeighborMetric> {
        Box::new(WiredNeighbor {
            config: *self,
            histories: HelloHistories::default(),
            transmit_cost: INFINITY,
        })
    }
}

struct WiredNeighbor {
    config: WiredMetric,
    histories: HelloHistories,
    transmit_cost: u16,
}

impl WiredNeighbor {
    fn reachable(&self) -> bool {
        let enough =
            |history: HelloHistory| history.received(self.config.window) >= self.config.received;
        enough(self.histories.multicast) || enough(self.histories.unicast)
    }
}

impl NeighborMetric for WiredNeighbor {
    fn on_hello(&mut self, histories: HelloHistories) {
        self.histories = histories;
    }

    fn on_ihu(&mut self, receive_cost: u16) {
        self.transmit_cost = receive_cost;
    }

    fn on_rtt_sample(&mut self, _sample_us: u32, _now_ms: u64) {}

    fn receive_cost(&self) -> u16 {
        if self.reachable() {
            self.config.nominal_cost
        } else {
            INFINITY
        }
    }

    fn transmit_cost(&self) -> u16 {
        self.transmit_cost
    }

    fn link_cost(&self) -> u16 {
        if self.receive_cost() == INFINITY
            || self.transmit_cost == 0
            || self.transmit_cost == INFINITY
        {
            INFINITY
        } else {
            self.transmit_cost
        }
    }

    fn status(&self) -> MetricStatus {
        MetricStatus {
            algorithm: "wired".into(),
            receive_cost: self.receive_cost(),
            transmit_cost: self.transmit_cost(),
            link_cost: self.link_cost(),
            last_rtt_us: None,
            smoothed_rtt_us: None,
            rtt_penalty: 0,
        }
    }
}

/// RFC 8966 Appendix A.2.2 Expected Transmission Cost.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EtxMetric {
    window: u8,
}

impl EtxMetric {
    pub const NOMINAL_COST: u16 = 256;
    pub const DEFAULT_WINDOW: u8 = 6;

    pub fn new(window: u8) -> Option<Self> {
        (window > 0 && window <= 16).then_some(Self { window })
    }
}

impl Default for EtxMetric {
    fn default() -> Self {
        Self::new(Self::DEFAULT_WINDOW).expect("RFC default is valid")
    }
}

impl MetricProfile for EtxMetric {
    fn name(&self) -> String {
        "etx".into()
    }

    fn new_neighbor(&self, _interface: &str) -> Box<dyn NeighborMetric> {
        Box::new(EtxNeighbor {
            config: *self,
            histories: HelloHistories::default(),
            transmit_cost: INFINITY,
        })
    }
}

struct EtxNeighbor {
    config: EtxMetric,
    histories: HelloHistories,
    transmit_cost: u16,
}

impl EtxNeighbor {
    fn computed_receive_cost(&self) -> u16 {
        let received = u32::from(self.histories.multicast.received(self.config.window));
        if received == 0 {
            return INFINITY;
        }
        let numerator = u32::from(EtxMetric::NOMINAL_COST) * u32::from(self.config.window);
        numerator.div_ceil(received).min(u32::from(INFINITY)) as u16
    }
}

impl NeighborMetric for EtxNeighbor {
    fn on_hello(&mut self, histories: HelloHistories) {
        self.histories = histories;
    }

    fn on_ihu(&mut self, receive_cost: u16) {
        self.transmit_cost = receive_cost;
    }

    fn on_rtt_sample(&mut self, _sample_us: u32, _now_ms: u64) {}

    fn receive_cost(&self) -> u16 {
        self.computed_receive_cost()
    }

    fn transmit_cost(&self) -> u16 {
        self.transmit_cost
    }

    fn link_cost(&self) -> u16 {
        let receive = self.receive_cost();
        if receive == INFINITY || self.transmit_cost == 0 || self.transmit_cost == INFINITY {
            return INFINITY;
        }
        let transmit = self.transmit_cost.max(EtxMetric::NOMINAL_COST);
        let product = u32::from(transmit) * u32::from(receive);
        let cost = product.div_ceil(u32::from(EtxMetric::NOMINAL_COST));
        cost.min(u32::from(INFINITY)) as u16
    }

    fn status(&self) -> MetricStatus {
        MetricStatus {
            algorithm: "etx".into(),
            receive_cost: self.receive_cost(),
            transmit_cost: self.transmit_cost(),
            link_cost: self.link_cost(),
            last_rtt_us: None,
            smoothed_rtt_us: None,
            rtt_penalty: 0,
        }
    }
}

/// RFC 9616 delay metric, composed over another Babel metric profile.
#[derive(Clone)]
pub struct RttMetric {
    base: Arc<dyn MetricProfile>,
    probe_interval_ms: u64,
    half_life_ms: u64,
    min_rtt_us: u32,
    max_rtt_us: u32,
    max_penalty: u16,
}

impl RttMetric {
    pub const DEFAULT_PROBE_INTERVAL_MS: u64 = 2_000;
    pub const DEFAULT_HALF_LIFE_MS: u64 = 6_000;
    pub const DEFAULT_MIN_RTT_US: u32 = 10_000;
    pub const DEFAULT_MAX_RTT_US: u32 = 120_000;
    pub const DEFAULT_MAX_PENALTY: u16 = 150;

    pub fn new(
        base: Arc<dyn MetricProfile>,
        probe_interval_ms: u64,
        half_life_ms: u64,
        min_rtt_us: u32,
        max_rtt_us: u32,
        max_penalty: u16,
    ) -> Option<Self> {
        (probe_interval_ms > 0
            && half_life_ms > 0
            && min_rtt_us < max_rtt_us
            && max_penalty < INFINITY)
            .then_some(Self {
                base,
                probe_interval_ms,
                half_life_ms,
                min_rtt_us,
                max_rtt_us,
                max_penalty,
            })
    }

    pub fn recommended(base: Arc<dyn MetricProfile>) -> Self {
        Self::new(
            base,
            Self::DEFAULT_PROBE_INTERVAL_MS,
            Self::DEFAULT_HALF_LIFE_MS,
            Self::DEFAULT_MIN_RTT_US,
            Self::DEFAULT_MAX_RTT_US,
            Self::DEFAULT_MAX_PENALTY,
        )
        .expect("RFC defaults are valid")
    }
}

impl MetricProfile for RttMetric {
    fn name(&self) -> String {
        format!("rtt({})", self.base.name())
    }

    fn new_neighbor(&self, interface: &str) -> Box<dyn NeighborMetric> {
        Box::new(RttNeighbor {
            base: self.base.new_neighbor(interface),
            algorithm: self.name(),
            half_life_ms: self.half_life_ms,
            min_rtt_us: self.min_rtt_us,
            max_rtt_us: self.max_rtt_us,
            max_penalty: self.max_penalty,
            last_rtt_us: None,
            smoothed_rtt_us: None,
            last_rtt_sample_ms: None,
        })
    }

    fn timestamps_enabled(&self) -> bool {
        true
    }

    fn rtt_probe_interval_ms(&self) -> Option<u64> {
        Some(self.probe_interval_ms)
    }
}

struct RttNeighbor {
    base: Box<dyn NeighborMetric>,
    algorithm: String,
    half_life_ms: u64,
    min_rtt_us: u32,
    max_rtt_us: u32,
    max_penalty: u16,
    last_rtt_us: Option<u32>,
    smoothed_rtt_us: Option<f64>,
    last_rtt_sample_ms: Option<u64>,
}

impl RttNeighbor {
    fn penalty(&self) -> u16 {
        let Some(rtt) = self.smoothed_rtt_us else {
            return 0;
        };
        if rtt <= f64::from(self.min_rtt_us) {
            return 0;
        }
        if rtt >= f64::from(self.max_rtt_us) {
            return self.max_penalty;
        }
        let position =
            (rtt - f64::from(self.min_rtt_us)) / f64::from(self.max_rtt_us - self.min_rtt_us);
        (position * f64::from(self.max_penalty)).round() as u16
    }
}

impl NeighborMetric for RttNeighbor {
    fn on_hello(&mut self, histories: HelloHistories) {
        self.base.on_hello(histories);
    }

    fn on_ihu(&mut self, receive_cost: u16) {
        self.base.on_ihu(receive_cost);
    }

    fn on_rtt_sample(&mut self, sample_us: u32, now_ms: u64) {
        self.base.on_rtt_sample(sample_us, now_ms);
        self.last_rtt_us = Some(sample_us);
        self.smoothed_rtt_us = Some(self.smoothed_rtt_us.zip(self.last_rtt_sample_ms).map_or(
            f64::from(sample_us),
            |(old, previous_ms)| {
                let elapsed_ms = now_ms.saturating_sub(previous_ms);
                let alpha = 2.0_f64.powf(-(elapsed_ms as f64) / self.half_life_ms as f64);
                alpha * old + (1.0 - alpha) * f64::from(sample_us)
            },
        ));
        self.last_rtt_sample_ms = Some(now_ms);
    }

    fn receive_cost(&self) -> u16 {
        self.base.receive_cost()
    }

    fn transmit_cost(&self) -> u16 {
        self.base.transmit_cost()
    }

    fn link_cost(&self) -> u16 {
        let base = self.base.link_cost();
        if base == INFINITY {
            return INFINITY;
        }
        let cost = u32::from(base) + u32::from(self.penalty());
        cost.min(u32::from(INFINITY)) as u16
    }

    fn status(&self) -> MetricStatus {
        MetricStatus {
            algorithm: self.algorithm.clone(),
            receive_cost: self.receive_cost(),
            transmit_cost: self.transmit_cost(),
            link_cost: self.link_cost(),
            last_rtt_us: self.last_rtt_us,
            smoothed_rtt_us: self.smoothed_rtt_us.map(|value| value.round() as u32),
            rtt_penalty: self.penalty(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn histories(multicast: &[u16], unicast: &[u16]) -> HelloHistories {
        let mut result = HelloHistories::default();
        for value in multicast {
            result.multicast.record(*value);
        }
        for value in unicast {
            result.unicast.record(*value);
        }
        result
    }

    #[test]
    fn hello_history_fast_forwards_and_undoes_history() {
        let mut history = HelloHistory::default();
        assert_eq!(history.record(10), HelloHistoryUpdate::Recorded);
        assert_eq!(history.record(12), HelloHistoryUpdate::Recorded);
        assert_eq!(history.record(12), HelloHistoryUpdate::Recorded);
        assert_eq!(history.record(11), HelloHistoryUpdate::Recorded);
        assert_eq!(history.received(3), 2);
        assert_eq!(history.observed(), 2);
    }

    #[test]
    fn wired_uses_two_of_three_and_peer_receive_cost() {
        let profile = WiredMetric::default();
        let mut metric = profile.new_neighbor("eth0");
        metric.on_hello(histories(&[1], &[]));
        metric.on_ihu(96);
        assert_eq!(metric.receive_cost(), INFINITY);
        assert_eq!(metric.link_cost(), INFINITY);
        metric.on_hello(histories(&[1, 2], &[]));
        assert_eq!(metric.receive_cost(), 96);
        assert_eq!(metric.link_cost(), 96);
        metric.on_ihu(128);
        assert_eq!(metric.link_cost(), 128);
    }

    #[test]
    fn etx_penalises_multicast_loss_in_both_directions() {
        let profile = EtxMetric::new(6).unwrap();
        let mut metric = profile.new_neighbor("mesh0");
        metric.on_hello(histories(&[1, 3, 5], &[]));
        metric.on_ihu(512);
        assert_eq!(metric.receive_cost(), 512);
        assert_eq!(metric.link_cost(), 1024);
    }

    #[test]
    fn rtt_uses_rfc_bounded_penalty() {
        let base: Arc<dyn MetricProfile> = Arc::new(WiredMetric::default());
        let profile = RttMetric::recommended(base);
        let mut metric = profile.new_neighbor("eth0");
        metric.on_hello(histories(&[1, 2], &[]));
        metric.on_ihu(96);
        metric.on_rtt_sample(10_000, 0);
        assert_eq!(metric.link_cost(), 96);
        metric.on_rtt_sample(120_000, 6_000);
        assert_eq!(metric.status().smoothed_rtt_us, Some(65_000));
        assert!(metric.link_cost() > 96);
        for sample in 2..=65 {
            metric.on_rtt_sample(120_000, sample * 6_000);
        }
        assert_eq!(metric.link_cost(), 246);
    }

    #[test]
    fn additive_algebra_saturates_and_rejects_zero_cost() {
        let algebra = AdditiveMetric;
        assert_eq!(algebra.extend(10, 96), 106);
        assert_eq!(algebra.extend(10, 0), INFINITY);
        assert_eq!(algebra.extend(INFINITY - 1, 2), INFINITY);
    }
}
