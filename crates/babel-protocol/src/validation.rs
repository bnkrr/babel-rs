use crate::{EngineConfig, Event, INFINITY, InterfacePolicy, RouteKey, RouteSelectionConfig};

/// Invalid local configuration supplied by an embedding application.
/// Wire decoding errors are reported separately by [`crate::WireError`].
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum ConfigError {
    /// Percentage margins are inclusive: both 0 and 100 are valid.
    #[error("switch_margin_percent must be in 0..=100")]
    SwitchMarginPercent,
    /// A finite absolute margin may be zero, but cannot be Babel infinity.
    #[error("switch_margin_metric must be below infinity")]
    SwitchMarginMetric,
    /// Periodic intervals use centiseconds and must be nonzero.
    #[error("{field} must be nonzero")]
    ZeroInterval { field: &'static str },
    /// Use [`RouteKey::new`] to normalize prefixes and check address families.
    #[error("route key must use canonical prefixes of the same address family")]
    InvalidRouteKey,
    /// Withdrawal is a separate operation, not an infinite local origin.
    #[error("originated route metric must be below Babel infinity")]
    InvalidOriginMetric,
}

impl RouteSelectionConfig {
    /// Check percentage and absolute margins. A zero dwell time is valid.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.switch_margin_percent > 100 {
            return Err(ConfigError::SwitchMarginPercent);
        }
        if self.switch_margin_metric == INFINITY {
            return Err(ConfigError::SwitchMarginMetric);
        }
        Ok(())
    }
}

impl InterfacePolicy {
    /// Check periodic Hello and Update intervals before activating the policy.
    /// Custom metric implementations must satisfy [`crate::MetricProfile`]'s contract.
    pub fn validate(&self) -> Result<(), ConfigError> {
        validate_intervals(self.hello_interval_cs, self.update_interval_cs)
    }
}

impl EngineConfig {
    /// Check all built-in configuration constraints without creating an engine.
    /// All resource limits, including zero, and all sequence numbers are valid.
    pub fn validate(&self) -> Result<(), ConfigError> {
        self.route_selection.validate()?;
        validate_intervals(self.hello_interval_cs, self.update_interval_cs)
    }
}

impl RouteKey {
    /// Reject unnormalized public struct literals or mixed address families.
    /// Construct keys with [`Self::new`] to obtain their canonical representation.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if Self::new(self.destination, self.source) != Some(*self) {
            return Err(ConfigError::InvalidRouteKey);
        }
        Ok(())
    }

    /// Check a local origin. Zero is a valid origin metric; infinity is not.
    pub fn validate_origin(&self, metric: u16) -> Result<(), ConfigError> {
        self.validate()?;
        if metric == INFINITY {
            return Err(ConfigError::InvalidOriginMetric);
        }
        Ok(())
    }
}

impl Event {
    /// Validate local policy and origin changes before mutating engine state.
    /// Received packets must come from [`crate::decode_packet`]; this method
    /// does not repeat wire validation or check monotonic clock progression.
    pub fn validate(&self) -> Result<(), ConfigError> {
        match self {
            Self::InterfaceUpWithPolicy { policy, .. }
            | Self::InterfacePolicyChanged { policy, .. } => policy.validate(),
            Self::Originate { key, metric, .. } => key.validate_origin(*metric),
            Self::Withdraw { key, .. } => key.validate(),
            Self::ReplaceOrigins { origins, .. } => {
                for (key, metric) in origins {
                    key.validate_origin(*metric)?;
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }
}

fn validate_intervals(hello_interval_cs: u16, update_interval_cs: u16) -> Result<(), ConfigError> {
    for (field, interval) in [
        ("hello_interval_cs", hello_interval_cs),
        ("update_interval_cs", update_interval_cs),
    ] {
        if interval == 0 {
            return Err(ConfigError::ZeroInterval { field });
        }
    }
    Ok(())
}
