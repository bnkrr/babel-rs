//! Keep only startup FIB entries, never inject them into the protocol RIB.
//! Deadlines use boot time and survive restart loops in the same namespace.
use super::*;
use crate::config::Config;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Clone, Serialize, Deserialize)]
struct PendingRoute {
    route: RouteIdentity,
    deadline_ms: u64,
}

#[derive(Clone, Default, Serialize, Deserialize)]
struct Journal {
    version: u8,
    boot: String,
    namespace: String,
    protocol: u8,
    #[serde(default)]
    export_scope: String,
    pending: Vec<PendingRoute>,
}

#[derive(Clone, Default)]
pub(super) struct Recovery {
    path: Option<PathBuf>,
    journal: Journal,
    rules: Vec<RuleIdentity>,
    // Revocation must also remove the persisted entries after kernel cleanup.
    revoked: bool,
    dirty: bool,
}

pub(super) struct JournalWrite {
    path: PathBuf,
    data: String,
}

type WriteJob = (
    JournalWrite,
    tokio::sync::oneshot::Sender<std::io::Result<()>>,
);

/// A serial, dedicated I/O thread preserves write order even if a caller is
/// cancelled. Unlike Tokio blocking jobs, stalled fsync cannot hold up runtime
/// destruction. Ownership remains held until the thread exits or process dies.
#[derive(Clone)]
pub(super) struct JournalWriter(std::sync::mpsc::Sender<WriteJob>);

impl JournalWriter {
    pub(super) fn new(
        ownership: Arc<crate::ownership::ProtocolOwnership>,
    ) -> std::io::Result<Self> {
        let (tx, rx) = std::sync::mpsc::channel::<WriteJob>();
        std::thread::Builder::new()
            .name("babel-fib-journal".into())
            .spawn(move || {
                let _ownership = ownership;
                for (write, reply) in rx {
                    let _ = reply.send(crate::state::write_atomic(&write.path, &write.data));
                }
            })?;
        Ok(Self(tx))
    }

    pub(super) async fn save(&self, write: Option<JournalWrite>) -> Result<(), LinuxError> {
        if let Some(write) = write {
            let (tx, rx) = tokio::sync::oneshot::channel();
            self.0
                .send((write, tx))
                .map_err(|_| std::io::Error::other("FIB journal worker stopped"))?;
            rx.await
                .map_err(|_| std::io::Error::other("FIB journal write cancelled"))??;
        }
        Ok(())
    }
}

pub(super) fn boot_ms() -> Result<u64, std::io::Error> {
    let mut value = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: clock_gettime writes one initialized timespec through this pointer.
    if unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, &mut value) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok((value.tv_sec as u64).saturating_mul(1000) + value.tv_nsec as u64 / 1_000_000)
}

impl Recovery {
    fn load(
        config: &Config,
        routes: Vec<RouteIdentity>,
        mut rules: Vec<RuleIdentity>,
        now: u64,
    ) -> Result<Self, LinuxError> {
        let path = PathBuf::from(format!("{}.fib-recovery.json", config.state_file));
        let boot = std::fs::read_to_string("/proc/sys/kernel/random/boot_id")?;
        let namespace = std::fs::read_link("/proc/self/ns/net")?
            .to_string_lossy()
            .into_owned();
        let previous: Journal = match std::fs::read(&path) {
            Ok(data) => serde_json::from_slice(&data).map_err(std::io::Error::other)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Journal::default(),
            Err(e) => return Err(e.into()),
        };
        // Kernel entries no longer carry their original Babel source key or
        // admission context. A changed policy conservatively revokes recovery.
        let export_scope = recovery_scope(config)?;
        let scoped = config.policy.import != Default::default()
            || config
                .export
                .views
                .iter()
                .any(|view| view.filter != Default::default());
        let scope_changed = if previous.export_scope.is_empty() {
            scoped
        } else {
            previous.export_scope != export_scope
        };
        if scope_changed {
            rules.clear();
        }
        let compatible = previous.version == 1
            && previous.boot == boot
            && previous.namespace == namespace
            && previous.protocol == config.export.protocol;
        let old: HashMap<_, _> = if compatible {
            previous
                .pending
                .into_iter()
                .map(|p| (p.route, p.deadline_ms))
                .collect()
        } else {
            HashMap::new()
        };
        // Persist rejected identities with expired deadlines, rather than
        // forgetting them. A crash before kernel deletion must not renew them.
        let deadline = if scope_changed {
            0
        } else {
            now.saturating_add(u64::from(config.restart_recovery_seconds) * 1000)
        };
        let pending = routes
            .into_iter()
            .map(|route| PendingRoute {
                deadline_ms: old.get(&route).copied().unwrap_or(deadline).min(deadline),
                route,
            })
            .collect();
        let value = Self {
            path: Some(path),
            journal: Journal {
                version: 1,
                boot,
                namespace,
                protocol: config.export.protocol,
                export_scope,
                pending,
            },
            rules,
            revoked: false,
            dirty: false,
        };
        // Persist before any empty snapshot can be processed or the engine starts.
        value.save()?;
        Ok(value)
    }

