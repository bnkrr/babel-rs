use std::collections::{BTreeMap, HashMap};
use std::net::{IpAddr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::time::Instant;

use babel_proto::{
    Action, AdditiveMetric, DecodeContext, Engine, EngineConfig, Event, InterfacePolicy,
    MetricAlgebra, MetricProfile, NeighborStatus, ResourceLimits, ResourceStatus, RouteKey,
    RouteSelectionConfig, RouterId, WiredMetric, decode_packet, stamp_hello_timestamps,
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
use crate::transport::{InterfaceSocket, payload_budget_for_mtu};

#[derive(Debug, Error)]
pub enum RouterError {
    #[error("router-id is required")]
    MissingRouterId,
    #[error("open Babel interface {interface}: {source}")]
    OpenInterface {
        interface: String,
        source: std::io::Error,
    },
    #[error("Babel interface {0} is not active")]
    InterfaceNotFound(String),
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
    /// Retained for compatibility; shutdown checkpoint failures are now logged.
    #[error("persist Babel sequence number: {0}")]
    SequenceStore(String),
}

#[derive(Clone, Debug, Default)]
pub struct RouterInterfaceStatus {
    pub name: String,
    pub index: u32,
    pub local_addresses: Vec<Ipv6Addr>,
    pub mtu: u32,
    pub udp_payload_budget: usize,
    pub metric: String,
    pub hello_interval_ms: u64,
    pub update_interval_ms: u64,
    pub split_horizon: bool,
    pub output: OutputStatus,
}

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

pub type RouteStream = watch::Receiver<RouteSnapshot>;

enum Command {
    Originate(RouteKey, u16),
    Withdraw(RouteKey),
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
        now_ms: u64,
    },
    Failed {
        interface: String,
        index: u32,
        error: String,
    },
}

struct Runtime {
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

// Bound each interface's share of the common input queue so a route burst
// cannot put hundreds of expensive updates ahead of another link's Hello.
const RECEIVED_PER_INTERFACE: usize = 4;

// A checkpoint improves orderly restart recovery but must not prevent shutdown.
const SEQUENCE_CHECKPOINT_TIMEOUT: Duration = Duration::from_secs(1);

fn elapsed_ms(started: &Instant) -> u64 {
    started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64
}

#[derive(Clone)]
pub struct RouterHandle {
    commands: mpsc::Sender<Command>,
    shutdown: watch::Sender<bool>,
    routes: RouteStream,
}

impl RouterHandle {
    pub async fn originate(&self, key: RouteKey, metric: u16) -> Result<(), RouterError> {
        self.commands
            .send(Command::Originate(key, metric))
            .await
            .map_err(|_| RouterError::Stopped)
    }

    pub async fn withdraw(&self, key: RouteKey) -> Result<(), RouterError> {
        self.commands
            .send(Command::Withdraw(key))
            .await
            .map_err(|_| RouterError::Stopped)
    }

    pub async fn replace_origins(&self, origins: Vec<(RouteKey, u16)>) -> Result<(), RouterError> {
        let mut desired = BTreeMap::new();
        for (key, metric) in origins {
            if metric == babel_proto::INFINITY {
                return Err(RouterError::InvalidOriginMetric);
            }
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

    pub async fn add_interface(&self, interface: impl Into<String>) -> Result<(), RouterError> {
        self.add_interface_inner(interface.into(), None).await
    }

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

    pub async fn remove_interface(&self, interface: impl Into<String>) -> Result<(), RouterError> {
        let (send, receive) = oneshot::channel();
        self.commands
            .send(Command::RemoveInterface(interface.into(), send))
            .await
            .map_err(|_| RouterError::Stopped)?;
        receive.await.map_err(|_| RouterError::Stopped)?
    }

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

    pub async fn status(&self) -> Result<RouterStatus, RouterError> {
        let (send, receive) = oneshot::channel();
        self.commands
            .send(Command::Status(send))
            .await
            .map_err(|_| RouterError::Stopped)?;
        receive.await.map_err(|_| RouterError::Stopped)
    }

    pub fn shutdown(&self) {
        let _ = self.shutdown.send(true);
    }

    pub fn subscribe_routes(&self) -> RouteStream {
        self.routes.clone()
    }
}

pub struct BabelRouter {
    handle: RouterHandle,
    task: JoinHandle<Result<(), RouterError>>,
}

impl BabelRouter {
    pub fn builder() -> BabelRouterBuilder {
        BabelRouterBuilder::default()
    }

    pub fn handle(&self) -> RouterHandle {
        self.handle.clone()
    }

    pub async fn run(self) -> Result<(), RouterError> {
        self.task
            .await
            .map_err(|error| RouterError::Task(error.to_string()))?
    }
}

#[derive(Default)]
pub struct BabelRouterBuilder {
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
    pub fn router_id(mut self, value: RouterId) -> Self {
        self.router_id = Some(value);
        self
    }
    pub fn interface(mut self, value: impl Into<String>) -> Self {
        self.interfaces.push((value.into(), None));
        self
    }
    pub fn interface_with_policy(
        mut self,
        value: impl Into<String>,
        policy: InterfacePolicy,
    ) -> Self {
        self.interfaces.push((value.into(), Some(policy)));
        self
    }
    pub fn originate(mut self, key: RouteKey, metric: u16) -> Self {
        self.origins.push((key, metric));
        self
    }
    pub fn exporter(mut self, value: impl RouteExporter) -> Self {
        self.exporter = Some(Arc::new(value));
        self
    }
    pub fn metric(mut self, value: impl MetricProfile) -> Self {
        self.metric = Some(Arc::new(value));
        self
    }
    pub fn metric_profile(mut self, value: Arc<dyn MetricProfile>) -> Self {
        self.metric = Some(value);
        self
    }
    pub fn metric_algebra(mut self, value: impl MetricAlgebra) -> Self {
        self.metric_algebra = Some(Arc::new(value));
        self
    }
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
    pub fn sequence_number(mut self, value: u16) -> Self {
        self.sequence_number = value;
        self
    }
    /// Save the final sequence number once during orderly shutdown. Runtime
    /// changes remain in memory. Checkpoint errors/timeouts are logged, and the
    /// router continues cleanup; see [`SequenceStore`] for cancellation details.
    pub fn sequence_store(mut self, value: impl SequenceStore) -> Self {
        self.sequence_store = Some(Arc::new(value));
        self
    }

    pub async fn build(self) -> Result<BabelRouter, RouterError> {
        let router_id = self.router_id.ok_or(RouterError::MissingRouterId)?;
        let mut sockets = HashMap::new();
        for (name, policy) in &self.interfaces {
            if let Some(policy) = policy {
                validate_interface_policy(policy)?;
            }
            let socket =
                InterfaceSocket::open(name).map_err(|source| RouterError::OpenInterface {
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
        spawn_exporter(Arc::clone(&exporter), export_stream, shutdown_rx.clone());
        let mut interface_stops = HashMap::new();
        let mut outbound = HashMap::new();
        let started = Arc::new(Instant::now());
        let output_counters = Arc::new(OutputCounters::default());
        for (name, socket) in &sockets {
            let (stop, stop_rx) = watch::channel(false);
            interface_stops.insert(name.clone(), stop);
            spawn_receiver(
                Arc::clone(socket),
                received_tx.clone(),
                shutdown_rx.clone(),
                stop_rx.clone(),
                Arc::clone(&started),
            );
            outbound.insert(
                name.clone(),
                spawn_sender(
                    Arc::clone(socket),
                    stop_rx,
                    Arc::clone(&started),
                    Arc::clone(&output_counters),
                ),
            );
        }
        let task = tokio::spawn(run_loop(Runtime {
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
        Ok(BabelRouter {
            handle: RouterHandle {
                commands: commands_tx,
                shutdown: shutdown_tx,
                routes: route_stream,
            },
            task,
        })
    }
}

fn validate_interface_policy(policy: &InterfacePolicy) -> Result<(), RouterError> {
    if policy.hello_interval_cs == 0 || policy.update_interval_cs == 0 {
        Err(RouterError::InvalidInterfacePolicy)
    } else {
        Ok(())
    }
}

fn spawn_receiver(
    socket: Arc<InterfaceSocket>,
    received: mpsc::Sender<Received>,
    mut shutdown: watch::Receiver<bool>,
    mut stop: watch::Receiver<bool>,
    started: Arc<Instant>,
) {
    tokio::spawn(async move {
        let mut buffer = vec![0u8; 65535];
        let slots = Arc::new(Semaphore::new(RECEIVED_PER_INTERFACE));
        loop {
            let permit = tokio::select! {
                _ = shutdown.changed() => return,
                _ = stop.changed() => return,
                permit = Arc::clone(&slots).acquire_owned() => permit.expect("receiver owns semaphore"),
            };
            tokio::select! {
                changed = shutdown.changed() => if changed.is_err() || *shutdown.borrow() { return; },
                changed = stop.changed() => if changed.is_err() || *stop.borrow() { return; },
                result = socket.socket.recv_from(&mut buffer) => match result {
                    Ok((length, SocketAddr::V6(source)))
                        if valid_babel_source(&source, &socket.local_addresses) => {
                        let now_ms = elapsed_ms(&started);
                        let item = Received::Packet { _permit: permit, interface: socket.name.clone(), index: socket.index, source: IpAddr::V6(*source.ip()), bytes: buffer[..length].to_vec(), now_ms };
                        if received.send(item).await.is_err() { return; }
                    }
                    Ok(_) => {}
                    Err(error) => {
                        warn!(interface = %socket.name, %error, "Babel receive failed");
                        let _ = received.send(Received::Failed {
                            interface: socket.name.clone(),
                            index: socket.index,
                            error: error.to_string(),
                        }).await;
                        return;
                    }
                }
            }
        }
    });
}

fn valid_babel_source(source: &std::net::SocketAddrV6, local: &[Ipv6Addr]) -> bool {
    source.port() == babel_proto::wire::PORT
        && source.ip().is_unicast_link_local()
        && !local.contains(source.ip())
}

#[async_trait::async_trait]
trait OutputTransport: Send + Sync + 'static {
    fn payload_budget(&self) -> std::io::Result<usize>;
    async fn send(&self, bytes: &[u8], destination: Ipv6Addr) -> std::io::Result<usize>;
}

#[async_trait::async_trait]
impl OutputTransport for InterfaceSocket {
    fn payload_budget(&self) -> std::io::Result<usize> {
        InterfaceSocket::payload_budget(self)
    }

    async fn send(&self, bytes: &[u8], destination: Ipv6Addr) -> std::io::Result<usize> {
        self.socket
            .send_to(bytes, self.destination(destination))
            .await
    }
}

fn spawn_sender(
    socket: Arc<InterfaceSocket>,
    stop: watch::Receiver<bool>,
    started: Arc<Instant>,
    counters: Arc<OutputCounters>,
) -> OutputQueue {
    let (queue, receive) = OutputQueue::new(
        OUTPUT_QUEUE_CAPACITY,
        OUTPUT_BUDGET_BYTES,
        counters,
        Arc::clone(&started),
    );
    tokio::spawn(run_sender(
        socket.clone(),
        stop,
        started,
        queue.clone(),
        receive,
        output_seed(&socket),
    ));
    queue
}

async fn run_sender(
    transport: Arc<impl OutputTransport>,
    mut stop: watch::Receiver<bool>,
    started: Arc<Instant>,
    queue: OutputQueue,
    mut receive: mpsc::Receiver<QueuedIntent>,
    seed: u64,
) {
    let mut scheduler = OutputScheduler::new(seed);
    loop {
        if *stop.borrow() {
            return;
        }
        let now_ms = elapsed_ms(&started);
        let (expired_batches, expired_datagrams) = scheduler.expire(now_ms);
        queue.expired_batches(expired_batches);
        for _ in 0..expired_datagrams {
            queue.drop_datagram(true, false);
        }
        if scheduler.next_wake_ms().is_some_and(|wake| wake <= now_ms) {
            let payload_budget = match transport.payload_budget() {
                Ok(value) => value,
                Err(_) => {
                    // Retry locally, but continue expiry and observe interface removal.
                    tokio::select! {
                        _ = stop.changed() => return,
                        _ = tokio::time::sleep(Duration::from_millis(SEND_TIMEOUT_MS)) => {}
                    }
                    continue;
                }
            };
            match scheduler.pop_due(now_ms, payload_budget) {
                Ok(Some(mut datagram)) => {
                    let send_ms = elapsed_ms(&started);
                    if send_ms >= datagram.expires_ms {
                        queue.drop_datagram(true, false);
                        continue;
                    }
                    if send_ms > datagram.deadline_ms {
                        queue.missed_deadline();
                    }
                    if stamp_hello_timestamps(
                        &mut datagram.bytes,
                        send_ms.wrapping_mul(1_000) as u32,
                    )
                    .is_err()
                    {
                        queue.drop_datagram(false, false);
                        continue;
                    }
                    let wait_ms = SEND_TIMEOUT_MS.min(datagram.expires_ms - send_ms);
                    let send_deadline =
                        *started + Duration::from_millis(send_ms.saturating_add(wait_ms));
                    let result = {
                        let send = transport.send(&datagram.bytes, datagram.destination);
                        tokio::pin!(send);
                        // Check the clock before every socket poll as well as
                        // arming a timer. A ready socket may otherwise run
                        // before the timer driver notices a runtime stall.
                        let fresh_send = std::future::poll_fn(|cx| {
                            if Instant::now() >= send_deadline {
                                std::task::Poll::Ready(None)
                            } else {
                                send.as_mut().poll(cx).map(Some)
                            }
                        });
                        tokio::select! {
                            biased;
                            _ = stop.changed() => return,
                            _ = tokio::time::sleep_until(send_deadline) => None,
                            result = fresh_send => result,
                        }
                    };
                    match result {
                        Some(Ok(_)) => {}
                        Some(Err(_)) => queue.drop_datagram(false, false),
                        None => {
                            queue.drop_datagram(elapsed_ms(&started) >= datagram.expires_ms, true)
                        }
                    }
                    // Keep the reservation alive until the socket operation ends.
                    drop(datagram);
                    // A ready full-table dump must not monopolise a runtime worker.
                    tokio::task::yield_now().await;
                    continue;
                }
                Ok(None) => {}
                Err(_) => {
                    queue.drop_datagram(false, false);
                    continue;
                }
            }
        }
        let wake_ms = scheduler.next_wake_ms();
        tokio::select! {
            _ = stop.changed() => return,
            item = receive.recv() => {
                let Some(item) = item else { return; };
                let now_ms = elapsed_ms(&started);
                if now_ms >= item.expires_ms {
                    queue.expired_batches(1);
                } else {
                    scheduler.enqueue(item, now_ms);
                }
            }
            _ = sleep_until_ms(&started, wake_ms), if wake_ms.is_some() => {}
        }
    }
}

async fn sleep_until_ms(started: &Instant, wake_ms: Option<u64>) {
    let Some(wake_ms) = wake_ms else {
        std::future::pending::<()>().await;
        return;
    };
    let delay = wake_ms.saturating_sub(elapsed_ms(started));
    tokio::time::sleep(Duration::from_millis(delay)).await;
}

fn output_seed(socket: &InterfaceSocket) -> u64 {
    let mut value = 0xcbf2_9ce4_8422_2325u64 ^ u64::from(socket.index);
    for byte in socket.name.as_bytes() {
        value = (value ^ u64::from(*byte)).wrapping_mul(0x100_0000_01b3);
    }
    value
}

fn spawn_exporter(
    exporter: Arc<dyn RouteExporter>,
    mut snapshots: watch::Receiver<RouteSnapshot>,
    mut shutdown: watch::Receiver<bool>,
) {
    tokio::spawn(async move {
        let mut last_generation = None;
        loop {
            let snapshot = snapshots.borrow_and_update().clone();
            if last_generation != Some(snapshot.generation) {
                if let Err(error) = exporter.reconcile(snapshot.clone()).await {
                    warn!(%error, generation = snapshot.generation, "route export failed");
                } else {
                    last_generation = Some(snapshot.generation);
                }
            }
            tokio::select! {
                changed = snapshots.changed() => if changed.is_err() { return; },
                changed = shutdown.changed() => if changed.is_err() || *shutdown.borrow() { return; },
                _ = tokio::time::sleep(Duration::from_secs(2)) => {},
            }
        }
    });
}

async fn run_loop(runtime: Runtime) -> Result<(), RouterError> {
    let Runtime {
        router_id,
        interfaces,
        origins,
        mut sockets,
        mut outbound,
        output_counters,
        mut interface_stops,
        exporter,
        export_updates,
        mut commands,
        mut received,
        received_tx,
        mut shutdown,
        route_updates,
        metric,
        metric_algebra,
        route_selection,
        limits,
        sequence_number,
        sequence_store,
        started,
    } = runtime;
    let now = || elapsed_ms(&started);
    let default_policy = InterfacePolicy {
        metric: Arc::clone(&metric),
        hello_interval_cs: 400,
        update_interval_cs: 1600,
        split_horizon: true,
    };
    let mut engine = Engine::new(EngineConfig {
        limits,
        router_id,
        metric: Arc::clone(&metric),
        metric_algebra,
        sequence_number,
        hello_interval_cs: 400,
        update_interval_cs: 1600,
        route_selection,
    });
    let initial = RouteSnapshot::default();
    export_updates.send_replace(initial.clone());
    route_updates.send_replace(initial);
    let mut interface_policies = HashMap::new();
    for (interface, configured_policy) in &interfaces {
        let policy = configured_policy
            .clone()
            .unwrap_or_else(|| default_policy.clone());
        interface_policies.insert(interface.clone(), policy.clone());
        apply_actions(
            &outbound,
            &export_updates,
            engine.handle(Event::InterfaceUpWithPolicy {
                interface: interface.clone(),
                local_addresses: sockets
                    .get(interface)
                    .into_iter()
                    .flat_map(|socket| socket.local_addresses.iter().copied())
                    .map(IpAddr::V6)
                    .collect(),
                policy,
                now_ms: now(),
            }),
        );
    }
    for (key, metric) in origins {
        apply_actions(
            &outbound,
            &export_updates,
            engine.handle(Event::Originate {
                key,
                metric,
                now_ms: now(),
            }),
        );
    }
    let mut last_rejections = (0, 0, 0);
    let mut next_limit_log_ms = 0;
    let mut ticker = tokio::time::interval(Duration::from_millis(100));
    let mut status = RouterStatus {
        sequence_number: engine.sequence_number(),
        metric: effective_metric_name(&interface_policies, &metric),
        interfaces: sorted_interface_names(&sockets),
        interface_details: interface_status(&sockets, &interface_policies, &default_policy),
        ..RouterStatus::default()
    };
    loop {
        tokio::select! {
            changed = shutdown.changed() => if changed.is_err() || *shutdown.borrow() {
                // Babel updates outlive an abruptly disappearing speaker until
                // their advertised interval expires.  Retract our origins while
                // the interface sockets are still open so neighbours can remove
                // them immediately during an orderly shutdown/restart.  Repeat
                // the datagrams because UDP provides no delivery acknowledgement
                // and the process cannot rely on a later periodic update.
                let actions = engine.handle(Event::ReplaceOrigins {
                    origins: BTreeMap::new(),
                    now_ms: now(),
                });
                let retractions = actions.iter()
                    .filter(|action| matches!(action, Action::Send { .. }))
                    .cloned()
                    .collect::<Vec<_>>();
                apply_actions(&outbound, &export_updates, actions);
                for _ in 0..2 {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    apply_actions(&outbound, &export_updates, retractions.clone());
                }
                let empty = RouteSnapshot {
                    generation: status.route_generation.wrapping_add(1),
                    routes: vec![],
                    unreachable: vec![],
                };
                checkpoint_sequence_number(&sequence_store, engine.sequence_number()).await;
                if let Err(error) = exporter.shutdown(empty.clone()).await { warn!(%error, "final route export cleanup failed"); }
                route_updates.send_replace(empty);
                return Ok(());
            },
            _ = ticker.tick() => {
                for (interface, queue) in &outbound { queue.log_losses(interface); }
                let resources = engine.resource_status();
                let rejections = (resources.rejected_neighbors, resources.rejected_candidates_global, resources.rejected_candidates_per_neighbor);
                if rejections != last_rejections && now() >= next_limit_log_ms {
                    warn!(rejected_neighbors = rejections.0, rejected_candidates_global = rejections.1, rejected_candidates_per_neighbor = rejections.2, "Babel admission limits rejected new state");
                    last_rejections = rejections;
                    next_limit_log_ms = now().saturating_add(30_000);
                }
                apply_actions_with_status(&outbound, &export_updates, &route_updates, &mut status, engine.handle(Event::Tick { now_ms: now() }));
                status.neighbours = engine.neighbour_count();
                status.neighbour_details = engine.neighbour_status(now());
                update_output_status(&mut status, &output_counters);
            },
            Some(item) = received.recv() => match item {
                Received::Packet { _permit, interface, index, source, bytes, now_ms } => {
                    if sockets
                        .get(&interface)
                        .is_none_or(|socket| socket.index != index)
                    {
                        continue;
                    }
                    match decode_packet(&bytes, DecodeContext { source }) {
                        Ok(packet) => {
                            apply_actions_with_status(&outbound, &export_updates, &route_updates, &mut status, engine.handle(Event::PacketReceived { interface, source, packet, now_ms }));
                            status.neighbours = engine.neighbour_count();
                            status.neighbour_details = engine.neighbour_status(now());
                        },
                        Err(error) => debug!(%error, "ignored invalid Babel packet"),
                    }
                }
                Received::Failed { interface, index, error } => {
                    let is_current = sockets.get(&interface).is_some_and(|socket| socket.index == index);
                    if is_current {
                        warn!(%interface, index, %error, "detaching failed Babel interface");
                        sockets.remove(&interface);
                        outbound.remove(&interface);
                        if let Some(stop) = interface_stops.remove(&interface) {
                            let _ = stop.send(true);
                        }
                        interface_policies.remove(&interface);
                        apply_actions_with_status(&outbound, &export_updates, &route_updates, &mut status, engine.handle(Event::InterfaceDown { interface, now_ms: now() }));
                        status.interfaces = sorted_interface_names(&sockets);
                        status.metric = effective_metric_name(&interface_policies, &metric);
                        status.interface_details = interface_status(&sockets, &interface_policies, &default_policy);
                        status.neighbours = engine.neighbour_count();
                        status.neighbour_details = engine.neighbour_status(now());
                    }
                }
            },
            Some(command) = commands.recv() => match command {
                Command::ReplaceOrigins(origins, reply) => {
                    apply_actions_with_status(
                        &outbound,
                        &export_updates,
                        &route_updates,
                        &mut status,
                        engine.handle(Event::ReplaceOrigins { origins, now_ms: now() }),
                    );
                    let _ = reply.send(Ok(()));
                },
                Command::Originate(key, metric) => {
                    apply_actions_with_status(&outbound, &export_updates, &route_updates, &mut status, engine.handle(Event::Originate { key, metric, now_ms: now() }))
                },
                Command::Withdraw(key) => {
                    apply_actions_with_status(&outbound, &export_updates, &route_updates, &mut status, engine.handle(Event::Withdraw { key, now_ms: now() }))
                },
                Command::AddInterface(interface, configured_policy, reply) => {
                    let result = if sockets.contains_key(&interface) {
                        Ok(())
                    } else {
                        match InterfaceSocket::open(&interface) {
                            Ok(socket) => {
                                let policy = configured_policy.unwrap_or_else(|| default_policy.clone());
                                let socket = Arc::new(socket);
                                let (stop, stop_rx) = watch::channel(false);
                                spawn_receiver(socket.clone(), received_tx.clone(), shutdown.clone(), stop_rx.clone(), Arc::clone(&started));
                                outbound.insert(interface.clone(), spawn_sender(
                                    socket.clone(),
                                    stop.subscribe(),
                                    Arc::clone(&started),
                                    Arc::clone(&output_counters),
                                ));
                                sockets.insert(interface.clone(), socket);
                                interface_policies.insert(interface.clone(), policy.clone());
                                interface_stops.insert(interface.clone(), stop);
                                let local_addresses = sockets
                                    .get(&interface)
                                    .into_iter()
                                    .flat_map(|socket| socket.local_addresses.iter().copied())
                                    .map(IpAddr::V6)
                                    .collect();
                                apply_actions_with_status(&outbound, &export_updates, &route_updates, &mut status, engine.handle(Event::InterfaceUpWithPolicy { interface, local_addresses, policy, now_ms: now() }));
                                status.interfaces = sorted_interface_names(&sockets);
                                status.metric = effective_metric_name(&interface_policies, &metric);
                                status.interface_details = interface_status(&sockets, &interface_policies, &default_policy);
                                Ok(())
                            }
                            Err(source) => Err(RouterError::OpenInterface { interface, source }),
                        }
                    };
                    let _ = reply.send(result);
                }
                Command::UpdateInterfacePolicy(interface, policy, reset_metric, reply) => {
                    let result = if sockets.contains_key(&interface) {
                        apply_actions_with_status(
                            &outbound,
                            &export_updates,
                                            &route_updates,
                            &mut status,
                            engine.handle(Event::InterfacePolicyChanged {
                                interface: interface.clone(),
                                policy: policy.clone(),
                                reset_metric,
                                now_ms: now(),
                            }),
                        );
                        interface_policies.insert(interface, policy);
                        status.metric = effective_metric_name(&interface_policies, &metric);
                        status.interface_details = interface_status(&sockets, &interface_policies, &default_policy);
                        Ok(())
                    } else {
                        Err(RouterError::InterfaceNotFound(interface))
                    };
                    let _ = reply.send(result);
                }
                Command::RemoveInterface(interface, reply) => {
                    let result = if sockets.remove(&interface).is_some() {
                        outbound.remove(&interface);
                        if let Some(stop) = interface_stops.remove(&interface) {
                            let _ = stop.send(true);
                        }
                        interface_policies.remove(&interface);
                        apply_actions_with_status(&outbound, &export_updates, &route_updates, &mut status, engine.handle(Event::InterfaceDown { interface, now_ms: now() }));
                        status.interfaces = sorted_interface_names(&sockets);
                        status.metric = effective_metric_name(&interface_policies, &metric);
                        status.interface_details = interface_status(&sockets, &interface_policies, &default_policy);
                        Ok(())
                    } else {
                        Err(RouterError::InterfaceNotFound(interface))
                    };
                    let _ = reply.send(result);
                }
                Command::Status(reply) => {
                    status.neighbours = engine.neighbour_count();
                    status.neighbour_details = engine.neighbour_status(now());
                    status.interface_details = interface_status(&sockets, &interface_policies, &default_policy);
                    for interface in &mut status.interface_details {
                        if let Some(queue) = outbound.get(&interface.name) {
                            interface.output = queue.status();
                        }
                    }
                    update_output_status(&mut status, &output_counters);
                    status.resources = engine.resource_status();
                    status.sequence_number = engine.sequence_number();
                    let _ = reply.send(status.clone());
                }
            }
        }
    }
}

fn sorted_interface_names(sockets: &HashMap<String, Arc<InterfaceSocket>>) -> Vec<String> {
    let mut names: Vec<_> = sockets.keys().cloned().collect();
    names.sort();
    names
}

fn effective_metric_name(
    policies: &HashMap<String, InterfacePolicy>,
    fallback: &Arc<dyn MetricProfile>,
) -> String {
    let mut names: Vec<_> = policies
        .values()
        .map(|policy| policy.metric.name())
        .collect();
    names.sort();
    names.dedup();
    match names.as_slice() {
        [] => fallback.name(),
        [name] => name.clone(),
        _ => "per-interface".into(),
    }
}

fn interface_status(
    sockets: &HashMap<String, Arc<InterfaceSocket>>,
    policies: &HashMap<String, InterfacePolicy>,
    fallback: &InterfacePolicy,
) -> Vec<RouterInterfaceStatus> {
    let mut result: Vec<_> = sockets
        .values()
        .map(|socket| {
            let mtu = socket.current_mtu().unwrap_or(socket.mtu);
            let policy = policies.get(&socket.name).unwrap_or(fallback);
            RouterInterfaceStatus {
                name: socket.name.clone(),
                index: socket.index,
                local_addresses: socket.local_addresses.clone(),
                mtu,
                udp_payload_budget: payload_budget_for_mtu(mtu).unwrap_or_default(),
                metric: policy.metric.name(),
                hello_interval_ms: u64::from(policy.hello_interval_cs) * 10,
                update_interval_ms: u64::from(policy.update_interval_cs) * 10,
                split_horizon: policy.split_horizon,
                output: OutputStatus::default(),
            }
        })
        .collect();
    result.sort_by(|left, right| left.name.cmp(&right.name));
    result
}

fn update_output_status(status: &mut RouterStatus, counters: &OutputCounters) {
    status.dropped_outbound_datagrams = counters.dropped.load(Ordering::Relaxed);
    status.missed_outbound_deadlines = counters.missed_deadlines.load(Ordering::Relaxed);
}

async fn checkpoint_sequence_number(store: &Arc<dyn SequenceStore>, sequence_number: u16) {
    match tokio::time::timeout(SEQUENCE_CHECKPOINT_TIMEOUT, store.persist(sequence_number)).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => warn!(%error, "final Babel sequence checkpoint failed"),
        Err(_) => warn!("final Babel sequence checkpoint timed out"),
    }
}

fn apply_actions(
    outbound: &HashMap<String, OutputQueue>,
    export_updates: &watch::Sender<RouteSnapshot>,
    actions: Vec<Action>,
) {
    let mut ignored = RouterStatus::default();
    let (updates, _) = watch::channel(RouteSnapshot::default());
    apply_actions_with_status(outbound, export_updates, &updates, &mut ignored, actions)
}

fn apply_actions_with_status(
    outbound: &HashMap<String, OutputQueue>,
    export_updates: &watch::Sender<RouteSnapshot>,
    route_updates: &watch::Sender<RouteSnapshot>,
    status: &mut RouterStatus,
    actions: Vec<Action>,
) {
    for action in batch_send_actions(actions) {
        match action {
            Action::Send {
                interface,
                destination: IpAddr::V6(destination),
                packet,
                timing,
            } => {
                let Some(sender) = outbound.get(&interface) else {
                    continue;
                };
                sender.submit(OutboundIntent {
                    destination,
                    packet,
                    timing,
                });
            }
            Action::Send { .. } => {}
            Action::RoutesChanged {
                generation,
                routes,
                unreachable,
            } => {
                status.route_generation = generation;
                status.selected_routes = routes.len();
                let snapshot = RouteSnapshot {
                    generation,
                    routes,
                    unreachable,
                };
                route_updates.send_replace(snapshot.clone());
                export_updates.send_replace(snapshot);
            }
            Action::SequenceNumberChanged(value) => status.sequence_number = value,
        }
    }
}

/// Batch same-destination, same-timing Sends within one uninterrupted run.
/// Preserve TLV order and sequence/snapshot action boundaries. This avoids waiting
/// for one channel slot per prefix during large triggered announcements.
fn batch_send_actions(actions: Vec<Action>) -> Vec<Action> {
    let mut result: Vec<Action> = Vec::new();
    let mut positions = HashMap::new();
    for action in actions {
        if let Action::Send {
            interface,
            destination,
            packet,
            timing,
        } = action
        {
            let key = (
                interface.clone(),
                destination,
                timing.deadline_ms,
                timing.max_jitter_ms,
            );
            if let Some(&index) = positions.get(&key) {
                if let Action::Send {
                    packet: previous, ..
                } = &mut result[index]
                {
                    previous.tlvs.extend(packet.tlvs);
                }
            } else {
                positions.insert(key, result.len());
                result.push(Action::Send {
                    interface,
                    destination,
                    packet,
                    timing,
                });
            }
        } else {
            positions.clear();
            result.push(action);
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use async_trait::async_trait;

    use super::*;

    fn send_action(interface: &str, nonce: u16, timing: babel_proto::SendTiming) -> Action {
        Action::Send {
            interface: interface.into(),
            destination: "ff02::1:6".parse().unwrap(),
            packet: babel_proto::OutboundPacket {
                tlvs: vec![babel_proto::OutboundTlv::Ack { nonce }],
            },
            timing,
        }
    }

    #[derive(Default)]
    struct TestTransport {
        blocked: std::sync::atomic::AtomicBool,
        missing_mtu: std::sync::atomic::AtomicBool,
        attempts: AtomicU64,
        sent: std::sync::Mutex<Vec<Vec<u8>>>,
        wake: tokio::sync::Notify,
    }

    #[async_trait]
    impl OutputTransport for TestTransport {
        fn payload_budget(&self) -> std::io::Result<usize> {
            if self.missing_mtu.load(Ordering::Relaxed) {
                Err(std::io::Error::other("injected MTU failure"))
            } else {
                Ok(1232)
            }
        }

        async fn send(&self, bytes: &[u8], _: Ipv6Addr) -> std::io::Result<usize> {
            self.attempts.fetch_add(1, Ordering::Relaxed);
            loop {
                let wake = self.wake.notified();
                if !self.blocked.load(Ordering::Relaxed) {
                    break;
                }
                wake.await;
            }
            self.sent.lock().unwrap().push(bytes.to_vec());
            Ok(bytes.len())
        }
    }

    fn queue_test_ack(queue: &OutputQueue, now: u64) {
        queue.submit(OutboundIntent {
            destination: Ipv6Addr::LOCALHOST,
            packet: babel_proto::OutboundPacket {
                tlvs: vec![babel_proto::OutboundTlv::Ack { nonce: 1 }],
            },
            timing: babel_proto::SendTiming::immediate(now),
        });
    }

    async fn settle_tasks() {
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
    }

    #[tokio::test(start_paused = true)]
    async fn stalled_send_times_out_recovers_and_cancels_on_interface_removal() {
        let started = Arc::new(Instant::now());
        let (queue, receive) = OutputQueue::new(
            2,
            OUTPUT_BUDGET_BYTES,
            Arc::new(OutputCounters::default()),
            started.clone(),
        );
        let transport = Arc::new(TestTransport::default());
        transport.blocked.store(true, Ordering::Relaxed);
        let (stop, stop_rx) = watch::channel(false);
        let task = tokio::spawn(run_sender(
            transport.clone(),
            stop_rx,
            started.clone(),
            queue.clone(),
            receive,
            1,
        ));
        queue_test_ack(&queue, 0);
        settle_tasks().await;
        assert_eq!(transport.attempts.load(Ordering::Relaxed), 1);
        assert!(queue.status().used_bytes > 0);
        // Simulate writability returning while the runtime is stalled. Both
        // the send and its timer will be ready at the next poll: timeout wins.
        transport.blocked.store(false, Ordering::Relaxed);
        transport.wake.notify_waiters();
        tokio::time::advance(Duration::from_millis(SEND_TIMEOUT_MS + 1)).await;
        settle_tasks().await;
        assert!(transport.sent.lock().unwrap().is_empty());
        assert_eq!(queue.status().send_timeouts, 1);
        assert_eq!(queue.status().used_bytes, 0);
        transport.blocked.store(false, Ordering::Relaxed);
        queue_test_ack(&queue, elapsed_ms(&started));
        settle_tasks().await;
        assert_eq!(transport.sent.lock().unwrap().len(), 1);
        assert_eq!(queue.status().used_bytes, 0);
        transport.blocked.store(true, Ordering::Relaxed);
        queue_test_ack(&queue, elapsed_ms(&started));
        settle_tasks().await;
        assert!(queue.status().used_bytes > 0);
        stop.send(true).unwrap();
        tokio::time::timeout(Duration::from_millis(1), task)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(queue.status().used_bytes, 0);
    }

    #[tokio::test(start_paused = true)]
    async fn failed_mtu_lookup_expires_pending_and_channel_work_then_recovers() {
        let started = Arc::new(Instant::now());
        let (queue, receive) = OutputQueue::new(
            2,
            OUTPUT_BUDGET_BYTES,
            Arc::new(OutputCounters::default()),
            started.clone(),
        );
        let transport = Arc::new(TestTransport::default());
        transport.missing_mtu.store(true, Ordering::Relaxed);
        let (stop, stop_rx) = watch::channel(false);
        let task = tokio::spawn(run_sender(
            transport.clone(),
            stop_rx,
            started.clone(),
            queue.clone(),
            receive,
            1,
        ));
        queue_test_ack(&queue, 0);
        settle_tasks().await;
        queue_test_ack(&queue, 0);
        tokio::time::advance(Duration::from_millis(1_100)).await;
        settle_tasks().await;
        assert_eq!(queue.status().expired_batches, 2);
        assert_eq!(queue.status().used_bytes, 0);
        assert_eq!(transport.attempts.load(Ordering::Relaxed), 0);
        transport.missing_mtu.store(false, Ordering::Relaxed);
        queue_test_ack(&queue, elapsed_ms(&started));
        settle_tasks().await;
        assert_eq!(transport.sent.lock().unwrap().len(), 1);
        stop.send(true).unwrap();
        task.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn full_bad_interface_does_not_stall_engine_status_or_healthy_output() {
        let started = Arc::new(Instant::now());
        let totals = Arc::new(OutputCounters::default());
        let (bad, bad_receive) = OutputQueue::new(2, 64 * 1024, totals.clone(), started.clone());
        let (good, good_receive) =
            OutputQueue::new(256, OUTPUT_BUDGET_BYTES, totals.clone(), started.clone());
        let bad_transport = Arc::new(TestTransport::default());
        bad_transport.blocked.store(true, Ordering::Relaxed);
        let good_transport = Arc::new(TestTransport::default());
        let (bad_stop, bad_stop_rx) = watch::channel(false);
        let (good_stop, good_stop_rx) = watch::channel(false);
        let bad_task = tokio::spawn(run_sender(
            bad_transport.clone(),
            bad_stop_rx,
            started.clone(),
            bad.clone(),
            bad_receive,
            1,
        ));
        let good_task = tokio::spawn(run_sender(
            good_transport.clone(),
            good_stop_rx,
            started.clone(),
            good.clone(),
            good_receive,
            2,
        ));
        let (commands_tx, commands) = mpsc::channel(64);
        let (received_tx, received) = mpsc::channel(256);
        let (shutdown_tx, shutdown) = watch::channel(false);
        let (route_updates, routes) = watch::channel(RouteSnapshot::default());
        let (export_updates, _) = watch::channel(RouteSnapshot::default());
        let handle = RouterHandle {
            commands: commands_tx,
            shutdown: shutdown_tx,
            routes,
        };
        // No privileged sockets: the real engine and command loop run against
        // independently controlled sender transports, on one runtime thread.
        let router = tokio::spawn(run_loop(Runtime {
            router_id: RouterId::new([1; 8]).unwrap(),
            interfaces: vec![("bad".into(), None), ("good".into(), None)],
            origins: vec![],
            sockets: HashMap::new(),
            outbound: HashMap::from([("bad".into(), bad.clone()), ("good".into(), good.clone())]),
            output_counters: totals,
            interface_stops: HashMap::from([("bad".into(), bad_stop), ("good".into(), good_stop)]),
            exporter: Arc::new(MemoryExporter::default()),
            export_updates,
            commands,
            received,
            received_tx,
            shutdown,
            route_updates,
            metric: Arc::new(WiredMetric::default()),
            metric_algebra: Arc::new(AdditiveMetric),
            route_selection: RouteSelectionConfig::default(),
            limits: ResourceLimits::default(),
            sequence_number: 0,
            sequence_store: Arc::new(NoopSequenceStore),
            started: started.clone(),
        }));
        let mut keys = Vec::new();
        for i in 0..32 {
            let key = RouteKey::new(format!("fd00::{i:x}/128").parse().unwrap(), None).unwrap();
            keys.push(key);
            handle.originate(key, 0).await.unwrap();
            settle_tasks().await;
        }
        tokio::time::timeout(Duration::from_millis(10), handle.status())
            .await
            .unwrap()
            .unwrap();
        // Advance beyond triggered jitter but remain inside the first send timeout.
        tokio::time::advance(Duration::from_millis(99)).await;
        settle_tasks().await;
        assert!(bad.status().rejected_batches > 0);
        assert!(bad.status().used_bytes <= bad.status().budget_bytes);
        assert_eq!(bad.status().send_timeouts, 0);
        assert!(!good_transport.sent.lock().unwrap().is_empty());
        assert_eq!(good.status().rejected_batches, 0);
        tokio::time::timeout(
            Duration::from_millis(10),
            handle.replace_origins(vec![(keys[31], 0)]),
        )
        .await
        .unwrap()
        .unwrap();
        tokio::time::timeout(Duration::from_millis(10), handle.status())
            .await
            .unwrap()
            .unwrap();
        // Let old output expire, then recover the transport and wait for the
        // ordinary protocol timer to advertise current state on both links.
        tokio::time::advance(Duration::from_secs(2)).await;
        settle_tasks().await;
        assert!(bad.status().expired_datagrams > 0);
        assert!(bad.status().used_bytes <= bad.status().budget_bytes);
        assert!(bad_transport.sent.lock().unwrap().is_empty());
        assert!(good_transport.sent.lock().unwrap().iter().any(|bytes| {
            decode_packet(
                bytes,
                DecodeContext {
                    source: "fe80::1".parse().unwrap(),
                },
            )
            .unwrap()
            .tlvs
            .into_iter()
            .any(|tlv| {
                matches!(tlv,
                    babel_proto::Tlv::Update(update)
                        if update.key == Some(keys[0]) && update.metric == babel_proto::INFINITY)
            })
        }));
        bad_transport.blocked.store(false, Ordering::Relaxed);
        bad_transport.wake.notify_waiters();
        tokio::time::advance(Duration::from_secs(16)).await;
        settle_tasks().await;
        for transport in [&bad_transport, &good_transport] {
            assert!(transport.sent.lock().unwrap().iter().any(|bytes| {
                decode_packet(
                    bytes,
                    DecodeContext {
                        source: "fe80::1".parse().unwrap(),
                    },
                )
                .unwrap()
                .tlvs
                .into_iter()
                .any(|tlv| {
                    matches!(tlv,
                        babel_proto::Tlv::Update(update)
                            if update.key == Some(keys[31]) && update.metric == 0)
                })
            }));
        }
        handle.shutdown();
        tokio::time::timeout(Duration::from_secs(1), router)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        bad_task.await.unwrap();
        good_task.await.unwrap();
        assert_eq!(bad.status().used_bytes, 0);
        assert_eq!(good.status().used_bytes, 0);
    }

    #[tokio::test]
    async fn large_output_batch_progresses_with_one_available_channel_slot() {
        let (send, mut receive) = OutputQueue::new(
            1,
            OUTPUT_BUDGET_BYTES,
            Arc::new(OutputCounters::default()),
            Arc::new(Instant::now()),
        );
        let outbound = HashMap::from([("eth0".into(), send)]);
        let (exports, _) = watch::channel(RouteSnapshot::default());
        let timing = babel_proto::SendTiming::urgent(100);
        let actions = (0..1024).map(|n| send_action("eth0", n, timing)).collect();
        // No consumer runs until apply_actions returns. Per-prefix sends would
        // deadlock here after filling the single channel slot.
        apply_actions(&outbound, &exports, actions);
        let intent = receive.recv().await.unwrap().intent;
        assert_eq!(intent.timing, timing);
        assert_eq!(
            intent.packet.tlvs,
            (0..1024)
                .map(|nonce| babel_proto::OutboundTlv::Ack { nonce })
                .collect::<Vec<_>>()
        );
        assert!(receive.try_recv().is_err());
    }

    #[test]
    fn output_batch_preserves_destinations_deadlines_and_sequence_order() {
        let urgent = babel_proto::SendTiming::urgent(100);
        let later = babel_proto::SendTiming::urgent(200);
        let actions = batch_send_actions(vec![
            send_action("eth0", 1, urgent),
            send_action("eth1", 2, urgent),
            send_action("eth0", 3, urgent),
            Action::SequenceNumberChanged(4),
            send_action("eth0", 5, urgent),
            send_action("eth0", 6, later),
        ]);
        assert_eq!(actions.len(), 5);
        assert!(matches!(&actions[0], Action::Send { packet, timing, .. }
            if packet.tlvs == vec![babel_proto::OutboundTlv::Ack { nonce: 1 }, babel_proto::OutboundTlv::Ack { nonce: 3 }] && *timing == urgent));
        assert!(matches!(&actions[1], Action::Send { interface, .. } if interface == "eth1"));
        assert_eq!(actions[2], Action::SequenceNumberChanged(4));
        assert_eq!(actions[3], send_action("eth0", 5, urgent));
        assert_eq!(actions[4], send_action("eth0", 6, later));
    }

    #[test]
    fn rfc8966_transport_accepts_only_link_local_port_6696_sources() {
        let local = ["fe80::1".parse().unwrap()];
        assert!(valid_babel_source(
            &"[fe80::2]:6696".parse().unwrap(),
            &local
        ));
        assert!(!valid_babel_source(
            &"[fe80::2]:1234".parse().unwrap(),
            &local
        ));
        assert!(!valid_babel_source(
            &"[2001:db8::2]:6696".parse().unwrap(),
            &local
        ));
        assert!(!valid_babel_source(
            &"[fe80::1]:6696".parse().unwrap(),
            &local
        ));
    }

    #[derive(Clone, Copy)]
    enum CheckpointOutcome {
        Success,
        Failure,
        Pending,
    }

    #[derive(Clone)]
    struct TestSequenceStore {
        saved: Arc<std::sync::Mutex<Vec<u16>>>,
        outcome: CheckpointOutcome,
    }

    impl TestSequenceStore {
        fn new(outcome: CheckpointOutcome) -> Self {
            Self {
                saved: Arc::new(std::sync::Mutex::new(Vec::new())),
                outcome,
            }
        }
    }

    #[async_trait]
    impl SequenceStore for TestSequenceStore {
        async fn persist(
            &self,
            sequence_number: u16,
        ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
            self.saved.lock().unwrap().push(sequence_number);
            match self.outcome {
                CheckpointOutcome::Success => Ok(()),
                CheckpointOutcome::Failure => Err(std::io::Error::other("disk unavailable").into()),
                CheckpointOutcome::Pending => std::future::pending().await,
            }
        }
    }

    #[derive(Clone, Default)]
    struct CleanupExporter {
        cleanups: Arc<AtomicU64>,
    }

    #[async_trait]
    impl RouteExporter for CleanupExporter {
        async fn reconcile(
            &self,
            _snapshot: RouteSnapshot,
        ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
            Ok(())
        }

        async fn shutdown(
            &self,
            snapshot: RouteSnapshot,
        ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
            assert!(snapshot.routes.is_empty());
            assert!(snapshot.unreachable.is_empty());
            self.cleanups.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    #[tokio::test(start_paused = true)]
    async fn sequence_changes_stay_in_memory_and_shutdown_checkpoints_all_origins_once() {
        let store = TestSequenceStore::new(CheckpointOutcome::Success);
        let exporter = CleanupExporter::default();
        let keys: Vec<_> = (1..=3)
            .map(|i| RouteKey::new(format!("fd00::{i}/128").parse().unwrap(), None).unwrap())
            .collect();
        let mut builder = BabelRouterBuilder::default()
            .router_id(RouterId::new([1; 8]).unwrap())
            .sequence_number(u16::MAX - 1)
            .sequence_store(store.clone())
            .exporter(exporter.clone());
        for key in &keys {
            builder = builder.originate(*key, 0);
        }
        let router = builder.build().await.unwrap();
        let handle = router.handle();
        assert_eq!(handle.status().await.unwrap().sequence_number, u16::MAX - 1);
        assert!(store.saved.lock().unwrap().is_empty());

        // Changing all existing origin metrics causes one sequence increment.
        // A status round trip confirms the change has been applied without I/O.
        handle
            .replace_origins(keys.iter().map(|key| (*key, 1)).collect())
            .await
            .unwrap();
        assert_eq!(handle.status().await.unwrap().sequence_number, u16::MAX);
        assert!(store.saved.lock().unwrap().is_empty());
        handle.shutdown();
        tokio::time::timeout(Duration::from_secs(1), router.run())
            .await
            .unwrap()
            .unwrap();

        // The final batch withdrawal increments once for all three origins,
        // wraps normally, and only that final sequence is checkpointed.
        assert_eq!(*store.saved.lock().unwrap(), vec![0]);
        assert_eq!(exporter.cleanups.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn failed_sequence_checkpoint_does_not_fail_or_skip_shutdown_cleanup() {
        let store = TestSequenceStore::new(CheckpointOutcome::Failure);
        let exporter = CleanupExporter::default();
        let router = BabelRouterBuilder::default()
            .router_id(RouterId::new([1; 8]).unwrap())
            .sequence_number(17)
            .sequence_store(store.clone())
            .exporter(exporter.clone())
            .build()
            .await
            .unwrap();
        let handle = router.handle();
        handle.status().await.unwrap();
        handle.shutdown();
        tokio::time::timeout(Duration::from_secs(1), router.run())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(*store.saved.lock().unwrap(), vec![17]);
        assert_eq!(exporter.cleanups.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn pending_sequence_checkpoint_times_out_and_continues_shutdown_cleanup() {
        let store = TestSequenceStore::new(CheckpointOutcome::Pending);
        let exporter = CleanupExporter::default();
        let router = BabelRouterBuilder::default()
            .router_id(RouterId::new([1; 8]).unwrap())
            .sequence_number(23)
            .sequence_store(store.clone())
            .exporter(exporter.clone())
            .build()
            .await
            .unwrap();
        let handle = router.handle();
        handle.status().await.unwrap();
        let started = Instant::now();
        handle.shutdown();
        tokio::time::timeout(Duration::from_secs(2), router.run())
            .await
            .unwrap()
            .unwrap();
        assert!(started.elapsed() >= SEQUENCE_CHECKPOINT_TIMEOUT);
        assert_eq!(*store.saved.lock().unwrap(), vec![23]);
        assert_eq!(exporter.cleanups.load(Ordering::SeqCst), 1);
    }

    #[derive(Clone, Default)]
    struct SlowExporter {
        generation: Arc<AtomicU64>,
    }

    #[async_trait]
    impl RouteExporter for SlowExporter {
        async fn reconcile(
            &self,
            snapshot: RouteSnapshot,
        ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
            tokio::time::sleep(Duration::from_millis(25)).await;
            self.generation.store(snapshot.generation, Ordering::SeqCst);
            Ok(())
        }
    }

    #[tokio::test]
    async fn slow_exporter_coalesces_to_the_latest_complete_snapshot() {
        let exporter = SlowExporter::default();
        let observed = exporter.generation.clone();
        let (snapshots, stream) = watch::channel(RouteSnapshot::default());
        let (shutdown, shutdown_stream) = watch::channel(false);
        spawn_exporter(Arc::new(exporter), stream, shutdown_stream);
        tokio::task::yield_now().await;
        for generation in 1..=20 {
            snapshots.send_replace(RouteSnapshot {
                generation,
                routes: Vec::new(),
                unreachable: Vec::new(),
            });
        }
        tokio::time::timeout(Duration::from_secs(1), async {
            while observed.load(Ordering::SeqCst) != 20 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        assert!(observed.load(Ordering::SeqCst) == 20);
        let _ = shutdown.send(true);
    }
}
