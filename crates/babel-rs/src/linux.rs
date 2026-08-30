use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use babel_proto::SelectedRoute;
use babel_router::{RouteExporter, RouteSnapshot};
use futures::TryStreamExt;
use ipnet::{IpNet, Ipv4Net, Ipv6Net};
use netlink_packet_route::AddressFamily;
use netlink_packet_route::route::{
    RouteAddress, RouteAttribute, RouteMessage, RouteProtocol, RouteScope, RouteVia,
};
use netlink_packet_route::rule::{RuleAction, RuleAttribute, RuleMessage};
use rtnetlink::{Handle, IpVersion, RouteMessageBuilder, new_connection};
use thiserror::Error;
use tokio::sync::{Mutex, RwLock, watch};
use tokio::time::MissedTickBehavior;
use tracing::{debug, warn};

use crate::config::{Export, ExportView};

const DYNAMIC_PRIORITY_BASE: u32 = 65_535;

#[derive(Clone)]
pub struct LinuxExporter {
    handle: Handle,
    state: Arc<RwLock<ExportState>>,
    apply_lock: Arc<Mutex<()>>,
}

#[derive(Clone)]
struct ExportState {
    export: Export,
    snapshot: RouteSnapshot,
    retain_rules: bool,
    retired: Vec<Export>,
    last_success: Option<Instant>,
    last_error: Option<String>,
}

