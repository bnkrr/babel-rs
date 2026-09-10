use async_trait::async_trait;
use babel_router::{BabelRouter, RouteExporter, RouteSnapshot, RouterError, RouterId};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;
use tokio::sync::Notify;

#[derive(Clone, Default)]
struct Exporter {
    entered: Arc<Notify>,
    release: Arc<Notify>,
    active: Arc<AtomicBool>,
    cleaned: Arc<AtomicBool>,
    fail_cleanup: bool,
}

struct Active(Arc<AtomicBool>);
impl Drop for Active {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

#[async_trait]
impl RouteExporter for Exporter {
    async fn reconcile(
        &self,
        _: RouteSnapshot,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.active.store(true, Ordering::SeqCst);
        let _active = Active(self.active.clone());
        self.entered.notify_one();
        self.release.notified().await;
        Ok(())
    }
    async fn shutdown(
        &self,
        snapshot: RouteSnapshot,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        assert!(
            !self.active.load(Ordering::SeqCst),
            "cleanup overlapped reconcile"
        );
        assert!(snapshot.routes.is_empty());
        self.cleaned.store(true, Ordering::SeqCst);
        if self.fail_cleanup {
            Err("cleanup unavailable".into())
        } else {
            Ok(())
        }
    }
}

fn builder() -> babel_router::BabelRouterBuilder {
    BabelRouter::builder().router_id(RouterId::new([1; 8]).unwrap())
}

#[tokio::test]
async fn orderly_shutdown_waits_for_inflight_export_before_cleanup() {
    let exporter = Exporter::default();
    let router = builder().exporter(exporter.clone()).start().await.unwrap();
    exporter.entered.notified().await;
    let task = tokio::spawn(router.shutdown());
    tokio::time::sleep(Duration::from_millis(350)).await;
    assert!(!exporter.cleaned.load(Ordering::SeqCst));
    exporter.release.notify_one();
    tokio::time::timeout(Duration::from_secs(1), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(exporter.cleaned.load(Ordering::SeqCst));
}

#[tokio::test]
async fn dropping_owner_cancels_workers_even_with_live_control_handles() {
    let exporter = Exporter::default();
    let router = builder().exporter(exporter.clone()).start().await.unwrap();
    let handle = router.handle();
    drop(handle.clone());
    handle.status().await.unwrap();
    exporter.entered.notified().await;
    drop(router);
    tokio::time::timeout(Duration::from_secs(1), async {
        while exporter.active.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(handle.status().await.is_err());
    assert!(!exporter.cleaned.load(Ordering::SeqCst));
}

#[tokio::test]
async fn cancelling_wait_cancels_the_owned_router() {
    let router = builder().start().await.unwrap();
    let handle = router.handle();
    let task = tokio::spawn(router.wait());
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    tokio::task::yield_now().await;
    assert!(handle.status().await.is_err());
}

#[tokio::test]
async fn shutdown_deadline_cancels_stalled_exporter_and_reports_timeout() {
    let exporter = Exporter::default();
    let router = builder().exporter(exporter.clone()).start().await.unwrap();
    exporter.entered.notified().await;
    router
        .handle()
        .set_shutdown_timeout(Duration::from_millis(400))
        .await
        .unwrap();
    let error = tokio::time::timeout(Duration::from_secs(2), router.shutdown())
        .await
        .unwrap()
        .unwrap_err();
    assert!(matches!(error, RouterError::ShutdownTimeout));
    tokio::task::yield_now().await;
    assert!(!exporter.active.load(Ordering::SeqCst));
    assert!(!exporter.cleaned.load(Ordering::SeqCst));
}

#[tokio::test]
async fn cleanup_failure_is_observable() {
    let exporter = Exporter {
        fail_cleanup: true,
        ..Default::default()
    };
    let router = builder().exporter(exporter.clone()).start().await.unwrap();
    exporter.entered.notified().await;
    exporter.release.notify_one();
    assert!(matches!(
        router.shutdown().await,
        Err(RouterError::Cleanup(_))
    ));
}

#[tokio::test]
async fn zero_shutdown_deadline_is_rejected_without_starting() {
    assert!(matches!(
        builder().shutdown_timeout(Duration::ZERO).validate(),
        Err(RouterError::InvalidShutdownTimeout)
    ));
}
