//! Server-owned session state with idle and active-turn tracking.
//!
//! `NatsSession` has no server idle timestamp, so the ACP bridge records activity
//! on prompt start, turn completion, and cancel. Active-turn state also owns the
//! local cancellation sender and stays occupied until the turn's guard drops.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use harnx_runtime::NatsSession;
use parking_lot::Mutex;
use tokio::sync::mpsc;

use crate::handoff::HandoffTarget;

/// Default idle timeout before reaping an inactive session (15 minutes).
pub const SESSION_IDLE_TTL: Duration = Duration::from_secs(15 * 60);

enum SessionBackend {
    Nats(Box<NatsSession>),
    #[cfg(test)]
    Test,
}

struct InFlightTurn {
    cancel_tx: mpsc::Sender<()>,
}

#[derive(Default)]
struct SessionState {
    active_turn: Option<InFlightTurn>,
    handoff_target: Option<HandoffTarget>,
}

/// Reason a prompt cannot acquire the session turn gate.
#[derive(Debug, Eq, PartialEq)]
pub(crate) enum BeginTurnError {
    /// Another prompt is still running or cancelling.
    Active,
    /// Source stopped accepting prompts after this committed handoff.
    HandedOff(HandoffTarget),
}

/// Releases one session's active-turn gate on every exit path.
///
/// `finish()` handles normal completion before `prompt()` returns. `Drop` is the
/// backstop for future cancellation or panic while a turn is still running.
pub(crate) struct TurnGuard {
    session: Arc<SessionContext>,
    finished: bool,
}

impl TurnGuard {
    pub(crate) fn finish(mut self) {
        self.session.finish_turn();
        self.finished = true;
    }
}

impl Drop for TurnGuard {
    fn drop(&mut self) {
        if !self.finished {
            self.session.finish_turn();
        }
    }
}

/// Server-owned state for one NATS session.
pub struct SessionContext {
    backend: SessionBackend,
    /// Milliseconds since `created_at`, stored atomically for idle checks.
    last_active_ms: AtomicU64,
    created_at: Instant,
    /// One lock linearizes handoff commit, turn admission, finish, and cancel.
    /// No session-state operation acquires a second mutex.
    state: Mutex<SessionState>,
}

impl SessionContext {
    /// Create a new session context wrapping a NATS session.
    pub fn new(nats_session: NatsSession) -> Self {
        Self::from_backend(SessionBackend::Nats(Box::new(nats_session)))
    }

    fn from_backend(backend: SessionBackend) -> Self {
        Self {
            backend,
            last_active_ms: AtomicU64::new(0),
            created_at: Instant::now(),
            state: Mutex::new(SessionState::default()),
        }
    }

    #[cfg(test)]
    pub(crate) fn new_for_test() -> Self {
        Self::from_backend(SessionBackend::Test)
    }

    /// Update the last activity timestamp to prevent idle reaping.
    pub fn touch(&self) {
        let now_ms = Instant::now()
            .saturating_duration_since(self.created_at)
            .as_millis() as u64;
        self.last_active_ms.store(now_ms, Ordering::Release);
    }

    /// Check if this session has been idle longer than the given TTL.
    pub fn is_idle_longer_than(&self, ttl: Duration) -> bool {
        self.elapsed_since_touch() > ttl
    }

    /// Get the elapsed time since the last touch.
    pub fn elapsed_since_touch(&self) -> Duration {
        self.created_at
            .elapsed()
            .saturating_sub(self.last_touched())
    }

    /// Get a reference to the underlying NATS session.
    pub fn nats_session(&self) -> &NatsSession {
        match &self.backend {
            SessionBackend::Nats(session) => session,
            #[cfg(test)]
            SessionBackend::Test => panic!("test session has no NATS backend"),
        }
    }

    /// Record first authoritative target. Later duplicate commits have no effect.
    pub(crate) fn commit_handoff(&self, target: HandoffTarget) -> bool {
        let mut state = self.state.lock();
        if state.handoff_target.is_some() {
            return false;
        }
        state.handoff_target = Some(target);
        true
    }

    /// Return committed target when source no longer accepts prompts.
    pub fn handoff_target(&self) -> Option<HandoffTarget> {
        self.state.lock().handoff_target.clone()
    }

