use std::collections::HashMap;
use std::net::{IpAddr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use babel_proto::{
    Action, AdditiveMetric, DecodeContext, Engine, EngineConfig, Event, MetricAlgebra,
    MetricProfile, NeighborStatus, RouteKey, RouteSelectionConfig, RouterId, WiredMetric,
    decode_packet, encode_packet,
};
use thiserror::Error;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tracing::{debug, warn};

use crate::export::{
    MemoryExporter, NoopSequenceStore, RouteExporter, RouteSnapshot, SequenceStore,
};
use crate::transport::InterfaceSocket;

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
    #[error("router task stopped")]
    Stopped,
    #[error("router task failed: {0}")]
    Task(String),
    #[error("persist Babel sequence number: {0}")]
    SequenceStore(String),
}

#[derive(Clone, Debug, Default)]
pub struct RouterInterfaceStatus {
    pub name: String,
    pub index: u32,
    pub local_addresses: Vec<Ipv6Addr>,
}

#[derive(Clone, Debug, Default)]
pub struct RouterStatus {
    pub metric: String,
    pub interfaces: Vec<String>,
    pub interface_details: Vec<RouterInterfaceStatus>,
    pub neighbours: usize,
    pub neighbour_details: Vec<NeighborStatus>,
    pub route_generation: u64,
    pub selected_routes: usize,
}

pub type RouteStream = watch::Receiver<RouteSnapshot>;

enum Command {
    Originate(RouteKey, u16),
    Withdraw(RouteKey),
    AddInterface(String, oneshot::Sender<Result<(), RouterError>>),
    RemoveInterface(String, oneshot::Sender<Result<(), RouterError>>),
    Status(oneshot::Sender<RouterStatus>),
}

enum Received {
    Packet {
        interface: String,
        index: u32,
        source: IpAddr,
        bytes: Vec<u8>,
    },
    Failed {
        interface: String,
        index: u32,
        error: String,
    },
}

struct Runtime {
    router_id: RouterId,
    interfaces: Vec<String>,
    origins: Vec<(RouteKey, u16)>,
    sockets: HashMap<String, Arc<InterfaceSocket>>,
    interface_stops: HashMap<String, watch::Sender<bool>>,
    exporter: Arc<dyn RouteExporter>,
    commands: mpsc::Receiver<Command>,
    received: mpsc::Receiver<Received>,
    received_tx: mpsc::Sender<Received>,
    shutdown: watch::Receiver<bool>,
    route_updates: watch::Sender<RouteSnapshot>,
    metric: Arc<dyn MetricProfile>,
    metric_algebra: Arc<dyn MetricAlgebra>,
    route_selection: RouteSelectionConfig,
    sequence_number: u16,
    sequence_store: Arc<dyn SequenceStore>,
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

