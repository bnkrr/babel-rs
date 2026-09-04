use std::io;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use babel_proto::INFINITY;
use babel_router::{RouteSnapshot, RouterHandle};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;
use tokio::io::{
    AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader,
};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{RwLock, Semaphore, mpsc, oneshot, watch};
use tracing::warn;

use crate::linux::LinuxExporter;

pub const API_VERSION: u32 = 1;
const MAX_FRAME: usize = 1024 * 1024;
const MAX_CLIENTS: usize = 64;
const CLIENT_IO_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Debug)]
pub struct RuntimeMetadata {
    pub config_generation: u64,
    pub active_config_sha256: String,
    pub last_reload_error: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ReloadResult {
    pub config_generation: u64,
    pub active_config_sha256: String,
}

pub enum DaemonCommand {
    Reload {
        reply: oneshot::Sender<Result<ReloadResult, String>>,
    },
    Shutdown {
        accepted: oneshot::Sender<()>,
        response_flushed: oneshot::Receiver<()>,
    },
}

#[derive(Clone)]
pub struct Shared {
    pub started: Instant,
    pub metadata: Arc<RwLock<RuntimeMetadata>>,
    pub router: RouterHandle,
    pub exporter: LinuxExporter,
}

#[derive(Debug, Error)]
pub enum ControlError {
    #[error("prepare control socket {path}: {source}")]
    Prepare { path: PathBuf, source: io::Error },
    #[error("bind control socket {path}: {source}")]
    Bind { path: PathBuf, source: io::Error },
}

#[derive(Debug, Deserialize)]
struct Request {
    api_version: u32,
    id: u64,
    command: String,
    #[serde(default)]
    params: Value,
}

#[derive(Debug, Deserialize, Default)]
struct RouteFilter {
    destination: Option<String>,
    source: Option<String>,
    interface: Option<String>,
}

#[derive(Serialize)]
struct Response<'a> {
    api_version: u32,
    id: u64,
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<ErrorBody<'a>>,
}

#[derive(Serialize)]
struct ErrorBody<'a> {
    code: &'a str,
    message: String,
}

struct DispatchResult<'a> {
    response: Response<'a>,
    after_flush: Option<oneshot::Sender<()>>,
}

struct SocketGuard(PathBuf);

impl Drop for SocketGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

pub async fn serve(
    path: PathBuf,
    shared: Shared,
    commands: mpsc::Sender<DaemonCommand>,
    mut shutdown: watch::Receiver<bool>,
) -> Result<(), ControlError> {
    prepare_socket(&path)
        .await
        .map_err(|source| ControlError::Prepare {
            path: path.clone(),
            source,
        })?;
    let listener = UnixListener::bind(&path).map_err(|source| ControlError::Bind {
        path: path.clone(),
        source,
    })?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).map_err(|source| {
        ControlError::Prepare {
            path: path.clone(),
            source,
        }
    })?;
    let _guard = SocketGuard(path);
    let clients = Arc::new(Semaphore::new(MAX_CLIENTS));

    loop {
        tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    return Ok(());
                }
            }
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => {
                    let Ok(permit) = Arc::clone(&clients).try_acquire_owned() else {
                        warn!(limit = MAX_CLIENTS, "control client limit reached");
                        continue;
                    };
                    let shared = shared.clone();
                    let commands = commands.clone();
                    tokio::spawn(async move {
                        let _permit = permit;
                        if let Err(error) = handle_client(stream, shared, commands).await {
                            warn!(%error, "control client disconnected");
                        }
                    });
                }
                Err(error) => {
                    warn!(%error, "control accept failed; retrying");
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
            }
        }
    }
}

async fn prepare_socket(path: &Path) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "socket has no parent"))?;
    let parent_existed = parent.exists();
    std::fs::create_dir_all(parent)?;
    if !parent_existed {
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
    }
    if !path.exists() {
        return Ok(());
    }
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.file_type().is_socket() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "refusing to replace a non-socket path",
        ));
    }
    if UnixStream::connect(path).await.is_ok() {
        return Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            "another daemon is accepting connections",
        ));
    }
    std::fs::remove_file(path)
}

