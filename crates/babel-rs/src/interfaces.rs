use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::io;
use std::net::Ipv6Addr;
use std::path::Path;
use std::time::Duration;

use babel_router::{RouterError, RouterHandle};
use futures::StreamExt;
use rtnetlink::{MulticastGroup, new_multicast_connection};
use thiserror::Error;
use tokio::sync::watch;
use tokio::time::MissedTickBehavior;
use tracing::{debug, info, warn};

use crate::config::Config;

#[derive(Clone, Debug, Eq, PartialEq)]
struct InterfaceSignature {
    index: u32,
    up: bool,
    link_local_addresses: Vec<Ipv6Addr>,
}

#[derive(Debug, Error)]
pub enum InterfaceManagerError {
    #[error("open interface netlink monitor: {0}")]
    Monitor(#[from] io::Error),
    #[error("enumerate interfaces: {0}")]
    Enumerate(io::Error),
    #[error("Babel router stopped")]
    RouterStopped,
}

pub async fn run(
    router: RouterHandle,
    mut config: watch::Receiver<Config>,
    mut shutdown: watch::Receiver<bool>,
) -> Result<(), InterfaceManagerError> {
    let (connection, _monitor_handle, mut messages) =
        new_multicast_connection(&[MulticastGroup::Link, MulticastGroup::Ipv6Ifaddr])?;
    tokio::spawn(connection);

    let mut attached = BTreeMap::new();
    let mut events_open = true;
    let mut interval = tokio::time::interval(Duration::from_secs(2));
    interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    return Ok(());
                }
            }
            changed = config.changed() => {
                if changed.is_err() {
                    return Ok(());
                }
            }
            message = messages.next(), if events_open => {
                if message.is_none() {
                    events_open = false;
                    warn!("interface netlink event stream ended; continuing with periodic scans");
                }
            }
            _ = interval.tick() => {}
        }
        let desired = config.borrow().clone();
        if let Err(error) = reconcile(&router, &desired, &mut attached).await {
            if matches!(error, InterfaceManagerError::RouterStopped) {
                return Err(error);
            }
            warn!(%error, "interface reconciliation failed; retrying");
        }
    }
}

async fn reconcile(
    router: &RouterHandle,
    config: &Config,
    attached: &mut BTreeMap<String, InterfaceSignature>,
) -> Result<(), InterfaceManagerError> {
    let current = snapshot_interfaces().map_err(InterfaceManagerError::Enumerate)?;
    let status = router
        .status()
        .await
        .map_err(|_| InterfaceManagerError::RouterStopped)?;
    let active: HashSet<_> = status.interfaces.into_iter().collect();
    attached.retain(|name, _| active.contains(name));

    let stale: Vec<_> = attached
        .iter()
        .filter(|(name, signature)| {
            !config.matches_interface(name)
                || current.get(*name).is_none_or(|value| value != *signature)
        })
        .map(|(name, _)| name.clone())
        .collect();
    for name in stale {
        match router.remove_interface(name.clone()).await {
            Ok(()) | Err(RouterError::InterfaceNotFound(_)) => {}
            Err(RouterError::Stopped) => return Err(InterfaceManagerError::RouterStopped),
            Err(error) => warn!(interface = %name, %error, "failed to detach Babel interface"),
        }
        attached.remove(&name);
        info!(interface = %name, "detached Babel interface");
    }

    for (name, signature) in current {
        if attached.contains_key(&name)
            || !config.matches_interface(&name)
            || !signature.up
            || signature.link_local_addresses.is_empty()
        {
            continue;
        }
        match router.add_interface(name.clone()).await {
            Ok(()) => {
                info!(interface = %name, index = signature.index, "attached Babel interface");
                attached.insert(name, signature);
            }
            Err(RouterError::Stopped) => return Err(InterfaceManagerError::RouterStopped),
            Err(error) => debug!(interface = %name, %error, "Babel interface is not ready"),
        }
    }
    Ok(())
}

fn snapshot_interfaces() -> io::Result<BTreeMap<String, InterfaceSignature>> {
    let addresses = link_local_addresses()?;
    let mut result = BTreeMap::new();
    for entry in fs::read_dir("/sys/class/net")? {
        let entry = entry?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Ok(index) = fs::read_to_string(entry.path().join("ifindex")) else {
            continue;
        };
        let Ok(index) = index.trim().parse() else {
            continue;
        };
        let Ok(flags) = fs::read_to_string(entry.path().join("flags")) else {
            continue;
        };
        let Ok(flags) = u32::from_str_radix(flags.trim().trim_start_matches("0x"), 16) else {
            continue;
        };
        let mut link_local_addresses = addresses.get(&name).cloned().unwrap_or_default();
        link_local_addresses.sort();
        result.insert(
            name,
            InterfaceSignature {
                index,
                up: flags & 1 != 0,
                link_local_addresses,
            },
        );
    }
    Ok(result)
}

fn link_local_addresses() -> io::Result<HashMap<String, Vec<Ipv6Addr>>> {
    let mut result: HashMap<String, Vec<Ipv6Addr>> = HashMap::new();
    let contents = fs::read_to_string(Path::new("/proc/net/if_inet6"))?;
    for line in contents.lines() {
        let fields: Vec<_> = line.split_ascii_whitespace().collect();
        if fields.len() != 6 || fields[0].len() != 32 {
            continue;
        }
        let mut raw = [0u8; 16];
        let mut valid = true;
        for (index, byte) in raw.iter_mut().enumerate() {
            match u8::from_str_radix(&fields[0][index * 2..index * 2 + 2], 16) {
                Ok(value) => *byte = value,
                Err(_) => {
                    valid = false;
                    break;
                }
            }
        }
        if valid {
            let address = Ipv6Addr::from(raw);
            if address.is_unicast_link_local() {
                result
                    .entry(fields[5].to_owned())
                    .or_default()
                    .push(address);
            }
        }
    }
    Ok(result)
}
