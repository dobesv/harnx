//! Per-server call liveness, independent of turn and cleanup lifetimes, and
//! which of a session's user messages its turn has yet to take in.
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
type InFlightMap = Mutex<InFlightState>;
static INSTANCE_IN_FLIGHT: OnceLock<std::sync::Mutex<HashMap<ServerScope, Weak<InFlightMap>>>> =
    OnceLock::new();

/// Shared handle used by tool-process supervision to fail active NATS calls,
/// and by a session-scoped cancel to find every call it should reach.
#[derive(Clone, Default)]
pub struct NatsInFlightCalls {
    state: Arc<InFlightMap>,
}

#[derive(Default)]
struct InFlightState {
    calls: HashMap<String, InFlightCall>,
    /// Keyed like the calls' `session_id`. One lock covers both maps, so a
    /// message noted while a call registers either finds the call or is
    /// already pending when the call checks.
    messages: HashMap<String, MessageMarks>,
}

/// The newest user message the session watcher saw, and the newest the turn
/// has taken in. Sequences only move forward, so a watcher that sees a
/// message after the turn already read it doesn't make it pending again.
#[derive(Clone, Copy, Default)]
struct MessageMarks {
    seen: u64,
    consumed: u64,
}

impl MessageMarks {
    fn pending(&self) -> bool {
        self.seen > self.consumed
    }
}

struct InFlightCall {
    server: String,
    session_id: String,
    control_subject: String,
    stop_on_new_message: bool,
    failure: oneshot::Sender<InFlightFailure>,
    identity: Arc<()>,
}

impl InFlightCall {
    fn target(&self, call_id: &str) -> InFlightCancelTarget {
        InFlightCancelTarget {
            call_id: call_id.to_owned(),
            server: self.server.clone(),
            control_subject: self.control_subject.clone(),
        }
    }
}

/// Everything a call is registered under: its own identity, the server
/// running it, the session it belongs to, where a cancel for it goes, and
/// whether its tool ends a wait when a new message arrives.
pub(crate) struct InFlightRegistration {
    pub call_id: String,
    pub server: String,
    pub session_id: String,
    pub control_subject: String,
    pub stop_on_new_message: bool,
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

impl<F: FnOnce()> InFlightCompletionGuard<F> {
    pub(super) fn complete(&self) -> bool {
        self.calls.complete(&self.call_id, &self.identity)
    }
}

impl<F: FnOnce()> Drop for InFlightCompletionGuard<F> {
    fn drop(&mut self) {
        // Completion and supervision remove the entry first. A replacement
        // reservation has a different identity, even when it reuses the call ID.
        if self.complete() {
            if let Some(on_drop) = self.on_drop.take() {
                on_drop();
            }
        }
    }
}

/// Forgets a session's message marks when dropped. The session watcher holds
/// one while it follows the session, so a worker that runs many sessions
/// keeps marks only for the ones it is running.
pub(crate) struct SessionMessagesGuard {
    calls: NatsInFlightCalls,
    session_id: String,
}

impl Drop for SessionMessagesGuard {
    fn drop(&mut self) {
        self.calls.state.lock().messages.remove(&self.session_id);
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
        if let Some(state) = registry.get(instance_id).and_then(Weak::upgrade) {
            return Self { state };
        }
        let state = Arc::new(Mutex::new(InFlightState::default()));
        registry.insert(instance_id.clone(), Arc::downgrade(&state));
        Self { state }
    }

    pub(crate) async fn register(
        &self,
        registration: InFlightRegistration,
    ) -> (oneshot::Receiver<InFlightFailure>, Arc<()>) {
        let identity = Arc::new(());
        let receiver = self.insert(registration, Arc::clone(&identity));
        (receiver, identity)
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
            stop_on_new_message,
        } = registration;
        let (failure, receiver) = oneshot::channel();
        self.state.lock().calls.insert(
            call_id,
            InFlightCall {
                server,
                session_id,
                control_subject,
                stop_on_new_message,
                failure,
                identity,
            },
        );
        receiver
    }

    /// Retire only this reservation, never another wait for the same call ID.
    pub(crate) fn complete(&self, call_id: &str, identity: &Arc<()>) -> bool {
        let calls = &mut self.state.lock().calls;
        if calls
            .get(call_id)
            .is_some_and(|call| Arc::ptr_eq(&call.identity, identity))
        {
            calls.remove(call_id);
            true
        } else {
            false
        }
    }

