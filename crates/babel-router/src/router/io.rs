//! Independent bounded receiver, sender and exporter workers.

use super::*;

pub(super) fn spawn_receiver(
    socket: Arc<InterfaceSocket>,
    received: mpsc::Sender<Received>,
    mut shutdown: watch::Receiver<bool>,
    mut stop: watch::Receiver<bool>,
    started: Arc<Instant>,
) -> Worker {
    Worker(tokio::spawn(async move {
        let mut buffer = vec![0u8; 65535];
        let slots = Arc::new(Semaphore::new(RECEIVED_PER_INTERFACE));
        loop {
            let permit = tokio::select! {
                _ = shutdown.changed() => return,
                _ = stop.changed() => return,
                permit = Arc::clone(&slots).acquire_owned() => permit.expect("receiver owns semaphore"),
            };
            tokio::select! {
                changed = shutdown.changed() => if changed.is_err() || *shutdown.borrow() { return; },
                changed = stop.changed() => if changed.is_err() || *stop.borrow() { return; },
                result = socket.socket.recv_from(&mut buffer) => match result {
                    Ok((length, source))
                        if valid_socket_source(&socket, source) => {
                        let received_timestamp_us = started.elapsed().as_micros() as u32;
                        let item = Received::Packet { _permit: permit, interface: socket.name.clone(), index: socket.index, source: source.ip(), bytes: buffer[..length].to_vec(), received_timestamp_us };
                        tokio::select! {
                            _ = shutdown.changed() => return,
                            _ = stop.changed() => return,
                            result = received.send(item) => if result.is_err() { return; },
                        }
                    }
                    Ok(_) => {}
                    Err(error) => {
                        warn!(interface = %socket.name, %error, "Babel receive failed");
                        let failed = Received::Failed {
                            interface: socket.name.clone(),
                            index: socket.index,
                            error: error.to_string(),
                        };
                        tokio::select! {
                            _ = shutdown.changed() => {},
                            _ = stop.changed() => {},
                            _ = received.send(failed) => {},
                        }
                        return;
                    }
                }
            }
        }
    }))
}

fn valid_socket_source(socket: &InterfaceSocket, source: SocketAddr) -> bool {
    let local = socket.addresses.read().expect("address lock");
    match (socket.transport, source) {
        (babel_protocol::ControlTransport::Ipv6, SocketAddr::V6(source)) => {
            valid_babel_source(&source, &local)
        }
        (babel_protocol::ControlTransport::Ipv4, SocketAddr::V4(source)) => {
            source.port() == babel_protocol::wire::PORT
                && !source.ip().is_unspecified()
                && !source.ip().is_multicast()
                && !source.ip().is_loopback()
                && !source.ip().is_broadcast()
                && !local.contains(&IpAddr::V4(*source.ip()))
                && crate::transport::ipv4_on_link(&socket.name, *source.ip())
        }
        _ => false,
    }
}

pub(super) fn valid_babel_source(source: &std::net::SocketAddrV6, local: &[IpAddr]) -> bool {
    source.port() == babel_protocol::wire::PORT
        && source.ip().is_unicast_link_local()
        && !local.contains(&IpAddr::V6(*source.ip()))
}

#[async_trait::async_trait]
pub(super) trait OutputTransport: Send + Sync + 'static {
    fn payload_budget(&self) -> std::io::Result<usize>;
    async fn send(&self, bytes: &[u8], destination: IpAddr) -> std::io::Result<usize>;
}

#[async_trait::async_trait]
impl OutputTransport for InterfaceSocket {
    fn payload_budget(&self) -> std::io::Result<usize> {
        InterfaceSocket::payload_budget(self)
    }

    async fn send(&self, bytes: &[u8], destination: IpAddr) -> std::io::Result<usize> {
        self.socket
            .send_to(bytes, self.destination(destination))
            .await
    }
}

pub(super) fn spawn_sender(
    socket: Arc<InterfaceSocket>,
    stop: watch::Receiver<bool>,
    started: Arc<Instant>,
    counters: Arc<OutputCounters>,
) -> (OutputQueue, Worker) {
    let (queue, receive) = OutputQueue::new(
        OUTPUT_QUEUE_CAPACITY,
        OUTPUT_BUDGET_BYTES,
        counters,
        Arc::clone(&started),
    );
    let worker = Worker(tokio::spawn(run_sender(
        socket.clone(),
        stop,
        started,
        queue.clone(),
        receive,
        output_seed(&socket),
    )));
    (queue, worker)
}

