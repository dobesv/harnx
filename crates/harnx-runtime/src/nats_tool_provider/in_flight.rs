//! Per-server call liveness, independent of turn and cleanup lifetimes.
use harnx_core::instance::ServerScope;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::{Arc, OnceLock, Weak};
use tokio::sync::oneshot;
#[derive(Clone, Debug)]
pub(crate) enum InFlightFailure {
    Unavailable(String),
}

// Every critical section is synchronous. A sync mutex lets dropped waits remove
// their entries immediately, without spawning cleanup that can outlive a runtime.
type InFlightMap = Mutex<HashMap<String, InFlightCall>>;
static INSTANCE_IN_FLIGHT: OnceLock<std::sync::Mutex<HashMap<ServerScope, Weak<InFlightMap>>>> =
    OnceLock::new();

/// Shared handle used by tool-process supervision to fail active NATS calls,
/// and by a session-scoped cancel to find every call it should reach.
#[derive(Clone, Default)]
pub struct NatsInFlightCalls {
    calls: Arc<InFlightMap>,
}

struct InFlightCall {
    server: String,
    session_id: String,
    control_subject: String,
    failure: oneshot::Sender<InFlightFailure>,
    identity: Arc<()>,
}

/// Everything a call is registered under: its own identity, the server
/// running it, the session it belongs to, and where a cancel for it goes.
pub(crate) struct InFlightRegistration {
    pub call_id: String,
    pub server: String,
    pub session_id: String,
    pub control_subject: String,
}

/// One in-flight call a session-scoped cancel can address: which server owns
/// it and where to publish the cancel control message.
#[derive(Clone, Debug)]
pub struct InFlightCancelTarget {
    pub call_id: String,
    pub server: String,
    pub control_subject: String,
}

/// Remove an abandoned registration and cancel only the invocation it owned.
pub(super) struct InFlightCompletionGuard<F: FnOnce()> {
    calls: NatsInFlightCalls,
    call_id: String,
    identity: Arc<()>,
    on_drop: Option<F>,
}

impl<F: FnOnce()> Drop for InFlightCompletionGuard<F> {
    fn drop(&mut self) {
        let removed = {
            let mut calls = self.calls.calls.lock();
            if calls
                .get(&self.call_id)
                .is_some_and(|call| Arc::ptr_eq(&call.identity, &self.identity))
            {
                calls.remove(&self.call_id)
            } else {
                None
            }
        };
        // Completion and supervision remove the entry first. A replacement
        // reservation has a different identity, even when it reuses the call ID.
        if let Some(on_drop) = removed.and(self.on_drop.take()) {
            on_drop();
        }
    }
}

impl NatsInFlightCalls {
    pub(super) async fn register_guarded<F: FnOnce()>(
        &self,
        registration: InFlightRegistration,
        on_drop: F,
    ) -> (
        oneshot::Receiver<InFlightFailure>,
        InFlightCompletionGuard<F>,
    ) {
        let identity = Arc::new(());
        let guard = InFlightCompletionGuard {
            calls: self.clone(),
            call_id: registration.call_id.clone(),
            identity: Arc::clone(&identity),
            on_drop: Some(on_drop),
        };
        let receiver = self.insert(registration, identity);
        (receiver, guard)
    }

    /// Return the process-wide handle shared by provider and supervisor for an instance.
    pub fn for_instance(instance_id: &ServerScope) -> Self {
        let registry = INSTANCE_IN_FLIGHT.get_or_init(|| std::sync::Mutex::new(HashMap::new()));
        let mut registry = registry
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        registry.retain(|_, calls| calls.strong_count() > 0);
        if let Some(calls) = registry.get(instance_id).and_then(Weak::upgrade) {
            return Self { calls };
        }
        let calls = Arc::new(Mutex::new(HashMap::new()));
        registry.insert(instance_id.clone(), Arc::downgrade(&calls));
        Self { calls }
    }

    pub(crate) async fn register(
        &self,
        registration: InFlightRegistration,
    ) -> oneshot::Receiver<InFlightFailure> {
        self.insert(registration, Arc::new(()))
    }