    fn save(&self) -> Result<(), LinuxError> {
        if let Some(path) = &self.path {
            let data = serde_json::to_string(&self.journal).map_err(std::io::Error::other)?;
            crate::state::write_atomic(path, &data)?;
        }
        Ok(())
    }

    pub(super) fn revoke(&mut self) {
        self.revoked = true;
    }

    fn live(&self, pending: &PendingRoute, now: u64) -> bool {
        !self.revoked && now < pending.deadline_ms
    }

    pub(super) fn sources(&self, now: u64, export: &Export) -> BTreeMap<IpNet, u32> {
        if !export.automatic_sources() {
            return BTreeMap::new();
        }
        self.rules
            .iter()
            .filter(|rule| {
                !export
                    .views
                    .iter()
                    .any(|view| view.source == Some(rule.source))
                    && self
                        .journal
                        .pending
                        .iter()
                        .any(|p| p.route.table == rule.table && self.live(p, now))
            })
            .map(|r| (r.source, r.table))
            .collect()
    }

    pub(super) fn keep(
        &self,
        route: &RouteIdentity,
        confirmed: &HashSet<(u32, IpNet)>,
        now: u64,
    ) -> bool {
        !confirmed.contains(&(route.table, route.destination))
            && self
                .journal
                .pending
                .iter()
                .any(|p| &p.route == route && self.live(p, now))
    }

    pub(super) fn applied(
        &mut self,
        current: &HashSet<RouteIdentity>,
        confirmed: &HashSet<(u32, IpNet)>,
        now: u64,
    ) -> Result<Option<JournalWrite>, LinuxError> {
        let before = self.journal.pending.len();
        let revoked = self.revoked;
        self.journal.pending.retain(|p| {
            !revoked
                && now < p.deadline_ms
                && current.contains(&p.route)
                && !confirmed.contains(&(p.route.table, p.route.destination))
        });
        self.dirty |= before != self.journal.pending.len();
        if self.dirty {
            self.write_request()
        } else {
            Ok(None)
        }
    }

    pub(super) fn saved(&mut self) {
        self.dirty = false;
    }

    fn write_request(&self) -> Result<Option<JournalWrite>, LinuxError> {
        self.path
            .as_ref()
            .map(|path| {
                Ok(JournalWrite {
                    path: path.clone(),
                    data: serde_json::to_string(&self.journal).map_err(std::io::Error::other)?,
                })
            })
            .transpose()
    }

    pub(super) fn remaining(&self, now: u64) -> (usize, u64) {
        let pending: Vec<_> = self
            .journal
            .pending
            .iter()
            .filter(|p| self.live(p, now))
            .collect();
        (
            pending.len(),
            pending
                .iter()
                .map(|p| p.deadline_ms.saturating_sub(now))
                .max()
                .unwrap_or(0),
        )
    }
}

fn eligible_rule(rule: &RuleIdentity, export: &Export) -> bool {
    export.manage_rules
        && (export.views.iter().any(|v| {
            v.source == Some(rule.source)
                && v.table == rule.table
                && export.rule_priority(v) == rule.priority
        }) || (export.automatic_sources()
            && rule.table >= export.source_table_base
            && !export.views.iter().any(|v| v.table == rule.table)
            && export.rule_priority(&ExportView {
                table: rule.table,
                source: Some(rule.source),
                rule_priority: None,
                filter: Default::default(),
            }) == rule.priority))
}

