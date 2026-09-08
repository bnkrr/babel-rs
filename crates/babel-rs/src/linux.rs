use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use babel_proto::{INFINITY, RouteKey, SelectedRoute};
use babel_router::{RouteExporter, RouteSnapshot};
use futures::TryStreamExt;
use ipnet::{IpNet, Ipv4Net, Ipv6Net};
use netlink_packet_route::AddressFamily;
use netlink_packet_route::route::{
    RouteAddress, RouteAttribute, RouteMessage, RouteProtocol, RouteScope, RouteType, RouteVia,
};
use netlink_packet_route::rule::{RuleAction, RuleAttribute, RuleMessage};
use rtnetlink::{Handle, IpVersion, RouteMessageBuilder, new_connection};
use thiserror::Error;
use tokio::sync::{Mutex, Notify, RwLock, watch};
use tokio::time::MissedTickBehavior;
use tracing::{debug, warn};

use crate::config::{Export, ExportView};

const DYNAMIC_PRIORITY_BASE: u32 = 65_535;

#[derive(Clone)]
pub struct LinuxExporter {
    handle: Handle,
    state: Arc<RwLock<ExportState>>,
    apply_lock: Arc<Mutex<()>>,
    reconcile_notify: Arc<Notify>,
}

#[derive(Clone)]
struct ExportState {
    export: Export,
    snapshot: RouteSnapshot,
    retain_rules: bool,
    stopping: bool,
    retired: Vec<Export>,
    config_generation: u64,
    last_success_revision: Option<ExportRevision>,
    last_success: Option<Instant>,
    last_error: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ExportRevision {
    route_generation: u64,
    config_generation: u64,
}

impl ExportState {
    fn revision(&self) -> ExportRevision {
        ExportRevision {
            route_generation: self.snapshot.generation,
            config_generation: self.config_generation,
        }
    }

