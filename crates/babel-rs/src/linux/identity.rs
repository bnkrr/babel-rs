//! Parse kernel route and rule identities for reconciliation.

use super::*;

pub(super) fn route_table(route: &RouteMessage) -> u32 {
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

pub(super) fn route_identity(route: &RouteMessage) -> Option<RouteIdentity> {
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
        unreachable: route.header.kind == RouteType::Unreachable,
    })
}

pub(super) fn rule_identity(rule: &RuleMessage) -> Option<RuleIdentity> {
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

pub(super) fn route_net(value: RouteAddress, prefix: u8) -> Option<IpNet> {
    match value {
        RouteAddress::Inet(address) => Some(IpNet::V4(Ipv4Net::new(address, prefix).ok()?)),
        RouteAddress::Inet6(address) => Some(IpNet::V6(Ipv6Net::new(address, prefix).ok()?)),
        _ => None,
    }
}

pub(super) fn route_address(value: &RouteAddress) -> Option<IpAddr> {
    match value {
        RouteAddress::Inet(address) => Some(IpAddr::V4(*address)),
        RouteAddress::Inet6(address) => Some(IpAddr::V6(*address)),
        _ => None,
    }
}
