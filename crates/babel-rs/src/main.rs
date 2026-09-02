use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use babel_router::{BabelRouter, RouteKey, RouterHandle};
use clap::{Parser, Subcommand};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::{RwLock, mpsc, watch};
use tokio::task::JoinSet;
use tracing::{info, warn};

mod config;
mod control;
mod interfaces;
mod linux;
mod state;

use config::Config;
use control::{DaemonCommand, ReloadResult, RuntimeMetadata};
use linux::LinuxExporter;

const DEFAULT_CONTROL_SOCKET: &str = "/run/babel-rs/babel-rs.ctl";

#[derive(Parser)]
#[command(version, about)]
struct Args {
    #[command(subcommand)]
    command: Option<CliCommand>,

    // Compatibility with the v0.1 daemon invocation.
    #[arg(long)]
    config: Option<PathBuf>,
    #[arg(long)]
    control_socket: Option<PathBuf>,
}

#[derive(Subcommand)]
enum CliCommand {
    Run {
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        control_socket: Option<PathBuf>,
    },
    Check {
        #[arg(long)]
        config: PathBuf,
    },
    Status {
        #[arg(long, default_value = DEFAULT_CONTROL_SOCKET)]
        socket: PathBuf,
    },
    Interfaces {
        #[arg(long, default_value = DEFAULT_CONTROL_SOCKET)]
        socket: PathBuf,
    },
    Neighbors {
        #[arg(long, default_value = DEFAULT_CONTROL_SOCKET)]
        socket: PathBuf,
    },
    Routes {
        #[arg(long, default_value = DEFAULT_CONTROL_SOCKET)]
        socket: PathBuf,
        #[arg(long)]
        destination: Option<String>,
        #[arg(long)]
        source: Option<String>,
        #[arg(long)]
        interface: Option<String>,
    },
    Reload {
        #[arg(long, default_value = DEFAULT_CONTROL_SOCKET)]
        socket: PathBuf,
    },
    Shutdown {
        #[arg(long, default_value = DEFAULT_CONTROL_SOCKET)]
        socket: PathBuf,
    },
}

enum Mode {
    Run {
        config: PathBuf,
        control_socket: Option<PathBuf>,
    },
    Check(PathBuf),
    Request {
        socket: PathBuf,
        command: &'static str,
        params: Value,
    },
}

enum ServiceExit {
    Interfaces(Result<(), interfaces::InterfaceManagerError>),
    Exporter,
    Control(Result<(), control::ControlError>),
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    match mode(Args::parse())? {
        Mode::Run {
            config,
            control_socket,
        } => run_daemon(config, control_socket).await,
        Mode::Check(path) => {
            Config::load(&path)?;
            Ok(())
        }
        Mode::Request {
            socket,
            command,
            params,
        } => {
            let value = control::request(&socket, command, params).await?;
            println!("{}", serde_json::to_string_pretty(&value)?);
            Ok(())
        }
    }
}

fn mode(args: Args) -> Result<Mode, Box<dyn std::error::Error>> {
    let value = match args.command {
        Some(CliCommand::Run {
            config,
            control_socket,
        }) => Mode::Run {
            config,
            control_socket: Some(
                control_socket.unwrap_or_else(|| PathBuf::from(DEFAULT_CONTROL_SOCKET)),
            ),
        },
        Some(CliCommand::Check { config }) => Mode::Check(config),
        Some(CliCommand::Status { socket }) => request_mode(socket, "status", json!({})),
        Some(CliCommand::Interfaces { socket }) => request_mode(socket, "interfaces", json!({})),
        Some(CliCommand::Neighbors { socket }) => request_mode(socket, "neighbors", json!({})),
        Some(CliCommand::Routes {
            socket,
            destination,
            source,
            interface,
        }) => request_mode(
            socket,
            "routes",
            json!({"destination": destination, "source": source, "interface": interface}),
        ),
        Some(CliCommand::Reload { socket }) => request_mode(socket, "reload", json!({})),
        Some(CliCommand::Shutdown { socket }) => request_mode(socket, "shutdown", json!({})),
        None => Mode::Run {
            config: args.config.ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "use `babel-rs run --config PATH` or `babel-rs --config PATH`",
                )
            })?,
            control_socket: args.control_socket,
        },
    };
    Ok(value)
}

