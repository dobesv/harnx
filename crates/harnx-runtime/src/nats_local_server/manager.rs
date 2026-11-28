//! Process-wide owner of managed-local startup and recovery.

use super::supervision::BrokerWatch;
use super::{BrokerNoticeSink, LocalBroker, LocalBrokerStatus};
use anyhow::{bail, Result};
use async_nats::Client;
use harnx_core::abort::{wait_abort_signal, AbortSignal};
use parking_lot::Mutex as SyncMutex;
use std::{
    future::Future,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, OnceLock,
    },
};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

static MANAGER: OnceLock<BrokerManager> = OnceLock::new();

#[derive(Default)]
struct BrokerManager {
    broker: Mutex<Option<LocalBroker>>,
    scope_started: AtomicBool,
    notices: Arc<SyncMutex<Option<BrokerNoticeSink>>>,
    stop: CancellationToken,
}

impl BrokerManager {
    async fn observer(&self) -> Result<BrokerWatch> {
        let mut broker = tokio::select! {
            biased;
            () = self.stop.cancelled() => bail!("shared local NATS is shutting down"),
            slot = self.broker.lock() => slot,
        };
        Ok(self.ensure_broker(&mut broker))
    }

    fn ensure_broker(&self, slot: &mut Option<LocalBroker>) -> BrokerWatch {
        // Do not retain a failed producer: its existing observers get its
        // error, while later callers can try a new acquisition.
        if slot.as_ref().is_some_and(|broker| !broker.is_running()) {
            slot.take();
        }
        slot.get_or_insert_with(|| self.launch()).observer()
    }

    fn launch(&self) -> LocalBroker {
        LocalBroker::launch(self.stop.child_token(), Some(self.notice_reporter()))
    }

    fn notice_reporter(&self) -> BrokerNoticeSink {
        let notices = Arc::clone(&self.notices);
        Arc::new(move |notice| {
            // Snapshot the route, then release the lock before calling it.
            // A notice already dispatched to the old route is not forwarded.
            let sink = notices.lock().clone();
            if let Some(sink) = sink {
                sink(notice);
            }
        })
    }

    /// Install `sink` and return the previously installed sink, if any, so
    /// callers that later fail can restore the route they replaced.
    fn set_notice_sink(&self, sink: Option<BrokerNoticeSink>) -> Option<BrokerNoticeSink> {
        std::mem::replace(&mut *self.notices.lock(), sink)
    }

    async fn shutdown(&self) -> Result<()> {
        let mut slot = self.broker.lock().await;
        if let Some(broker) = slot.as_ref() {
            broker.request_stop();
        }
        self.stop.cancel();
        self.set_notice_sink(None);
        // Retain the join handle if the caller drops this shutdown future.
        // New observers select on `stop` rather than blocking behind this join.
        let result = match slot.as_mut() {
            Some(broker) => broker.shutdown().await,
            None => Ok(()),
        };
        slot.take();
        result
    }
}

fn manager() -> &'static BrokerManager {
    MANAGER.get_or_init(BrokerManager::default)
}

/// Move the frontend's broker-wait notice route to the sink the current
/// frontend owns (the TUI's live event channel, installed right before
/// terminal setup). Only future notices take the new route; the manager
/// retains nothing for replay, so switching cannot flood the new sink.
/// Returns the previously installed sink so the frontend can restore it
/// if it fails before its replacement route is live (bootstrap failure).
/// Fails when no frontend scope was installed; there is then nothing being
/// routed anyway.
pub fn reroute_frontend_broker_notices(sink: BrokerNoticeSink) -> Result<Option<BrokerNoticeSink>> {
    let manager = manager();
    anyhow::ensure!(
        manager.scope_started.load(Ordering::Acquire),
        "local NATS frontend scope was not installed"
    );
    Ok(manager.set_notice_sink(Some(sink)))
}

/// Restore the route `reroute_frontend_broker_notices` replaced, for the
/// window between a frontend failure (e.g. terminal setup error) and the
/// frontend scope's own shutdown. Passing `None` stops notice delivery.
/// Idempotent: a second restore with the same value changes nothing.
pub fn restore_frontend_broker_notices(previous: Option<BrokerNoticeSink>) {
    let manager = manager();
    if !manager.scope_started.load(Ordering::Acquire) {
        // No frontend scope: there is no route to restore.
        return;
    }
    manager.set_notice_sink(previous);
}

/// Frontend-owned lifetime of the process-wide broker manager. Install before
/// dispatching any command or spawning work. All configuration and worker
/// requests share this manager, including any startup already in progress.
/// Use `shutdown().await` before returning from the frontend's async entry point.
pub struct LocalBrokerScope {
    manager: &'static BrokerManager,
}

