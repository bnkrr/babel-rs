use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use babel_protocol::RouterId;
use babel_router::SequenceStore;
use serde::{Deserialize, Serialize};
use thiserror::Error;

const STATE_VERSION: u8 = 2;
static TEMPORARY_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug)]
pub struct LoadedState {
    pub router_id: RouterId,
    pub sequence_number: u16,
    pub store: StateStore,
}

#[derive(Clone, Debug)]
pub struct StateStore {
    path: PathBuf,
    router_id: RouterId,
    // A timed-out checkpoint may still finish on its dedicated thread. Retain
    // daemon ownership until it finishes or the process exits.
    ownership: Option<Arc<crate::ownership::ProtocolOwnership>>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct DiskState {
    version: u8,
    router_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    sequence_number: Option<u16>,
}

#[derive(Debug, Error)]
pub enum StateError {
    #[error("invalid router-id; expected 16 hexadecimal digits, optionally separated by colons")]
    InvalidRouterId,
    #[error("unsupported state version {0}")]
    UnsupportedVersion(u8),
    #[error("version 1 state is missing its sequence number")]
    MissingSequenceNumber,
    #[error("invalid state: {0}")]
    InvalidState(#[from] toml::de::Error),
    #[error("encode state: {0}")]
    Encode(#[from] toml::ser::Error),
    #[error("state I/O: {0}")]
    Io(#[from] std::io::Error),
}

pub fn load_or_create(explicit: Option<&str>, path: &Path) -> Result<LoadedState, StateError> {
    let configured_id = explicit.map(parse_router_id).transpose()?;
    let existing = match fs::read_to_string(path) {
        Ok(value) => Some(parse_disk_state(&value)?),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    let router_id = match configured_id.or_else(|| existing.as_ref().map(|state| state.router_id)) {
        Some(id) => id,
        None => random_router_id()?,
    };
    let sequence_number =
        initial_sequence_number(router_id, existing.as_ref(), random_sequence_number)?;
    let store = StateStore {
        path: path.to_owned(),
        router_id,
        ownership: None,
    };
    // Consume any saved sequence before advertising. A crash must leave only
    // the stable identity, not a checkpoint from a previous orderly shutdown.
    store.write_state(None)?;
    Ok(LoadedState {
        router_id,
        sequence_number,
        store,
    })
}

impl StateStore {
    pub fn with_ownership(mut self, ownership: Arc<crate::ownership::ProtocolOwnership>) -> Self {
        self.ownership = Some(ownership);
        self
    }

    fn write_state(&self, sequence_number: Option<u16>) -> Result<(), StateError> {
        let state = DiskState {
            version: STATE_VERSION,
            router_id: format_router_id(self.router_id),
            sequence_number,
        };
        write_atomic(&self.path, &toml::to_string(&state)?)?;
        Ok(())
    }
}

#[async_trait]
impl SequenceStore for StateStore {
    async fn persist(
        &self,
        sequence_number: u16,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let store = self.clone();
        let (send, receive) = tokio::sync::oneshot::channel();
        // This runs only once, at orderly shutdown. Unlike Tokio's blocking
        // pool, a dedicated, unjoined thread does not hold runtime destruction
        // open if the filesystem stalls beyond the router's checkpoint timeout.
        std::thread::Builder::new()
            .name("babel-checkpoint".into())
            .spawn(move || {
                let _ = send.send(store.write_state(Some(sequence_number)));
            })?;
        receive.await?.map_err(|error| Box::new(error) as _)
    }
}

fn parse_disk_state(value: &str) -> Result<LoadedDiskState, StateError> {
    // Accept the pre-v0.1 single-line Router-ID once and rewrite it immediately.
    if !value.contains('=') {
        return Ok(LoadedDiskState {
            router_id: parse_router_id(value.trim())?,
            sequence_number: None,
        });
    }
    let state: DiskState = toml::from_str(value)?;
    match state.version {
        1 if state.sequence_number.is_none() => return Err(StateError::MissingSequenceNumber),
        1 | STATE_VERSION => {}
        version => return Err(StateError::UnsupportedVersion(version)),
    }
    Ok(LoadedDiskState {
        router_id: parse_router_id(&state.router_id)?,
        sequence_number: state.sequence_number,
    })
}

struct LoadedDiskState {
    router_id: RouterId,
    sequence_number: Option<u16>,
}

fn initial_sequence_number(
    router_id: RouterId,
    existing: Option<&LoadedDiskState>,
    random: impl FnOnce() -> Result<u16, StateError>,
) -> Result<u16, StateError> {
    match existing
        .filter(|state| state.router_id == router_id)
        .and_then(|state| state.sequence_number)
    {
        Some(sequence_number) => Ok(sequence_number.wrapping_add(1)),
        None => random(),
    }
}

pub fn parse_router_id(value: &str) -> Result<RouterId, StateError> {
    let compact: String = value.chars().filter(|value| *value != ':').collect();
    if compact.len() != 16 || !compact.is_ascii() {
        return Err(StateError::InvalidRouterId);
    }
    let mut raw = [0u8; 8];
    for (index, byte) in raw.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&compact[index * 2..index * 2 + 2], 16)
            .map_err(|_| StateError::InvalidRouterId)?;
    }
    RouterId::new(raw).ok_or(StateError::InvalidRouterId)
}

fn random_router_id() -> Result<RouterId, StateError> {
    loop {
        let mut raw = [0u8; 8];
        File::open("/dev/urandom")?.read_exact(&mut raw)?;
        if let Some(id) = RouterId::new(raw) {
            return Ok(id);
        }
    }
}

fn random_sequence_number() -> Result<u16, StateError> {
    let mut raw = [0u8; 2];
    File::open("/dev/urandom")?.read_exact(&mut raw)?;
    Ok(u16::from_be_bytes(raw))
}

fn format_router_id(id: RouterId) -> String {
    id.octets()
        .iter()
        .map(|value| format!("{value:02x}"))
        .collect()
}

fn write_atomic(path: &Path, value: &str) -> Result<(), std::io::Error> {
    let parent = path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(parent)?;
    let temporary = loop {
        let id = TEMPORARY_ID.fetch_add(1, Ordering::Relaxed);
        let mut candidate = PathBuf::from(path);
        candidate.set_extension(format!("tmp.{}.{id}", std::process::id()));
        match OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&candidate)
        {
            Ok(file) => break (candidate, file),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    };
    let (temporary_path, mut file) = temporary;
    let result = (|| {
        file.write_all(value.as_bytes())?;
        file.sync_all()?;
        fs::rename(&temporary_path, path)?;
        File::open(parent)?.sync_all()
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary_path);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "babel-rs-state-test-{}-{}",
                std::process::id(),
                TEMPORARY_ID.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn state_path(&self) -> PathBuf {
            self.0.join("state.toml")
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn on_disk(path: &Path) -> DiskState {
        toml::from_str(&fs::read_to_string(path).unwrap()).unwrap()
    }

    #[test]
    fn parses_colon_form() {
        assert_eq!(
            parse_router_id("01:02:03:04:05:06:07:08").unwrap().octets(),
            [1, 2, 3, 4, 5, 6, 7, 8]
        );
    }

    #[tokio::test]
    async fn restart_consumes_checkpoint_and_retains_identity() {
        let directory = TestDirectory::new();
        let path = directory.state_path();
        let first = load_or_create(Some("01:02:03:04:05:06:07:08"), &path).unwrap();
        assert_eq!(on_disk(&path).sequence_number, None);
        first.store.persist(400).await.unwrap();
        assert_eq!(on_disk(&path).sequence_number, Some(400));
        let second = load_or_create(None, &path).unwrap();
        assert_eq!(second.router_id, first.router_id);
        assert_eq!(second.sequence_number, 401);
        assert_eq!(on_disk(&path).version, STATE_VERSION);
        assert_eq!(on_disk(&path).sequence_number, None);

        // Simulate an unclean exit: no final persist, only identity remains.
        let third = load_or_create(None, &path).unwrap();
        assert_eq!(third.router_id, first.router_id);
        assert_eq!(on_disk(&path).sequence_number, None);
        assert_eq!(fs::read_dir(&directory.0).unwrap().count(), 1);
    }

    #[test]
    fn checkpoints_are_used_only_for_the_same_identity_and_wrap() {
        let id = parse_router_id("0102030405060708").unwrap();
        let other_id = parse_router_id("0807060504030201").unwrap();
        let saved = LoadedDiskState {
            router_id: id,
            sequence_number: Some(u16::MAX),
        };
        assert_eq!(
            initial_sequence_number(id, Some(&saved), || panic!("unexpected RNG")).unwrap(),
            0
        );
        assert_eq!(
            initial_sequence_number(other_id, Some(&saved), || Ok(900)).unwrap(),
            900
        );
        assert_eq!(initial_sequence_number(id, None, || Ok(901)).unwrap(), 901);
        let identity_only = LoadedDiskState {
            router_id: id,
            sequence_number: None,
        };
        assert_eq!(
            initial_sequence_number(id, Some(&identity_only), || Ok(902)).unwrap(),
            902
        );
    }

    #[test]
    fn migrates_v1_checkpoint_and_legacy_identity() {
        let directory = TestDirectory::new();
        let path = directory.state_path();
        fs::write(
            &path,
            "version = 1\nrouter_id = \"0102030405060708\"\nsequence_number = 65535\n",
        )
        .unwrap();
        let loaded = load_or_create(None, &path).unwrap();
        assert_eq!(loaded.sequence_number, 0);
        assert_eq!(on_disk(&path).version, 2);
        assert_eq!(on_disk(&path).sequence_number, None);

        fs::write(&path, "01:02:03:04:05:06:07:08\n").unwrap();
        let legacy = parse_disk_state(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(legacy.sequence_number, None);
        let migrated = load_or_create(None, &path).unwrap();
        assert_eq!(migrated.router_id, loaded.router_id);
        assert_eq!(on_disk(&path).version, 2);
        assert_eq!(on_disk(&path).sequence_number, None);
    }

    #[test]
    fn explicit_identity_replaces_stored_identity() {
        let directory = TestDirectory::new();
        let path = directory.state_path();
        fs::write(
            &path,
            "version = 2\nrouter_id = \"0102030405060708\"\nsequence_number = 100\n",
        )
        .unwrap();
        let loaded = load_or_create(Some("0807060504030201"), &path).unwrap();
        assert_eq!(
            loaded.router_id,
            parse_router_id("0807060504030201").unwrap()
        );
        assert_eq!(on_disk(&path).router_id, "0807060504030201");
        assert_eq!(on_disk(&path).sequence_number, None);
    }

    #[test]
    fn invalid_state_cannot_be_silently_replaced() {
        let directory = TestDirectory::new();
        let path = directory.state_path();
        for value in [
            "version = 1\nrouter_id = \"0102030405060708\"\n",
            "version = 3\nrouter_id = \"0102030405060708\"\n",
            "version = 2\nrouter_id = \"0102030405060708\"\nsequence_number = 65536\n",
            "version = 2\nrouter_id = \"0102030405060708\"\nunknown = 1\n",
            "version = 2\nrouter_id = \"invalid\"\n",
        ] {
            fs::write(&path, value).unwrap();
            assert!(load_or_create(None, &path).is_err());
            assert_eq!(fs::read_to_string(&path).unwrap(), value);
        }
    }

    #[test]
    fn failed_write_does_not_leave_a_temporary_file() {
        let directory = TestDirectory::new();
        let path = directory.state_path();
        // Renaming a file over a directory fails even in privileged test runs.
        fs::create_dir(&path).unwrap();
        assert!(write_atomic(&path, "checkpoint").is_err());
        assert_eq!(fs::read_dir(&directory.0).unwrap().count(), 1);
        assert!(load_or_create(None, &path).is_err());
    }
}
