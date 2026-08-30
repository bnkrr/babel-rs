use std::sync::Arc;

use crate::model::INFINITY;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LinkSample {
    pub hello_received: u16,
    pub hello_expected: u16,
    pub remote_rxcost: u16,
}

pub trait LinkMetric: Send + Sync + 'static {
    fn cost(&self, sample: LinkSample) -> u16;
}

#[derive(Clone, Copy, Debug)]
pub struct FixedMetric {
    cost: u16,
}

impl FixedMetric {
    pub fn new(cost: u16) -> Option<Self> {
        (cost < INFINITY).then_some(Self { cost })
    }
}

impl Default for FixedMetric {
    fn default() -> Self {
        Self { cost: 96 }
    }
}

impl LinkMetric for FixedMetric {
    fn cost(&self, sample: LinkSample) -> u16 {
        if sample.remote_rxcost == INFINITY {
            INFINITY
        } else {
            self.cost.max(sample.remote_rxcost)
        }
    }
}

impl<T: LinkMetric + ?Sized> LinkMetric for Arc<T> {
    fn cost(&self, sample: LinkSample) -> u16 {
        (**self).cost(sample)
    }
}
