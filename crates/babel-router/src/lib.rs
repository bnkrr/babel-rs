#![forbid(unsafe_code)]

mod export;
mod router;
mod transport;

pub use babel_proto::{FixedMetric, LinkMetric, RouteKey, RouterId, SelectedRoute};
pub use export::{MemoryExporter, NoopSequenceStore, RouteExporter, RouteSnapshot, SequenceStore};
pub use router::{
    BabelRouter, BabelRouterBuilder, RouteStream, RouterError, RouterHandle, RouterStatus,
};
