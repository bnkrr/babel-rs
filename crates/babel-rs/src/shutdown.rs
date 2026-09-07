use std::future::Future;
use std::time::Duration;

use thiserror::Error;
use tokio::time::Instant;

/// One deadline shared by all daemon shutdown stages, including response flush.
pub struct Deadline {
    at: Instant,
    timeout_ms: u32,
}

#[derive(Debug, Error)]
#[error(
    "shutdown timed out after {timeout_ms}ms while waiting for {phase}; cleanup may be incomplete"
)]
pub struct TimedOut {
    pub phase: &'static str,
    pub timeout_ms: u32,
}

impl Deadline {
    pub fn new(timeout_ms: u32) -> Self {
        Self {
            at: Instant::now() + Duration::from_millis(u64::from(timeout_ms)),
            timeout_ms,
        }
    }

    pub async fn wait<F: Future>(
        &self,
        phase: &'static str,
        future: F,
    ) -> Result<F::Output, TimedOut> {
        let timed_out = || TimedOut {
            phase,
            timeout_ms: self.timeout_ms,
        };
        if Instant::now() >= self.at {
            return Err(timed_out());
        }
        tokio::time::timeout_at(self.at, future)
            .await
            .map_err(|_| timed_out())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn stages_share_one_deadline_and_report_the_unfinished_stage() {
        let started = Instant::now();
        let deadline = Deadline::new(5_000);
        deadline
            .wait("router cleanup", tokio::time::sleep(Duration::from_secs(4)))
            .await
            .unwrap();
        let error = deadline
            .wait(
                "background tasks",
                tokio::time::sleep(Duration::from_secs(4)),
            )
            .await
            .unwrap_err();
        assert_eq!(started.elapsed(), Duration::from_secs(5));
        assert_eq!(error.phase, "background tasks");
        assert_eq!(error.timeout_ms, 5_000);
        assert!(
            deadline
                .wait("late ready task", std::future::ready(()))
                .await
                .is_err()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn pending_cleanup_is_dropped_at_the_configured_deadline() {
        let started = Instant::now();
        let deadline = Deadline::new(250);
        let (dropped, receive) = tokio::sync::oneshot::channel::<()>();
        let cleanup = async move {
            let _guard = dropped;
            std::future::pending::<()>().await;
        };
        assert!(
            deadline
                .wait("route export cleanup", cleanup)
                .await
                .is_err()
        );
        assert_eq!(started.elapsed(), Duration::from_millis(250));
        assert!(receive.await.is_err());
    }
}
