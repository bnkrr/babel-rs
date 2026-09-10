use std::sync::Arc;

use async_trait::async_trait;
use babel_protocol::{RouteKey, SelectedRoute};
use tokio::sync::RwLock;

/// Complete desired learned RIB and unreachable hold state for one generation.
/// Local origins are advertised separately and are not installed by this snapshot.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RouteSnapshot {
    /// Generation identifies a replacement, not an export-completion acknowledgement.
    pub generation: u64,
    /// Selected learned routes.
    pub routes: Vec<SelectedRoute>,
    /// Exact destinations that must not fall through to a less-specific route
    /// while a withdrawn Babel route is retained as an unreachable tombstone.
    pub unreachable: Vec<RouteKey>,
}

/// Desired-state sink with coalesced snapshots and retries after failure.
///
/// Reconciliation must be idempotent, yield during I/O and converge to the newest
/// complete snapshot. The runtime may skip intermediate generations. It stops
/// submitting snapshots and waits for an in-flight reconciliation before
/// [`Self::shutdown`]; these callbacks never overlap within one router.
/// Make external operations cancellation-safe: Drop or a shutdown deadline can
/// drop either future. Any independently spawned work remains the implementer's
/// responsibility and must not reinstall state after final cleanup.
/// The runtime logs exporter failures; successful router commands do not imply
/// successful export. The router enforces its configured overall cleanup deadline.
#[async_trait]
pub trait RouteExporter: Send + Sync + 'static {
    /// Opt in only if the forwarding backend can install IPv4 routes via IPv6
    /// and originate ICMPv4 on unnumbered links (RFC 9229 §§2.2–3).
    /// The runtime gates route selection before export; default is unsupported.
    fn supports_ipv4_via_ipv6(&self) -> bool {
        false
    }

    /// Reconcile the complete desired state; errors are logged and retried.
    async fn reconcile(
        &self,
        snapshot: RouteSnapshot,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;

    /// Release externally owned state on shutdown. An empty running RIB may
    /// still need policy rules; this hook distinguishes final cleanup from an
    /// ordinary empty snapshot. The default performs one final reconciliation.
    async fn shutdown(
        &self,
        snapshot: RouteSnapshot,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.reconcile(snapshot).await
    }
}

/// Best-effort checkpoint of the local sequence number on orderly shutdown.
///
/// The router calls this once after withdrawing its origins, never for runtime
/// sequence changes. Errors are logged and a pending future is dropped after
/// one second so route cleanup can continue. Errors/timeouts are returned after
/// exporter cleanup as `RouterError::SequenceStore`. Implementations must yield while
/// waiting for I/O; dropping this future does not stop detached tasks or an
/// already running blocking operation. Applications own their runtime shutdown
/// policy for any such work.
#[async_trait]
pub trait SequenceStore: Send + Sync + 'static {
    async fn persist(
        &self,
        sequence_number: u16,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;
}

/// Default checkpoint sink: retains no sequence state across restarts.
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

/// In-memory snapshot sink for applications that do not need a kernel exporter.
#[derive(Clone, Default)]
pub struct MemoryExporter {
    snapshot: Arc<RwLock<RouteSnapshot>>,
}

impl MemoryExporter {
    /// Clone the most recently reconciled snapshot.
    pub async fn snapshot(&self) -> RouteSnapshot {
        self.snapshot.read().await.clone()
    }
}

#[async_trait]
impl RouteExporter for MemoryExporter {
    // This sink represents an abstract RIB, not a packet-forwarding backend.
    fn supports_ipv4_via_ipv6(&self) -> bool {
        true
    }

    async fn reconcile(
        &self,
        snapshot: RouteSnapshot,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        *self.snapshot.write().await = snapshot;
        Ok(())
    }
}
