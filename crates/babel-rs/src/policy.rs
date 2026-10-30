//! Declarative route policy. Matching never rewrites a Babel route key.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use babel_protocol::{ExportContext, ImportContext, RouteKey, RoutePolicy};
use ipnet::IpNet;
use serde::{Deserialize, Serialize};

use crate::config::{Config, wildcard_match};

#[derive(Clone, Debug, Default, Deserialize, Serialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Policies {
    #[serde(default)]
    pub import: Filter,
    #[serde(default)]
    pub announce: Filter,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    #[default]
    Allow,
    Deny,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Filter {
    #[serde(default)]
    pub default: Action,
    #[serde(default)]
    pub rules: Vec<Rule>,
}

#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    pub action: Action,
    pub source: Option<PrefixMatch>,
    pub destination: Option<PrefixMatch>,
    pub interfaces: Option<Vec<String>>,
    pub neighbor: Option<IpAddr>,
    pub locally_originated: Option<bool>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PrefixMatch {
    pub prefix: IpNet,
    #[serde(rename = "match")]
    pub mode: MatchMode,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum MatchMode {
    Exact,
    Within,
}

#[derive(Clone, Copy)]
pub enum Stage {
    Import,
    Announce,
    Install,
}

impl PrefixMatch {
    fn matches(self, prefix: IpNet) -> bool {
        match self.mode {
            MatchMode::Exact => self.prefix == prefix,
            MatchMode::Within => self.prefix.contains(&prefix),
        }
    }
}

impl Filter {
    pub fn normalize(&mut self) {
        for rule in &mut self.rules {
            for matcher in [&mut rule.source, &mut rule.destination]
                .into_iter()
                .flatten()
            {
                matcher.prefix = matcher.prefix.trunc();
            }
        }
    }

    pub fn validate(&self, stage: Stage) -> Result<(), String> {
        for (index, rule) in self.rules.iter().enumerate() {
            let error =
                if rule
                    .source
                    .zip(rule.destination)
                    .is_some_and(|(s, d)| s.prefix.addr().is_ipv4() != d.prefix.addr().is_ipv4())
                {
                    Some("source and destination must use the same address family")
                } else if rule.interfaces.as_ref().is_some_and(|names| {
                    names.is_empty() || names.iter().any(|name| name.is_empty())
                }) {
                    Some("interfaces must contain nonempty patterns")
                } else if rule.neighbor.is_some() && !matches!(stage, Stage::Import) {
                    Some("neighbor is only supported by policy.import")
                } else if rule.locally_originated.is_some() && !matches!(stage, Stage::Announce) {
                    Some("locally_originated is only supported by policy.announce")
                } else if rule.interfaces.is_some() && matches!(stage, Stage::Install) {
                    Some("export filters support only source and destination conditions")
                } else {
                    None
                };
            if let Some(error) = error {
                return Err(format!("rule {}: {error}", index + 1));
            }
        }
        Ok(())
    }

    pub fn allows_key(&self, key: RouteKey) -> bool {
        self.allows(key, None, None, None)
    }

    fn allows(
        &self,
        key: RouteKey,
        interface: Option<&str>,
        neighbor: Option<IpAddr>,
        locally_originated: Option<bool>,
    ) -> bool {
        // RFC 9079 represents an ordinary route with a zero-length source.
        let source = key.source.unwrap_or_else(|| {
            let address = if key.destination.addr().is_ipv4() {
                IpAddr::V4(Ipv4Addr::UNSPECIFIED)
            } else {
                IpAddr::V6(Ipv6Addr::UNSPECIFIED)
            };
            IpNet::new(address, 0).expect("zero-length source prefix")
        });
        self.rules
            .iter()
            .find(|rule| {
                rule.source.is_none_or(|matcher| matcher.matches(source))
                    && rule
                        .destination
                        .is_none_or(|matcher| matcher.matches(key.destination))
                    && rule.interfaces.as_ref().is_none_or(|patterns| {
                        interface
                            .is_some_and(|name| patterns.iter().any(|p| wildcard_match(p, name)))
                    })
                    && rule
                        .neighbor
                        .is_none_or(|address| neighbor == Some(address))
                    && rule
                        .locally_originated
                        .is_none_or(|local| locally_originated == Some(local))
            })
            .map_or(self.default, |rule| rule.action)
            == Action::Allow
    }
}

/// User policies and forwarding capability checks are separate constraints.
pub struct DaemonPolicy {
    policies: Policies,
    source_support: crate::linux::SourcePolicy,
}

impl DaemonPolicy {
    pub fn new(config: &Config) -> Self {
        Self {
            policies: config.policy.clone(),
            source_support: crate::linux::SourcePolicy(config.export.clone()),
        }
    }
}

impl RoutePolicy for DaemonPolicy {
    fn accept(&self, route: &ImportContext<'_>) -> bool {
        self.source_support.accept(route)
            && self.policies.import.allows(
                route.key,
                Some(route.interface),
                Some(route.neighbor),
                None,
            )
    }

    fn announce(&self, route: &ExportContext<'_>) -> bool {
        self.source_support.announce(route)
            && self.policies.announce.allows(
                route.key,
                Some(route.interface),
                None,
                Some(route.locally_originated),
            )
    }
}

#[cfg(test)]
mod tests;
