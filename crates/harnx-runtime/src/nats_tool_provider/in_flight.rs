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

pub(super) struct InFlightCompletionGuard {
    calls: NatsInFlightCalls,
    call_id: String,
}

impl Drop for InFlightCompletionGuard {
    fn drop(&mut self) {
        self.calls.calls.lock().remove(&self.call_id);
    }
}

impl NatsInFlightCalls {
    pub(super) fn completion_guard(&self, call_id: &str) -> InFlightCompletionGuard {
        InFlightCompletionGuard {
            calls: self.clone(),
            call_id: call_id.to_owned(),
        }
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
        let failure = calls.register(registration("dropped", "s1")).await;
        let _other = calls.register(registration("other", "s1")).await;
        let guard = calls.completion_guard("dropped");
        drop(guard);
        assert!(failure.await.is_err());
        let remaining = calls.snapshot_for_session("s1").await;
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].call_id, "other");
    }

    #[tokio::test]
    async fn completion_guard_tolerates_prior_removal() {
        let calls = NatsInFlightCalls::default();
        for unavailable in [false, true] {
            let _failure = calls.register(registration("call", "s1")).await;
            let guard = calls.completion_guard("call");
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
