#![forbid(unsafe_code)]

mod export;
mod output;
mod output_queue;
mod router;
mod transport;

pub use babel_proto::{
    AdditiveMetric, EtxMetric, InterfacePolicy, MetricAlgebra, MetricProfile, NeighborMetric,
    ResourceLimits, ResourceStatus, RouteKey, RouteSelectionConfig, RouterId, RttMetric,
    SelectedRoute, WiredMetric,
};
pub use export::{MemoryExporter, NoopSequenceStore, RouteExporter, RouteSnapshot, SequenceStore};
pub use output_queue::OutputStatus;
pub use router::{
    BabelRouter, BabelRouterBuilder, RouteStream, RouterError, RouterHandle, RouterInterfaceStatus,
    RouterStatus,
};
