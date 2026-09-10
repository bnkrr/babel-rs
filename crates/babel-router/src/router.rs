use std::collections::{BTreeMap, HashMap};
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::time::Instant;

use babel_protocol::{
    Action, AdditiveMetric, DecodeContext, Engine, EngineConfig, Event, InterfacePolicy,
    Ipv4NextHop, MetricAlgebra, MetricProfile, NeighborStatus, ResourceLimits, ResourceStatus,
    RouteKey, RouteSelectionConfig, RouterId, WiredMetric, decode_packet, stamp_hello_timestamps,
};
use thiserror::Error;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tracing::{debug, warn};

use crate::export::{
    MemoryExporter, NoopSequenceStore, RouteExporter, RouteSnapshot, SequenceStore,
};
use crate::output::{OutboundIntent, OutputScheduler};
use crate::output_queue::{
    OUTPUT_BUDGET_BYTES, OUTPUT_QUEUE_CAPACITY, OutputCounters, OutputQueue, OutputStatus,
    QueuedIntent, SEND_TIMEOUT_MS,
};
use crate::transport::{InterfaceSocket, payload_budget_for_transport};

/// Configuration, interface activation or runtime command failure.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum RouterError {
    #[error("router-id is required")]
    MissingRouterId,
    #[error(transparent)]
    InvalidConfig(#[from] babel_protocol::ConfigError),
    #[error("duplicate Babel interface {0}")]
    DuplicateInterface(String),
    #[error("open Babel interface {interface}: {source}")]
    OpenInterface {
        interface: String,
        source: std::io::Error,
    },
    #[error("Babel interface {0} is not active")]
    InterfaceNotFound(String),
    #[error("changing control transport on {0} requires removing and reattaching the interface")]
    TransportChangeRequiresReattach(String),
    #[error("duplicate originated route {0:?}")]
    DuplicateOrigin(RouteKey),
    #[error("originated route metric must be below Babel infinity")]
    InvalidOriginMetric,
    #[error("interface Hello and Update intervals must be nonzero")]
    InvalidInterfacePolicy,
    #[error("router task stopped")]
    Stopped,
    #[error("router task failed: {0}")]
    Task(String),
    #[error("router shutdown exceeded its deadline")]
    ShutdownTimeout,
    #[error("final route export cleanup failed: {0}")]
    Cleanup(String),
    #[error("shutdown timeout must be nonzero")]
    InvalidShutdownTimeout,
    #[error("persist Babel sequence number: {0}")]
    SequenceStore(String),
}

/// Interface policy, live MTU and bounded output statistics.
#[derive(Clone, Debug, Default)]
pub struct RouterInterfaceStatus {
    pub name: String,
    pub index: u32,
    pub local_addresses: Vec<IpAddr>,
    pub control_transport: babel_protocol::ControlTransport,
    pub mtu: u32,
    pub udp_payload_budget: usize,
    pub metric: String,
    pub hello_interval_ms: u64,
    pub update_interval_ms: u64,
    pub split_horizon: bool,
    /// Configured IPv4 announcement policy.
    pub ipv4_next_hop: Ipv4NextHop,
    /// Chosen ordinary IPv4 next hop. None means IPv6 for Auto/Ipv6, unavailable for Ipv4.
    pub ipv4_address: Option<std::net::Ipv4Addr>,
    pub output: OutputStatus,
}

/// Point-in-time engine and transport status; not a route-export completion barrier.
#[derive(Clone, Debug, Default)]
pub struct RouterStatus {
    /// Current in-memory sequence number for locally originated routes.
    pub sequence_number: u16,
    pub resources: ResourceStatus,
    pub metric: String,
    pub interfaces: Vec<String>,
    pub interface_details: Vec<RouterInterfaceStatus>,
    pub neighbours: usize,
    pub neighbour_details: Vec<NeighborStatus>,
    pub route_generation: u64,
    pub selected_routes: usize,
    pub dropped_outbound_datagrams: u64,
    pub missed_outbound_deadlines: u64,
}

