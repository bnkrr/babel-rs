use std::net::Ipv6Addr;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;

use super::*;

fn send_action(interface: &str, nonce: u16, timing: babel_protocol::SendTiming) -> Action {
    Action::Send {
        interface: interface.into(),
        destination: "ff02::1:6".parse().unwrap(),
        packet: babel_protocol::OutboundPacket {
            tlvs: vec![babel_protocol::OutboundTlv::Ack { nonce }],
        },
        timing,
    }
}

#[derive(Default)]
struct TestTransport {
    blocked: std::sync::atomic::AtomicBool,
    missing_mtu: std::sync::atomic::AtomicBool,
    attempts: AtomicU64,
    sent: std::sync::Mutex<Vec<Vec<u8>>>,
    wake: tokio::sync::Notify,
}

#[async_trait]
impl OutputTransport for TestTransport {
    fn payload_budget(&self) -> std::io::Result<usize> {
        if self.missing_mtu.load(Ordering::Relaxed) {
            Err(std::io::Error::other("injected MTU failure"))
        } else {
            Ok(1232)
        }
    }

    async fn send(&self, bytes: &[u8], _: IpAddr) -> std::io::Result<usize> {
        self.attempts.fetch_add(1, Ordering::Relaxed);
        loop {
            let wake = self.wake.notified();
            if !self.blocked.load(Ordering::Relaxed) {
                break;
            }
            wake.await;
        }
        self.sent.lock().unwrap().push(bytes.to_vec());
        Ok(bytes.len())
    }
}

fn queue_test_ack(queue: &OutputQueue, now: u64) {
    queue.submit(OutboundIntent {
        destination: Ipv6Addr::LOCALHOST.into(),
        packet: babel_protocol::OutboundPacket {
            tlvs: vec![babel_protocol::OutboundTlv::Ack { nonce: 1 }],
        },
        timing: babel_protocol::SendTiming::immediate(now),
    });
}

async fn settle_tasks() {
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
}

#[tokio::test(start_paused = true)]
async fn stalled_send_times_out_recovers_and_cancels_on_interface_removal() {
    let started = Arc::new(Instant::now());
    let (queue, receive) = OutputQueue::new(
        2,
        OUTPUT_BUDGET_BYTES,
        Arc::new(OutputCounters::default()),
        started.clone(),
    );
    let transport = Arc::new(TestTransport::default());
    transport.blocked.store(true, Ordering::Relaxed);
    let (stop, stop_rx) = watch::channel(false);
    let task = tokio::spawn(run_sender(
        transport.clone(),
        stop_rx,
        started.clone(),
        queue.clone(),
        receive,
        1,
    ));
    queue_test_ack(&queue, 0);
    settle_tasks().await;
    assert_eq!(transport.attempts.load(Ordering::Relaxed), 1);
    assert!(queue.status().used_bytes > 0);
    // Simulate writability returning while the runtime is stalled. Both
    // the send and its timer will be ready at the next poll: timeout wins.
    transport.blocked.store(false, Ordering::Relaxed);
    transport.wake.notify_waiters();
    tokio::time::advance(Duration::from_millis(SEND_TIMEOUT_MS + 1)).await;
    settle_tasks().await;
    assert!(transport.sent.lock().unwrap().is_empty());
    assert_eq!(queue.status().send_timeouts, 1);
    assert_eq!(queue.status().used_bytes, 0);
    transport.blocked.store(false, Ordering::Relaxed);
    queue_test_ack(&queue, elapsed_ms(&started));
    settle_tasks().await;
    assert_eq!(transport.sent.lock().unwrap().len(), 1);
    assert_eq!(queue.status().used_bytes, 0);
    transport.blocked.store(true, Ordering::Relaxed);
    queue_test_ack(&queue, elapsed_ms(&started));
    settle_tasks().await;
    assert!(queue.status().used_bytes > 0);
    stop.send(true).unwrap();
    tokio::time::timeout(Duration::from_millis(1), task)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(queue.status().used_bytes, 0);
}

