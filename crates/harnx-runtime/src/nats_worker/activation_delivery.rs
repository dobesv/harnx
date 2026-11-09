//! Lifecycle guard for one JetStream activation delivery.
//!
//! A delivery starts sending progress acknowledgements as soon as admission
//! begins. Every terminal disposition stops and joins that task first, so no
//! progress acknowledgement can race a final ACK, NAK, or Term.

use super::activation_failure::ActivationFailureTracker;
use crate::nats_lease::NatsSessionLease;
use anyhow::Result;
use async_nats::jetstream::{AckKind, Message};
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum ActivationNakReason {
    Busy,
    PreflightNotReady,
    Shutdown,
    SettlementRejection,
    ClaimError,
    PreparationError,
}

impl ActivationNakReason {
    pub(super) const fn label(self) -> &'static str {
        match self {
            Self::Busy => "busy",
            Self::PreflightNotReady => "preflight_not_ready",
            Self::Shutdown => "shutdown",
            Self::SettlementRejection => "settlement_rejection",
            Self::ClaimError => "claim_error",
            Self::PreparationError => "preparation_error",
        }
    }
}

pub(super) struct ActivationDelivery {
    message: Message,
    failure_key: String,
    delivered: i64,
    heartbeat_cancel: CancellationToken,
    heartbeat_task: Option<JoinHandle<()>>,
    lease_tx: watch::Sender<Option<watch::Receiver<bool>>>,
}

impl ActivationDelivery {
    pub(super) async fn start(message: Message, heartbeat_interval: Duration) -> Result<Self> {
        let info = message
            .info()
            .map_err(|error| anyhow::anyhow!("read SessionActivate delivery info: {error}"))?;
        let failure_key = format!("{}/{}", info.stream, info.stream_sequence);
        let delivered = info.delivered;
        message.ack_with(AckKind::Progress).await.map_err(|error| {
            anyhow::anyhow!("start SessionActivate progress heartbeat: {error}")
        })?;

        let heartbeat_cancel = CancellationToken::new();
        let (lease_tx, lease_rx) = watch::channel(None);
        let heartbeat_message = message.clone();
        let task_cancel = heartbeat_cancel.clone();
        let heartbeat_task = tokio::spawn(run_heartbeat(
            heartbeat_interval,
            task_cancel,
            lease_rx,
            move || {
                let message = heartbeat_message.clone();
                async move {
                    if let Err(error) = message.ack_with(AckKind::Progress).await {
                        log::warn!("SessionActivate progress heartbeat failed: {error}");
                    }
                }
            },
        ));

        Ok(Self {
            message,
            failure_key,
            delivered,
            heartbeat_cancel,
            heartbeat_task: Some(heartbeat_task),
            lease_tx,
        })
    }

    pub(super) fn message(&self) -> &Message {
        &self.message
    }

    pub(super) fn stream_sequence(&self) -> Option<u64> {
        self.message.info().ok().map(|info| info.stream_sequence)
    }

    /// How many times JetStream has delivered this activation, this delivery
    /// included.
    pub(super) fn delivered(&self) -> i64 {
        self.delivered
    }

    /// The session the activation names, when its payload decodes.
    pub(super) fn session_id(&self) -> Option<String> {
        serde_json::from_slice::<super::activation::SessionActivate>(&self.message.payload)
            .ok()
            .map(|activation| activation.session_id)
    }

    pub(super) fn failure_key(&self) -> &str {
        &self.failure_key
    }

    pub(super) async fn clear_failure_count(
        &self,
        tracker: &ActivationFailureTracker,
    ) -> Result<()> {
        tracker.clear(&self.failure_key).await
    }

    pub(super) fn attach_lease(&self, lease: &Arc<NatsSessionLease>) {
        self.lease_tx.send_replace(Some(lease.lost_watch()));
    }

    /// Whether the progress heartbeat task is still running. It stops on its
    /// own when an attached lease is lost, so anything that wants to keep
    /// holding the delivery — parking it on a busy waiter — must check this
    /// first: without heartbeats JetStream redelivers the delivery.
    pub(super) fn heartbeat_alive(&self) -> bool {
        self.heartbeat_task
            .as_ref()
            .is_some_and(|task| !task.is_finished())
    }

    /// Resolves once the progress heartbeat task has stopped. It can stop on
    /// its own when an attached lease is lost, and `heartbeat_alive` can miss
    /// that by a scheduling step, so a waiter holding the delivery watches
    /// this instead of sleeping past the acknowledgement deadline.
    ///
    /// Cancellation-safe by construction: the handle is only touched after
    /// the await observed the stop. Taking it before awaiting would hand
    /// ownership to a future `select!` can drop at any wake, silently
    /// detaching the heartbeat; leaving it in after a completed await hands
    /// `stop_heartbeat` a spent JoinHandle, which panics when polled again.
    pub(super) async fn heartbeat_stopped(&mut self) {
        wait_for_heartbeat_task(&mut self.heartbeat_task).await;
    }

    /// Cancel the heartbeat task's token and leave its handle in place, so
    /// the task stops on its own — the way it does when an attached lease is
    /// lost — with its spent handle left for `heartbeat_stopped` to observe
    /// and reap. Mid-sleep test seam.
    pub(super) fn cancel_heartbeat(&mut self) {
        self.heartbeat_cancel.cancel();
    }

    pub(super) fn detach_lease(&self) {
        self.lease_tx.send_replace(None);
    }

    pub(super) async fn stop_heartbeat(&mut self) {
        self.heartbeat_cancel.cancel();
        stop_heartbeat_task(self.heartbeat_task.take()).await;
    }

