//! Read-only admission and announcement policy, separate from metric composition.

use std::{fmt, net::IpAddr};

use crate::{RouteKey, RouterId};

/// A finite received Update, after fixed protocol checks and before admission.
#[derive(Clone, Copy, Debug)]
#[non_exhaustive]
pub struct ImportContext<'a> {
    pub key: RouteKey,
    pub interface: &'a str,
    /// The Babel speaker sending the Update, which can differ from its next hop.
    pub neighbor: IpAddr,
    pub next_hop: IpAddr,
    pub router_id: RouterId,
    pub seqno: u16,
    /// The peer's metric before local link cost is added.
    pub advertised_metric: u16,
}

/// A finite announcement on an outgoing interface, including unicast replies.
/// Policy is per interface: multicast cannot express per-neighbor export rules.
#[derive(Clone, Copy, Debug)]
#[non_exhaustive]
pub struct ExportContext<'a> {
    pub key: RouteKey,
    pub interface: &'a str,
    pub router_id: RouterId,
    pub locally_originated: bool,
}

/// Immutable allow/deny rules for Babel routes. Defaults allow all routes that
/// pass the engine's fixed protocol checks. Implement either method as needed.
///
/// Callbacks run synchronously inside the engine: they must return promptly,
/// perform no blocking I/O, and give the same answer for the same context during
/// a policy version's lifetime. Build a new policy and explicitly replace it
/// with [`crate::Event::ReplaceRoutePolicy`] to change rules; silently changing
/// shared state would leave existing candidates and advertisements inconsistent.
///
/// Rules cannot rewrite a prefix, sequence number or metric. Retractions bypass
/// callbacks, so denial never prevents cleanup. Export denial emits unreachable
/// Updates where needed; it is not a promise to hide the existence of a prefix.
pub trait RoutePolicy: Send + Sync + 'static {
    fn accept(&self, _route: &ImportContext<'_>) -> bool {
        true
    }

    fn announce(&self, _route: &ExportContext<'_>) -> bool {
        true
    }
}

impl fmt::Debug for dyn RoutePolicy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("RoutePolicy { .. }")
    }
}

/// Default policy: accept and announce all protocol-valid routes.
#[derive(Clone, Copy, Debug, Default)]
pub struct AllowAllRoutes;

impl RoutePolicy for AllowAllRoutes {}