    pub async fn add_interface(&self, interface: impl Into<String>) -> Result<(), RouterError> {
        let (send, receive) = oneshot::channel();
        self.commands
            .send(Command::AddInterface(interface.into(), send))
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
    interfaces: Vec<String>,
    origins: Vec<(RouteKey, u16)>,
    exporter: Option<Arc<dyn RouteExporter>>,
    metric: Option<Arc<dyn MetricProfile>>,
    metric_algebra: Option<Arc<dyn MetricAlgebra>>,
    route_selection: Option<RouteSelectionConfig>,
    sequence_number: u16,
    sequence_store: Option<Arc<dyn SequenceStore>>,
}

impl BabelRouterBuilder {
    pub fn router_id(mut self, value: RouterId) -> Self {
        self.router_id = Some(value);
        self
    }
    pub fn interface(mut self, value: impl Into<String>) -> Self {
        self.interfaces.push(value.into());
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
    pub fn sequence_number(mut self, value: u16) -> Self {
        self.sequence_number = value;
        self
    }
    pub fn sequence_store(mut self, value: impl SequenceStore) -> Self {
        self.sequence_store = Some(Arc::new(value));
        self
    }

    pub async fn build(self) -> Result<BabelRouter, RouterError> {
        let router_id = self.router_id.ok_or(RouterError::MissingRouterId)?;
        let mut sockets = HashMap::new();
        for name in &self.interfaces {
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
        let mut interface_stops = HashMap::new();
        for (name, socket) in &sockets {
            let (stop, stop_rx) = watch::channel(false);
            interface_stops.insert(name.clone(), stop);
            spawn_receiver(
                Arc::clone(socket),
                received_tx.clone(),
                shutdown_rx.clone(),
                stop_rx,
            );
        }
        let task = tokio::spawn(run_loop(Runtime {
            router_id,
            interfaces: self.interfaces,
            origins: self.origins,
            sockets,
            interface_stops,
            exporter,
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
            sequence_number: self.sequence_number,
            sequence_store: self
                .sequence_store
                .unwrap_or_else(|| Arc::new(NoopSequenceStore)),
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

fn spawn_receiver(
    socket: Arc<InterfaceSocket>,
    received: mpsc::Sender<Received>,
    mut shutdown: watch::Receiver<bool>,
    mut stop: watch::Receiver<bool>,
) {
    tokio::spawn(async move {
        let mut buffer = vec![0u8; 65535];
        loop {
            tokio::select! {
                changed = shutdown.changed() => if changed.is_err() || *shutdown.borrow() { return; },
                changed = stop.changed() => if changed.is_err() || *stop.borrow() { return; },
                result = socket.socket.recv_from(&mut buffer) => match result {
                    Ok((length, SocketAddr::V6(source)))
                        if source.ip().is_unicast_link_local()
                            && !socket.local_addresses.contains(source.ip()) => {
                        let item = Received::Packet { interface: socket.name.clone(), index: socket.index, source: IpAddr::V6(*source.ip()), bytes: buffer[..length].to_vec() };
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

async fn run_loop(runtime: Runtime) -> Result<(), RouterError> {
    let Runtime {
        router_id,
        interfaces,
        origins,
        mut sockets,
        mut interface_stops,
        exporter,
        mut commands,
        mut received,
        received_tx,
        mut shutdown,
        route_updates,
        metric,
        metric_algebra,
        route_selection,
        sequence_number,
        sequence_store,
    } = runtime;
    let started = Instant::now();
    let now = || started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
    let mut engine = Engine::new(EngineConfig {
        router_id,
        metric,
        metric_algebra,
        sequence_number,
        hello_interval_cs: 400,
        update_interval_cs: 1600,
        route_selection,
    });
    let initial = RouteSnapshot::default();
    if let Err(error) = exporter.reconcile(initial.clone()).await {
        warn!(%error, "initial route export failed");
    }
    route_updates.send_replace(initial);
    for interface in &interfaces {
        apply_actions(
            &sockets,
            &exporter,
            &sequence_store,
            engine.handle(Event::InterfaceUp {
                interface: interface.clone(),
                local_addresses: sockets
                    .get(interface)
                    .into_iter()
                    .flat_map(|socket| socket.local_addresses.iter().copied())
                    .map(IpAddr::V6)
                    .collect(),
                now_ms: now(),
            }),
        )
        .await?;
    }
    let mut origin_keys = std::collections::HashSet::new();
    for (key, metric) in origins {
        origin_keys.insert(key);
        apply_actions(
            &sockets,
            &exporter,
            &sequence_store,
            engine.handle(Event::Originate {
                key,
                metric,
                now_ms: now(),
            }),
        )
        .await?;
    }
    let mut ticker = tokio::time::interval(Duration::from_millis(100));
    let mut status = RouterStatus {
        metric: engine.metric_name(),
        interfaces,
        interface_details: interface_status(&sockets),
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
                let mut retractions = Vec::new();
                for key in &origin_keys {
                    let actions = engine.handle(Event::Withdraw { key: *key, now_ms: now() });
                    retractions.extend(actions.iter().filter(|action| matches!(action, Action::Send { .. })).cloned());
                    apply_actions(&sockets, &exporter, &sequence_store, actions).await?;
                }
                for _ in 0..2 {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    apply_actions(&sockets, &exporter, &sequence_store, retractions.clone()).await?;
                }
                let empty = RouteSnapshot { generation: status.route_generation.wrapping_add(1), routes: vec![] };
                if let Err(error) = exporter.shutdown(empty.clone()).await { warn!(%error, "final route export cleanup failed"); }
                route_updates.send_replace(empty);
                return Ok(());
            },
            _ = ticker.tick() => {
                apply_actions_with_status(&sockets, &exporter, &sequence_store, &route_updates, &mut status, engine.handle(Event::Tick { now_ms: now() })).await?;
                status.neighbours = engine.neighbour_count();
                status.neighbour_details = engine.neighbour_status(now());
            },
            Some(item) = received.recv() => match item {
                Received::Packet { interface, index, source, bytes } => {
                    if sockets
                        .get(&interface)
                        .is_none_or(|socket| socket.index != index)
                    {
                        continue;
                    }
                    match decode_packet(&bytes, DecodeContext { source }) {
                        Ok(packet) => {
                            apply_actions_with_status(&sockets, &exporter, &sequence_store, &route_updates, &mut status, engine.handle(Event::PacketReceived { interface, source, packet, now_ms: now() })).await?;
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
                        if let Some(stop) = interface_stops.remove(&interface) {
                            let _ = stop.send(true);
                        }
                        apply_actions_with_status(&sockets, &exporter, &sequence_store, &route_updates, &mut status, engine.handle(Event::InterfaceDown { interface, now_ms: now() })).await?;
                        status.interfaces = sorted_interface_names(&sockets);
                        status.interface_details = interface_status(&sockets);
                        status.neighbours = engine.neighbour_count();
                        status.neighbour_details = engine.neighbour_status(now());
                    }
                }
            },
            Some(command) = commands.recv() => match command {
                Command::Originate(key, metric) => {
                    origin_keys.insert(key);
                    apply_actions_with_status(&sockets, &exporter, &sequence_store, &route_updates, &mut status, engine.handle(Event::Originate { key, metric, now_ms: now() })).await?
                },
                Command::Withdraw(key) => {
                    origin_keys.remove(&key);
                    apply_actions_with_status(&sockets, &exporter, &sequence_store, &route_updates, &mut status, engine.handle(Event::Withdraw { key, now_ms: now() })).await?
                },
                Command::AddInterface(interface, reply) => {
                    let result = if sockets.contains_key(&interface) {
                        Ok(())
                    } else {
                        match InterfaceSocket::open(&interface) {
                            Ok(socket) => {
                                let socket = Arc::new(socket);
                                let (stop, stop_rx) = watch::channel(false);
                                spawn_receiver(socket.clone(), received_tx.clone(), shutdown.clone(), stop_rx);
                                sockets.insert(interface.clone(), socket);
                                interface_stops.insert(interface.clone(), stop);
                                let local_addresses = sockets
                                    .get(&interface)
                                    .into_iter()
                                    .flat_map(|socket| socket.local_addresses.iter().copied())
                                    .map(IpAddr::V6)
                                    .collect();
                                apply_actions_with_status(&sockets, &exporter, &sequence_store, &route_updates, &mut status, engine.handle(Event::InterfaceUp { interface, local_addresses, now_ms: now() })).await?;
                                status.interfaces = sorted_interface_names(&sockets);
                                status.interface_details = interface_status(&sockets);
                                Ok(())
                            }
                            Err(source) => Err(RouterError::OpenInterface { interface, source }),
                        }
                    };
                    let _ = reply.send(result);
                }
                Command::RemoveInterface(interface, reply) => {
                    let result = if sockets.remove(&interface).is_some() {
                        if let Some(stop) = interface_stops.remove(&interface) {
                            let _ = stop.send(true);
                        }
                        apply_actions_with_status(&sockets, &exporter, &sequence_store, &route_updates, &mut status, engine.handle(Event::InterfaceDown { interface, now_ms: now() })).await?;
                        status.interfaces = sorted_interface_names(&sockets);
                        status.interface_details = interface_status(&sockets);
                        Ok(())
                    } else {
                        Err(RouterError::InterfaceNotFound(interface))
                    };
                    let _ = reply.send(result);
                }
                Command::Status(reply) => {
                    status.neighbours = engine.neighbour_count();
                    status.neighbour_details = engine.neighbour_status(now());
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

fn interface_status(sockets: &HashMap<String, Arc<InterfaceSocket>>) -> Vec<RouterInterfaceStatus> {
    let mut result: Vec<_> = sockets
        .values()
        .map(|socket| RouterInterfaceStatus {
            name: socket.name.clone(),
            index: socket.index,
            local_addresses: socket.local_addresses.clone(),
        })
        .collect();
    result.sort_by(|left, right| left.name.cmp(&right.name));
    result
}

async fn apply_actions(
    sockets: &HashMap<String, Arc<InterfaceSocket>>,
    exporter: &Arc<dyn RouteExporter>,
    sequence_store: &Arc<dyn SequenceStore>,
    actions: Vec<Action>,
) -> Result<(), RouterError> {
    let mut ignored = RouterStatus::default();
    let (updates, _) = watch::channel(RouteSnapshot::default());
    apply_actions_with_status(
        sockets,
        exporter,
        sequence_store,
        &updates,
        &mut ignored,
        actions,
    )
    .await
}

async fn apply_actions_with_status(
    sockets: &HashMap<String, Arc<InterfaceSocket>>,
    exporter: &Arc<dyn RouteExporter>,
    sequence_store: &Arc<dyn SequenceStore>,
    route_updates: &watch::Sender<RouteSnapshot>,
    status: &mut RouterStatus,
    actions: Vec<Action>,
) -> Result<(), RouterError> {
    for action in actions {
        match action {
            Action::Send {
                interface,
                destination: IpAddr::V6(destination),
                packet,
            } => {
                let Some(socket) = sockets.get(&interface) else {
                    continue;
                };
                match encode_packet(&packet) {
                    Ok(bytes) => {
                        if let Err(error) = socket
                            .socket
                            .send_to(&bytes, socket.destination(destination))
                            .await
                        {
                            warn!(%interface, %error, "Babel send failed");
                        }
                    }
                    Err(error) => warn!(%interface, %error, "Babel packet encode failed"),
                }
            }
            Action::Send { .. } => {}
            Action::RoutesChanged { generation, routes } => {
                status.route_generation = generation;
                status.selected_routes = routes.len();
                let snapshot = RouteSnapshot { generation, routes };
                route_updates.send_replace(snapshot.clone());
                if let Err(error) = exporter.reconcile(snapshot).await {
                    warn!(%error, "route export failed");
                }
            }
            Action::SequenceNumberChanged(value) => {
                sequence_store
                    .persist(value)
                    .map_err(|error| RouterError::SequenceStore(error.to_string()))?;
            }
        }
    }
    Ok(())
}
