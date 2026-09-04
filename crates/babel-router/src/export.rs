use std::sync::Arc;

use async_trait::async_trait;
use babel_proto::{RouteKey, SelectedRoute};
use tokio::sync::RwLock;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RouteSnapshot {
    pub generation: u64,
    pub routes: Vec<SelectedRoute>,
    /// Exact destinations that must not fall through to a less-specific route
    /// while a withdrawn Babel route is retained as an unreachable tombstone.
    pub unreachable: Vec<RouteKey>,
}

#[async_trait]
pub trait RouteExporter: Send + Sync + 'static {
    async fn reconcile(
        &self,
        snapshot: RouteSnapshot,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;

    // An empty running RIB may still require persistent policy state.  The
    // separate shutdown hook lets exporters remove that state when ownership
    // of the external data plane is ending.  Simple exporters can treat it as
    // one final reconciliation.
    async fn shutdown(
        &self,
        snapshot: RouteSnapshot,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.reconcile(snapshot).await
    }
}

#[async_trait]
pub trait SequenceStore: Send + Sync + 'static {
    async fn persist(
        &self,
        sequence_number: u16,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct NoopSequenceStore;

#[async_trait]
impl SequenceStore for NoopSequenceStore {
    async fn persist(
        &self,
        _sequence_number: u16,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        Ok(())
    }
}

#[derive(Clone, Default)]
pub struct MemoryExporter {
    snapshot: Arc<RwLock<RouteSnapshot>>,
}

impl MemoryExporter {
    pub async fn snapshot(&self) -> RouteSnapshot {
        self.snapshot.read().await.clone()
    }
}

#[async_trait]
impl RouteExporter for MemoryExporter {
    async fn reconcile(
        &self,
        snapshot: RouteSnapshot,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        *self.snapshot.write().await = snapshot;
        Ok(())
    }
}
