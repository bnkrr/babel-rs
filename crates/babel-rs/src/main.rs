use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

use babel_router::{BabelRouter, RouteKey, RouterHandle};
use clap::Parser;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::watch;
use tracing::{info, warn};

mod config;
mod interfaces;
mod linux;
mod state;

use config::Config;
use linux::LinuxExporter;

#[derive(Parser)]
struct Args {
    #[arg(long)]
    config: PathBuf,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let args = Args::parse();
    let mut active_config = Config::load(&args.config)?;
    let state = state::load_or_create(
        active_config.router_id.as_deref(),
        PathBuf::from(&active_config.state_file).as_path(),
    )?;
    let exporter = LinuxExporter::new(active_config.export.clone())?;
    let mut builder = BabelRouter::builder()
        .router_id(state.router_id)
        .sequence_number(state.sequence_number)
        .sequence_store(state.store)
        .exporter(exporter.clone());
    for origin in &active_config.origins {
        builder = builder.originate(origin.key()?, origin.metric);
    }
    let router = builder.build().await?;
    let handle = router.handle();
    let mut running = Box::pin(router.run());

    let (config_tx, config_rx) = watch::channel(active_config.clone());
    let (services_shutdown, shutdown_rx) = watch::channel(false);
    let interface_task = {
        let handle = handle.clone();
        let shutdown = shutdown_rx.clone();
        tokio::spawn(async move {
            let result = interfaces::run(handle, config_rx, shutdown).await;
            if let Err(error) = &result {
                warn!(%error, "interface manager stopped");
            }
            result
        })
    };
    let exporter_task = {
        let exporter = exporter.clone();
        tokio::spawn(async move { exporter.run_reconciler(shutdown_rx).await })
    };

    let mut interrupt = signal(SignalKind::interrupt())?;
    let mut terminate = signal(SignalKind::terminate())?;
    let mut hangup = signal(SignalKind::hangup())?;
    let mut generation = 1_u64;
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
                match reload(
                    &args.config,
                    &mut active_config,
                    &handle,
                    &config_tx,
                    &exporter,
                ).await {
                    Ok(()) => {
                        generation = generation.wrapping_add(1);
                        info!(generation, "configuration reload committed");
                    }
                    Err(error) => warn!(%error, generation, "configuration reload rejected; old configuration remains active"),
                }
            }
        }
    };

    let _ = services_shutdown.send(true);
    match interface_task.await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => warn!(%error, "interface manager stopped"),
        Err(error) => warn!(%error, "interface manager task join failed"),
    }
    if let Err(error) = exporter_task.await {
        warn!(%error, "route reconciler task join failed");
    }
    handle.shutdown();
    if !router_finished {
        running.await?;
    }
    run_result
}

async fn reload(
    path: &Path,
    active: &mut Config,
    router: &RouterHandle,
    config_tx: &watch::Sender<Config>,
    exporter: &LinuxExporter,
) -> Result<(), Box<dyn std::error::Error>> {
    let candidate = Config::load(path)?;
    if !active.reload_identity_matches(&candidate) {
        return Err(Box::new(io::Error::new(
            io::ErrorKind::InvalidInput,
            "router_id and state_file cannot change during reload",
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
    Ok(())
}

fn origin_map(config: &Config) -> Result<BTreeMap<RouteKey, u16>, config::ConfigError> {
    config
        .origins
        .iter()
        .map(|origin| Ok((origin.key()?, origin.metric)))
        .collect()
}