async fn handle_client(
    stream: UnixStream,
    shared: Shared,
    commands: mpsc::Sender<DaemonCommand>,
) -> io::Result<()> {
    let (read, mut write) = stream.into_split();
    let mut read = BufReader::new(read);
    io_timeout(write_json(
        &mut write,
        &json!({
            "type": "hello",
            "api_version": API_VERSION,
            "server_version": env!("CARGO_PKG_VERSION"),
            "capabilities": ["status", "interfaces", "neighbors", "routes", "reload", "shutdown"]
        }),
    ))
    .await?;

    loop {
        let Some(frame) = io_timeout(read_frame(&mut read)).await? else {
            return Ok(());
        };
        let request: Request = match serde_json::from_slice(&frame) {
            Ok(value) => value,
            Err(error) => {
                io_timeout(write_json(
                    &mut write,
                    &Response {
                        api_version: API_VERSION,
                        id: 0,
                        ok: false,
                        result: None,
                        error: Some(ErrorBody {
                            code: "invalid_request",
                            message: error.to_string(),
                        }),
                    },
                ))
                .await?;
                continue;
            }
        };
        let dispatched = dispatch(&request, &shared, &commands).await;
        io_timeout(write_json(&mut write, &dispatched.response)).await?;
        if let Some(flushed) = dispatched.after_flush {
            let _ = flushed.send(());
            return Ok(());
        }
    }
}

async fn dispatch<'a>(
    request: &Request,
    shared: &Shared,
    commands: &mpsc::Sender<DaemonCommand>,
) -> DispatchResult<'a> {
    if request.api_version != API_VERSION {
        return DispatchResult {
            response: failure(
                request.id,
                "unsupported_version",
                format!(
                    "control API version {} is unsupported; expected {}",
                    request.api_version, API_VERSION
                ),
            ),
            after_flush: None,
        };
    }
    let (result, after_flush) = match request.command.as_str() {
        "capabilities" => (
            Ok(json!({
                "api_version": API_VERSION,
                "server_version": env!("CARGO_PKG_VERSION"),
                "commands": ["status", "interfaces", "neighbors", "routes", "reload", "shutdown"]
            })),
            None,
        ),
        "status" => (status(shared).await, None),
        "interfaces" => (interfaces(shared).await, None),
        "neighbors" | "neighbours" => (neighbors(shared).await, None),
        "routes" => (routes(shared, &request.params), None),
        "reload" => (reload(commands).await, None),
        "shutdown" => shutdown(commands).await,
        _ => (
            Err((
                "unknown_command",
                format!("unknown command {:?}", request.command),
            )),
            None,
        ),
    };
    let response = match result {
        Ok(value) => Response {
            api_version: API_VERSION,
            id: request.id,
            ok: true,
            result: Some(value),
            error: None,
        },
        Err((code, message)) => failure(request.id, code, message),
    };
    DispatchResult {
        response,
        after_flush,
    }
}

async fn status(shared: &Shared) -> Result<Value, (&'static str, String)> {
    let router = shared
        .router
        .status()
        .await
        .map_err(|error| ("router_stopped", error.to_string()))?;
    let metadata = shared.metadata.read().await.clone();
    let export = shared.exporter.health().await;
    Ok(json!({
        "version": env!("CARGO_PKG_VERSION"),
        "uptime_seconds": shared.started.elapsed().as_secs(),
        "ready": true,
        "config_generation": metadata.config_generation,
        "active_config_sha256": metadata.active_config_sha256,
        "last_reload_error": metadata.last_reload_error,
        "metric": router.metric,
        "attached_interfaces": router.interfaces.len(),
        "neighbors": router.neighbours,
        "route_generation": router.route_generation,
        "selected_routes": router.selected_routes,
        "dropped_outbound_datagrams": router.dropped_outbound_datagrams,
        "export": {
            "last_success_age_seconds": export.last_success_age.map(|value| value.as_secs()),
            "last_error": export.last_error,
        }
    }))
}

