//! Tracks in-flight tool requests so shutdown can wait for them to finish
//! before deregistering, instead of deleting the registration while a
//! caller is still waiting on a reply (which would leave that caller
//! blocked until its own 60s timeout).
//!
//! Split out of `lib.rs`: this tracking shares no data with the idempotency
//! cache or registration-refresh logic, and keeping it there pushed the
//! module over CodeScene's cohesion threshold.

use std::time::Duration;
use tokio::sync::watch;

/// How long shutdown waits for in-flight tool requests to finish replying
/// before deregistering anyway. Bounded so a stuck tool invocation can't
/// block shutdown forever; well under typical Kubernetes
/// terminationGracePeriodSeconds defaults (30s), leaving room for the rest
/// of shutdown.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(10);

/// Shared counter of tool requests currently being processed.
#[derive(Clone)]
pub(crate) struct InFlightRequests {
    count: watch::Sender<usize>,
}

impl InFlightRequests {
    /// Build a fresh, empty tracker plus the receiver `drain` needs to wait
    /// for it to reach zero.
    pub(crate) fn new() -> (Self, watch::Receiver<usize>) {
        let (count, count_rx) = watch::channel(0usize);
        (Self { count }, count_rx)
    }

    /// Mark one request as started. The count is decremented automatically
    /// when the returned guard drops, including on panic, so a request
    /// handler can't leak the count by returning early or unwinding.
    pub(crate) fn enter(&self) -> InFlightGuard {
        self.count.send_modify(|count| *count += 1);
        InFlightGuard {
            count: self.count.clone(),
        }
    }
}

pub(crate) struct InFlightGuard {
    count: watch::Sender<usize>,
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.count.send_modify(|count| *count -= 1);
    }
}

/// Wait for `active_request_count` to reach zero, up to `DRAIN_TIMEOUT`.
///
/// Uses `watch` rather than `Notify` so a decrement that lands between our
/// check and the wait can't be missed: `watch::Receiver::changed` always
/// fires for a value this receiver hasn't observed yet, where
/// `Notify::notified` can miss a waiter that hasn't polled it yet.
///
/// Requests still running past the deadline are not aborted -- they keep
/// replying to their NATS reply subject in the background, which doesn't
/// depend on this server's registration still existing. Aborting them would
/// turn "the caller gets a delayed reply" into "the caller gets no reply at
/// all", which is worse.
pub(crate) async fn drain(mut active_request_count: watch::Receiver<usize>) {
    if *active_request_count.borrow() == 0 {
        return;
    }
    let drained = tokio::time::timeout(DRAIN_TIMEOUT, async {
        loop {
            if *active_request_count.borrow() == 0 {
                return;
            }
            if active_request_count.changed().await.is_err() {
                return;
            }
        }
    })
    .await;
    if drained.is_err() {
        log::warn!(
            "shutdown reached the {}s drain deadline with {} tool request(s) still in flight; \
             deregistering without waiting further",
            DRAIN_TIMEOUT.as_secs(),
            *active_request_count.borrow()
        );
    }
}