impl LocalBrokerScope {
    pub fn begin(sink: BrokerNoticeSink) -> Result<Self> {
        let manager = manager();
        if manager.scope_started.swap(true, Ordering::AcqRel) {
            bail!("local NATS frontend scope was already installed");
        }
        manager.set_notice_sink(Some(sink));
        Ok(Self { manager })
    }

    /// Replace the bootstrap route before entering the terminal. Only future
    /// notices go to the new sink; none are retained for replay.
    // qual:api
    pub fn set_notice_sink(&self, sink: BrokerNoticeSink) {
        self.manager.set_notice_sink(Some(sink));
    }

    /// Close admission, wake waiters, and join startup/recovery after reaping
    /// any owned child. This shuts down the process-wide manager permanently.
    // qual:api
    pub async fn shutdown(&mut self) -> Result<()> {
        self.manager.shutdown().await
    }
}

impl Drop for LocalBrokerScope {
    fn drop(&mut self) {
        // Fallback for unwinding. The async exit path must use shutdown().
        self.manager.set_notice_sink(None);
        self.manager.stop.cancel();
    }
}

pub(crate) async fn connect_local_broker(
    abort: Option<&AbortSignal>,
) -> Result<(BrokerWatch, LocalBrokerStatus, Client)> {
    cancel_broker_wait(abort, async {
        let observer = manager().observer().await?;
        let (server, client) = observer.clone().connect().await?;
        Ok((observer, server, client))
    })
    .await
}

/// Cancellation surrounds state acquisition as well as waiting on readiness.
/// The producer is never given this signal.
pub(crate) async fn cancel_broker_wait<T>(
    abort: Option<&AbortSignal>,
    wait: impl Future<Output = Result<T>>,
) -> Result<T> {
    let Some(abort) = abort else {
        return wait.await;
    };
    tokio::select! {
        biased;
        () = wait_abort_signal(abort) => bail!("cancelled while waiting for NATS"),
        result = wait => result,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::poll;
    use harnx_core::abort::create_abort_signal;

    #[test]
    fn notice_dispatch_releases_route_lock_and_does_not_replay() {
        use super::super::BrokerWaitNotice;
        use std::sync::mpsc;

        let manager = BrokerManager::default();
        let report = manager.notice_reporter();
        let (bootstrap_tx, bootstrap) = mpsc::channel();
        let routes = Arc::downgrade(&manager.notices);
        manager.set_notice_sink(Some(Arc::new(move |notice| {
            // A real sink may hand off to a separate output worker. Route
            // replacement and clearing must never need that worker to finish.
            assert!(routes.upgrade().unwrap().try_lock().is_some());
            bootstrap_tx.send(notice.to_string()).unwrap();
        })));
        report(BrokerWaitNotice::test_notice(11));

        let (live_tx, live) = mpsc::channel();
        let previous = manager.set_notice_sink(Some(Arc::new(move |notice| {
            live_tx.send(notice.to_string()).unwrap();
        })));
        assert!(live.try_recv().is_err(), "no bootstrap backlog on transfer");
        report(BrokerWaitNotice::test_notice(12));
        assert!(bootstrap.try_recv().unwrap().contains("PID 11"));
        assert!(bootstrap.try_recv().is_err(), "no new notices on old route");
        assert!(live.try_recv().unwrap().contains("PID 12"));

        manager.set_notice_sink(previous);
        report(BrokerWaitNotice::test_notice(13));
        assert!(bootstrap.try_recv().unwrap().contains("PID 13"));
        assert!(live.try_recv().is_err(), "no notices on retired route");
        manager.set_notice_sink(None);
        report(BrokerWaitNotice::test_notice(14));
        assert_eq!(bootstrap.try_recv(), Err(mpsc::TryRecvError::Disconnected));
    }

    #[tokio::test]
    async fn waiting_for_nats_manager_state_is_cancellable() {
        let slot = manager().broker.lock().await;
        let abort = create_abort_signal();
        let waiting = connect_local_broker(Some(&abort));
        tokio::pin!(waiting);
        assert!(poll!(&mut waiting).is_pending());
        abort.set_ctrlc();
        // Poll immediately with the lock still held. A select placed inside
        // observer() (after lock acquisition) would leave this pending.
        let std::task::Poll::Ready(Err(error)) = poll!(&mut waiting) else {
            panic!("cancel must wake the state wait");
        };
        assert!(error.to_string().contains("cancelled"));
        assert!(slot.is_none());
    }
}