async fn interfaces(shared: &Shared) -> Result<Value, (&'static str, String)> {
    let status = shared
        .router
        .status()
        .await
        .map_err(|error| ("router_stopped", error.to_string()))?;
    Ok(Value::Array(
        status
            .interface_details
            .into_iter()
            .map(|item| {
                json!({
                    "name": item.name,
                    "ifindex": item.index,
                    "local_addresses": item.local_addresses.into_iter().map(|value| value.to_string()).collect::<Vec<_>>(),
                    "attached": true,
                })
            })
            .collect(),
    ))
}

async fn neighbors(shared: &Shared) -> Result<Value, (&'static str, String)> {
    let status = shared
        .router
        .status()
        .await
        .map_err(|error| ("router_stopped", error.to_string()))?;
    Ok(Value::Array(
        status
            .neighbour_details
            .into_iter()
            .map(|item| {
                json!({
                    "interface": item.interface,
                    "address": item.address.to_string(),
                    "algorithm": item.algorithm,
                    "reachable": item.link_cost != INFINITY,
                    "hello_received": item.hello_received,
                    "hello_expected": item.hello_expected,
                    "multicast_hello_history": item.multicast_hello_history,
                    "unicast_hello_history": item.unicast_hello_history,
                    "receive_cost": item.receive_cost,
                    "transmit_cost": item.transmit_cost,
                    "link_cost": item.link_cost,
                    "last_rtt_us": item.last_rtt_us,
                    "smoothed_rtt_us": item.smoothed_rtt_us,
                    "rtt_penalty": item.rtt_penalty,
                    "last_hello_age_ms": item.last_hello_age_ms,
                })
            })
            .collect(),
    ))
}

fn routes(shared: &Shared, params: &Value) -> Result<Value, (&'static str, String)> {
    let filter: RouteFilter = serde_json::from_value(params.clone())
        .map_err(|error| ("invalid_params", error.to_string()))?;
    let snapshot = shared.router.subscribe_routes().borrow().clone();
    route_value(snapshot, filter)
}

fn route_value(
    snapshot: RouteSnapshot,
    filter: RouteFilter,
) -> Result<Value, (&'static str, String)> {
    let mut result = Vec::new();
    for route in snapshot.routes {
        if filter
            .destination
            .as_ref()
            .is_some_and(|value| value != &route.key.destination.to_string())
            || filter.source.as_ref().is_some_and(|value| {
                route.key.source.map(|source| source.to_string()).as_ref() != Some(value)
            })
            || filter
                .interface
                .as_ref()
                .is_some_and(|value| value != &route.interface)
        {
            continue;
        }
        result.push(json!({
            "destination": route.key.destination.to_string(),
            "source": route.key.source.map(|value| value.to_string()),
            "router_id": route.router_id.to_string(),
            "sequence_number": route.seqno,
            "metric": route.metric,
            "next_hop": route.next_hop.to_string(),
            "interface": route.interface,
            "selected": true,
        }));
    }
    Ok(json!({"generation": snapshot.generation, "routes": result}))
}

async fn reload(commands: &mpsc::Sender<DaemonCommand>) -> Result<Value, (&'static str, String)> {
    let (send, receive) = oneshot::channel();
    commands
        .send(DaemonCommand::Reload { reply: send })
        .await
        .map_err(|_| ("daemon_stopping", "daemon is stopping".into()))?;
    match receive.await {
        Ok(Ok(value)) => {
            serde_json::to_value(value).map_err(|error| ("internal_error", error.to_string()))
        }
        Ok(Err(error)) => Err(("reload_rejected", error)),
        Err(_) => Err(("daemon_stopping", "daemon is stopping".into())),
    }
}

