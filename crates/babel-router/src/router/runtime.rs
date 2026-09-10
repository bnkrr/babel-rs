//! Serialized engine commands, snapshots and shutdown sequencing.

use super::*;

pub(super) async fn run_loop(runtime: Runtime) -> Result<(), RouterError> {
    let Runtime {
        mut workers,
        mut export_worker,
        mut shutdown_timeout,
        router_id,
        interfaces,
        origins,
        mut sockets,
        mut outbound,
        output_counters,
        mut interface_stops,
        exporter,
        export_updates,
        mut commands,
        mut received,
        received_tx,
        mut shutdown,
        route_updates,
        metric,
        metric_algebra,
        route_selection,
        limits,
        sequence_number,
        sequence_store,
        started,
    } = runtime;
    let now = || elapsed_ms(&started);
    let default_policy = InterfacePolicy {
        control_transport: Default::default(),
        ipv4_next_hop: Default::default(),
        metric: Arc::clone(&metric),
        hello_interval_cs: 400,
        update_interval_cs: 1600,
        split_horizon: true,
    };
    let mut engine = Engine::try_new(EngineConfig {
        ipv4_via_ipv6: exporter.supports_ipv4_via_ipv6(),
        limits,
        router_id,
        metric: Arc::clone(&metric),
        metric_algebra,
        sequence_number,
        hello_interval_cs: 400,
        update_interval_cs: 1600,
        route_selection,
    })?;
    let initial = RouteSnapshot::default();
    export_updates.send_replace(initial.clone());
    route_updates.send_replace(initial);
    let mut interface_policies = HashMap::new();
    for (interface, configured_policy) in &interfaces {
        let policy = configured_policy
            .clone()
            .unwrap_or_else(|| default_policy.clone());
        interface_policies.insert(interface.clone(), policy.clone());
        apply_actions(
            &outbound,
            &export_updates,
            engine.handle(Event::InterfaceUpWithPolicy {
                interface: interface.clone(),
                local_addresses: sockets
                    .get(interface)
                    .into_iter()
                    .flat_map(|socket| socket.addresses.read().expect("address lock").clone())
                    .collect(),
                policy,
                now_ms: now(),
            }),
        );
    }
    for (key, metric) in origins {
        apply_actions(
            &outbound,
            &export_updates,
            engine.handle(Event::Originate {
                key,
                metric,
                now_ms: now(),
            }),
        );
    }
    let mut last_rejections = (0, 0, 0);
    let mut next_limit_log_ms = 0;
    let mut next_address_scan_ms = 0;
    let mut ticker = tokio::time::interval(Duration::from_millis(100));
    let mut status = RouterStatus {
        sequence_number: engine.sequence_number(),
        metric: effective_metric_name(&interface_policies, &metric),
        interfaces: sorted_interface_names(&sockets),
        interface_details: interface_status(&sockets, &interface_policies, &default_policy),
        ..RouterStatus::default()
    };
    loop {
        tokio::select! {
            changed = shutdown.changed() => if changed.is_err() || *shutdown.borrow() {
                // Babel updates outlive an abruptly disappearing speaker until
                // their advertised interval expires.  Retract our origins while
                // the interface sockets are still open so neighbours can remove
                // them immediately during an orderly shutdown/restart.  Repeat
                // the datagrams because UDP provides no delivery acknowledgement
                // and the process cannot rely on a later periodic update.
                commands.close();
                let cleanup = async {
                let actions = engine.handle(Event::ReplaceOrigins {
                    origins: BTreeMap::new(),
                    now_ms: now(),
                });
                let retractions = actions.iter()
                    .filter(|action| matches!(action, Action::Send { .. }))
                    .cloned()
                    .collect::<Vec<_>>();
                apply_actions(&outbound, &export_updates, actions);
                for _ in 0..2 {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    apply_actions(&outbound, &export_updates, retractions.clone());
                }
                let empty = RouteSnapshot {
                    generation: status.route_generation.wrapping_add(1),
                    routes: vec![],
                    unreachable: vec![],
                };
                // Give the final repeat time to leave the bounded output scheduler.
                tokio::time::sleep(Duration::from_millis(100)).await;
                let checkpoint = checkpoint_sequence_number(&sequence_store, engine.sequence_number()).await;
                if let Some(worker) = export_worker.as_mut() { worker.join().await?; }
                let exported = exporter.shutdown(empty.clone()).await
                    .map_err(|error| RouterError::Cleanup(error.to_string()));
                route_updates.send_replace(empty);
                for stop in interface_stops.values() { let _ = stop.send(true); }
                for group in workers.values_mut() {
                    for worker in group { worker.join().await?; }
                }
                exported?;
                checkpoint
                };
                return tokio::time::timeout(shutdown_timeout, cleanup).await
                    .map_err(|_| RouterError::ShutdownTimeout)?;
            },
            _ = ticker.tick() => {
                if export_worker.as_ref().is_some_and(|worker| worker.0.is_finished()) && !*shutdown.borrow() {
                    if let Some(worker) = export_worker.as_mut() { worker.join().await?; }
                    return Err(RouterError::Task("export worker stopped unexpectedly".into()));
                }
                if now() >= next_address_scan_ms {
                    next_address_scan_ms = now().saturating_add(2_000);
                    for (name, socket) in &sockets {
                        match crate::transport::interface_addresses(name) {
                            Ok(addresses) => {
                                let changed = *socket.addresses.read().expect("address lock") != addresses;
                                if changed {
                                    *socket.addresses.write().expect("address lock") = addresses.clone();
                                    apply_actions_with_status(&outbound, &export_updates, &route_updates, &mut status,
                                        engine.handle(Event::InterfaceAddressesChanged { interface: name.clone(), local_addresses: addresses, now_ms: now() }));
                                }
                            }
                            Err(error) => debug!(interface = %name, %error, "could not refresh interface addresses"),
                        }
                    }
                }
                for (interface, queue) in &outbound { queue.log_losses(interface); }
                let resources = engine.resource_status();
                let rejections = (resources.rejected_neighbors, resources.rejected_candidates_global, resources.rejected_candidates_per_neighbor);
                if rejections != last_rejections && now() >= next_limit_log_ms {
                    warn!(rejected_neighbors = rejections.0, rejected_candidates_global = rejections.1, rejected_candidates_per_neighbor = rejections.2, "Babel admission limits rejected new state");
                    last_rejections = rejections;
                    next_limit_log_ms = now().saturating_add(30_000);
                }
                apply_actions_with_status(&outbound, &export_updates, &route_updates, &mut status, engine.handle(Event::Tick { now_ms: now() }));
                status.neighbours = engine.neighbour_count();
                status.neighbour_details = engine.neighbour_status(now());
                update_output_status(&mut status, &output_counters);
            },
            Some(item) = received.recv() => match item {
                Received::Packet { _permit, interface, index, source, bytes, received_timestamp_us } => {
                    if sockets
                        .get(&interface)
                        .is_none_or(|socket| socket.index != index)
                    {
                        continue;
                    }
                    match decode_packet(&bytes, DecodeContext { source }) {
                        Ok(packet) => {
                            apply_actions_with_status(&outbound, &export_updates, &route_updates, &mut status, engine.handle(Event::PacketReceivedWithTimestamp { interface, source, packet, now_ms: now(), received_timestamp_us }));
                            status.neighbours = engine.neighbour_count();
                            status.neighbour_details = engine.neighbour_status(now());
                        },
                        Err(error) => debug!(%error, "ignored invalid Babel packet"),
                    }
                }
                Received::Failed { interface, index, error } => {
                    let is_current = sockets.get(&interface).is_some_and(|socket| socket.index == index);
                    if is_current {
                        warn!(%interface, index, %error, "detaching failed Babel interface");
                        sockets.remove(&interface);
                        outbound.remove(&interface);
                        workers.remove(&interface);
                        if let Some(stop) = interface_stops.remove(&interface) {
                            let _ = stop.send(true);
                        }
                        interface_policies.remove(&interface);
                        apply_actions_with_status(&outbound, &export_updates, &route_updates, &mut status, engine.handle(Event::InterfaceDown { interface, now_ms: now() }));
                        status.interfaces = sorted_interface_names(&sockets);
                        status.metric = effective_metric_name(&interface_policies, &metric);
                        status.interface_details = interface_status(&sockets, &interface_policies, &default_policy);
                        status.neighbours = engine.neighbour_count();
                        status.neighbour_details = engine.neighbour_status(now());
                    }
                }
            },
            Some(command) = commands.recv() => match command {
                Command::ReplaceOrigins(origins, reply) => {
                    apply_actions_with_status(
                        &outbound,
                        &export_updates,
                        &route_updates,
                        &mut status,
                        engine.handle(Event::ReplaceOrigins { origins, now_ms: now() }),
                    );
                    let _ = reply.send(Ok(()));
                },
                Command::ShutdownTimeout(timeout, reply) => {
                    shutdown_timeout = timeout;
                    let _ = reply.send(());
                },
                Command::Originate(key, metric, reply) => {
                    apply_actions_with_status(&outbound, &export_updates, &route_updates, &mut status, engine.handle(Event::Originate { key, metric, now_ms: now() })) ;
                    let _ = reply.send(());
                },
                Command::Withdraw(key, reply) => {
                    apply_actions_with_status(&outbound, &export_updates, &route_updates, &mut status, engine.handle(Event::Withdraw { key, now_ms: now() })) ;
                    let _ = reply.send(());
                },
                Command::AddInterface(interface, configured_policy, reply) => {
                    let result = if sockets.contains_key(&interface) {
                        Ok(())
                    } else {
                        let policy = configured_policy.unwrap_or_else(|| default_policy.clone());
                        match InterfaceSocket::open(&interface, policy.control_transport) {
                            Ok(socket) => {
                                let socket = Arc::new(socket);
                                let (stop, stop_rx) = watch::channel(false);
                                let receiver = spawn_receiver(socket.clone(), received_tx.clone(), shutdown.clone(), stop_rx.clone(), Arc::clone(&started));
                                let (sender, sender_worker) = spawn_sender(
                                    socket.clone(),
                                    stop.subscribe(),
                                    Arc::clone(&started),
                                    Arc::clone(&output_counters),
                                );
                                outbound.insert(interface.clone(), sender);
                                workers.insert(interface.clone(), vec![receiver, sender_worker]);
                                sockets.insert(interface.clone(), socket);
                                interface_policies.insert(interface.clone(), policy.clone());
                                interface_stops.insert(interface.clone(), stop);
                                let local_addresses = sockets
                                    .get(&interface)
                                    .into_iter()
                                    .flat_map(|socket| socket.addresses.read().expect("address lock").clone())
                                    .collect();
                                apply_actions_with_status(&outbound, &export_updates, &route_updates, &mut status, engine.handle(Event::InterfaceUpWithPolicy { interface, local_addresses, policy, now_ms: now() }));
                                status.interfaces = sorted_interface_names(&sockets);
                                status.metric = effective_metric_name(&interface_policies, &metric);
                                status.interface_details = interface_status(&sockets, &interface_policies, &default_policy);
                                Ok(())
                            }
                            Err(source) => Err(RouterError::OpenInterface { interface, source }),
                        }
                    };
                    let _ = reply.send(result);
                }
                Command::UpdateInterfacePolicy(interface, policy, reset_metric, reply) => {
                    let result = if sockets.get(&interface).is_some_and(|s| s.transport != policy.control_transport) {
                        Err(RouterError::TransportChangeRequiresReattach(interface))
                    } else if sockets.contains_key(&interface) {
                        apply_actions_with_status(
                            &outbound,
                            &export_updates,
                                            &route_updates,
                            &mut status,
                            engine.handle(Event::InterfacePolicyChanged {
                                interface: interface.clone(),
                                policy: policy.clone(),
                                reset_metric,
                                now_ms: now(),
                            }),
                        );
                        interface_policies.insert(interface, policy);
                        status.metric = effective_metric_name(&interface_policies, &metric);
                        status.interface_details = interface_status(&sockets, &interface_policies, &default_policy);
                        Ok(())
                    } else {
                        Err(RouterError::InterfaceNotFound(interface))
                    };
                    let _ = reply.send(result);
                }
                Command::RemoveInterface(interface, reply) => {
                    let result = if sockets.remove(&interface).is_some() {
                        outbound.remove(&interface);
                        workers.remove(&interface);
                        if let Some(stop) = interface_stops.remove(&interface) {
                            let _ = stop.send(true);
                        }
                        interface_policies.remove(&interface);
                        apply_actions_with_status(&outbound, &export_updates, &route_updates, &mut status, engine.handle(Event::InterfaceDown { interface, now_ms: now() }));
                        status.interfaces = sorted_interface_names(&sockets);
                        status.metric = effective_metric_name(&interface_policies, &metric);
                        status.interface_details = interface_status(&sockets, &interface_policies, &default_policy);
                        Ok(())
                    } else {
                        Err(RouterError::InterfaceNotFound(interface))
                    };
                    let _ = reply.send(result);
                }
                Command::Status(reply) => {
                    status.neighbours = engine.neighbour_count();
                    status.neighbour_details = engine.neighbour_status(now());
                    status.interface_details = interface_status(&sockets, &interface_policies, &default_policy);
                    for interface in &mut status.interface_details {
                        if let Some(queue) = outbound.get(&interface.name) {
                            interface.output = queue.status();
                        }
                    }
                    update_output_status(&mut status, &output_counters);
                    status.resources = engine.resource_status();
                    status.sequence_number = engine.sequence_number();
                    let _ = reply.send(status.clone());
                }
            }
        }
    }
}