/// A coalescing watch stream of complete selected learned RIBs and unreachable keys.
pub type RouteStream = watch::Receiver<RouteSnapshot>;

enum Command {
    Originate(RouteKey, u16, oneshot::Sender<()>),
    Withdraw(RouteKey, oneshot::Sender<()>),
    ShutdownTimeout(Duration, oneshot::Sender<()>),
    ReplaceOrigins(
        BTreeMap<RouteKey, u16>,
        oneshot::Sender<Result<(), RouterError>>,
    ),
    AddInterface(
        String,
        Option<InterfacePolicy>,
        oneshot::Sender<Result<(), RouterError>>,
    ),
    UpdateInterfacePolicy(
        String,
        InterfacePolicy,
        bool,
        oneshot::Sender<Result<(), RouterError>>,
    ),
    RemoveInterface(String, oneshot::Sender<Result<(), RouterError>>),
    Status(oneshot::Sender<RouterStatus>),
}

enum Received {
    Packet {
        _permit: OwnedSemaphorePermit,
        interface: String,
        index: u32,
        source: IpAddr,
        bytes: Vec<u8>,
        received_timestamp_us: u32,
    },
    Failed {
        interface: String,
        index: u32,
        error: String,
    },
}

struct Runtime {
    workers: HashMap<String, Vec<Worker>>,
    export_worker: Option<Worker>,
    shutdown_timeout: Duration,
    router_id: RouterId,
    interfaces: Vec<(String, Option<InterfacePolicy>)>,
    origins: Vec<(RouteKey, u16)>,
    sockets: HashMap<String, Arc<InterfaceSocket>>,
    outbound: HashMap<String, OutputQueue>,
    output_counters: Arc<OutputCounters>,
    interface_stops: HashMap<String, watch::Sender<bool>>,
    exporter: Arc<dyn RouteExporter>,
    export_updates: watch::Sender<RouteSnapshot>,
    commands: mpsc::Receiver<Command>,
    received: mpsc::Receiver<Received>,
    received_tx: mpsc::Sender<Received>,
    shutdown: watch::Receiver<bool>,
    route_updates: watch::Sender<RouteSnapshot>,
    metric: Arc<dyn MetricProfile>,
    metric_algebra: Arc<dyn MetricAlgebra>,
    route_selection: RouteSelectionConfig,
    limits: ResourceLimits,
    sequence_number: u16,
    sequence_store: Arc<dyn SequenceStore>,
    started: Arc<Instant>,
}

/// Every worker is owned by the runtime; cancellation cannot detach it.
struct Worker(JoinHandle<()>);

impl Drop for Worker {
    fn drop(&mut self) {
        self.0.abort();
    }
}

impl Worker {
    async fn join(&mut self) -> Result<(), RouterError> {
        (&mut self.0)
            .await
            .map_err(|error| RouterError::Task(error.to_string()))
    }
}

// Bound each interface's share of the common input queue so a route burst
// cannot put hundreds of expensive updates ahead of another link's Hello.
const RECEIVED_PER_INTERFACE: usize = 4;

// A checkpoint improves orderly restart recovery but must not prevent shutdown.
const SEQUENCE_CHECKPOINT_TIMEOUT: Duration = Duration::from_secs(1);

fn elapsed_ms(started: &Instant) -> u64 {
    started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64
}

/// Cloneable asynchronous command handle. Fallible local input is checked before enqueueing.
#[derive(Clone)]
pub struct RouterHandle {
    commands: mpsc::Sender<Command>,
    shutdown: watch::Sender<bool>,
    routes: RouteStream,
}

impl RouterHandle {
    /// Apply a finite local origin. Success acknowledges engine application, not delivery/export.
    pub async fn originate(&self, key: RouteKey, metric: u16) -> Result<(), RouterError> {
        validate_origin(key, metric)?;
        let (reply, done) = oneshot::channel();
        self.commands
            .send(Command::Originate(key, metric, reply))
            .await
            .map_err(|_| RouterError::Stopped)?;
        done.await.map_err(|_| RouterError::Stopped)
    }