fn allowed_indices(config: &Config) -> Result<HashSet<u32>, LinuxError> {
    let mut indices = HashSet::new();
    for entry in std::fs::read_dir("/sys/class/net")? {
        let name = entry?.file_name().to_string_lossy().into_owned();
        if config.effective_interface(&name).is_some() {
            indices.insert(netlink::interface_index(&name)?);
        }
    }
    Ok(indices)
}

impl LinuxExporter {
    pub async fn begin_recovery(&self, config: &Config) -> Result<(), LinuxError> {
        let _guard = self.apply_lock.lock().await;
        let indices = allowed_indices(config)?;
        let rules: Vec<_> = self
            .owned_rules(config.export.protocol)
            .await?
            .iter()
            .filter_map(rule_identity)
            .filter(|r| eligible_rule(r, &config.export))
            .collect();
        let routes = self
            .owned_routes(config.export.protocol)
            .await?
            .iter()
            .filter(|r| r.header.kind == RouteType::Unicast)
            .filter_map(route_identity)
            .filter(|r| {
                !r.unreachable
                    && r.priority >= DYNAMIC_PRIORITY_BASE
                    && r.priority < DYNAMIC_PRIORITY_BASE + u32::from(INFINITY)
                    && indices.contains(&r.output_interface)
                    && (config.export.views.iter().any(|v| v.table == r.table)
                        || rules.iter().any(|v| v.table == r.table))
            })
            .collect();
        *self.recovery.lock().await = Recovery::load(config, routes, rules, boot_ms()?)?;
        Ok(())
    }

    pub async fn recovery_interfaces(&self, config: &Config) -> Result<(), LinuxError> {
        let _guard = self.apply_lock.lock().await;
        let indices = allowed_indices(config)?;
        let mut recovery = self.recovery.lock().await;
        let scope_changed = recovery.journal.export_scope != recovery_scope(config)?;
        // Keep the old deadline as a tombstone until deletion succeeds.
        for p in &mut recovery.journal.pending {
            if scope_changed || !indices.contains(&p.route.output_interface) {
                p.deadline_ms = 0;
            }
        }
        // Keep the startup scope in the journal through live reload. Newly
        // installed routes also lack their admission context; the next restart
        // must detect the change even if this process dies before reconciliation.
        recovery.dirty = true;
        let write = recovery.write_request()?;
        drop(recovery);
        self.journal_writer.save(write).await?;
        self.recovery.lock().await.saved();
        Ok(())
    }
}