#[derive(Clone, Debug)]
pub struct ExportHealth {
    pub last_success_age: Option<Duration>,
    pub last_error: Option<String>,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct RouteIdentity {
    table: u32,
    destination: IpNet,
    priority: u32,
    output_interface: u32,
    gateway: Option<IpAddr>,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct RuleIdentity {
    table: u32,
    priority: u32,
    source: IpNet,
}

#[derive(Clone, Debug)]
struct ProjectedRoute {
    table: u32,
    selected: SelectedRoute,
    source_specific: bool,
}

#[derive(Debug, Error)]
pub enum LinuxError {
    #[error("open route netlink: {0}")]
    Open(#[from] std::io::Error),
    #[error("netlink request: {0}")]
    Netlink(#[from] rtnetlink::Error),
    #[error("invalid route: {0}")]
    InvalidRoute(String),
    #[error("interface {0} has no ifindex")]
    Interface(String),
}

impl LinuxExporter {
    pub fn new(export: Export) -> Result<Self, LinuxError> {
        let (connection, handle, _) = new_connection()?;
        tokio::spawn(connection);
        Ok(Self {
            handle,
            state: Arc::new(RwLock::new(ExportState {
                export,
                snapshot: RouteSnapshot::default(),
                retain_rules: true,
                retired: Vec::new(),
                last_success: None,
                last_error: None,
            })),
            apply_lock: Arc::new(Mutex::new(())),
        })
    }

    pub async fn update_export(&self, export: Export) {
        let _apply_guard = self.apply_lock.lock().await;
        {
            let mut state = self.state.write().await;
            let old_export = state.export.clone();
            if old_export.protocol != export.protocol && !state.retired.contains(&old_export) {
                state.retired.push(old_export);
            }
            state.export = export.clone();
        }
        if let Err(error) = self.reconcile_locked().await {
            warn!(%error, "route export after configuration reload failed; retry scheduled");
        }
    }

    pub async fn reconcile_current(&self) -> Result<(), LinuxError> {
        let _apply_guard = self.apply_lock.lock().await;
        self.reconcile_locked().await
    }

    pub async fn health(&self) -> ExportHealth {
        let state = self.state.read().await;
        ExportHealth {
            last_success_age: state.last_success.map(|value| value.elapsed()),
            last_error: state.last_error.clone(),
        }
    }

    async fn reconcile_locked(&self) -> Result<(), LinuxError> {
        let result = self.reconcile_attempt().await;
        let mut state = self.state.write().await;
        match &result {
            Ok(()) => {
                state.last_success = Some(Instant::now());
                state.last_error = None;
            }
            Err(error) => state.last_error = Some(error.to_string()),
        }
        result
    }

    async fn reconcile_attempt(&self) -> Result<(), LinuxError> {
        let state = self.state.read().await.clone();
        self.apply_locked(&state.export, state.snapshot, state.retain_rules)
            .await?;
        let mut cleaned = Vec::new();
        for retired in &state.retired {
            self.apply_locked(retired, RouteSnapshot::default(), false)
                .await?;
            cleaned.push(retired.clone());
        }
        if !cleaned.is_empty() {
            self.state
                .write()
                .await
                .retired
                .retain(|export| !cleaned.contains(export));
        }
        Ok(())
    }

    pub async fn run_reconciler(&self, mut shutdown: watch::Receiver<bool>) {
        let mut interval = tokio::time::interval(Duration::from_secs(2));
        interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
        interval.tick().await;
        loop {
            tokio::select! {
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        return;
                    }
                }
                _ = interval.tick() => {
                    if let Err(error) = self.reconcile_current().await {
                        warn!(%error, "periodic route export reconciliation failed");
                    }
                }
            }
        }
    }

    async fn apply_locked(
        &self,
        export: &Export,
        snapshot: RouteSnapshot,
        retain_rules: bool,
    ) -> Result<(), LinuxError> {
        let projected = project_routes(&export.views, &snapshot);
        debug!(
            generation = snapshot.generation,
            selected = snapshot.routes.len(),
            projected = projected.len(),
            protocol = export.protocol,
            "reconciling Linux route snapshot"
        );

        let mut desired = HashSet::new();
        let mut messages = Vec::with_capacity(projected.len());
        for route in projected {
            let ifindex = interface_index(&route.selected.interface)?;
            let priority = DYNAMIC_PRIORITY_BASE + u32::from(route.selected.metric);
            let mut builder = RouteMessageBuilder::<IpAddr>::new()
                .destination_prefix(
                    route.selected.key.destination.addr(),
                    route.selected.key.destination.prefix_len(),
                )
                .map_err(|error| LinuxError::InvalidRoute(error.to_string()))?
                .table_id(route.table)
                .protocol(RouteProtocol::Other(export.protocol))
                .output_interface(ifindex)
                .priority(priority);
            let gateway = if export.device_only {
                builder = builder.scope(RouteScope::Link);
                None
            } else {
                builder = builder
                    .gateway(route.selected.next_hop)
                    .map_err(|error| LinuxError::InvalidRoute(error.to_string()))?
                    .onlink();
                Some(route.selected.next_hop)
            };
            let message = builder.build();
            let identity = RouteIdentity {
                table: route.table,
                destination: route.selected.key.destination,
                priority,
                output_interface: ifindex,
                gateway,
            };
            desired.insert(identity.clone());
            debug!(
                table = route.table,
                destination = %route.selected.key.destination,
                source = ?route.selected.key.source,
                source_specific = route.source_specific,
                interface = %route.selected.interface,
                next_hop = %route.selected.next_hop,
                priority,
                "installing selected route"
            );
            messages.push((identity, message));
        }

        // Add the new generation before removing stale identities. A changed
        // metric is part of a Linux route identity, so explicit stale deletion
        // is required even after RouteReplace.
        let current = self.owned_routes(export.protocol).await?;
        let current_identities: HashSet<_> = current.iter().filter_map(route_identity).collect();
        for (identity, message) in messages {
            if !current_identities.contains(&identity) {
                self.handle.route().add(message).replace().execute().await?;
            }
        }
        for message in current {
            if let Some(identity) = route_identity(&message)
                && !desired.contains(&identity)
            {
                debug!(table = identity.table, destination = %identity.destination, "deleting stale owned route");
                self.handle.route().del(message).execute().await?;
            }
        }

        self.reconcile_rules(export, retain_rules).await?;
        Ok(())
    }

    async fn reconcile_rules(&self, export: &Export, retain: bool) -> Result<(), LinuxError> {
        let desired: HashSet<_> = if retain && export.manage_rules {
            export
                .views
                .iter()
                .filter_map(|view| {
                    Some(RuleIdentity {
                        table: view.table,
                        priority: view.effective_rule_priority(),
                        source: view.source?,
                    })
                })
                .collect()
        } else {
            HashSet::new()
        };
        let current = self.owned_rules(export.protocol).await?;
        let current_identities: HashSet<_> = current.iter().filter_map(rule_identity).collect();
        for rule in desired.difference(&current_identities) {
            self.add_rule(export.protocol, *rule).await?;
        }
        for message in current {
            if let Some(identity) = rule_identity(&message)
                && !desired.contains(&identity)
            {
                self.handle.rule().del(message).execute().await?;
            }
        }
        Ok(())
    }

    async fn add_rule(&self, protocol: u8, rule: RuleIdentity) -> Result<(), LinuxError> {
        let protocol = RouteProtocol::Other(protocol);
        match rule.source {
            IpNet::V4(source) => {
                let mut request = self
                    .handle
                    .rule()
                    .add()
                    .table_id(rule.table)
                    .priority(rule.priority)
                    .action(RuleAction::ToTable)
                    .v4()
                    .source_prefix(source.addr(), source.prefix_len());
                request
                    .message_mut()
                    .attributes
                    .push(RuleAttribute::Protocol(protocol));
                request.execute().await?;
            }
            IpNet::V6(source) => {
                let mut request = self
                    .handle
                    .rule()
                    .add()
                    .table_id(rule.table)
                    .priority(rule.priority)
                    .action(RuleAction::ToTable)
                    .v6()
                    .source_prefix(source.addr(), source.prefix_len());
                request
                    .message_mut()
                    .attributes
                    .push(RuleAttribute::Protocol(protocol));
                request.execute().await?;
            }
        }
        Ok(())
    }

    async fn owned_routes(&self, protocol: u8) -> Result<Vec<RouteMessage>, LinuxError> {
        let mut result = Vec::new();
        for query in [
            RouteMessageBuilder::<Ipv4Addr>::new().build(),
            RouteMessageBuilder::<Ipv6Addr>::new().build(),
        ] {
            let mut stream = self.handle.route().get(query).execute();
            while let Some(route) = stream.try_next().await? {
                if route.header.protocol == RouteProtocol::Other(protocol) {
                    result.push(route);
                }
            }
        }
        Ok(result)
    }

    async fn owned_rules(&self, protocol: u8) -> Result<Vec<RuleMessage>, LinuxError> {
        let mut result = Vec::new();
        for version in [IpVersion::V4, IpVersion::V6] {
            let mut stream = self.handle.rule().get(version).execute();
            while let Some(rule) = stream.try_next().await? {
                if rule.attributes.iter().any(|attribute| {
                    matches!(attribute, RuleAttribute::Protocol(value) if *value == RouteProtocol::Other(protocol))
                }) {
                    result.push(rule);
                }
            }
        }
        Ok(result)
    }
}

#[async_trait]
impl RouteExporter for LinuxExporter {
    async fn reconcile(
        &self,
        snapshot: RouteSnapshot,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let _apply_guard = self.apply_lock.lock().await;
        {
            let mut state = self.state.write().await;
            state.snapshot = snapshot;
            state.retain_rules = true;
        }
        self.reconcile_locked()
            .await
            .map_err(|error| Box::new(error) as _)
    }

    async fn shutdown(
        &self,
        snapshot: RouteSnapshot,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let _apply_guard = self.apply_lock.lock().await;
        {
            let mut state = self.state.write().await;
            state.snapshot = snapshot;
            state.retain_rules = false;
        }
        self.reconcile_locked()
            .await
            .map_err(|error| Box::new(error) as _)
    }
}

fn project_routes(views: &[ExportView], snapshot: &RouteSnapshot) -> Vec<ProjectedRoute> {
    let mut projected: HashMap<(u32, IpNet), ProjectedRoute> = HashMap::new();
    for view in views {
        for route in &snapshot.routes {
            let destination_is_v4 = route.key.destination.addr().is_ipv4();
            let route_source = route.key.source.filter(|source| source.prefix_len() != 0);
            let matches = match (view.source, route_source) {
                (None, None) => true,
                (Some(view_source), None) => view_source.addr().is_ipv4() == destination_is_v4,
                (Some(view_source), Some(route_source)) => view_source == route_source,
                (None, Some(_)) => false,
            };
            if !matches {
                continue;
            }
            let source_specific = route_source.is_some();
            let key = (view.table, route.key.destination);
            let replace = projected
                .get(&key)
                .is_none_or(|current| source_specific && !current.source_specific);
            if replace {
                projected.insert(
                    key,
                    ProjectedRoute {
                        table: view.table,
                        selected: route.clone(),
                        source_specific,
                    },
                );
            }
        }
    }
    let mut result: Vec<_> = projected.into_values().collect();
    result.sort_by_key(|route| (route.table, route.selected.key.destination));
    result
}

fn interface_index(name: &str) -> Result<u32, LinuxError> {
    std::fs::read_to_string(format!("/sys/class/net/{name}/ifindex"))
        .ok()
        .and_then(|value| value.trim().parse().ok())
        .ok_or_else(|| LinuxError::Interface(name.into()))
}

fn route_table(route: &RouteMessage) -> u32 {
    route
        .attributes
        .iter()
        .find_map(|attribute| {
            if let RouteAttribute::Table(value) = attribute {
                Some(*value)
            } else {
                None
            }
        })
        .unwrap_or(u32::from(route.header.table))
}

fn route_identity(route: &RouteMessage) -> Option<RouteIdentity> {
    let destination = route
        .attributes
        .iter()
        .find_map(|attribute| {
            if let RouteAttribute::Destination(value) = attribute {
                route_net(value.clone(), route.header.destination_prefix_length)
            } else {
                None
            }
        })
        .or_else(|| {
            match (
                route.header.address_family,
                route.header.destination_prefix_length,
            ) {
                (AddressFamily::Inet, 0) => {
                    Some(IpNet::V4(Ipv4Net::new(Ipv4Addr::UNSPECIFIED, 0).ok()?))
                }
                (AddressFamily::Inet6, 0) => {
                    Some(IpNet::V6(Ipv6Net::new(Ipv6Addr::UNSPECIFIED, 0).ok()?))
                }
                _ => None,
            }
        })?;
    let priority = route
        .attributes
        .iter()
        .find_map(|attribute| {
            if let RouteAttribute::Priority(value) = attribute {
                Some(*value)
            } else {
                None
            }
        })
        .unwrap_or(0);
    let output_interface = route
        .attributes
        .iter()
        .find_map(|attribute| {
            if let RouteAttribute::Oif(value) = attribute {
                Some(*value)
            } else {
                None
            }
        })
        .unwrap_or(0);
    let gateway = route
        .attributes
        .iter()
        .find_map(|attribute| match attribute {
            RouteAttribute::Gateway(value) => route_address(value),
            RouteAttribute::Via(RouteVia::Inet(value)) => Some(IpAddr::V4(*value)),
            RouteAttribute::Via(RouteVia::Inet6(value)) => Some(IpAddr::V6(*value)),
            _ => None,
        });
    Some(RouteIdentity {
        table: route_table(route),
        destination,
        priority,
        output_interface,
        gateway,
    })
}

fn rule_identity(rule: &RuleMessage) -> Option<RuleIdentity> {
    let table = rule
        .attributes
        .iter()
        .find_map(|attribute| {
            if let RuleAttribute::Table(value) = attribute {
                Some(*value)
            } else {
                None
            }
        })
        .unwrap_or(u32::from(rule.header.table));
    let priority = rule.attributes.iter().find_map(|attribute| {
        if let RuleAttribute::Priority(value) = attribute {
            Some(*value)
        } else {
            None
        }
    })?;
    let source_address = rule.attributes.iter().find_map(|attribute| {
        if let RuleAttribute::Source(value) = attribute {
            Some(*value)
        } else {
            None
        }
    })?;
    let source = match source_address {
        IpAddr::V4(address) => IpNet::V4(Ipv4Net::new(address, rule.header.src_len).ok()?),
        IpAddr::V6(address) => IpNet::V6(Ipv6Net::new(address, rule.header.src_len).ok()?),
    };
    Some(RuleIdentity {
        table,
        priority,
        source,
    })
}

fn route_net(value: RouteAddress, prefix: u8) -> Option<IpNet> {
    match value {
        RouteAddress::Inet(address) => Some(IpNet::V4(Ipv4Net::new(address, prefix).ok()?)),
        RouteAddress::Inet6(address) => Some(IpNet::V6(Ipv6Net::new(address, prefix).ok()?)),
        _ => None,
    }
}

fn route_address(value: &RouteAddress) -> Option<IpAddr> {
    match value {
        RouteAddress::Inet(address) => Some(IpAddr::V4(*address)),
        RouteAddress::Inet6(address) => Some(IpAddr::V6(*address)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use babel_proto::{RouteKey, RouterId};

    fn selected(destination: &str, source: Option<&str>, metric: u16) -> SelectedRoute {
        SelectedRoute {
            key: RouteKey::new(
                destination.parse().unwrap(),
                source.map(|value| value.parse().unwrap()),
            )
            .unwrap(),
            router_id: RouterId::new([1; 8]).unwrap(),
            seqno: 1,
            metric,
            next_hop: "fe80::1".parse().unwrap(),
            interface: "wg0".into(),
        }
    }

    #[test]
    fn ordinary_routes_are_materialized_into_every_matching_view() {
        let snapshot = RouteSnapshot {
            generation: 1,
            routes: vec![selected("192.0.2.0/24", None, 256)],
        };
        let views = vec![
            ExportView {
                table: 20000,
                source: None,
                rule_priority: None,
            },
            ExportView {
                table: 20001,
                source: Some("10.0.0.0/8".parse().unwrap()),
                rule_priority: None,
            },
        ];
        let routes = project_routes(&views, &snapshot);
        assert_eq!(routes.len(), 2);
        assert!(routes.iter().all(|route| !route.source_specific));
    }

    #[test]
    fn exact_source_route_overrides_ordinary_route_in_its_view() {
        let snapshot = RouteSnapshot {
            generation: 1,
            routes: vec![
                selected("0.0.0.0/0", None, 128),
                selected("0.0.0.0/0", Some("10.0.0.0/8"), 512),
            ],
        };
        let views = vec![ExportView {
            table: 20001,
            source: Some("10.0.0.0/8".parse().unwrap()),
            rule_priority: None,
        }];
        let routes = project_routes(&views, &snapshot);
        assert_eq!(routes.len(), 1);
        assert!(routes[0].source_specific);
        assert_eq!(routes[0].selected.metric, 512);
    }

    #[test]
    fn explicit_zero_source_is_an_ordinary_route() {
        let snapshot = RouteSnapshot {
            generation: 1,
            routes: vec![selected("203.0.113.0/24", Some("0.0.0.0/0"), 96)],
        };
        let views = vec![
            ExportView {
                table: 20000,
                source: None,
                rule_priority: None,
            },
            ExportView {
                table: 20001,
                source: Some("10.0.0.0/8".parse().unwrap()),
                rule_priority: None,
            },
        ];
        let routes = project_routes(&views, &snapshot);
        assert_eq!(routes.len(), 2);
        assert!(routes.iter().all(|route| !route.source_specific));
    }
}