pub(super) fn sorted_interface_names(
    sockets: &HashMap<String, Arc<InterfaceSocket>>,
) -> Vec<String> {
    let mut names: Vec<_> = sockets.keys().cloned().collect();
    names.sort();
    names
}

pub(super) fn effective_metric_name(
    policies: &HashMap<String, InterfacePolicy>,
    fallback: &Arc<dyn MetricProfile>,
) -> String {
    let mut names: Vec<_> = policies
        .values()
        .map(|policy| policy.metric.name())
        .collect();
    names.sort();
    names.dedup();
    match names.as_slice() {
        [] => fallback.name(),
        [name] => name.clone(),
        _ => "per-interface".into(),
    }
}

pub(super) fn interface_status(
    sockets: &HashMap<String, Arc<InterfaceSocket>>,
    policies: &HashMap<String, InterfacePolicy>,
    fallback: &InterfacePolicy,
) -> Vec<RouterInterfaceStatus> {
    let mut result: Vec<_> = sockets
        .values()
        .map(|socket| {
            let mtu = socket.current_mtu().unwrap_or(socket.mtu);
            let policy = policies.get(&socket.name).unwrap_or(fallback);
            RouterInterfaceStatus {
                name: socket.name.clone(),
                index: socket.index,
                local_addresses: socket
                    .addresses
                    .read()
                    .expect("address lock")
                    .iter()
                    .copied()
                    .collect(),
                control_transport: socket.transport,
                mtu,
                udp_payload_budget: payload_budget_for_transport(mtu, socket.transport)
                    .unwrap_or_default(),
                metric: policy.metric.name(),
                hello_interval_ms: u64::from(policy.hello_interval_cs) * 10,
                update_interval_ms: u64::from(policy.update_interval_cs) * 10,
                split_horizon: policy.split_horizon,
                ipv4_next_hop: policy.ipv4_next_hop,
                ipv4_address: policy
                    .ipv4_next_hop
                    .ipv4_address(&socket.addresses.read().expect("address lock")),
                output: OutputStatus::default(),
            }
        })
        .collect();
    result.sort_by(|left, right| left.name.cmp(&right.name));
    result
}