fn recovery_scope(config: &Config) -> Result<String, LinuxError> {
    serde_json::to_string(&(&config.export, &config.policy.import))
        .map_err(|error| std::io::Error::other(error).into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn route(destination: &str) -> RouteIdentity {
        RouteIdentity {
            table: 201,
            destination: destination.parse().unwrap(),
            priority: DYNAMIC_PRIORITY_BASE + 96,
            output_interface: 7,
            gateway: None,
            unreachable: false,
        }
    }

    #[test]
    fn changed_scope_rejection_survives_crash_before_kernel_cleanup() {
        for import in [false, true] {
            let root = std::env::temp_dir()
                .join(format!("babel-fib-scope-{}-{import}", std::process::id()));
            std::fs::create_dir_all(&root).unwrap();
            let mut config = Config::parse(&format!(
                "state_file = {:?}\nrestart_recovery_seconds = 10\n[[interfaces]]\nmatch = [\"wg*\"]\n[export]\n[[export.views]]\ntable = 201\n",
                root.join("state").to_str().unwrap())).unwrap();
            let routes = vec![route("192.0.2.1/32")];
            let first = Recovery::load(&config, routes.clone(), vec![], 1000).unwrap();
            assert_eq!(first.remaining(1000), (1, 10_000));
            if import {
                config.policy.import.default = crate::policy::Action::Deny;
            } else {
                config.export.views[0].filter.default = crate::policy::Action::Deny;
            }
            let changed = Recovery::load(&config, routes.clone(), vec![], 2000).unwrap();
            assert_eq!(changed.remaining(2000), (0, 0));
            // Kernel still has the old route: simulate death before apply.
            let restarted = Recovery::load(&config, routes.clone(), vec![], 3000).unwrap();
            assert!(!restarted.keep(&routes[0], &HashSet::new(), 3000));
            assert_eq!(restarted.remaining(3000), (0, 0));
            // Once kernel deletion completes, a genuinely fresh installation
            // can be recovered on a later restart.
            Recovery::load(&config, vec![], vec![], 4000).unwrap();
            let fresh = Recovery::load(&config, routes, vec![], 5000).unwrap();
            assert_eq!(fresh.remaining(5000), (1, 10_000));
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn crash_loops_preserve_deadline_and_confirmation_releases_it() {
        let root = std::env::temp_dir().join(format!("babel-fib-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let mut config = Config::parse(&format!(
            "state_file = {:?}\nrestart_recovery_seconds = 10\n[[interfaces]]\nmatch = [\"wg*\"]\n[export]\n[[export.views]]\ntable = 201\n",
            root.join("state").to_str().unwrap()
        )).unwrap();
        let routes = vec![route("192.0.2.1/32"), route("2001:db8::1/128")];
        let current = routes.iter().cloned().collect();
        let first = Recovery::load(&config, routes.clone(), vec![], 1000).unwrap();
        assert_eq!(first.remaining(1000), (2, 10_000));
        let mut next = Recovery::load(&config, routes.clone(), vec![], 5000).unwrap();
        assert_eq!(next.remaining(5000), (2, 6000));
        assert!(!next.keep(&route("192.0.2.2/32"), &HashSet::new(), 5000));
        let confirmed = HashSet::from([(201, routes[0].destination)]);
        assert!(!next.keep(&routes[0], &confirmed, 5000));
        assert!(next.keep(&routes[1], &confirmed, 5000));
        let write = next.applied(&current, &confirmed, 5000).unwrap().unwrap();
        crate::state::write_atomic(&write.path, &write.data).unwrap();
        let expired = Recovery::load(&config, routes.clone(), vec![], 12000).unwrap();
        // A newly confirmed route may recover on the next crash. The old,
        // unconfirmed IPv6 route still expires at its original deadline.
        assert!(expired.keep(&routes[0], &HashSet::new(), 12000));
        assert!(!expired.keep(&routes[1], &HashSet::new(), 12000));
        config.restart_recovery_seconds = 0;
        let mut disabled = Recovery::load(&config, routes, vec![], 12000).unwrap();
        assert_eq!(disabled.remaining(12000), (0, 0));
        disabled.applied(&current, &HashSet::new(), 12000).unwrap();
        assert!(disabled.journal.pending.is_empty());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn automatic_source_table_survives_until_expiry_without_collision() {
        let export = super::super::tests::automatic_export();
        let source: IpNet = "2001:db8:1::/64".parse().unwrap();
        let rule = RuleIdentity {
            source,
            table: export.source_table_base,
            priority: export.rule_priority(&ExportView {
                source: Some(source),
                table: export.source_table_base,
                rule_priority: None,
                filter: Default::default(),
            }),
        };
        assert!(eligible_rule(&rule, &export));
        assert!(!eligible_rule(
            &RuleIdentity {
                priority: 1,
                ..rule
            },
            &export
        ));
        let mut pending = route("2001:db8::1/128");
        pending.table = rule.table;
        let mut recovery = Recovery {
            rules: vec![rule],
            journal: Journal {
                pending: vec![PendingRoute {
                    route: pending,
                    deadline_ms: 100,
                }],
                ..Journal::default()
            },
            ..Recovery::default()
        };
        let snapshot = RouteSnapshot {
            routes: vec![super::super::tests::selected(
                "2001:db8::2/128",
                Some("2001:db8:2::/64"),
                96,
            )],
            ..RouteSnapshot::default()
        };
        let mut allocated = BTreeMap::new();
        projection::source_views_with_recovery(
            &export,
            &snapshot,
            &mut allocated,
            &recovery.sources(10, &export),
        )
        .unwrap();
        assert_eq!(allocated[&source], rule.table);
        assert_ne!(allocated[&"2001:db8:2::/64".parse().unwrap()], rule.table);
        assert!(recovery.sources(100, &export).is_empty());
        recovery.revoke();
        assert!(recovery.sources(10, &export).is_empty());
    }
}