fn request_mode(socket: PathBuf, command: &'static str, params: Value) -> Mode {
    Mode::Request {
        socket,
        command,
        params,
    }
}

async fn run_daemon(
    config_path: PathBuf,
    control_socket: Option<PathBuf>,
) -> Result<(), Box<dyn std::error::Error>> {
    let (mut active_config, digest) = load_config(&config_path)?;
    let state = state::load_or_create(
        active_config.router_id.as_deref(),
        PathBuf::from(&active_config.state_file).as_path(),
    )?;
    let exporter = LinuxExporter::new(active_config.export.clone())?;
    let mut builder = BabelRouter::builder()
        .router_id(state.router_id)
        .sequence_number(state.sequence_number)
        .sequence_store(state.store)
        .metric_profile(active_config.metric.build()?)
        .exporter(exporter.clone());
    for origin in &active_config.origins {
        builder = builder.originate(origin.key()?, origin.metric);
    }
    let router = builder.build().await?;
    let handle = router.handle();
    let mut running = Box::pin(router.run());

    let metadata = Arc::new(RwLock::new(RuntimeMetadata {
        config_generation: 1,
        active_config_sha256: digest,
        last_reload_error: None,
    }));
    let (config_tx, config_rx) = watch::channel(active_config.clone());
    let (services_shutdown, shutdown_rx) = watch::channel(false);
    let (command_tx, mut command_rx) = mpsc::channel(16);
    let control_enabled = control_socket.is_some();
    let _command_keepalive = command_tx.clone();
    let mut services = JoinSet::new();
    {
        let handle = handle.clone();
        let shutdown = shutdown_rx.clone();
        services.spawn(async move {
            ServiceExit::Interfaces(interfaces::run(handle, config_rx, shutdown).await)
        });
    }
    {
        let exporter = exporter.clone();
        let shutdown = shutdown_rx.clone();
        services.spawn(async move {
            exporter.run_reconciler(shutdown).await;
            ServiceExit::Exporter
        });
    }
    if let Some(path) = control_socket {
        let shared = control::Shared {
            started: Instant::now(),
            metadata: Arc::clone(&metadata),
            router: handle.clone(),
            exporter: exporter.clone(),
        };
        let shutdown = shutdown_rx.clone();
        services.spawn(async move {
            ServiceExit::Control(control::serve(path, shared, command_tx, shutdown).await)
        });
    }

    let mut interrupt = signal(SignalKind::interrupt())?;
    let mut terminate = signal(SignalKind::terminate())?;
    let mut hangup = signal(SignalKind::hangup())?;
    let mut router_finished = false;
    let run_result: Result<(), Box<dyn std::error::Error>> = loop {
        tokio::select! {
            result = &mut running => {
                router_finished = true;
                break result.map_err(|error| Box::new(error) as _);
            },
            _ = interrupt.recv() => break Ok(()),
            _ = terminate.recv() => break Ok(()),
            value = hangup.recv() => {
                if value.is_none() {
                    break Err(Box::new(io::Error::new(io::ErrorKind::BrokenPipe, "SIGHUP signal stream ended")) as _);
                }
                if let Err(error) = reload_and_record(
                    &config_path,
                    &mut active_config,
                    &handle,
                    &config_tx,
                    &exporter,
                    &metadata,
                ).await {
                    warn!(%error, "configuration reload rejected; old configuration remains active");
                }
            }
            command = command_rx.recv(), if control_enabled => match command {
                Some(DaemonCommand::Reload { reply }) => {
                    let result = reload_and_record(
                        &config_path,
                        &mut active_config,
                        &handle,
                        &config_tx,
                        &exporter,
                        &metadata,
                    ).await.map_err(|error| error.to_string());
                    let _ = reply.send(result);
                }
                Some(DaemonCommand::Shutdown { accepted, response_flushed }) => {
                    let _ = accepted.send(());
                    let _ = tokio::time::timeout(Duration::from_secs(1), response_flushed).await;
                    break Ok(());
                }
                None => break Err(Box::new(io::Error::new(io::ErrorKind::BrokenPipe, "control command channel closed")) as _),
            },
            service = services.join_next(), if !services.is_empty() => {
                let message = match service {
                    Some(Ok(ServiceExit::Interfaces(Ok(())))) => "interface manager stopped unexpectedly".into(),
                    Some(Ok(ServiceExit::Interfaces(Err(error)))) => format!("interface manager failed: {error}"),
                    Some(Ok(ServiceExit::Exporter)) => "route exporter reconciler stopped unexpectedly".into(),
                    Some(Ok(ServiceExit::Control(Ok(())))) => "control server stopped unexpectedly".into(),
                    Some(Ok(ServiceExit::Control(Err(error)))) => format!("control server failed: {error}"),
                    Some(Err(error)) => format!("critical task panicked: {error}"),
                    None => "all critical tasks stopped unexpectedly".into(),
                };
                break Err(Box::new(io::Error::other(message)) as _);
            }
        }
    };

    let _ = services_shutdown.send(true);
    handle.shutdown();
    if !router_finished {
        running.await?;
    }
    while let Some(result) = services.join_next().await {
        if let Err(error) = result {
            warn!(%error, "critical task join failed during shutdown");
        }
    }
    run_result
}

