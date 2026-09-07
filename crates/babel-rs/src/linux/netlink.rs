//! Install desired state and remove stale state in the owned protocol scope.

use super::*;

impl LinuxExporter {
    pub(super) async fn apply_locked(
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
            let priority = DYNAMIC_PRIORITY_BASE
                + u32::from(
                    route
                        .selected
                        .as_ref()
                        .map_or(INFINITY, |value| value.metric),
                );
            let mut builder = RouteMessageBuilder::<IpAddr>::new()
                .destination_prefix(
                    route.key.destination.addr(),
                    route.key.destination.prefix_len(),
                )
                .map_err(|error| LinuxError::InvalidRoute(error.to_string()))?
                .table_id(route.table)
                .protocol(RouteProtocol::Other(export.protocol))
                .priority(priority);
            let (message, ifindex, gateway, kind) = if let Some(selected) = &route.selected {
                let ifindex = interface_index(&selected.interface)?;
                builder = builder.output_interface(ifindex);
                let gateway = if export.device_only {
                    builder = builder.scope(RouteScope::Link);
                    None
                } else {
                    builder = builder
                        .gateway(selected.next_hop)
                        .map_err(|error| LinuxError::InvalidRoute(error.to_string()))?
                        .onlink();
                    Some(selected.next_hop)
                };
                (builder.build(), ifindex, gateway, RouteType::Unicast)
            } else {
                let mut message = builder.build();
                message.header.kind = RouteType::Unreachable;
                (message, 0, None, RouteType::Unreachable)
            };
            let identity = RouteIdentity {
                table: route.table,
                destination: route.key.destination,
                priority,
                output_interface: ifindex,
                gateway,
                unreachable: kind == RouteType::Unreachable,
            };
            desired.insert(identity.clone());
            debug!(
                table = route.table,
                destination = %route.key.destination,
                source = ?route.key.source,
                source_specific = route.source_specific,
                interface = ?route.selected.as_ref().map(|value| value.interface.as_str()),
                next_hop = ?route.selected.as_ref().map(|value| value.next_hop),
                unreachable = route.selected.is_none(),
                priority,
                "installing Babel route"
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
            if route_identity(&message).is_none_or(|identity| !desired.contains(&identity)) {
                debug!(table = route_table(&message), "deleting stale owned route");
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
            if rule_identity(&message).is_none_or(|identity| !desired.contains(&identity)) {
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

fn interface_index(name: &str) -> Result<u32, LinuxError> {
    std::fs::read_to_string(format!("/sys/class/net/{name}/ifindex"))
        .ok()
        .and_then(|value| value.trim().parse().ok())
        .ok_or_else(|| LinuxError::Interface(name.into()))
}
