//! An armable parking gate shared by test seams that need to freeze one
//! request at a chosen point: a parked caller counts itself in `arrivals`
//! and waits until a test releases it.
//!
//! Production never arms the gate; unarmed, `wait` is a lock-free atomic
//! check.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

#[derive(Default)]
pub(crate) struct ParkingGate {
    /// Fast-path flag: unarmed callers never touch the mutex below.
    armed: AtomicBool,
    /// The armed gate: callers count themselves in `arrivals` and park until
    /// `released` turns true.
    state: tokio::sync::Mutex<Option<GateArm>>,
}

struct GateArm {
    arrivals: Arc<AtomicUsize>,
    released: tokio::sync::watch::Receiver<bool>,
}

impl ParkingGate {
    /// Park the caller while the gate is armed, counting it in `arrivals`.
    /// Unarmed this returns immediately.
    pub(crate) async fn wait(&self) {
        if !self.armed.load(Ordering::Acquire) {
            return;
        }
        let (arrivals, mut released) = match self.state.lock().await.as_ref() {
            Some(arm) => (Arc::clone(&arm.arrivals), arm.released.clone()),
            None => return,
        };
        arrivals.fetch_add(1, Ordering::Relaxed);
        while !*released.borrow() {
            if released.changed().await.is_err() {
                return;
            }
        }
    }

    /// Arm the gate; returns the arrival counter and the release switch.
    /// Only tests arm the gate. Re-arming replaces the stored arm — its
    /// receiver is dropped with it, while the old release switch (sender)
    /// stays with the test that armed it — so a second race round parks on
    /// a fresh switch instead of the previous round's already-released
    /// one. Callers already parked keep their own receiver clone, which
    /// their release switch still controls.
    #[cfg(test)]
    pub(crate) async fn arm(
        &self,
    ) -> (
        std::sync::Arc<AtomicUsize>,
        tokio::sync::watch::Sender<bool>,
    ) {
        let (release, released) = tokio::sync::watch::channel(false);
        let arrivals = Arc::new(AtomicUsize::new(0));
        *self.state.lock().await = Some(GateArm {
            arrivals: Arc::clone(&arrivals),
            released,
        });
        // Armed last: a caller that passes the atomic check is guaranteed to
        // find the arm below it.
        self.armed.store(true, Ordering::Release);
        (arrivals, release)
    }
}