async fn shutdown(
    commands: &mpsc::Sender<DaemonCommand>,
) -> (
    Result<Value, (&'static str, String)>,
    Option<oneshot::Sender<()>>,
) {
    let (accepted, receive) = oneshot::channel();
    let (flushed, response_flushed) = oneshot::channel();
    if commands
        .send(DaemonCommand::Shutdown {
            accepted,
            response_flushed,
        })
        .await
        .is_err()
    {
        return (Err(("daemon_stopping", "daemon is stopping".into())), None);
    }
    match receive.await {
        Ok(()) => (Ok(json!({"accepted": true})), Some(flushed)),
        Err(_) => (Err(("daemon_stopping", "daemon is stopping".into())), None),
    }
}

fn failure(id: u64, code: &'static str, message: String) -> Response<'static> {
    Response {
        api_version: API_VERSION,
        id,
        ok: false,
        result: None,
        error: Some(ErrorBody { code, message }),
    }
}

async fn read_frame<R: AsyncBufRead + Unpin>(read: &mut R) -> io::Result<Option<Vec<u8>>> {
    let mut frame = Vec::new();
    let length = read
        .take((MAX_FRAME + 1) as u64)
        .read_until(b'\n', &mut frame)
        .await?;
    if length == 0 {
        return Ok(None);
    }
    if frame.len() > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "control frame exceeds one MiB",
        ));
    }
    while matches!(frame.last(), Some(b'\n' | b'\r')) {
        frame.pop();
    }
    Ok(Some(frame))
}

async fn write_json<W: AsyncWrite + Unpin, T: Serialize>(
    write: &mut W,
    value: &T,
) -> io::Result<()> {
    let mut data = serde_json::to_vec(value).map_err(io::Error::other)?;
    data.push(b'\n');
    if data.len() > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "control response exceeds one MiB",
        ));
    }
    write.write_all(&data).await?;
    write.flush().await
}

async fn io_timeout<T>(future: impl Future<Output = io::Result<T>>) -> io::Result<T> {
    tokio::time::timeout(CLIENT_IO_TIMEOUT, future)
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "control client timed out"))?
}

pub async fn request(
    path: &Path,
    command: &str,
    params: Value,
) -> Result<Value, ControlClientError> {
    let stream = UnixStream::connect(path).await?;
    let (read, mut write) = stream.into_split();
    let mut read = BufReader::new(read);
    let hello = io_timeout(read_frame(&mut read))
        .await?
        .ok_or(ControlClientError::Closed)?;
    let hello: Value = serde_json::from_slice(&hello)?;
    if hello.get("api_version") != Some(&Value::from(API_VERSION)) {
        return Err(ControlClientError::Version(hello));
    }
    io_timeout(write_json(
        &mut write,
        &json!({"api_version": API_VERSION, "id": 1, "command": command, "params": params}),
    ))
    .await?;
    let response = io_timeout(read_frame(&mut read))
        .await?
        .ok_or(ControlClientError::Closed)?;
    let response: Value = serde_json::from_slice(&response)?;
    if response.get("ok") == Some(&Value::Bool(true)) {
        Ok(response.get("result").cloned().unwrap_or(Value::Null))
    } else {
        Err(ControlClientError::Rejected(response))
    }
}

#[derive(Debug, Error)]
pub enum ControlClientError {
    #[error("control socket: {0}")]
    Io(#[from] io::Error),
    #[error("decode control response: {0}")]
    Json(#[from] serde_json::Error),
    #[error("control socket closed before responding")]
    Closed,
    #[error("unsupported control API greeting: {0}")]
    Version(Value),
    #[error("control command rejected: {0}")]
    Rejected(Value),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn route_filter_matches_exact_fields() {
        let route = babel_proto::SelectedRoute {
            key: babel_proto::RouteKey::new("192.0.2.0/24".parse().unwrap(), None).unwrap(),
            router_id: babel_proto::RouterId::new([1; 8]).unwrap(),
            seqno: 2,
            metric: 96,
            next_hop: "fe80::1".parse().unwrap(),
            interface: "wg0".into(),
        };
        let value = route_value(
            RouteSnapshot {
                generation: 3,
                routes: vec![route],
                unreachable: vec![],
            },
            RouteFilter {
                interface: Some("wg0".into()),
                ..RouteFilter::default()
            },
        )
        .unwrap();
        assert_eq!(value["generation"], 3);
        assert_eq!(value["routes"].as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn oversized_frame_is_bounded_before_newline() {
        let (mut client, server) = tokio::io::duplex(MAX_FRAME + 2);
        let writer = tokio::spawn(async move {
            client.write_all(&vec![b'x'; MAX_FRAME + 1]).await.unwrap();
        });
        let mut server = BufReader::new(server);
        let error = read_frame(&mut server).await.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        writer.await.unwrap();
    }
}