    /// Apply a local withdrawal. An absent key is harmless; success acknowledges engine application.
    pub async fn withdraw(&self, key: RouteKey) -> Result<(), RouterError> {
        key.validate()?;
        let (reply, done) = oneshot::channel();
        self.commands
            .send(Command::Withdraw(key, reply))
            .await
            .map_err(|_| RouterError::Stopped)?;
        done.await.map_err(|_| RouterError::Stopped)
    }

    /// Change the total orderly-cleanup deadline for the next shutdown.
    pub async fn set_shutdown_timeout(&self, timeout: Duration) -> Result<(), RouterError> {
        if timeout.is_zero() {
            return Err(RouterError::InvalidShutdownTimeout);
        }
        let (reply, done) = oneshot::channel();
        self.commands
            .send(Command::ShutdownTimeout(timeout, reply))
            .await
            .map_err(|_| RouterError::Stopped)?;
        done.await.map_err(|_| RouterError::Stopped)
    }

    /// Validate the complete set, reject duplicate keys, and wait for atomic engine replacement.
    /// Success does not wait for packet delivery or route export.
    pub async fn replace_origins(&self, origins: Vec<(RouteKey, u16)>) -> Result<(), RouterError> {
        let mut desired = BTreeMap::new();
        for (key, metric) in origins {
            validate_origin(key, metric)?;
            if desired.insert(key, metric).is_some() {
                return Err(RouterError::DuplicateOrigin(key));
            }
        }
        let (send, receive) = oneshot::channel();
        self.commands
            .send(Command::ReplaceOrigins(desired, send))
            .await
            .map_err(|_| RouterError::Stopped)?;
        receive.await.map_err(|_| RouterError::Stopped)?
    }

    /// Attach using default policy and wait for socket/engine activation. Already active is a no-op.
    pub async fn add_interface(&self, interface: impl Into<String>) -> Result<(), RouterError> {
        self.add_interface_inner(interface.into(), None).await
    }

    /// Validate a policy before opening an interface. Already active is a no-op;
    /// use [`Self::update_interface_policy`] to change an existing interface.
    pub async fn add_interface_with_policy(
        &self,
        interface: impl Into<String>,
        policy: InterfacePolicy,
    ) -> Result<(), RouterError> {
        validate_interface_policy(&policy)?;
        self.add_interface_inner(interface.into(), Some(policy))
            .await
    }

    async fn add_interface_inner(
        &self,
        interface: String,
        policy: Option<InterfacePolicy>,
    ) -> Result<(), RouterError> {
        let (send, receive) = oneshot::channel();
        self.commands
            .send(Command::AddInterface(interface, policy, send))
            .await
            .map_err(|_| RouterError::Stopped)?;
        receive.await.map_err(|_| RouterError::Stopped)?
    }

    /// Wait for interface detachment and RIB reselection; an absent interface returns an error.
    pub async fn remove_interface(&self, interface: impl Into<String>) -> Result<(), RouterError> {
        let (send, receive) = oneshot::channel();
        self.commands
            .send(Command::RemoveInterface(interface.into(), send))
            .await
            .map_err(|_| RouterError::Stopped)?;
        receive.await.map_err(|_| RouterError::Stopped)?
    }

    /// Validate and apply policy to an active interface. When `reset_metric` is true,
    /// rebuild neighbor metric state from retained Hello/IHU observations.
    pub async fn update_interface_policy(
        &self,
        interface: impl Into<String>,
        policy: InterfacePolicy,
        reset_metric: bool,
    ) -> Result<(), RouterError> {
        validate_interface_policy(&policy)?;
        let (send, receive) = oneshot::channel();
        self.commands
            .send(Command::UpdateInterfacePolicy(
                interface.into(),
                policy,
                reset_metric,
                send,
            ))
            .await
            .map_err(|_| RouterError::Stopped)?;
        receive.await.map_err(|_| RouterError::Stopped)?
    }

