use std::collections::HashSet;
use std::fs;
use std::path::Path;
use std::sync::Arc;

use ipnet::IpNet;
use serde::Deserialize;
use thiserror::Error;

use babel_proto::{
    EtxMetric, MetricProfile, RouteKey, RouteSelectionConfig, RttMetric, WiredMetric,
};

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub router_id: Option<String>,
    #[serde(default = "default_state_file")]
    pub state_file: String,
    pub interfaces: Interfaces,
    pub metric: Option<MetricConfig>,
    #[serde(default)]
    pub route_selection: RouteSelection,
    #[serde(default)]
    pub origins: Vec<Origin>,
    pub export: Export,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum Interfaces {
    Legacy(Vec<String>),
    Sections(Vec<InterfaceSection>),
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct InterfaceSection {
    #[serde(rename = "match")]
    pub patterns: Vec<String>,
    #[serde(default)]
    pub link_type: LinkType,
    pub split_horizon: Option<bool>,
    pub hello_interval_ms: Option<u64>,
    pub update_interval_ms: Option<u64>,
    pub metric: Option<MetricConfig>,
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
    pub section: usize,
    pub link_type: LinkType,
    pub metric: MetricConfig,
    pub hello_interval_cs: u16,
    pub update_interval_cs: u16,
    pub split_horizon: bool,
}

impl EffectiveInterface {
    pub fn build_policy(&self) -> Result<babel_proto::InterfacePolicy, ConfigError> {
        Ok(babel_proto::InterfacePolicy {
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
        #[serde(default = "default_rtt_half_life_ms")]
        half_life_ms: u64,
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
                half_life_ms: default_rtt_half_life_ms(),
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
                    *half_life_ms,
                    min_rtt_us,
                    max_rtt_us,
                    *max_penalty,
                )
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
    pub views: Vec<ExportView>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ExportView {
    pub table: u32,
    pub source: Option<IpNet>,
    pub rule_priority: Option<u32>,
}

impl ExportView {
    pub fn effective_rule_priority(self) -> u32 {
        self.rule_priority.unwrap_or(self.table)
    }
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("read configuration: {0}")]
    Read(#[from] std::io::Error),
    #[error("parse TOML configuration: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("interfaces must not be empty")]
    NoInterfaces,
    #[error("top-level metric is only valid with the legacy interfaces = [...] syntax")]
    LegacyMetricWithInterfaceSections,
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
    #[error(
        "overlapping source-specific export views are not supported by the Linux exporter: {0} and {1}"
    )]
    OverlappingSourceViews(IpNet, IpNet),
    #[error("rule_priority is only valid on a source-specific export view")]
    OrdinaryRulePriority,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        Self::parse(&fs::read_to_string(path)?)
    }

    pub fn parse(contents: &str) -> Result<Self, ConfigError> {
        let mut value: Self = toml::from_str(contents)?;
        for origin in &mut value.origins {
            origin.source = origin.source.filter(|source| source.prefix_len() != 0);
        }
        for view in &mut value.export.views {
            view.source = view.source.filter(|source| source.prefix_len() != 0);
        }
        value.validate()?;
        Ok(value)
    }

    fn validate(&self) -> Result<(), ConfigError> {
        let mut interfaces = HashSet::new();
        match &self.interfaces {
            Interfaces::Legacy(patterns) => {
                validate_patterns(patterns, &mut interfaces)?;
                self.metric.clone().unwrap_or_default().build()?;
            }
            Interfaces::Sections(sections) => {
                if self.metric.is_some() {
                    return Err(ConfigError::LegacyMetricWithInterfaceSections);
                }
                if sections.is_empty() {
                    return Err(ConfigError::NoInterfaces);
                }
                for section in sections {
                    validate_patterns(&section.patterns, &mut interfaces)?;
                    effective_interval(section.hello_interval_ms, "hello_interval_ms")?;
                    effective_update_interval(section)?;
                    section
                        .metric
                        .clone()
                        .unwrap_or_else(|| MetricConfig::for_link_type(section.link_type))
                        .build()?;
                }
            }
        }
        if self.export.protocol == 0 {
            return Err(ConfigError::InvalidProtocol);
        }
        if self.export.views.is_empty() {
            return Err(ConfigError::NoExportViews);
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
        let sources: Vec<_> = self
            .export
            .views
            .iter()
            .filter_map(|view| view.source)
            .collect();
        for (index, left) in sources.iter().enumerate() {
            for right in &sources[index + 1..] {
                if prefixes_overlap(*left, *right) {
                    return Err(ConfigError::OverlappingSourceViews(*left, *right));
                }
            }
        }
        let mut origins = HashSet::new();
        for origin in &self.origins {
            let key = origin.key()?;
            if origin.metric == babel_proto::INFINITY {
                return Err(ConfigError::InvalidOriginMetric);
            }
            if !origins.insert(key) {
                return Err(ConfigError::DuplicateOrigin(key));
            }
        }
        if self.route_selection.switch_margin_percent > 100 {
            return Err(ConfigError::InvalidRouteSelection(
                "switch_margin_percent must be in 0..=100".into(),
            ));
        }
        if self.route_selection.switch_margin_metric == babel_proto::INFINITY {
            return Err(ConfigError::InvalidRouteSelection(
                "switch_margin_metric must be below infinity".into(),
            ));
        }
        Ok(())
    }

    pub fn effective_interface(&self, name: &str) -> Option<EffectiveInterface> {
        match &self.interfaces {
            Interfaces::Legacy(patterns) => patterns
                .iter()
                .any(|pattern| wildcard_match(pattern, name))
                .then(|| EffectiveInterface {
                    section: 0,
                    link_type: LinkType::Wired,
                    metric: self.metric.clone().unwrap_or_default(),
                    hello_interval_cs: DEFAULT_HELLO_INTERVAL_CS,
                    update_interval_cs: DEFAULT_UPDATE_INTERVAL_CS,
                    split_horizon: true,
                }),
            Interfaces::Sections(sections) => {
                sections.iter().enumerate().find_map(|(index, item)| {
                    item.patterns
                        .iter()
                        .any(|pattern| wildcard_match(pattern, name))
                        .then(|| {
                            let hello_interval_cs =
                                effective_interval(item.hello_interval_ms, "hello_interval_ms")
                                    .expect("validated interface interval");
                            EffectiveInterface {
                                section: index,
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
        }
    }

    pub fn reload_identity_matches(&self, candidate: &Self) -> bool {
        self.router_id == candidate.router_id
            && self.state_file == candidate.state_file
            && self.route_selection == candidate.route_selection
            && self.export.protocol == candidate.export.protocol
    }
}

const DEFAULT_HELLO_INTERVAL_CS: u16 = 400;
const DEFAULT_UPDATE_INTERVAL_CS: u16 = 1600;

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
fn default_rtt_half_life_ms() -> u64 {
    RttMetric::DEFAULT_HALF_LIFE_MS
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
mod tests {
    use super::*;

    #[test]
    fn strict_config_supports_policy_views() {
        let config: Config = toml::from_str(
            r#"
interfaces = ["wg0"]
[[origins]]
destination = "192.0.2.0/24"
source = "10.0.0.0/8"
[export]
manage_rules = true
[[export.views]]
table = 20000
[[export.views]]
table = 20001
source = "10.0.0.0/8"
"#,
        )
        .unwrap();
        config.validate().unwrap();
        assert!(config.origins[0].key().unwrap().source.is_some());
        assert_eq!(config.export.views[1].effective_rule_priority(), 20001);
    }

    #[test]
    fn mixed_families_are_rejected() {
        let origin = Origin {
            destination: "192.0.2.0/24".parse().unwrap(),
            source: Some("2001:db8::/32".parse().unwrap()),
            metric: 0,
        };
        assert!(matches!(
            origin.key(),
            Err(ConfigError::MixedAddressFamilies)
        ));
    }

    #[test]
    fn source_views_cannot_collide_in_one_table() {
        let config: Config = toml::from_str(
            r#"
interfaces = ["wg0"]
[export]
[[export.views]]
table = 20001
source = "10.0.0.0/8"
[[export.views]]
table = 20001
source = "10.1.0.0/16"
"#,
        )
        .unwrap();
        assert!(matches!(
            config.validate(),
            Err(ConfigError::SharedSourceTable)
        ));
    }

    #[test]
    fn overlapping_source_views_are_rejected_even_in_different_tables() {
        let error = Config::parse(
            r#"
interfaces = ["wg0"]
[export]
[[export.views]]
table = 20001
source = "10.0.0.0/8"
[[export.views]]
table = 20002
source = "10.1.0.0/16"
"#,
        )
        .unwrap_err();
        assert!(matches!(error, ConfigError::OverlappingSourceViews(_, _)));
    }

    #[test]
    fn zero_length_source_is_normalised_to_the_ordinary_view() {
        let config = Config::parse(
            r#"
interfaces = ["wg0"]
[export]
[[export.views]]
table = 20000
source = "0.0.0.0/0"
"#,
        )
        .unwrap();
        assert_eq!(config.export.views[0].source, None);
    }

    #[test]
    fn interface_patterns_support_exact_star_and_question() {
        let config: Config = toml::from_str(
            r#"
interfaces = ["vl-*", "backbone?"]
[export]
[[export.views]]
table = 20000
"#,
        )
        .unwrap();
        config.validate().unwrap();
        assert!(config.effective_interface("vl-a-b").is_some());
        assert!(config.effective_interface("backbone0").is_some());
        assert!(config.effective_interface("access0").is_none());
        assert!(config.effective_interface("backbone10").is_none());
    }

    #[test]
    fn duplicate_interface_patterns_are_rejected() {
        let config: Config = toml::from_str(
            r#"
interfaces = ["vl-*", "vl-*"]
[export]
[[export.views]]
table = 20000
"#,
        )
        .unwrap();
        assert!(matches!(
            config.validate(),
            Err(ConfigError::DuplicateInterfacePattern(_))
        ));
    }

    #[test]
    fn interface_sections_use_first_match_and_type_defaults() {
        let config = Config::parse(
            r#"
[[interfaces]]
match = ["vl-special-*"]
link_type = "wireless"
hello_interval_ms = 1000

[[interfaces]]
match = ["vl-*"]
link_type = "tunnel"

[export]
[[export.views]]
table = 20000
"#,
        )
        .unwrap();

        let special = config.effective_interface("vl-special-0").unwrap();
        assert_eq!(special.section, 0);
        assert_eq!(special.link_type, LinkType::Wireless);
        assert_eq!(special.metric.build().unwrap().name(), "etx");
        assert!(!special.split_horizon);
        assert_eq!(special.hello_interval_cs, 100);
        assert_eq!(special.update_interval_cs, 400);

        let tunnel = config.effective_interface("vl-normal-0").unwrap();
        assert_eq!(tunnel.section, 1);
        assert_eq!(tunnel.metric.build().unwrap().name(), "rtt(wired)");
        assert!(tunnel.split_horizon);
        assert_eq!(tunnel.hello_interval_cs, 400);
        assert_eq!(tunnel.update_interval_cs, 1600);
        assert!(config.effective_interface("eth0").is_none());
    }

    #[test]
    fn explicit_interface_values_replace_type_defaults() {
        let config = Config::parse(
            r#"
[[interfaces]]
match = ["mesh0"]
link_type = "wireless"
split_horizon = true
hello_interval_ms = 2500
update_interval_ms = 7000
[interfaces.metric]
type = "wired"
nominal_cost = 128

[export]
[[export.views]]
table = 20000
"#,
        )
        .unwrap();
        let policy = config.effective_interface("mesh0").unwrap();
        assert_eq!(policy.metric.build().unwrap().name(), "wired");
        assert!(policy.split_horizon);
        assert_eq!(policy.hello_interval_cs, 250);
        assert_eq!(policy.update_interval_cs, 700);
    }

    #[test]
    fn structured_interfaces_reject_legacy_global_metric() {
        let error = Config::parse(
            r#"
[[interfaces]]
match = ["eth0"]
[metric]
type = "wired"
[export]
[[export.views]]
table = 20000
"#,
        )
        .unwrap_err();
        assert!(matches!(
            error,
            ConfigError::LegacyMetricWithInterfaceSections
        ));
    }

    #[test]
    fn interface_intervals_require_wire_representable_centiseconds() {
        for value in [0, 11, 655_360] {
            let error = Config::parse(&format!(
                r#"
[[interfaces]]
match = ["eth0"]
hello_interval_ms = {value}
[export]
[[export.views]]
table = 20000
"#
            ))
            .unwrap_err();
            assert!(matches!(
                error,
                ConfigError::InvalidInterfaceInterval { .. }
            ));
        }
    }

    #[test]
    fn omitted_metric_uses_rfc_wired_defaults() {
        let config = Config::parse(
            r#"
interfaces = ["eth0"]
[export]
[[export.views]]
table = 20000
"#,
        )
        .unwrap();
        assert_eq!(config.metric, None);
        assert_eq!(
            config
                .effective_interface("eth0")
                .unwrap()
                .metric
                .build()
                .unwrap()
                .name(),
            "wired"
        );
    }

    #[test]
    fn rtt_metric_supports_a_configured_etx_base() {
        let config = Config::parse(
            r#"
interfaces = ["mesh0"]
[metric]
type = "rtt"
probe_interval_ms = 1500
half_life_ms = 5000
min_rtt_ms = 5
max_rtt_ms = 80
max_penalty = 200
[metric.base]
type = "etx"
window = 8
[route_selection]
switch_margin_percent = 7
switch_margin_metric = 12
better_for_ms = 5000
[export]
[[export.views]]
table = 20000
"#,
        )
        .unwrap();
        let profile = config.metric.as_ref().unwrap().build().unwrap();
        assert_eq!(profile.name(), "rtt(etx)");
        assert!(profile.timestamps_enabled());
        assert_eq!(profile.rtt_probe_interval_ms(), Some(1500));
        assert_eq!(config.route_selection.better_for_ms, 5000);
    }

    #[test]
    fn invalid_metric_parameters_are_rejected() {
        let error = Config::parse(
            r#"
interfaces = ["eth0"]
[metric]
type = "wired"
received = 4
window = 3
[export]
[[export.views]]
table = 20000
"#,
        )
        .unwrap_err();
        assert!(matches!(error, ConfigError::InvalidMetric(_)));
    }

    #[test]
    fn invalid_route_selection_margin_is_rejected() {
        let error = Config::parse(
            r#"
interfaces = ["eth0"]
[route_selection]
switch_margin_percent = 101
[export]
[[export.views]]
table = 20000
"#,
        )
        .unwrap_err();
        assert!(matches!(error, ConfigError::InvalidRouteSelection(_)));
    }

    #[test]
    fn duplicate_origins_are_rejected_before_reload_commit() {
        let error = Config::parse(
            r#"
interfaces = ["eth0"]
[[origins]]
destination = "2001:db8::/64"
[[origins]]
destination = "2001:db8::/64"
[export]
[[export.views]]
table = 20000
"#,
        )
        .unwrap_err();
        assert!(matches!(error, ConfigError::DuplicateOrigin(_)));
    }
}