async fn reload_and_record(
    path: &Path,
    active: &mut Config,
    router: &RouterHandle,
    config_tx: &watch::Sender<Config>,
    exporter: &LinuxExporter,
    metadata: &Arc<RwLock<RuntimeMetadata>>,
) -> Result<ReloadResult, Box<dyn std::error::Error>> {
    match reload(path, active, router, config_tx, exporter).await {
        Ok(digest) => {
            let mut state = metadata.write().await;
            state.config_generation = state.config_generation.wrapping_add(1);
            state.active_config_sha256 = digest.clone();
            state.last_reload_error = None;
            info!(
                generation = state.config_generation,
                "configuration reload committed"
            );
            Ok(ReloadResult {
                config_generation: state.config_generation,
                active_config_sha256: digest,
            })
        }
        Err(error) => {
            metadata.write().await.last_reload_error = Some(error.to_string());
            Err(error)
        }
    }
}

async fn reload(
    path: &Path,
    active: &mut Config,
    router: &RouterHandle,
    config_tx: &watch::Sender<Config>,
    exporter: &LinuxExporter,
) -> Result<String, Box<dyn std::error::Error>> {
    let (candidate, digest) = load_config(path)?;
    if !active.reload_identity_matches(&candidate) {
        return Err(Box::new(io::Error::new(
            io::ErrorKind::InvalidInput,
            "router_id, state_file, and metric cannot change during reload",
        )));
    }

    let old_origins = origin_map(active)?;
    let new_origins = origin_map(&candidate)?;
    config_tx
        .send(candidate.clone())
        .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "interface manager stopped"))?;
    for (key, metric) in &new_origins {
        if old_origins.get(key) != Some(metric) {
            router.originate(*key, *metric).await?;
        }
    }
    for key in old_origins.keys() {
        if !new_origins.contains_key(key) {
            router.withdraw(*key).await?;
        }
    }
    if active.export != candidate.export {
        exporter.update_export(candidate.export.clone()).await;
    }
    *active = candidate;
    Ok(digest)
}

fn origin_map(config: &Config) -> Result<BTreeMap<RouteKey, u16>, config::ConfigError> {
    config
        .origins
        .iter()
        .map(|origin| Ok((origin.key()?, origin.metric)))
        .collect()
}

fn load_config(path: &Path) -> Result<(Config, String), Box<dyn std::error::Error>> {
    let contents = std::fs::read_to_string(path)?;
    let digest = Sha256::digest(contents.as_bytes());
    Ok((Config::parse(&contents)?, format!("{digest:x}")))
}
