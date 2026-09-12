use std::collections::HashSet;
use std::fs;
use std::path::Path;
use std::sync::Arc;

use ipnet::IpNet;
use serde::Deserialize;
use thiserror::Error;

use babel_protocol::{
    EtxMetric, MetricProfile, RouteKey, RouteSelectionConfig, RttMetric, WiredMetric,
};

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub router_id: Option<String>,
    #[serde(default = "default_state_file")]
    pub state_file: String,
    #[serde(default = "default_shutdown_timeout_ms")]
    pub shutdown_timeout_ms: u32,
    pub interfaces: Vec<InterfaceSection>,
    #[serde(default)]
    pub route_selection: RouteSelection,
    #[serde(default)]
    pub limits: Limits,
    #[serde(default)]
    pub origins: Vec<Origin>,
    pub export: Export,
}

/// Optional overrides keep the protocol crate's defaults authoritative.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    pub max_neighbors: Option<usize>,
    pub max_candidates: Option<usize>,
    pub max_candidates_per_neighbor: Option<usize>,
}

impl Limits {
    pub fn effective(&self) -> babel_protocol::ResourceLimits {
        let defaults = babel_protocol::ResourceLimits::default();
        babel_protocol::ResourceLimits {
            max_neighbors: self.max_neighbors.unwrap_or(defaults.max_neighbors),
            max_candidates: self.max_candidates.unwrap_or(defaults.max_candidates),
            max_candidates_per_neighbor: self
                .max_candidates_per_neighbor
                .unwrap_or(defaults.max_candidates_per_neighbor),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct InterfaceSection {
    #[serde(rename = "match")]
    pub patterns: Vec<String>,
    #[serde(default)]
    pub link_type: LinkType,
    pub split_horizon: Option<bool>,
    #[serde(default)]
    pub ipv4_next_hop: Ipv4NextHop,
    #[serde(default)]
    pub control_transport: ControlTransport,
    pub hello_interval_ms: Option<u64>,
    pub update_interval_ms: Option<u64>,
    pub metric: Option<MetricConfig>,
    pub mac: Option<MacSection>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct MacSection {
    pub keys: Vec<MacKeyFile>,
    #[serde(default)]
    pub accept_unverified: bool,
    #[serde(skip)]
    pub resolved: Option<babel_router::MacConfig>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct MacKeyFile {
    #[serde(default)]
    pub algorithm: MacAlgorithm,
    /// A file containing only a hexadecimal key (optional surrounding whitespace).
    pub key_file: String,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum MacAlgorithm {
    #[default]
    HmacSha256,
    Blake2s128,
}

impl MacSection {
    fn resolve(&mut self) -> Result<(), ConfigError> {
        let mut keys = Vec::new();
        for key in &self.keys {
            let text = fs::read_to_string(&key.key_file)
                .map_err(|_| ConfigError::Mac("cannot read MAC key file".into()))?;
            let text = text.trim();
            if text.is_empty() || text.len() > 8192 || !text.len().is_multiple_of(2) {
                return Err(ConfigError::Mac(
                    "MAC key file must contain hexadecimal bytes".into(),
                ));
            }
            let bytes = text
                .as_bytes()
                .as_chunks::<2>()
                .0
                .iter()
                .map(|pair| {
                    let high = (pair[0] as char).to_digit(16);
                    let low = (pair[1] as char).to_digit(16);
                    high.zip(low)
                        .map(|(h, l)| (h * 16 + l) as u8)
                        .ok_or_else(|| {
                            ConfigError::Mac("MAC key file must contain hexadecimal bytes".into())
                        })
                })
                .collect::<Result<Vec<_>, _>>()?;
            let algorithm = match key.algorithm {
                MacAlgorithm::HmacSha256 => babel_router::MacAlgorithm::HmacSha256,
                MacAlgorithm::Blake2s128 => babel_router::MacAlgorithm::Blake2s128,
            };
            keys.push(
                babel_router::MacKey::new(algorithm, bytes)
                    .map_err(|e| ConfigError::Mac(e.to_string()))?,
            );
        }
        self.resolved = Some(
            babel_router::MacConfig::new(keys)
                .map_err(|e| ConfigError::Mac(e.to_string()))?
                .accept_unverified(self.accept_unverified),
        );
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum ControlTransport {
    #[default]
    Ipv6,
    Ipv4,
}
impl From<ControlTransport> for babel_protocol::ControlTransport {
    fn from(value: ControlTransport) -> Self {
        match value {
            ControlTransport::Ipv6 => Self::Ipv6,
            ControlTransport::Ipv4 => Self::Ipv4,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Ipv4NextHop {
    #[default]
    Auto,
    Ipv4,
    Ipv6,
}

impl From<Ipv4NextHop> for babel_protocol::Ipv4NextHop {
    fn from(value: Ipv4NextHop) -> Self {
        match value {
            Ipv4NextHop::Auto => Self::Auto,
            Ipv4NextHop::Ipv4 => Self::Ipv4,
            Ipv4NextHop::Ipv6 => Self::Ipv6,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum LinkType {
    #[default]
    Wired,
    Wireless,
    Tunnel,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EffectiveInterface {
    pub mac: Option<babel_router::MacConfig>,
    pub section: usize,
    pub control_transport: ControlTransport,
    pub link_type: LinkType,
    pub metric: MetricConfig,
    pub hello_interval_cs: u16,
    pub update_interval_cs: u16,
    pub split_horizon: bool,
    pub ipv4_next_hop: Ipv4NextHop,
}

impl EffectiveInterface {
    pub fn build_policy(&self) -> Result<babel_protocol::InterfacePolicy, ConfigError> {
        Ok(babel_protocol::InterfacePolicy {
            control_transport: self.control_transport.into(),
            ipv4_next_hop: self.ipv4_next_hop.into(),
            metric: self.metric.build()?,
            hello_interval_cs: self.hello_interval_cs,
            update_interval_cs: self.update_interval_cs,
            split_horizon: self.split_horizon,
        })
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum MetricConfig {
    Wired {
        #[serde(default = "default_wired_cost")]
        nominal_cost: u16,
        #[serde(default = "default_wired_received")]
        received: u8,
        #[serde(default = "default_wired_window")]
        window: u8,
    },
    Etx {
        #[serde(default = "default_etx_window")]
        window: u8,
    },
    Rtt {
        #[serde(default)]
        base: BaseMetricConfig,
        #[serde(default = "default_rtt_probe_interval_ms")]
        probe_interval_ms: u64,
        #[serde(default)]
        half_life_ms: Option<u64>,
        #[serde(default = "default_rtt_min_ms")]
        min_rtt_ms: u32,
        #[serde(default = "default_rtt_max_ms")]
        max_rtt_ms: u32,
        #[serde(default = "default_rtt_max_penalty")]
        max_penalty: u16,
    },
}

impl Default for MetricConfig {
    fn default() -> Self {
        Self::Wired {
            nominal_cost: default_wired_cost(),
            received: default_wired_received(),
            window: default_wired_window(),
        }
    }
}

impl MetricConfig {
    fn for_link_type(link_type: LinkType) -> Self {
        match link_type {
            LinkType::Wired => Self::default(),
            LinkType::Wireless => Self::Etx {
                window: default_etx_window(),
            },
            LinkType::Tunnel => Self::Rtt {
                base: BaseMetricConfig::default(),
                probe_interval_ms: default_rtt_probe_interval_ms(),
                half_life_ms: None,
                min_rtt_ms: default_rtt_min_ms(),
                max_rtt_ms: default_rtt_max_ms(),
                max_penalty: default_rtt_max_penalty(),
            },
        }
    }

    pub fn build(&self) -> Result<Arc<dyn MetricProfile>, ConfigError> {
        match self {
            Self::Wired {
                nominal_cost,
                received,
                window,
            } => WiredMetric::new(*nominal_cost, *received, *window)
                .map(|value| Arc::new(value) as Arc<dyn MetricProfile>)
                .ok_or_else(|| ConfigError::InvalidMetric("invalid wired parameters".into())),
            Self::Etx { window } => EtxMetric::new(*window)
                .map(|value| Arc::new(value) as Arc<dyn MetricProfile>)
                .ok_or_else(|| ConfigError::InvalidMetric("ETX window must be in 1..=16".into())),
            Self::Rtt {
                base,
                probe_interval_ms,
                half_life_ms,
                min_rtt_ms,
                max_rtt_ms,
                max_penalty,
            } => {
                let base = base.build()?;
                let min_rtt_us = min_rtt_ms
                    .checked_mul(1_000)
                    .ok_or_else(|| ConfigError::InvalidMetric("min_rtt_ms is too large".into()))?;
                let max_rtt_us = max_rtt_ms
                    .checked_mul(1_000)
                    .ok_or_else(|| ConfigError::InvalidMetric("max_rtt_ms is too large".into()))?;
                RttMetric::new(
                    base,
                    *probe_interval_ms,
                    half_life_ms.unwrap_or(RttMetric::DEFAULT_HALF_LIFE_MS),
                    min_rtt_us,
                    max_rtt_us,
                    *max_penalty,
                )
                .and_then(|value| {
                    if half_life_ms.is_none() {
                        value.with_sample_alpha(0.836)
                    } else {
                        Some(value)
                    }
                })
                .map(|value| Arc::new(value) as Arc<dyn MetricProfile>)
                .ok_or_else(|| {
                    ConfigError::InvalidMetric(format!(
                        "invalid RTT parameters (probe_interval_ms must be at least {})",
                        RttMetric::MIN_PROBE_INTERVAL_MS
                    ))
                })
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RouteSelection {
    #[serde(default = "default_switch_margin_percent")]
    pub switch_margin_percent: u8,
    #[serde(default = "default_switch_margin_metric")]
    pub switch_margin_metric: u16,
    #[serde(default = "default_better_for_ms")]
    pub better_for_ms: u64,
}

impl Default for RouteSelection {
    fn default() -> Self {
        Self {
            switch_margin_percent: default_switch_margin_percent(),
            switch_margin_metric: default_switch_margin_metric(),
            better_for_ms: default_better_for_ms(),
        }
    }
}

impl From<RouteSelection> for RouteSelectionConfig {
    fn from(value: RouteSelection) -> Self {
        Self {
            switch_margin_percent: value.switch_margin_percent,
            switch_margin_metric: value.switch_margin_metric,
            better_for_ms: value.better_for_ms,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum BaseMetricConfig {
    Wired {
        #[serde(default = "default_wired_cost")]
        nominal_cost: u16,
        #[serde(default = "default_wired_received")]
        received: u8,
        #[serde(default = "default_wired_window")]
        window: u8,
    },
    Etx {
        #[serde(default = "default_etx_window")]
        window: u8,
    },
}

impl Default for BaseMetricConfig {
    fn default() -> Self {
        Self::Wired {
            nominal_cost: default_wired_cost(),
            received: default_wired_received(),
            window: default_wired_window(),
        }
    }
}

impl BaseMetricConfig {
    fn build(&self) -> Result<Arc<dyn MetricProfile>, ConfigError> {
        match self {
            Self::Wired {
                nominal_cost,
                received,
                window,
            } => WiredMetric::new(*nominal_cost, *received, *window)
                .map(|value| Arc::new(value) as Arc<dyn MetricProfile>)
                .ok_or_else(|| ConfigError::InvalidMetric("invalid wired base parameters".into())),
            Self::Etx { window } => EtxMetric::new(*window)
                .map(|value| Arc::new(value) as Arc<dyn MetricProfile>)
                .ok_or_else(|| {
                    ConfigError::InvalidMetric("ETX base window must be in 1..=16".into())
                }),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Origin {
    pub destination: IpNet,
    pub source: Option<IpNet>,
    #[serde(default)]
    pub metric: u16,
}

impl Origin {
    pub fn key(&self) -> Result<RouteKey, ConfigError> {
        RouteKey::new(self.destination, self.source).ok_or(ConfigError::MixedAddressFamilies)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Export {
    #[serde(default = "default_protocol")]
    pub protocol: u8,
    #[serde(default)]
    pub device_only: bool,
    #[serde(default = "default_manage_rules")]
    pub manage_rules: bool,
    /// Allocate a view for every learned source prefix when rules are managed.
    #[serde(default = "default_manage_rules")]
    pub automatic_sources: bool,
    #[serde(default = "default_source_table_base")]
    pub source_table_base: u32,
    #[serde(default = "default_source_rule_priority")]
    pub source_rule_priority: u32,
    pub views: Vec<ExportView>,
}

fn default_source_table_base() -> u32 {
    1_000_000
}

fn default_source_rule_priority() -> u32 {
    10_000
}

impl Export {
    pub fn rule_priority(&self, view: ExportView) -> u32 {
        view.rule_priority.unwrap_or_else(|| {
            self.source_rule_priority
                .saturating_add(u32::from(128 - view.source.map_or(0, |s| s.prefix_len())))
        })
    }
    pub fn automatic_sources(&self) -> bool {
        self.automatic_sources && self.manage_rules
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ExportView {
    pub table: u32,
    pub source: Option<IpNet>,
    pub rule_priority: Option<u32>,
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("MAC configuration: {0}")]
    Mac(String),
    #[error("read configuration: {0}")]
    Read(#[from] std::io::Error),
    #[error("parse TOML configuration: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("interfaces must not be empty")]
    NoInterfaces,
    #[error("shutdown_timeout_ms must be greater than zero")]
    InvalidShutdownTimeout,
    #[error("invalid metric configuration: {0}")]
    InvalidMetric(String),
    #[error("invalid route-selection configuration: {0}")]
    InvalidRouteSelection(String),
    #[error("interface match patterns must not be empty")]
    EmptyInterfacePattern,
    #[error("interface match pattern {0} is duplicated")]
    DuplicateInterfacePattern(String),
    #[error("{field} must be a nonzero multiple of 10ms and at most 655350ms")]
    InvalidInterfaceInterval { field: &'static str },
    #[error("source and destination prefixes must use the same address family")]
    MixedAddressFamilies,
    #[error("origin {0:?} is duplicated")]
    DuplicateOrigin(RouteKey),
    #[error("origin metric must be below Babel infinity")]
    InvalidOriginMetric,
    #[error("export protocol must not be zero")]
    InvalidProtocol,
    #[error("export.views must not be empty")]
    NoExportViews,
    #[error("export view table {0} is reserved or invalid")]
    InvalidTable(u32),
    #[error("export view {0} is duplicated")]
    DuplicateView(String),
    #[error("source-specific export views in the same address family must use different tables")]
    SharedSourceTable,
    #[error("overlapping source views must query the more-specific source first: {0} and {1}")]
    SourceRuleOrder(IpNet, IpNet),
    #[error(
        "automatic source views require default rule priorities and source_table_base in 256..=2147483647"
    )]
    AutomaticSourceConfig,
    #[error("source_rule_priority must be in 1..=32638 and explicit priorities must be nonzero")]
    SourcePriorityRange,
    #[error("rule_priority is only valid on a source-specific export view")]
    OrdinaryRulePriority,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        Self::parse(&fs::read_to_string(path)?)
    }

    pub fn parse(contents: &str) -> Result<Self, ConfigError> {
        let mut value: Self = toml::from_str(contents)?;
        for interface in &mut value.interfaces {
            if let Some(mac) = &mut interface.mac {
                mac.resolve()?;
            }
        }
        for origin in &mut value.origins {
            origin.source = origin.source.filter(|source| source.prefix_len() != 0);
        }
        for view in &mut value.export.views {
            view.source = view
                .source
                .filter(|source| source.prefix_len() != 0)
                .map(|source| source.trunc());
        }
        value.validate()?;
        Ok(value)
    }

    fn validate(&self) -> Result<(), ConfigError> {
        if self.shutdown_timeout_ms == 0 {
            return Err(ConfigError::InvalidShutdownTimeout);
        }
        let mut interfaces = HashSet::new();
        if self.interfaces.is_empty() {
            return Err(ConfigError::NoInterfaces);
        }
        for section in &self.interfaces {
            validate_patterns(&section.patterns, &mut interfaces)?;
            effective_interval(section.hello_interval_ms, "hello_interval_ms")?;
            effective_update_interval(section)?;
            section
                .metric
                .clone()
                .unwrap_or_else(|| MetricConfig::for_link_type(section.link_type))
                .build()?;
        }
        if self.export.protocol == 0 {
            return Err(ConfigError::InvalidProtocol);
        }
        if self.export.views.is_empty() {
            return Err(ConfigError::NoExportViews);
        }
        if self.export.automatic_sources()
            && (!(256..=0x7fff_ffff).contains(&self.export.source_table_base)
                || self.export.views.iter().any(|v| {
                    v.rule_priority.is_some_and(|p| {
                        p != self.export.rule_priority(ExportView {
                            rule_priority: None,
                            ..*v
                        })
                    })
                }))
        {
            return Err(ConfigError::AutomaticSourceConfig);
        }
        if self.export.source_rule_priority == 0
            || self.export.source_rule_priority > 32_638
            || self.export.views.iter().any(|v| v.rule_priority == Some(0))
        {
            return Err(ConfigError::SourcePriorityRange);
        }
        let mut views = HashSet::new();
        let mut source_tables = HashSet::new();
        for view in &self.export.views {
            if view.table == 0 || matches!(view.table, 253..=255) {
                return Err(ConfigError::InvalidTable(view.table));
            }
            if view.source.is_none() && view.rule_priority.is_some() {
                return Err(ConfigError::OrdinaryRulePriority);
            }
            let key = (view.table, view.source);
            if !views.insert(key) {
                return Err(ConfigError::DuplicateView(format!(
                    "table={} source={:?}",
                    view.table, view.source
                )));
            }
            if let Some(source) = view.source {
                let family_key = (view.table, source.addr().is_ipv4());
                if !source_tables.insert(family_key) {
                    return Err(ConfigError::SharedSourceTable);
                }
            }
        }
        for (index, left) in self.export.views.iter().enumerate() {
            for right in &self.export.views[index + 1..] {
                if left.table == right.table && (left.source.is_none() || right.source.is_none()) {
                    return Err(ConfigError::SharedSourceTable);
                }
                if let (Some(l), Some(r)) = (left.source, right.source) {
                    if l == r {
                        return Err(ConfigError::DuplicateView(l.to_string()));
                    }
                    if prefixes_overlap(l, r)
                        && ((l.prefix_len() > r.prefix_len()
                            && self.export.rule_priority(*left)
                                >= self.export.rule_priority(*right))
                            || (r.prefix_len() > l.prefix_len()
                                && self.export.rule_priority(*right)
                                    >= self.export.rule_priority(*left)))
                    {
                        return Err(ConfigError::SourceRuleOrder(l, r));
                    }
                }
            }
        }
        let mut origins = HashSet::new();
        for origin in &self.origins {
            let key = origin.key()?;
            if origin.metric == babel_protocol::INFINITY {
                return Err(ConfigError::InvalidOriginMetric);
            }
            if !origins.insert(key) {
                return Err(ConfigError::DuplicateOrigin(key));
            }
        }
        RouteSelectionConfig::from(self.route_selection)
            .validate()
            .map_err(|error| ConfigError::InvalidRouteSelection(error.to_string()))?;
        Ok(())
    }

    pub fn effective_interface(&self, name: &str) -> Option<EffectiveInterface> {
        self.interfaces
            .iter()
            .enumerate()
            .find_map(|(index, item)| {
                item.patterns
                    .iter()
                    .any(|pattern| wildcard_match(pattern, name))
                    .then(|| {
                        let hello_interval_cs =
                            effective_interval(item.hello_interval_ms, "hello_interval_ms")
                                .expect("validated interface interval");
                        EffectiveInterface {
                            mac: item.mac.as_ref().and_then(|mac| mac.resolved.clone()),
                            section: index,
                            ipv4_next_hop: item.ipv4_next_hop,
                            control_transport: item.control_transport,
                            link_type: item.link_type,
                            metric: item
                                .metric
                                .clone()
                                .unwrap_or_else(|| MetricConfig::for_link_type(item.link_type)),
                            hello_interval_cs,
                            update_interval_cs: effective_update_interval(item)
                                .expect("validated interface interval"),
                            split_horizon: item
                                .split_horizon
                                .unwrap_or_else(|| item.link_type != LinkType::Wireless),
                        }
                    })
            })
    }

    pub fn reload_identity_matches(&self, candidate: &Self) -> bool {
        self.router_id == candidate.router_id
            && self.state_file == candidate.state_file
            && self.route_selection == candidate.route_selection
            && self.limits.effective() == candidate.limits.effective()
            && self.export.protocol == candidate.export.protocol
    }
}

const DEFAULT_HELLO_INTERVAL_CS: u16 = 400;

fn validate_patterns(patterns: &[String], seen: &mut HashSet<String>) -> Result<(), ConfigError> {
    if patterns.is_empty() {
        return Err(ConfigError::EmptyInterfacePattern);
    }
    for pattern in patterns {
        if pattern.is_empty() {
            return Err(ConfigError::EmptyInterfacePattern);
        }
        if !seen.insert(pattern.clone()) {
            return Err(ConfigError::DuplicateInterfacePattern(pattern.clone()));
        }
    }
    Ok(())
}

fn effective_interval(value_ms: Option<u64>, field: &'static str) -> Result<u16, ConfigError> {
    let value_ms = value_ms.unwrap_or(u64::from(DEFAULT_HELLO_INTERVAL_CS) * 10);
    if value_ms == 0 || value_ms > u64::from(u16::MAX) * 10 || !value_ms.is_multiple_of(10) {
        return Err(ConfigError::InvalidInterfaceInterval { field });
    }
    Ok((value_ms / 10) as u16)
}

fn effective_update_interval(section: &InterfaceSection) -> Result<u16, ConfigError> {
    match section.update_interval_ms {
        Some(value) => effective_interval(Some(value), "update_interval_ms"),
        None => effective_interval(section.hello_interval_ms, "hello_interval_ms")?
            .checked_mul(4)
            .ok_or(ConfigError::InvalidInterfaceInterval {
                field: "update_interval_ms",
            }),
    }
}

fn prefixes_overlap(left: IpNet, right: IpNet) -> bool {
    left.addr().is_ipv4() == right.addr().is_ipv4()
        && (left.contains(&right.network()) || right.contains(&left.network()))
}

fn wildcard_match(pattern: &str, value: &str) -> bool {
    let pattern = pattern.as_bytes();
    let value = value.as_bytes();
    let mut previous = vec![false; value.len() + 1];
    previous[0] = true;
    for token in pattern {
        let mut current = vec![false; value.len() + 1];
        if *token == b'*' {
            current[0] = previous[0];
        }
        for index in 1..=value.len() {
            current[index] = match token {
                b'*' => previous[index] || current[index - 1],
                b'?' => previous[index - 1],
                literal => previous[index - 1] && *literal == value[index - 1],
            };
        }
        previous = current;
    }
    previous[value.len()]
}

fn default_state_file() -> String {
    "/var/lib/babel-rs/router-id".into()
}
fn default_shutdown_timeout_ms() -> u32 {
    5_000
}
fn default_protocol() -> u8 {
    203
}
fn default_manage_rules() -> bool {
    true
}
fn default_wired_cost() -> u16 {
    WiredMetric::DEFAULT_NOMINAL_COST
}
fn default_wired_received() -> u8 {
    WiredMetric::DEFAULT_RECEIVED
}
fn default_wired_window() -> u8 {
    WiredMetric::DEFAULT_WINDOW
}
fn default_etx_window() -> u8 {
    EtxMetric::DEFAULT_WINDOW
}
fn default_rtt_probe_interval_ms() -> u64 {
    RttMetric::DEFAULT_PROBE_INTERVAL_MS
}
fn default_rtt_min_ms() -> u32 {
    RttMetric::DEFAULT_MIN_RTT_US / 1_000
}
fn default_rtt_max_ms() -> u32 {
    RttMetric::DEFAULT_MAX_RTT_US / 1_000
}
fn default_rtt_max_penalty() -> u16 {
    RttMetric::DEFAULT_MAX_PENALTY
}
fn default_switch_margin_percent() -> u8 {
    RouteSelectionConfig::default().switch_margin_percent
}
fn default_switch_margin_metric() -> u16 {
    RouteSelectionConfig::default().switch_margin_metric
}
fn default_better_for_ms() -> u64 {
    RouteSelectionConfig::default().better_for_ms
}

#[cfg(test)]
mod tests;