#[tokio::test(start_paused = true)]
async fn failed_mtu_lookup_expires_pending_and_channel_work_then_recovers() {
    let started = Arc::new(Instant::now());
    let (queue, receive) = OutputQueue::new(
        2,
        OUTPUT_BUDGET_BYTES,
        Arc::new(OutputCounters::default()),
        started.clone(),
    );
    let transport = Arc::new(TestTransport::default());
    transport.missing_mtu.store(true, Ordering::Relaxed);
    let (stop, stop_rx) = watch::channel(false);
    let task = tokio::spawn(run_sender(
        transport.clone(),
        stop_rx,
        started.clone(),
        queue.clone(),
        receive,
        1,
    ));
    queue_test_ack(&queue, 0);
    settle_tasks().await;
    queue_test_ack(&queue, 0);
    tokio::time::advance(Duration::from_millis(1_100)).await;
    settle_tasks().await;
    assert_eq!(queue.status().expired_batches, 2);
    assert_eq!(queue.status().used_bytes, 0);
    assert_eq!(transport.attempts.load(Ordering::Relaxed), 0);
    transport.missing_mtu.store(false, Ordering::Relaxed);
    queue_test_ack(&queue, elapsed_ms(&started));
    settle_tasks().await;
    assert_eq!(transport.sent.lock().unwrap().len(), 1);
    stop.send(true).unwrap();
    task.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn full_bad_interface_does_not_stall_engine_status_or_healthy_output() {
    let started = Arc::new(Instant::now());
    let totals = Arc::new(OutputCounters::default());
    let (bad, bad_receive) = OutputQueue::new(2, 64 * 1024, totals.clone(), started.clone());
    let (good, good_receive) =
        OutputQueue::new(256, OUTPUT_BUDGET_BYTES, totals.clone(), started.clone());
    let bad_transport = Arc::new(TestTransport::default());
    bad_transport.blocked.store(true, Ordering::Relaxed);
    let good_transport = Arc::new(TestTransport::default());
    let (bad_stop, bad_stop_rx) = watch::channel(false);
    let (good_stop, good_stop_rx) = watch::channel(false);
    let bad_task = tokio::spawn(run_sender(
        bad_transport.clone(),
        bad_stop_rx,
        started.clone(),
        bad.clone(),
        bad_receive,
        1,
    ));
    let good_task = tokio::spawn(run_sender(
        good_transport.clone(),
        good_stop_rx,
        started.clone(),
        good.clone(),
        good_receive,
        2,
    ));
    let (commands_tx, commands) = mpsc::channel(64);
    let (received_tx, received) = mpsc::channel(256);
    let (shutdown_tx, shutdown) = watch::channel(false);
    let (route_updates, routes) = watch::channel(RouteSnapshot::default());
    let (export_updates, _) = watch::channel(RouteSnapshot::default());
    let handle = RouterHandle {
        commands: commands_tx,
        shutdown: shutdown_tx,
        routes,
    };
    // No privileged sockets: the real engine and command loop run against
    // independently controlled sender transports, on one runtime thread.
    let router = tokio::spawn(run_loop(Runtime {
        workers: HashMap::new(),
        export_worker: None,
        shutdown_timeout: Duration::from_secs(5),
        router_id: RouterId::new([1; 8]).unwrap(),
        interfaces: vec![("bad".into(), None), ("good".into(), None)],
        origins: vec![],
        sockets: HashMap::new(),
        outbound: HashMap::from([("bad".into(), bad.clone()), ("good".into(), good.clone())]),
        output_counters: totals,
        interface_stops: HashMap::from([("bad".into(), bad_stop), ("good".into(), good_stop)]),
        exporter: Arc::new(MemoryExporter::default()),
        export_updates,
        commands,
        received,
        received_tx,
        shutdown,
        route_updates,
        metric: Arc::new(WiredMetric::default()),
        metric_algebra: Arc::new(AdditiveMetric),
        route_selection: RouteSelectionConfig::default(),
        limits: ResourceLimits::default(),
        sequence_number: 0,
        sequence_store: Arc::new(NoopSequenceStore),
        started: started.clone(),
    }));
    let mut keys = Vec::new();
    for i in 0..32 {
        let key = RouteKey::new(format!("fd00::{i:x}/128").parse().unwrap(), None).unwrap();
        keys.push(key);
        handle.originate(key, 0).await.unwrap();
        settle_tasks().await;
    }
    tokio::time::timeout(Duration::from_millis(10), handle.status())
        .await
        .unwrap()
        .unwrap();
    // Advance beyond triggered jitter but remain inside the first send timeout.
    tokio::time::advance(Duration::from_millis(99)).await;
    settle_tasks().await;
    assert!(bad.status().rejected_batches > 0);
    assert!(bad.status().used_bytes <= bad.status().budget_bytes);
    assert_eq!(bad.status().send_timeouts, 0);
    assert!(!good_transport.sent.lock().unwrap().is_empty());
    assert_eq!(good.status().rejected_batches, 0);
    tokio::time::timeout(
        Duration::from_millis(10),
        handle.replace_origins(vec![(keys[31], 0)]),
    )
    .await
    .unwrap()
    .unwrap();
    tokio::time::timeout(Duration::from_millis(10), handle.status())
        .await
        .unwrap()
        .unwrap();
    tokio::time::advance(Duration::from_millis(200)).await;
    settle_tasks().await;
    // Let old output expire, then recover the transport and wait for the
    // ordinary protocol timer to advertise current state on both links.
    tokio::time::advance(Duration::from_secs(2)).await;
    settle_tasks().await;
    assert!(bad.status().expired_datagrams > 0);
    assert!(bad.status().used_bytes <= bad.status().budget_bytes);
    assert!(bad_transport.sent.lock().unwrap().is_empty());
    assert!(good_transport.sent.lock().unwrap().iter().any(|bytes| {
        decode_packet(
            bytes,
            DecodeContext {
                source: "fe80::1".parse().unwrap(),
            },
        )
        .unwrap()
        .tlvs
        .into_iter()
        .any(|tlv| {
            matches!(tlv,
                babel_protocol::Tlv::Update(update)
                    if update.key == Some(keys[0]) && update.metric == babel_protocol::INFINITY)
        })
    }));
    bad_transport.blocked.store(false, Ordering::Relaxed);
    bad_transport.wake.notify_waiters();
    for _ in 0..20 {
        tokio::time::advance(Duration::from_secs(1)).await;
        settle_tasks().await;
    }
    for transport in [&bad_transport, &good_transport] {
        assert!(transport.sent.lock().unwrap().iter().any(|bytes| {
            decode_packet(
                bytes,
                DecodeContext {
                    source: "fe80::1".parse().unwrap(),
                },
            )
            .unwrap()
            .tlvs
            .into_iter()
            .any(|tlv| {
                matches!(tlv,
                    babel_protocol::Tlv::Update(update)
                        if update.key == Some(keys[31]) && update.metric == 0)
            })
        }));
    }
    handle.shutdown();
    tokio::time::timeout(Duration::from_secs(1), router)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    bad_task.await.unwrap();
    good_task.await.unwrap();
    assert_eq!(bad.status().used_bytes, 0);
    assert_eq!(good.status().used_bytes, 0);
}

#[tokio::test]
async fn large_output_batch_progresses_with_one_available_channel_slot() {
    let (send, mut receive) = OutputQueue::new(
        1,
        OUTPUT_BUDGET_BYTES,
        Arc::new(OutputCounters::default()),
        Arc::new(Instant::now()),
    );
    let outbound = HashMap::from([("eth0".into(), send)]);
    let (exports, _) = watch::channel(RouteSnapshot::default());
    let timing = babel_protocol::SendTiming::urgent(100);
    let actions = (0..1024).map(|n| send_action("eth0", n, timing)).collect();
    // No consumer runs until apply_actions returns. Per-prefix sends would
    // deadlock here after filling the single channel slot.
    apply_actions(&outbound, &exports, actions);
    let intent = receive.recv().await.unwrap().intent;
    assert_eq!(intent.timing, timing);
    assert_eq!(
        intent.packet.tlvs,
        (0..1024)
            .map(|nonce| babel_protocol::OutboundTlv::Ack { nonce })
            .collect::<Vec<_>>()
    );
    assert!(receive.try_recv().is_err());
}

#[test]
fn output_batch_preserves_destinations_deadlines_and_sequence_order() {
    let urgent = babel_protocol::SendTiming::urgent(100);
    let later = babel_protocol::SendTiming::urgent(200);
    let actions = batch_send_actions(vec![
        send_action("eth0", 1, urgent),
        send_action("eth1", 2, urgent),
        send_action("eth0", 3, urgent),
        Action::SequenceNumberChanged(4),
        send_action("eth0", 5, urgent),
        send_action("eth0", 6, later),
    ]);
    assert_eq!(actions.len(), 5);
    assert!(matches!(&actions[0], Action::Send { packet, timing, .. }
        if packet.tlvs == vec![babel_protocol::OutboundTlv::Ack { nonce: 1 }, babel_protocol::OutboundTlv::Ack { nonce: 3 }] && *timing == urgent));
    assert!(matches!(&actions[1], Action::Send { interface, .. } if interface == "eth1"));
    assert_eq!(actions[2], Action::SequenceNumberChanged(4));
    assert_eq!(actions[3], send_action("eth0", 5, urgent));
    assert_eq!(actions[4], send_action("eth0", 6, later));
}

#[test]
fn rfc8966_transport_accepts_only_link_local_port_6696_sources() {
    let local = ["fe80::1".parse().unwrap()];
    assert!(valid_babel_source(
        &"[fe80::2]:6696".parse().unwrap(),
        &local
    ));
    assert!(!valid_babel_source(
        &"[fe80::2]:1234".parse().unwrap(),
        &local
    ));
    assert!(!valid_babel_source(
        &"[2001:db8::2]:6696".parse().unwrap(),
        &local
    ));
    assert!(!valid_babel_source(
        &"[fe80::1]:6696".parse().unwrap(),
        &local
    ));
}

#[derive(Clone, Copy)]
enum CheckpointOutcome {
    Success,
    Failure,
    Pending,
}

#[derive(Clone)]
struct TestSequenceStore {
    saved: Arc<std::sync::Mutex<Vec<u16>>>,
    outcome: CheckpointOutcome,
}

impl TestSequenceStore {
    fn new(outcome: CheckpointOutcome) -> Self {
        Self {
            saved: Arc::new(std::sync::Mutex::new(Vec::new())),
            outcome,
        }
    }
}

#[async_trait]
impl SequenceStore for TestSequenceStore {
    async fn persist(
        &self,
        sequence_number: u16,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.saved.lock().unwrap().push(sequence_number);
        match self.outcome {
            CheckpointOutcome::Success => Ok(()),
            CheckpointOutcome::Failure => Err(std::io::Error::other("disk unavailable").into()),
            CheckpointOutcome::Pending => std::future::pending().await,
        }
    }
}

#[derive(Clone, Default)]
struct CleanupExporter {
    cleanups: Arc<AtomicU64>,
}

#[async_trait]
impl RouteExporter for CleanupExporter {
    async fn reconcile(
        &self,
        _snapshot: RouteSnapshot,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        Ok(())
    }

    async fn shutdown(
        &self,
        snapshot: RouteSnapshot,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        assert!(snapshot.routes.is_empty());
        assert!(snapshot.unreachable.is_empty());
        self.cleanups.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[tokio::test(start_paused = true)]
async fn sequence_changes_stay_in_memory_and_shutdown_checkpoints_all_origins_once() {
    let store = TestSequenceStore::new(CheckpointOutcome::Success);
    let exporter = CleanupExporter::default();
    let keys: Vec<_> = (1..=3)
        .map(|i| RouteKey::new(format!("fd00::{i}/128").parse().unwrap(), None).unwrap())
        .collect();
    let mut builder = BabelRouterBuilder::default()
        .router_id(RouterId::new([1; 8]).unwrap())
        .sequence_number(u16::MAX - 1)
        .sequence_store(store.clone())
        .exporter(exporter.clone());
    for key in &keys {
        builder = builder.originate(*key, 0);
    }
    let router = builder.build().await.unwrap();
    let handle = router.handle();
    assert_eq!(handle.status().await.unwrap().sequence_number, u16::MAX - 1);
    assert!(store.saved.lock().unwrap().is_empty());

    // Changing all existing origin metrics causes one sequence increment.
    // A status round trip confirms the change has been applied without I/O.
    handle
        .replace_origins(keys.iter().map(|key| (*key, 1)).collect())
        .await
        .unwrap();
    assert_eq!(handle.status().await.unwrap().sequence_number, u16::MAX);
    assert!(store.saved.lock().unwrap().is_empty());
    handle.shutdown();
    tokio::time::timeout(Duration::from_secs(1), router.run())
        .await
        .unwrap()
        .unwrap();

    // Withdrawal keeps the existing sequence; checkpoint once after cleanup.
    assert_eq!(*store.saved.lock().unwrap(), vec![u16::MAX]);
    assert_eq!(exporter.cleanups.load(Ordering::SeqCst), 1);
}

#[tokio::test(start_paused = true)]
async fn failed_sequence_checkpoint_is_reported_after_shutdown_cleanup() {
    let store = TestSequenceStore::new(CheckpointOutcome::Failure);
    let exporter = CleanupExporter::default();
    let router = BabelRouterBuilder::default()
        .router_id(RouterId::new([1; 8]).unwrap())
        .sequence_number(17)
        .sequence_store(store.clone())
        .exporter(exporter.clone())
        .build()
        .await
        .unwrap();
    let handle = router.handle();
    handle.status().await.unwrap();
    handle.shutdown();
    let error = tokio::time::timeout(Duration::from_secs(1), router.run())
        .await
        .unwrap()
        .unwrap_err();
    assert!(matches!(error, RouterError::SequenceStore(_)));
    assert_eq!(*store.saved.lock().unwrap(), vec![17]);
    assert_eq!(exporter.cleanups.load(Ordering::SeqCst), 1);
}

#[tokio::test(start_paused = true)]
async fn pending_sequence_checkpoint_times_out_and_continues_shutdown_cleanup() {
    let store = TestSequenceStore::new(CheckpointOutcome::Pending);
    let exporter = CleanupExporter::default();
    let router = BabelRouterBuilder::default()
        .router_id(RouterId::new([1; 8]).unwrap())
        .sequence_number(23)
        .sequence_store(store.clone())
        .exporter(exporter.clone())
        .build()
        .await
        .unwrap();
    let handle = router.handle();
    handle.status().await.unwrap();
    let started = Instant::now();
    handle.shutdown();
    let error = tokio::time::timeout(Duration::from_secs(2), router.run())
        .await
        .unwrap()
        .unwrap_err();
    assert!(matches!(error, RouterError::SequenceStore(_)));
    assert!(started.elapsed() >= SEQUENCE_CHECKPOINT_TIMEOUT);
    assert_eq!(*store.saved.lock().unwrap(), vec![23]);
    assert_eq!(exporter.cleanups.load(Ordering::SeqCst), 1);
}

#[derive(Clone, Default)]
struct SlowExporter {
    generation: Arc<AtomicU64>,
}

#[async_trait]
impl RouteExporter for SlowExporter {
    async fn reconcile(
        &self,
        snapshot: RouteSnapshot,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        tokio::time::sleep(Duration::from_millis(25)).await;
        self.generation.store(snapshot.generation, Ordering::SeqCst);
        Ok(())
    }
}

#[tokio::test]
async fn slow_exporter_coalesces_to_the_latest_complete_snapshot() {
    let exporter = SlowExporter::default();
    let observed = exporter.generation.clone();
    let (snapshots, stream) = watch::channel(RouteSnapshot::default());
    let (shutdown, shutdown_stream) = watch::channel(false);
    let _worker = spawn_exporter(Arc::new(exporter), stream, shutdown_stream);
    tokio::task::yield_now().await;
    for generation in 1..=20 {
        snapshots.send_replace(RouteSnapshot {
            generation,
            routes: Vec::new(),
            unreachable: Vec::new(),
        });
    }
    tokio::time::timeout(Duration::from_secs(1), async {
        while observed.load(Ordering::SeqCst) != 20 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    assert!(observed.load(Ordering::SeqCst) == 20);
    let _ = shutdown.send(true);
}
