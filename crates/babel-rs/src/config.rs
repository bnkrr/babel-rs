use std::collections::HashSet;
use std::fs;
use std::path::Path;

use ipnet::IpNet;
use serde::Deserialize;
use thiserror::Error;

use babel_proto::RouteKey;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub router_id: Option<String>,
    #[serde(default = "default_state_file")]
    pub state_file: String,
    pub interfaces: Vec<String>,
    #[serde(default)]
    pub origins: Vec<Origin>,
    pub export: Export,
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
    #[error("interface match patterns must not be empty")]
    EmptyInterfacePattern,
    #[error("interface match pattern {0} is duplicated")]
    DuplicateInterfacePattern(String),
    #[error("source and destination prefixes must use the same address family")]
    MixedAddressFamilies,
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
    #[error("rule_priority is only valid on a source-specific export view")]
    OrdinaryRulePriority,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        Self::parse(&fs::read_to_string(path)?)
    }

    pub fn parse(contents: &str) -> Result<Self, ConfigError> {
        let value: Self = toml::from_str(contents)?;
        value.validate()?;
        Ok(value)
    }

    fn validate(&self) -> Result<(), ConfigError> {
        if self.interfaces.is_empty() {
            return Err(ConfigError::NoInterfaces);
        }
        let mut interfaces = HashSet::new();
        for pattern in &self.interfaces {
            if pattern.is_empty() {
                return Err(ConfigError::EmptyInterfacePattern);
            }
            if !interfaces.insert(pattern) {
                return Err(ConfigError::DuplicateInterfacePattern(pattern.clone()));
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
        for origin in &self.origins {
            origin.key()?;
        }
        Ok(())
    }

    pub fn matches_interface(&self, name: &str) -> bool {
        self.interfaces
            .iter()
            .any(|pattern| wildcard_match(pattern, name))
    }

    pub fn reload_identity_matches(&self, candidate: &Self) -> bool {
        self.router_id == candidate.router_id && self.state_file == candidate.state_file
    }
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
        assert!(config.matches_interface("vl-a-b"));
        assert!(config.matches_interface("backbone0"));
        assert!(!config.matches_interface("access0"));
        assert!(!config.matches_interface("backbone10"));
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
}