pub(super) fn update_output_status(status: &mut RouterStatus, counters: &OutputCounters) {
    status.dropped_outbound_datagrams = counters.dropped.load(Ordering::Relaxed);
    status.missed_outbound_deadlines = counters.missed_deadlines.load(Ordering::Relaxed);
}

pub(super) async fn checkpoint_sequence_number(
    store: &Arc<dyn SequenceStore>,
    sequence_number: u16,
) -> Result<(), RouterError> {
    match tokio::time::timeout(SEQUENCE_CHECKPOINT_TIMEOUT, store.persist(sequence_number)).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => Err(RouterError::SequenceStore(error.to_string())),
        Err(_) => Err(RouterError::SequenceStore("checkpoint timed out".into())),
    }
}

pub(super) fn apply_actions(
    outbound: &HashMap<String, OutputQueue>,
    export_updates: &watch::Sender<RouteSnapshot>,
    actions: Vec<Action>,
) {
    let mut ignored = RouterStatus::default();
    let (updates, _) = watch::channel(RouteSnapshot::default());
    apply_actions_with_status(outbound, export_updates, &updates, &mut ignored, actions)
}

pub(super) fn apply_actions_with_status(
    outbound: &HashMap<String, OutputQueue>,
    export_updates: &watch::Sender<RouteSnapshot>,
    route_updates: &watch::Sender<RouteSnapshot>,
    status: &mut RouterStatus,
    actions: Vec<Action>,
) {
    for action in batch_send_actions(actions) {
        match action {
            Action::Send {
                interface,
                destination,
                packet,
                timing,
            } => {
                let Some(sender) = outbound.get(&interface) else {
                    continue;
                };
                sender.submit(OutboundIntent {
                    destination,
                    packet,
                    timing,
                });
            }
            Action::RoutesChanged {
                generation,
                routes,
                unreachable,
            } => {
                status.route_generation = generation;
                status.selected_routes = routes.len();
                let snapshot = RouteSnapshot {
                    generation,
                    routes,
                    unreachable,
                };
                route_updates.send_replace(snapshot.clone());
                export_updates.send_replace(snapshot);
            }
            Action::SequenceNumberChanged(value) => status.sequence_number = value,
        }
    }
}

/// Batch same-destination, same-timing Sends within one uninterrupted run.
/// Preserve TLV order and sequence/snapshot action boundaries. This avoids waiting
/// for one channel slot per prefix during large triggered announcements.
pub(super) fn batch_send_actions(actions: Vec<Action>) -> Vec<Action> {
    let mut result: Vec<Action> = Vec::new();
    let mut positions = HashMap::new();
    for action in actions {
        if let Action::Send {
            interface,
            destination,
            packet,
            timing,
        } = action
        {
            let key = (
                interface.clone(),
                destination,
                timing.deadline_ms,
                timing.max_jitter_ms,
            );
            if let Some(&index) = positions.get(&key) {
                if let Action::Send {
                    packet: previous, ..
                } = &mut result[index]
                {
                    previous.tlvs.extend(packet.tlvs);
                }
            } else {
                positions.insert(key, result.len());
                result.push(Action::Send {
                    interface,
                    destination,
                    packet,
                    timing,
                });
            }
        } else {
            positions.clear();
            result.push(action);
        }
    }
    result
}