    /// Fail current calls routed to a supervised server that became unavailable.
    pub async fn fail_server_unavailable(&self, server: &str, message: impl Into<String>) {
        let message = message.into();
        let failures = {
            let calls = &mut self.state.lock().calls;
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
        self.state
            .lock()
            .calls
            .iter()
            .filter(|(_, call)| call.session_id == session_id)
            .map(|(call_id, call)| call.target(call_id))
            .collect()
    }

    /// Record that a user message landed at `seq` for the session, and return
    /// the session's calls in flight that end a wait when one does. Returns
    /// none when the turn has already taken that message in.
    pub(super) fn pending_message_targets(
        &self,
        session_id: &str,
        seq: u64,
    ) -> Vec<InFlightCancelTarget> {
        let mut state = self.state.lock();
        let InFlightState { calls, messages } = &mut *state;
        let marks = messages.entry(session_id.to_owned()).or_default();
        marks.seen = marks.seen.max(seq);
        if seq <= marks.consumed {
            return Vec::new();
        }
        calls
            .iter()
            .filter(|(_, call)| call.session_id == session_id && call.stop_on_new_message)
            .map(|(call_id, call)| call.target(call_id))
            .collect()
    }

    /// The session's turn has taken in every message through `through_seq`.
    pub fn note_messages_consumed(&self, session_id: &str, through_seq: u64) {
        let messages = &mut self.state.lock().messages;
        let marks = messages.entry(session_id.to_owned()).or_default();
        marks.consumed = marks.consumed.max(through_seq);
    }

    /// Whether a user message landed that the session's turn hasn't taken in.
    pub fn message_pending(&self, session_id: &str) -> bool {
        self.state
            .lock()
            .messages
            .get(session_id)
            .is_some_and(MessageMarks::pending)
    }

    /// Keep the session's message marks until the returned guard drops.
    pub(crate) fn follow_session_messages(&self, session_id: &str) -> SessionMessagesGuard {
        SessionMessagesGuard {
            calls: self.clone(),
            session_id: session_id.to_owned(),
        }
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
            stop_on_new_message: false,
        }
    }

    fn waiting(call_id: &str, session_id: &str) -> InFlightRegistration {
        InFlightRegistration {
            stop_on_new_message: true,
            ..registration(call_id, session_id)
        }
    }

    fn call_ids(targets: Vec<InFlightCancelTarget>) -> Vec<String> {
        let mut ids = targets
            .into_iter()
            .map(|target| target.call_id)
            .collect::<Vec<_>>();
        ids.sort();
        ids
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
        let (mut replacement_failure, replacement_identity) =
            calls.register(registration("call", "s2")).await;
        assert!(old_failure.await.is_err());
        assert!(!old_guard.complete());
        drop(old_guard);
        assert!(calls.snapshot_for_session("s1").await.is_empty());
        assert_eq!(calls.snapshot_for_session("s2").await.len(), 1);
        assert!(matches!(
            replacement_failure.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        assert!(calls.complete("call", &replacement_identity));
        assert!(!calls.complete("call", &replacement_identity));
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
                assert!(guard.complete());
            }
            drop(guard);
            assert!(calls.snapshot_for_session("s1").await.is_empty());
        }
    }

    #[tokio::test]
    async fn in_flight_new_message_reaches_only_opted_in_calls_of_its_session() {
        let calls = NatsInFlightCalls::default();
        let _waiting = calls.register(waiting("wait", "s1")).await;
        let _plain = calls.register(registration("plain", "s1")).await;
        let _elsewhere = calls.register(waiting("elsewhere", "s2")).await;
        assert_eq!(
            call_ids(calls.pending_message_targets("s1", 10)),
            vec!["wait".to_string()]
        );
    }

    #[test]
    fn in_flight_message_stays_pending_until_its_turn_consumes_it() {
        let calls = NatsInFlightCalls::default();
        assert!(!calls.message_pending("s1"), "nothing seen yet");
        calls.pending_message_targets("s1", 10);
        assert!(calls.message_pending("s1"));
        assert!(!calls.message_pending("s2"), "marks are per session");
        calls.note_messages_consumed("s1", 9);
        assert!(calls.message_pending("s1"), "seq 10 is not consumed yet");
        calls.note_messages_consumed("s1", 10);
        assert!(!calls.message_pending("s1"));
        calls.note_messages_consumed("s1", 4);
        assert!(!calls.message_pending("s1"), "consumed never moves back");
    }

    #[tokio::test]
    async fn in_flight_message_the_turn_already_read_signals_nothing() {
        let calls = NatsInFlightCalls::default();
        let _waiting = calls.register(waiting("wait", "s1")).await;
        calls.note_messages_consumed("s1", 12);
        // A watcher running behind the turn's own read sees the message late.
        assert!(calls.pending_message_targets("s1", 12).is_empty());
        assert!(!calls.message_pending("s1"));
        assert_eq!(call_ids(calls.pending_message_targets("s1", 13)).len(), 1);
    }

    #[test]
    fn in_flight_message_marks_last_as_long_as_their_session_is_followed() {
        let calls = NatsInFlightCalls::default();
        let followed = calls.follow_session_messages("s1");
        let _other = calls.follow_session_messages("s2");
        calls.pending_message_targets("s1", 3);
        calls.pending_message_targets("s2", 3);
        drop(followed);
        assert!(
            !calls.message_pending("s1"),
            "dropping the guard forgets s1"
        );
        assert!(calls.message_pending("s2"), "other sessions keep theirs");
    }
}