pub(super) async fn run_sender(
    transport: Arc<impl OutputTransport>,
    mut stop: watch::Receiver<bool>,
    started: Arc<Instant>,
    queue: OutputQueue,
    mut receive: mpsc::Receiver<QueuedIntent>,
    seed: u64,
) {
    let mut scheduler = OutputScheduler::new(seed);
    loop {
        if *stop.borrow() {
            return;
        }
        let now_ms = elapsed_ms(&started);
        let (expired_batches, expired_datagrams) = scheduler.expire(now_ms);
        queue.expired_batches(expired_batches);
        for _ in 0..expired_datagrams {
            queue.drop_datagram(true, false);
        }
        if scheduler.next_wake_ms().is_some_and(|wake| wake <= now_ms) {
            let payload_budget = match transport.payload_budget() {
                Ok(value) => value,
                Err(_) => {
                    // Retry locally, but continue expiry and observe interface removal.
                    tokio::select! {
                        _ = stop.changed() => return,
                        _ = tokio::time::sleep(Duration::from_millis(SEND_TIMEOUT_MS)) => {}
                    }
                    continue;
                }
            };
            match scheduler.pop_due(now_ms, payload_budget) {
                Ok(Some(mut datagram)) => {
                    let send_ms = elapsed_ms(&started);
                    if send_ms >= datagram.expires_ms {
                        queue.drop_datagram(true, false);
                        continue;
                    }
                    if send_ms > datagram.deadline_ms {
                        queue.missed_deadline();
                    }
                    if stamp_hello_timestamps(
                        &mut datagram.bytes,
                        started.elapsed().as_micros() as u32,
                    )
                    .is_err()
                    {
                        queue.drop_datagram(false, false);
                        continue;
                    }
                    let wait_ms = SEND_TIMEOUT_MS.min(datagram.expires_ms - send_ms);
                    let send_deadline =
                        *started + Duration::from_millis(send_ms.saturating_add(wait_ms));
                    let result = {
                        let send = transport.send(&datagram.bytes, datagram.destination);
                        tokio::pin!(send);
                        // Check the clock before every socket poll as well as
                        // arming a timer. A ready socket may otherwise run
                        // before the timer driver notices a runtime stall.
                        let fresh_send = std::future::poll_fn(|cx| {
                            if Instant::now() >= send_deadline {
                                std::task::Poll::Ready(None)
                            } else {
                                send.as_mut().poll(cx).map(Some)
                            }
                        });
                        tokio::select! {
                            biased;
                            _ = stop.changed() => return,
                            _ = tokio::time::sleep_until(send_deadline) => None,
                            result = fresh_send => result,
                        }
                    };
                    match result {
                        Some(Ok(_)) => {}
                        Some(Err(_)) => queue.drop_datagram(false, false),
                        None => {
                            queue.drop_datagram(elapsed_ms(&started) >= datagram.expires_ms, true)
                        }
                    }
                    // Keep the reservation alive until the socket operation ends.
                    drop(datagram);
                    // A ready full-table dump must not monopolise a runtime worker.
                    tokio::task::yield_now().await;
                    continue;
                }
                Ok(None) => {}
                Err(_) => {
                    queue.drop_datagram(false, false);
                    continue;
                }
            }
        }
        let wake_ms = scheduler.next_wake_ms();
        tokio::select! {
            _ = stop.changed() => return,
            item = receive.recv() => {
                let Some(item) = item else { return; };
                let now_ms = elapsed_ms(&started);
                if now_ms >= item.expires_ms {
                    queue.expired_batches(1);
                } else {
                    scheduler.enqueue(item, now_ms);
                }
            }
            _ = sleep_until_ms(&started, wake_ms), if wake_ms.is_some() => {}
        }
    }
}

pub(super) async fn sleep_until_ms(started: &Instant, wake_ms: Option<u64>) {
    let Some(wake_ms) = wake_ms else {
        std::future::pending::<()>().await;
        return;
    };
    let delay = wake_ms.saturating_sub(elapsed_ms(started));
    tokio::time::sleep(Duration::from_millis(delay)).await;
}

pub(super) fn output_seed(socket: &InterfaceSocket) -> u64 {
    let mut value = 0xcbf2_9ce4_8422_2325u64 ^ u64::from(socket.index);
    for byte in socket.name.as_bytes() {
        value = (value ^ u64::from(*byte)).wrapping_mul(0x100_0000_01b3);
    }
    value
}

pub(super) fn spawn_exporter(
    exporter: Arc<dyn RouteExporter>,
    mut snapshots: watch::Receiver<RouteSnapshot>,
    mut shutdown: watch::Receiver<bool>,
) -> Worker {
    Worker(tokio::spawn(async move {
        let mut last_generation = None;
        loop {
            if *shutdown.borrow() {
                return;
            }
            let snapshot = snapshots.borrow_and_update().clone();
            if last_generation != Some(snapshot.generation) {
                if let Err(error) = exporter.reconcile(snapshot.clone()).await {
                    warn!(%error, generation = snapshot.generation, "route export failed");
                } else {
                    last_generation = Some(snapshot.generation);
                }
            }
            if *shutdown.borrow() {
                return;
            }
            tokio::select! {
                changed = snapshots.changed() => if changed.is_err() { return; },
                changed = shutdown.changed() => if changed.is_err() || *shutdown.borrow() { return; },
                _ = tokio::time::sleep(Duration::from_secs(2)) => {},
            }
        }
    }))
}