    /// Read serialized engine status after earlier queued commands have been handled.
    pub async fn status(&self) -> Result<RouterStatus, RouterError> {
        let (send, receive) = oneshot::channel();
        self.commands
            .send(Command::Status(send))
            .await
            .map_err(|_| RouterError::Stopped)?;
        receive.await.map_err(|_| RouterError::Stopped)
    }

    /// Compatibility alias for [`Self::request_shutdown`]. Does not wait.
    pub fn shutdown(&self) {
        self.request_shutdown();
    }

    /// Request orderly shutdown; await [`BabelRouter::wait`] to observe its result.
    pub fn request_shutdown(&self) {
        let _ = self.shutdown.send(true);
    }

    /// Subscribe to complete learned-route snapshots. Slow readers may skip generations;
    /// this stream is not an export-completion acknowledgement.
    pub fn subscribe_routes(&self) -> RouteStream {
        self.routes.clone()
    }
}

/// Owner of a running router. Drop cancels its tasks without asynchronous cleanup.
/// Use [`Self::shutdown`] for orderly retractions, checkpointing and export cleanup.
pub struct BabelRouter {
    handle: RouterHandle,
    task: JoinHandle<Result<(), RouterError>>,
}

impl Drop for BabelRouter {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl BabelRouter {
    /// Create a builder; a Router-ID is required before building.
    pub fn builder() -> BabelRouterBuilder {
        BabelRouterBuilder::default()
    }

    /// Clone the command/status handle for an already running router.
    pub fn handle(&self) -> RouterHandle {
        self.handle.clone()
    }

    /// Cancel the engine task when the host's orderly shutdown deadline expires.
    /// Aborting can skip retractions, checkpointing and exporter cleanup. Hosts
    /// must also stop their services and handle external state on the next start.
    pub fn abort_handle(&self) -> tokio::task::AbortHandle {
        self.task.abort_handle()
    }

    /// Compatibility alias for [`Self::wait`].
    pub async fn run(self) -> Result<(), RouterError> {
        self.wait().await
    }

    /// Wait for normal shutdown or a runtime failure. Dropping this future cancels the router.
    pub async fn wait(mut self) -> Result<(), RouterError> {
        (&mut self.task)
            .await
            .map_err(|error| RouterError::Task(error.to_string()))?
    }

