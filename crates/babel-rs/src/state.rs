use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use babel_proto::RouterId;
use babel_router::SequenceStore;
use serde::{Deserialize, Serialize};
use thiserror::Error;

const STATE_VERSION: u8 = 1;
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
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct DiskState {
    version: u8,
    router_id: String,
    sequence_number: u16,
}

#[derive(Debug, Error)]
pub enum StateError {
    #[error("invalid router-id; expected 16 hexadecimal digits, optionally separated by colons")]
    InvalidRouterId,
    #[error("unsupported state version {0}")]
    UnsupportedVersion(u8),
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
    let router_id = configured_id
        .or_else(|| existing.as_ref().map(|state| state.router_id))
        .unwrap_or(random_router_id()?);
    let sequence_number = existing.map_or_else(random_sequence_number, |state| {
        Ok(state.sequence_number.wrapping_add(1))
    })?;
    let store = StateStore {
        path: path.to_owned(),
        router_id,
    };
    store.persist_state(sequence_number)?;
    Ok(LoadedState {
        router_id,
        sequence_number,
        store,
    })
}

impl StateStore {
    fn persist_state(&self, sequence_number: u16) -> Result<(), StateError> {
        let state = DiskState {
            version: STATE_VERSION,
            router_id: format_router_id(self.router_id),
            sequence_number,
        };
        write_atomic(&self.path, &toml::to_string(&state)?)?;
        Ok(())
    }
}

impl SequenceStore for StateStore {
    fn persist(
        &self,
        sequence_number: u16,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.persist_state(sequence_number)
            .map_err(|error| Box::new(error) as _)
    }
}

fn parse_disk_state(value: &str) -> Result<LoadedDiskState, StateError> {
    // Accept the pre-v0.1 single-line Router-ID once and rewrite it immediately.
    if !value.contains('=') {
        return Ok(LoadedDiskState {
            router_id: parse_router_id(value.trim())?,
            sequence_number: random_sequence_number()?,
        });
    }
    let state: DiskState = toml::from_str(value)?;
    if state.version != STATE_VERSION {
        return Err(StateError::UnsupportedVersion(state.version));
    }
    Ok(LoadedDiskState {
        router_id: parse_router_id(&state.router_id)?,
        sequence_number: state.sequence_number,
    })
}

struct LoadedDiskState {
    router_id: RouterId,
    sequence_number: u16,
}

pub fn parse_router_id(value: &str) -> Result<RouterId, StateError> {
    let compact: String = value.chars().filter(|value| *value != ':').collect();
    if compact.len() != 16 {
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
    let parent = path.parent().unwrap_or(Path::new("."));
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
    file.write_all(value.as_bytes())?;
    file.sync_all()?;
    fs::rename(temporary_path, path)?;
    File::open(parent)?.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_colon_form() {
        assert_eq!(
            parse_router_id("01:02:03:04:05:06:07:08").unwrap().octets(),
            [1, 2, 3, 4, 5, 6, 7, 8]
        );
    }

    #[test]
    fn state_is_versioned_atomic_and_advances_on_restart() {
        let directory = std::env::temp_dir().join(format!(
            "babel-rs-state-test-{}-{}",
            std::process::id(),
            TEMPORARY_ID.fetch_add(1, Ordering::Relaxed)
        ));
        let path = directory.join("state.toml");
        let first = load_or_create(Some("01:02:03:04:05:06:07:08"), &path).unwrap();
        first.store.persist(400).unwrap();
        let second = load_or_create(None, &path).unwrap();
        assert_eq!(second.router_id, first.router_id);
        assert_eq!(second.sequence_number, 401);
        let on_disk: DiskState = toml::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(on_disk.version, STATE_VERSION);
        assert_eq!(on_disk.sequence_number, 401);
        fs::remove_dir_all(directory).unwrap();
    }
}
