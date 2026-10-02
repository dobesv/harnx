//! The lock behind [`GlobalConfig`](super::GlobalConfig).
//!
//! A Tokio worker that blocks on a contended lock stops running the
//! scheduler, and that is not always just lost time. The multi-thread
//! runtime lets one parked worker at a time poll the I/O driver. When that
//! worker wakes, runs the woken task itself and the task then blocks, no
//! thread polls the driver until new work wakes another worker. If the lock
//! holder is waiting for a NATS reply from synchronous code (`block_in_place`
//! plus `Handle::block_on`), nothing reads the reply: the holder never
//! releases the lock, the waiter never wakes, and the process sits at zero
//! CPU with every socket unread. Workers in staging stopped answering
//! `/healthz` this way while a title write and the end-of-turn maintenance
//! wait met on a session's config.
//!
//! [`YieldingRawRwLock`] takes the lock without blocking when it can. When
//! the lock is contended on a multi-thread runtime, it waits inside
//! `block_in_place`, which hands the worker's core to another thread for the
//! duration. The runtime then keeps polling I/O however long the holder
//! takes. Holders should still keep NATS round trips out of the critical
//! section; this only makes a slow holder cost latency rather than the
//! process.

use parking_lot::lock_api;

/// `parking_lot`'s raw rwlock, waiting off the Tokio worker when contended.
pub struct YieldingRawRwLock {
    inner: parking_lot::RawRwLock,
}

// SAFETY: every operation forwards to `parking_lot::RawRwLock`, which upholds
// the trait's contract. Waiting inside `block_in_place` changes only which
// thread runs the Tokio scheduler while this one blocks.
unsafe impl lock_api::RawRwLock for YieldingRawRwLock {
    #[allow(clippy::declare_interior_mutable_const)]
    const INIT: Self = Self {
        inner: parking_lot::RawRwLock::INIT,
    };

    type GuardMarker = <parking_lot::RawRwLock as lock_api::RawRwLock>::GuardMarker;

    fn lock_shared(&self) {
        if !self.inner.try_lock_shared() {
            wait_off_worker(|| self.inner.lock_shared());
        }
    }

    fn try_lock_shared(&self) -> bool {
        self.inner.try_lock_shared()
    }

    unsafe fn unlock_shared(&self) {
        // SAFETY: the caller holds a shared lock, as the trait requires.
        unsafe { self.inner.unlock_shared() }
    }

    fn lock_exclusive(&self) {
        if !self.inner.try_lock_exclusive() {
            wait_off_worker(|| self.inner.lock_exclusive());
        }
    }

    fn try_lock_exclusive(&self) -> bool {
        self.inner.try_lock_exclusive()
    }

    unsafe fn unlock_exclusive(&self) {
        // SAFETY: the caller holds the exclusive lock, as the trait requires.
        unsafe { self.inner.unlock_exclusive() }
    }

    fn is_locked(&self) -> bool {
        self.inner.is_locked()
    }

    fn is_locked_exclusive(&self) -> bool {
        self.inner.is_locked_exclusive()
    }
}

/// Run a blocking lock acquisition without holding a Tokio worker hostage.
///
/// `block_in_place` panics on a current-thread runtime, where there is no
/// other worker to hand the core to, so those callers (and threads outside
/// any runtime) block in place as before.
fn wait_off_worker(wait: impl FnOnce()) {
    let multi_thread = tokio::runtime::Handle::try_current()
        .is_ok_and(|handle| handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread);
    if multi_thread {
        tokio::task::block_in_place(wait);
    } else {
        wait();
    }
}

/// A read-write lock whose contended waits leave the Tokio worker free.
pub type YieldingRwLock<T> = lock_api::RwLock<YieldingRawRwLock, T>;

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Duration;

    /// The two operations the deadlock needs, so the reproduction can run
    /// against more than one lock type.
    pub(super) trait ContendedLock: Send + Sync + 'static {
        fn read_briefly(&self);
        fn write_while(&self, critical_section: &mut dyn FnMut());
    }

    impl ContendedLock for YieldingRwLock<()> {
        fn read_briefly(&self) {
            drop(self.read());
        }

        fn write_while(&self, critical_section: &mut dyn FnMut()) {
            let _guard = self.write();
            critical_section();
        }
    }

    /// Hold the lock while waiting in `block_in_place` for a timer, the way
    /// the NATS persistence bridges wait for a reply, and let a second task,
    /// woken by the driver on a worker thread, contend for the lock. Returns
    /// whether both finished: a waiter that blocks its worker leaves the
    /// timer with no thread to fire it.
    pub(super) fn holder_and_driver_woken_waiter_finish(lock: Arc<impl ContendedLock>) -> bool {
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .expect("build runtime");
            runtime.block_on(async move {
                let (held_tx, held_rx) = tokio::sync::oneshot::channel();
                let holder = {
                    let lock = Arc::clone(&lock);
                    tokio::spawn(async move {
                        let mut held_tx = Some(held_tx);
                        lock.write_while(&mut || {
                            if let Some(held_tx) = held_tx.take() {
                                let _ = held_tx.send(());
                            }
                            tokio::task::block_in_place(|| {
                                tokio::runtime::Handle::current()
                                    .block_on(tokio::time::sleep(Duration::from_millis(200)));
                            });
                        });
                    })
                };
                held_rx.await.expect("holder took the lock");
                let waiter = tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    lock.read_briefly();
                });
                holder.await.expect("holder task");
                waiter.await.expect("waiter task");
            });
            let _ = done_tx.send(());
        });
        // The deadlock stops Tokio's timers too, so the deadline has to come
        // from outside the runtime.
        done_rx.recv_timeout(Duration::from_secs(20)).is_ok()
    }

    #[test]
    fn contended_waiter_keeps_the_io_driver_polled() {
        assert!(
            holder_and_driver_woken_waiter_finish(Arc::new(YieldingRwLock::new(()))),
            "lock waiter stranded the runtime's I/O driver"
        );
    }

    #[test]
    fn current_thread_runtime_waits_in_place() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build runtime");
        let lock = Arc::new(YieldingRwLock::new(0));
        runtime.block_on(async {
            let writer = {
                let lock = Arc::clone(&lock);
                std::thread::spawn(move || {
                    let mut guard = lock.write();
                    std::thread::sleep(Duration::from_millis(50));
                    *guard = 1;
                })
            };
            while !lock.is_locked() {
                std::thread::yield_now();
            }
            // Contended on a current-thread runtime: must block, not panic.
            assert_eq!(*lock.read(), 1);
            writer.join().expect("writer thread");
        });
    }
}