    /// Atomically reject handed-off/active sessions or acquire turn ownership.
    pub(crate) fn begin_turn(
        self: &Arc<Self>,
    ) -> Result<(TurnGuard, mpsc::Receiver<()>), BeginTurnError> {
        let mut state = self.state.lock();
        if let Some(target) = &state.handoff_target {
            return Err(BeginTurnError::HandedOff(target.clone()));
        }
        if state.active_turn.is_some() {
            return Err(BeginTurnError::Active);
        }
        let (cancel_tx, cancel_rx) = mpsc::channel(1);
        state.active_turn = Some(InFlightTurn { cancel_tx });
        Ok((
            TurnGuard {
                session: Arc::clone(self),
                finished: false,
            },
            cancel_rx,
        ))
    }

    /// Clear the active-turn gate after the prompt follower has finished unwinding.
    fn finish_turn(&self) {
        self.state.lock().active_turn.take();
        self.touch();
    }

    /// Wake the in-process prompt follower without releasing the active-turn gate.
    pub(crate) fn cancel_local_turn(&self) -> bool {
        let state = self.state.lock();
        let Some(turn) = state.active_turn.as_ref() else {
            return false;
        };
        match turn.cancel_tx.try_send(()) {
            Ok(()) | Err(mpsc::error::TrySendError::Full(())) => true,
            Err(mpsc::error::TrySendError::Closed(())) => false,
        }
    }

    /// Monotonic timestamp of the most recent activity, relative to creation.
    pub fn last_touched(&self) -> Duration {
        Duration::from_millis(self.last_active_ms.load(Ordering::Acquire))
    }

    /// Get the session ID string.
    pub fn session_id(&self) -> &str {
        self.nats_session().session_id()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn touch_updates_real_session_context_timestamp() {
        let context = SessionContext::new_for_test();
        let before = context.last_touched();
        std::thread::sleep(Duration::from_millis(5));

        context.touch();

        assert!(context.last_touched() > before);
        assert!(context.elapsed_since_touch() < Duration::from_millis(20));
    }

    #[test]
    fn real_session_context_idle_check_respects_touch() {
        let context = SessionContext::new_for_test();
        std::thread::sleep(Duration::from_millis(5));
        assert!(context.is_idle_longer_than(Duration::from_millis(1)));

        context.touch();

        assert!(!context.is_idle_longer_than(Duration::from_secs(1)));
    }

    #[test]
    fn cancelled_turn_stays_in_flight_until_guard_drops() {
        let context = Arc::new(SessionContext::new_for_test());
        let (guard, mut cancel_rx) = context.begin_turn().expect("first turn starts");
        assert!(matches!(context.begin_turn(), Err(BeginTurnError::Active)));

        assert!(context.cancel_local_turn());
        assert_eq!(cancel_rx.try_recv(), Ok(()));
        assert!(
            matches!(context.begin_turn(), Err(BeginTurnError::Active)),
            "cancel signal must not release active-turn gate"
        );

        drop(guard);
        assert!(context.begin_turn().is_ok(), "guard drop releases gate");
    }

    #[test]
    fn committed_handoff_at_admission_boundary_rejects_source_turn() {
        let context = Arc::new(SessionContext::new_for_test());
        let first = HandoffTarget::from_committed("atlas@prod", "target-1", "source")
            .expect("valid target");
        let duplicate = HandoffTarget::from_committed("other@prod", "target-2", "source")
            .expect("valid target");

        // Reproduce the old race: prompt observed active source, then commit won
        // immediately before admission. `begin_turn` must make the final decision.
        assert!(context.handoff_target().is_none());
        assert!(context.commit_handoff(first.clone()));
        assert!(matches!(
            context.begin_turn(),
            Err(BeginTurnError::HandedOff(target)) if target == first
        ));
        assert!(!context.cancel_local_turn(), "no source turn was admitted");
        assert!(!context.commit_handoff(duplicate));
        assert_eq!(context.handoff_target(), Some(first));
    }

    #[test]
    fn explicit_finish_releases_active_turn_gate() {
        let context = Arc::new(SessionContext::new_for_test());
        let (guard, _cancel_rx) = context.begin_turn().expect("first turn starts");

        guard.finish();

        assert!(context.begin_turn().is_ok());
    }

    #[test]
    fn turn_guard_releases_gate_during_unwind() {
        let context = Arc::new(SessionContext::new_for_test());
        let panicking_context = Arc::clone(&context);

        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let (_guard, _cancel_rx) = panicking_context
                .begin_turn()
                .expect("turn starts before panic");
            panic!("simulate prompt task panic");
        }));

        assert!(panic.is_err());
        assert!(context.begin_turn().is_ok(), "unwind must release gate");
    }
}