    pub(super) async fn ack(&mut self, reason: &str) -> Result<()> {
        self.stop_heartbeat().await;
        self.message
            .ack()
            .await
            .map_err(|error| anyhow::anyhow!("ack {reason} SessionActivate: {error}"))
    }

    pub(super) async fn nak(
        &mut self,
        delay: Option<Duration>,
        reason: ActivationNakReason,
    ) -> Result<()> {
        self.stop_heartbeat().await;
        self.message
            .ack_with(AckKind::Nak(delay))
            .await
            .map_err(|error| anyhow::anyhow!("NAK {} SessionActivate: {error}", reason.label()))?;
        metrics::counter!(harnx_metrics::ACTIVATION_NAKS_TOTAL, "reason" => reason.label())
            .increment(1);
        Ok(())
    }

    pub(super) async fn terminate(&mut self, reason: &str) -> Result<()> {
        self.stop_heartbeat().await;
        self.message
            .ack_with(AckKind::Term)
            .await
            .map_err(|error| anyhow::anyhow!("terminate {reason} SessionActivate: {error}"))
    }
}

impl Drop for ActivationDelivery {
    fn drop(&mut self) {
        // Expected paths call `stop_heartbeat` and await the task. This fallback
        // covers task cancellation or panic, where Drop cannot await.
        self.heartbeat_cancel.cancel();
        if let Some(task) = self.heartbeat_task.take() {
            task.abort();
        }
    }
}

async fn wait_for_heartbeat_task(heartbeat_task: &mut Option<JoinHandle<()>>) {
    if let Some(task) = heartbeat_task.as_mut() {
        if task.is_finished() {
            *heartbeat_task = None;
            return;
        }
        let _ = task.await;
        *heartbeat_task = None;
    }
}

async fn stop_heartbeat_task(heartbeat_task: Option<JoinHandle<()>>) {
    if let Some(task) = heartbeat_task {
        if !task.is_finished() {
            let _ = task.await;
        }
    }
}

async fn run_heartbeat<Progress, ProgressFuture>(
    heartbeat_interval: Duration,
    cancel: CancellationToken,
    mut lease_updates: watch::Receiver<Option<watch::Receiver<bool>>>,
    mut progress: Progress,
) where
    Progress: FnMut() -> ProgressFuture,
    ProgressFuture: Future<Output = ()>,
{
    let first_tick = tokio::time::Instant::now() + heartbeat_interval;
    let mut ticker = tokio::time::interval_at(first_tick, heartbeat_interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut lease_status: Option<watch::Receiver<bool>> = None;

    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => break,
            update = lease_updates.changed() => {
                if update.is_err() {
                    break;
                }
                lease_status = lease_updates.borrow_and_update().clone();
                if lease_status.as_ref().is_some_and(|status| !*status.borrow()) {
                    break;
                }
            }
            lost = wait_for_lease_loss(&mut lease_status), if lease_status.is_some() => {
                if lost {
                    break;
                }
            }
            _ = ticker.tick() => progress().await,
        }
    }
}

async fn wait_for_lease_loss(lease_status: &mut Option<watch::Receiver<bool>>) -> bool {
    let Some(status) = lease_status else {
        return false;
    };
    status.changed().await.is_err() || !*status.borrow_and_update()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn heartbeat_is_joined_before_final_disposition() {
        harnx_core::require_nextest();
        let cancel = CancellationToken::new();
        let (_lease_tx, lease_rx) = watch::channel(None);
        let progress_count = Arc::new(AtomicUsize::new(0));
        let task_count = Arc::clone(&progress_count);
        let task_cancel = cancel.clone();
        let task = tokio::spawn(run_heartbeat(
            Duration::from_millis(5),
            task_cancel,
            lease_rx,
            move || {
                let count = Arc::clone(&task_count);
                async move {
                    count.fetch_add(1, Ordering::SeqCst);
                }
            },
        ));

        tokio::time::timeout(Duration::from_secs(1), async {
            while progress_count.load(Ordering::SeqCst) < 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("heartbeat did not run");

        cancel.cancel();
        task.await.expect("join heartbeat before disposition");
        let count_at_disposition = progress_count.load(Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(
            progress_count.load(Ordering::SeqCst),
            count_at_disposition,
            "progress was sent after final disposition"
        );
    }

    #[tokio::test]
    async fn heartbeat_stopped_without_task_returns_immediately() {
        harnx_core::require_nextest();
        let mut heartbeat_task = None;

        tokio::time::timeout(
            Duration::from_millis(100),
            wait_for_heartbeat_task(&mut heartbeat_task),
        )
        .await
        .expect("missing heartbeat task should return immediately");
        assert!(heartbeat_task.is_none());
    }

    #[tokio::test]
    async fn heartbeat_stopped_clears_finished_handle() {
        harnx_core::require_nextest();
        let mut heartbeat_task = Some(tokio::spawn(async {}));
        tokio::time::timeout(Duration::from_secs(1), async {
            while !heartbeat_task.as_ref().is_some_and(JoinHandle::is_finished) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("heartbeat task did not finish");

        wait_for_heartbeat_task(&mut heartbeat_task).await;

        assert!(heartbeat_task.is_none());
    }

    #[tokio::test]
    async fn stop_heartbeat_skips_finished_handle_already_polled() {
        harnx_core::require_nextest();
        let mut heartbeat_task = tokio::spawn(async {});
        tokio::time::timeout(Duration::from_secs(1), async {
            while !heartbeat_task.is_finished() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("heartbeat task did not finish");
        (&mut heartbeat_task)
            .await
            .expect("poll heartbeat task to completion");

        tokio::time::timeout(
            Duration::from_millis(100),
            stop_heartbeat_task(Some(heartbeat_task)),
        )
        .await
        .expect("finished heartbeat handle should not be awaited again");
    }
}