    fn record_reconcile(&mut self, revision: ExportRevision, result: &Result<(), LinuxError>) {
        match result {
            Ok(()) => {
                self.last_success_revision = Some(revision);
                self.last_success = Some(Instant::now());
                self.last_error = None;
            }
            Err(error) => self.last_error = Some(error.to_string()),
        }
    }
}

#[derive(Clone, Debug)]
pub struct ExportHealth {
    pub config_generation: u64,
    pub last_success_route_generation: Option<u64>,
    pub last_success_config_generation: Option<u64>,
    pub last_success_age: Option<Duration>,
    pub last_error: Option<String>,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct RouteIdentity {
    table: u32,
    destination: IpNet,
    priority: u32,
    output_interface: u32,
    gateway: Option<IpAddr>,
    unreachable: bool,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct RuleIdentity {
    table: u32,
    priority: u32,
    source: IpNet,
}

#[derive(Clone, Debug)]
struct ProjectedRoute {
    table: u32,
    key: RouteKey,
    selected: Option<SelectedRoute>,
    source_specific: bool,
}

#[derive(Debug, Error)]
pub enum LinuxError {
    #[error("open route netlink: {0}")]
    Open(#[from] std::io::Error),
    #[error("netlink request: {0}")]
    Netlink(#[from] rtnetlink::Error),
    #[error("invalid route: {0}")]
    InvalidRoute(String),
    #[error("interface {0} has no ifindex")]
    Interface(String),
}

impl LinuxExporter {
    pub fn new(
        export: Export,
        ownership: Arc<crate::ownership::ProtocolOwnership>,
    ) -> Result<Self, LinuxError> {
        let (connection, handle, _) = new_connection()?;
        tokio::spawn(async move {
            // Keep ownership while queued netlink work can still be executed,
            // including the short interval before runtime teardown on timeout.
            let _ownership = ownership;
            connection.await;
        });
        Ok(Self {
            handle,
            state: Arc::new(RwLock::new(ExportState {
                export,
                snapshot: RouteSnapshot::default(),
                retain_rules: true,
                stopping: false,
                retired: Vec::new(),
                config_generation: 0,
                last_success_revision: None,
                last_success: None,
                last_error: None,
            })),
            apply_lock: Arc::new(Mutex::new(())),
            reconcile_notify: Arc::new(Notify::new()),
        })
    }

    pub async fn update_export(&self, export: Export) {
        {
            let mut state = self.state.write().await;
            if state.stopping {
                return;
            }
            let old_export = state.export.clone();
            if old_export.protocol != export.protocol && !state.retired.contains(&old_export) {
                state.retired.push(old_export);
            }
            if state.export != export {
                state.config_generation = state.config_generation.wrapping_add(1);
                state.export = export;
            }
        }
        self.reconcile_notify.notify_one();
    }

    pub async fn reconcile_current(&self) -> Result<(), LinuxError> {
        let _apply_guard = self.apply_lock.lock().await;
        if self.state.read().await.stopping {
            return Ok(());
        }
        self.reconcile_locked().await
    }

    pub async fn health(&self) -> ExportHealth {
        let state = self.state.read().await;
        ExportHealth {
            config_generation: state.config_generation,
            last_success_route_generation: state
                .last_success_revision
                .map(|value| value.route_generation),
            last_success_config_generation: state
                .last_success_revision
                .map(|value| value.config_generation),
            last_success_age: state.last_success.map(|value| value.elapsed()),
            last_error: state.last_error.clone(),
        }
    }

    async fn reconcile_locked(&self) -> Result<(), LinuxError> {
        // Capture both inputs before I/O. A concurrent reload or shutdown may
        // replace desired state while this attempt is awaiting netlink.
        let attempted = self.state.read().await.clone();
        let revision = attempted.revision();
        let result = self.reconcile_attempt(attempted).await;
        self.state.write().await.record_reconcile(revision, &result);
        result
    }

    async fn reconcile_attempt(&self, state: ExportState) -> Result<(), LinuxError> {
        self.apply_locked(&state.export, state.snapshot, state.retain_rules)
            .await?;
        let mut cleaned = Vec::new();
        for retired in &state.retired {
            self.apply_locked(retired, RouteSnapshot::default(), false)
                .await?;
            cleaned.push(retired.clone());
        }
        if !cleaned.is_empty() {
            self.state
                .write()
                .await
                .retired
                .retain(|export| !cleaned.contains(export));
        }
        Ok(())
    }

    pub async fn run_reconciler(&self, mut shutdown: watch::Receiver<bool>) {
        let mut interval = tokio::time::interval(Duration::from_secs(2));
        interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
        interval.tick().await;
        loop {
            tokio::select! {
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        return;
                    }
                }
                _ = interval.tick() => {
                    if let Err(error) = self.reconcile_current().await {
                        warn!(%error, "periodic route export reconciliation failed");
                    }
                }
                _ = self.reconcile_notify.notified() => {
                    if let Err(error) = self.reconcile_current().await {
                        warn!(%error, "requested route export reconciliation failed; retry scheduled");
                    }
                }
            }
        }
    }
}

#[async_trait]
impl RouteExporter for LinuxExporter {
    async fn reconcile(
        &self,
        snapshot: RouteSnapshot,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let _apply_guard = self.apply_lock.lock().await;
        {
            let mut state = self.state.write().await;
            if state.stopping {
                return Ok(());
            }
            state.snapshot = snapshot;
            state.retain_rules = true;
        }
        self.reconcile_locked()
            .await
            .map_err(|error| Box::new(error) as _)
    }

    async fn shutdown(
        &self,
        snapshot: RouteSnapshot,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        {
            let mut state = self.state.write().await;
            // Latch before waiting for an in-flight reconciliation. No worker
            // may publish an old snapshot after shutdown cleanup completes.
            state.stopping = true;
            state.snapshot = snapshot;
            state.retain_rules = false;
        }
        let _apply_guard = self.apply_lock.lock().await;
        self.reconcile_locked()
            .await
            .map_err(|error| Box::new(error) as _)
    }
}

#[cfg(test)]
mod tests;

mod identity;
mod netlink;
mod projection;
use identity::{route_identity, route_table, rule_identity};
use projection::project_routes;
