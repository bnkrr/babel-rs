#![forbid(unsafe_code)]

//! Embeddable Tokio runtime for the independent Babel protocol engine.
//!
//! [`BabelRouterBuilder::build`] validates all local configuration before opening
//! sockets or starting tasks. It starts routing immediately; [`BabelRouter::run`]
//! joins that task. A Tokio runtime with I/O and timers is required. Attaching
//! interfaces uses the host's socket privileges and interface configuration.
//!
//! ```
//! use babel_router::{BabelRouter, RouterError, RouterId};
//! # #[tokio::main]
//! # async fn main() -> Result<(), RouterError> {
//! let builder = BabelRouter::builder().router_id(RouterId::new([1; 8]).unwrap());
//! builder.validate()?; // No I/O. Zero interfaces is valid for dynamic attachment.
//! let router = builder.build().await?;
//! let handle = router.handle();
//! assert!(handle.status().await?.interfaces.is_empty());
//! handle.shutdown();
//! router.run().await?;
//! # Ok(())
//! # }
//! ```
//!
//! [`RouterHandle`] supports local origins, dynamic interface policies, status and
//! [`RouteStream`] subscriptions. Streams and [`RouteExporter`] receive complete
//! desired-state snapshots and can skip intermediate generations. They describe
//! selected learned routes and unreachable hold state, not local origins.
//!
//! Embedders own stable identity and restart policy: the default initial sequence
//! is zero and the default [`SequenceStore`] is a no-op. No runtime sequence change
//! waits for persistence. Orderly shutdown retracts origins, attempts one bounded
//! checkpoint, then calls [`RouteExporter::shutdown`]. Apply an overall host
//! deadline if cleanup must be bounded; [`BabelRouter::abort_handle`] can cancel
//! the engine but does not guarantee cleanup of external state or detached I/O.

mod export;
mod output;
mod output_queue;
mod router;
mod transport;

pub use babel_proto::{
    AdditiveMetric, ConfigError, EtxMetric, InterfacePolicy, MetricAlgebra, MetricProfile,
    NeighborMetric, ResourceLimits, ResourceStatus, RouteKey, RouteSelectionConfig, RouterId,
    RttMetric, SelectedRoute, WiredMetric,
};
pub use export::{MemoryExporter, NoopSequenceStore, RouteExporter, RouteSnapshot, SequenceStore};
pub use output_queue::OutputStatus;
pub use router::{
    BabelRouter, BabelRouterBuilder, RouteStream, RouterError, RouterHandle, RouterInterfaceStatus,
    RouterStatus,
};