    /// Request and await orderly shutdown, bounded by the configured total cleanup deadline.
    pub async fn shutdown(self) -> Result<(), RouterError> {
        self.handle.request_shutdown();
        self.wait().await
    }
}

/// Deferred configuration for a Tokio router. Setters store values;
/// [`Self::validate`] and [`Self::build`] check the entire configuration before I/O.
/// No interfaces is valid: attach them later through [`RouterHandle`].
#[derive(Default)]
pub struct BabelRouterBuilder {
    shutdown_timeout: Option<Duration>,
    router_id: Option<RouterId>,
    interfaces: Vec<(String, Option<InterfacePolicy>)>,
    origins: Vec<(RouteKey, u16)>,
    exporter: Option<Arc<dyn RouteExporter>>,
    metric: Option<Arc<dyn MetricProfile>>,
    metric_algebra: Option<Arc<dyn MetricAlgebra>>,
    route_selection: Option<RouteSelectionConfig>,
    limits: ResourceLimits,
    sequence_number: u16,
    sequence_store: Option<Arc<dyn SequenceStore>>,
}

impl BabelRouterBuilder {
    /// Total orderly-cleanup budget, default five seconds; must be nonzero.
    pub fn shutdown_timeout(mut self, timeout: Duration) -> Self {
        self.shutdown_timeout = Some(timeout);
        self
    }
    /// Set the stable local origin identity (required).
    pub fn router_id(mut self, value: RouterId) -> Self {
        self.router_id = Some(value);
        self
    }
    /// Add an exact interface name with default policy; duplicates fail validation.
    pub fn interface(mut self, value: impl Into<String>) -> Self {
        self.interfaces.push((value.into(), None));
        self
    }
    /// Add an exact interface name with explicit metric, timers and split horizon.
    pub fn interface_with_policy(
        mut self,
        value: impl Into<String>,
        policy: InterfacePolicy,
    ) -> Self {
        self.interfaces.push((value.into(), Some(policy)));
        self
    }
    /// Add an initial canonical local route with a finite metric; duplicates fail validation.
    pub fn originate(mut self, key: RouteKey, metric: u16) -> Self {
        self.origins.push((key, metric));
        self
    }
    /// Set the desired-state backend (default: [`MemoryExporter`]).
    pub fn exporter(mut self, value: impl RouteExporter) -> Self {
        self.exporter = Some(Arc::new(value));
        self
    }
    /// Set the default metric factory (default: [`WiredMetric`]).
    pub fn metric(mut self, value: impl MetricProfile) -> Self {
        self.metric = Some(Arc::new(value));
        self
    }
    /// Set a shared default metric factory, creating independent state per neighbor.
    pub fn metric_profile(mut self, value: Arc<dyn MetricProfile>) -> Self {
        self.metric = Some(value);
        self
    }
    /// Set route metric composition (default: [`AdditiveMetric`]).
    pub fn metric_algebra(mut self, value: impl MetricAlgebra) -> Self {
        self.metric_algebra = Some(Arc::new(value));
        self
    }
    /// Set route-switch hysteresis, checked by [`Self::validate`] and [`Self::build`].
    pub fn route_selection(mut self, value: RouteSelectionConfig) -> Self {
        self.route_selection = Some(value);
        self
    }
    /// Bound admission of learned neighbors and candidates. Existing entries
    /// continue to update and expire; released capacity is immediately reusable.
    pub fn limits(mut self, value: ResourceLimits) -> Self {
        self.limits = value;
        self
    }
    /// Set the initial in-memory sequence (default: zero). The host owns restart recovery.
    pub fn sequence_number(mut self, value: u16) -> Self {
        self.sequence_number = value;
        self
    }
    /// Save the final sequence number once during orderly shutdown. Runtime
    /// changes remain in memory. Checkpoint errors/timeouts are reported after
    /// cleanup; see [`SequenceStore`] for cancellation details.
    pub fn sequence_store(mut self, value: impl SequenceStore) -> Self {
        self.sequence_store = Some(Arc::new(value));
        self
    }

    /// Validate all configuration without opening sockets or spawning tasks.
    /// Zero interfaces/origins are allowed for later dynamic attachment.
    pub fn validate(&self) -> Result<(), RouterError> {
        if self
            .shutdown_timeout
            .is_some_and(|timeout| timeout.is_zero())
        {
            return Err(RouterError::InvalidShutdownTimeout);
        }
        self.router_id.ok_or(RouterError::MissingRouterId)?;
        self.route_selection.unwrap_or_default().validate()?;
        let mut interfaces = std::collections::HashSet::new();
        for (name, policy) in &self.interfaces {
            if !interfaces.insert(name) {
                return Err(RouterError::DuplicateInterface(name.clone()));
            }
            if let Some(policy) = policy {
                validate_interface_policy(policy)?;
            }
        }
        let mut origins = std::collections::HashSet::new();
        for (key, metric) in &self.origins {
            validate_origin(*key, *metric)?;
            if !origins.insert(key) {
                return Err(RouterError::DuplicateOrigin(*key));
            }
        }
        Ok(())
    }

    /// Validate the complete configuration, open interfaces, and start routing.
    /// Requires a Tokio runtime with I/O and timers enabled. Socket failures
    /// are returned before any worker starts; configuration errors precede I/O.
    pub async fn build(self) -> Result<BabelRouter, RouterError> {
        self.start().await
    }