    fn insert(
        &self,
        registration: InFlightRegistration,
        identity: Arc<()>,
    ) -> oneshot::Receiver<InFlightFailure> {
        let InFlightRegistration {
            call_id,
            server,
            session_id,
            control_subject,
        } = registration;
        let (failure, receiver) = oneshot::channel();
        self.calls.lock().insert(
            call_id,
            InFlightCall {
                server,
                session_id,
                control_subject,
                failure,
                identity,
            },
        );
        receiver
    }

    pub(crate) async fn complete(&self, call_id: &str) {
        self.calls.lock().remove(call_id);
    }

    /// Fail current calls routed to a supervised server that became unavailable.
    pub async fn fail_server_unavailable(&self, server: &str, message: impl Into<String>) {
        let message = message.into();
        let failures = {
            let mut calls = self.calls.lock();
            let call_ids = calls
                .iter()
                .filter(|(_, call)| call.server == server)
                .map(|(call_id, _)| call_id.clone())
                .collect::<Vec<_>>();
            call_ids
                .into_iter()
                .filter_map(|call_id| calls.remove(&call_id).map(|call| call.failure))
                .collect::<Vec<_>>()
        };
        for failure in failures {
            let _ = failure.send(InFlightFailure::Unavailable(message.clone()));
        }
    }

    /// Snapshot the calls in flight for one session, so a session-scoped
    /// cancel can address each of them without holding the registry lock.
    pub async fn snapshot_for_session(&self, session_id: &str) -> Vec<InFlightCancelTarget> {
        self.calls
            .lock()
            .iter()
            .filter(|(_, call)| call.session_id == session_id)
            .map(|(call_id, call)| InFlightCancelTarget {
                call_id: call_id.clone(),
                server: call.server.clone(),
                control_subject: call.control_subject.clone(),
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registration(call_id: &str, session_id: &str) -> InFlightRegistration {
        InFlightRegistration {
            call_id: call_id.into(),
            server: "srv".into(),
            session_id: session_id.into(),
            control_subject: "ctl.srv".into(),
        }
    }

    #[tokio::test]
    async fn snapshot_is_scoped_to_one_session() {
        let calls = NatsInFlightCalls::default();
        let _a = calls.register(registration("a", "s1")).await;
        let _b = calls.register(registration("b", "s2")).await;
        let snapshot = calls.snapshot_for_session("s1").await;
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].call_id, "a");
        assert_eq!(snapshot[0].control_subject, "ctl.srv");
    }

    #[tokio::test]
    async fn completion_guard_removes_only_its_call() {
        let calls = NatsInFlightCalls::default();
        let cancellations = std::sync::atomic::AtomicUsize::new(0);
        let (failure, guard) = calls
            .register_guarded(registration("dropped", "s1"), || {
                cancellations.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            })
            .await;
        let _other = calls.register(registration("other", "s1")).await;
        drop(guard);
        assert!(failure.await.is_err());
        assert_eq!(cancellations.load(std::sync::atomic::Ordering::SeqCst), 1);
        let remaining = calls.snapshot_for_session("s1").await;
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].call_id, "other");
    }
    #[tokio::test]
    async fn stale_completion_guard_preserves_replacement_reservation() {
        let calls = NatsInFlightCalls::default();
        let (old_failure, old_guard) = calls
            .register_guarded(registration("call", "s1"), || {
                panic!("an old reservation must not cancel its replacement")
            })
            .await;
        let mut replacement_failure = calls.register(registration("call", "s2")).await;
        assert!(old_failure.await.is_err());
        drop(old_guard);
        assert!(calls.snapshot_for_session("s1").await.is_empty());
        assert_eq!(calls.snapshot_for_session("s2").await.len(), 1);
        assert!(matches!(
            replacement_failure.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        calls.complete("call").await;
    }

    #[tokio::test]
    async fn completion_guard_tolerates_prior_removal() {
        let calls = NatsInFlightCalls::default();
        for unavailable in [false, true] {
            let (_failure, guard) = calls
                .register_guarded(registration("call", "s1"), || {
                    panic!("a removed call must not be cancelled again")
                })
                .await;
            if unavailable {
                calls.fail_server_unavailable("srv", "stopped").await;
            } else {
                calls.complete("call").await;
            }
            drop(guard);
            assert!(calls.snapshot_for_session("s1").await.is_empty());
        }
    }
}