    /// Validate, open Linux interfaces, and start all workers. Success means
    /// initial interfaces and origins have been applied, not exported or delivered.
    pub async fn start(self) -> Result<BabelRouter, RouterError> {
        self.validate()?;
        let router_id = self.router_id.ok_or(RouterError::MissingRouterId)?;
        let mut sockets = HashMap::new();
        for (name, policy) in &self.interfaces {
            let socket = InterfaceSocket::open(
                name,
                policy
                    .as_ref()
                    .map_or(Default::default(), |p| p.control_transport),
            )
            .map_err(|source| RouterError::OpenInterface {
                interface: name.clone(),
                source,
            })?;
            sockets.insert(name.clone(), Arc::new(socket));
        }
        let exporter: Arc<dyn RouteExporter> = self
            .exporter
            .unwrap_or_else(|| Arc::new(MemoryExporter::default()));
        let (commands_tx, commands_rx) = mpsc::channel(64);
        let (received_tx, received_rx) = mpsc::channel(256);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let (route_updates, route_stream) = watch::channel(RouteSnapshot::default());
        let (export_updates, export_stream) = watch::channel(RouteSnapshot::default());
        let export_worker = Some(spawn_exporter(
            Arc::clone(&exporter),
            export_stream,
            shutdown_rx.clone(),
        ));
        let mut workers = HashMap::new();
        let mut interface_stops = HashMap::new();
        let mut outbound = HashMap::new();
        let started = Arc::new(Instant::now());
        let output_counters = Arc::new(OutputCounters::default());
        for (name, socket) in &sockets {
            let (stop, stop_rx) = watch::channel(false);
            interface_stops.insert(name.clone(), stop);
            let receiver = spawn_receiver(
                Arc::clone(socket),
                received_tx.clone(),
                shutdown_rx.clone(),
                stop_rx.clone(),
                Arc::clone(&started),
            );
            let (sender, sender_worker) = spawn_sender(
                Arc::clone(socket),
                stop_rx,
                Arc::clone(&started),
                Arc::clone(&output_counters),
            );
            outbound.insert(name.clone(), sender);
            workers.insert(name.clone(), vec![receiver, sender_worker]);
        }
        let task = tokio::spawn(run_loop(Runtime {
            workers,
            export_worker,
            shutdown_timeout: self.shutdown_timeout.unwrap_or(Duration::from_secs(5)),
            router_id,
            interfaces: self.interfaces,
            origins: self.origins,
            sockets,
            outbound,
            output_counters,
            interface_stops,
            exporter,
            export_updates,
            commands: commands_rx,
            received: received_rx,
            received_tx,
            shutdown: shutdown_rx,
            route_updates,
            metric: self
                .metric
                .unwrap_or_else(|| Arc::new(WiredMetric::default())),
            metric_algebra: self
                .metric_algebra
                .unwrap_or_else(|| Arc::new(AdditiveMetric)),
            route_selection: self.route_selection.unwrap_or_default(),
            limits: self.limits,
            sequence_number: self.sequence_number,
            sequence_store: self
                .sequence_store
                .unwrap_or_else(|| Arc::new(NoopSequenceStore)),
            started,
        }));
        let router = BabelRouter {
            handle: RouterHandle {
                commands: commands_tx,
                shutdown: shutdown_tx,
                routes: route_stream,
            },
            task,
        };
        router.handle.status().await?;
        Ok(router)
    }
}

fn validate_interface_policy(policy: &InterfacePolicy) -> Result<(), RouterError> {
    policy
        .validate()
        .map_err(|_| RouterError::InvalidInterfacePolicy)
}

fn validate_origin(key: RouteKey, metric: u16) -> Result<(), RouterError> {
    key.validate_origin(metric).map_err(|error| match error {
        babel_protocol::ConfigError::InvalidOriginMetric => RouterError::InvalidOriginMetric,
        other => RouterError::InvalidConfig(other),
    })
}

#[cfg(test)]
mod tests;

mod io;
mod runtime;
use io::*;
use runtime::*;
