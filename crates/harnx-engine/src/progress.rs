//! Call-bound tool progress accumulator.

use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use harnx_core::abort::AbortSignal;
use harnx_core::event::ToolStatus;
use harnx_core::tool::{ToolDisplayState, ToolProgress, ToolUpdatePatch};

use crate::tool::ToolUpdateEmitFn;

/// Interactive update budget. This is runtime behavior, not protocol.
const COALESCE_INTERVAL: Duration = Duration::from_millis(250);

#[derive(Default)]
struct ProgressState {
    display: ToolDisplayState,
    pending: bool,
    timer_scheduled: bool,
    terminal: bool,
}

/// Bounded, call-scoped progress handle used for in-process tool execution.
///
/// State is bounded to one merged snapshot and at most one scheduled flush.
/// First update emits immediately. Later patches merge until the timer fires or
/// [`finalize`](Self::finalize) flushes them before terminal event emission.
pub struct RuntimeToolProgress {
    call_id: String,
    emit_fn: Arc<ToolUpdateEmitFn>,
    abort: AbortSignal,
    state: Mutex<ProgressState>,
    self_weak: Weak<Self>,
}

impl RuntimeToolProgress {
    /// Bind progress to stable call identity, captured event callback, and abort signal.
    pub fn new(call_id: String, emit_fn: Arc<ToolUpdateEmitFn>, abort: AbortSignal) -> Arc<Self> {
        debug_assert!(!call_id.trim().is_empty());
        Arc::new_cyclic(|self_weak| Self {
            call_id,
            emit_fn,
            abort,
            state: Mutex::new(ProgressState::default()),
            self_weak: self_weak.clone(),
        })
    }

    /// Reject later updates and flush pending state before terminal event emission.
    pub fn finalize(&self) {
        let mut state = self.state.lock().expect("tool progress mutex poisoned");
        if state.terminal {
            return;
        }
        state.terminal = true;
        state.timer_scheduled = false;
        if self.abort.aborted() {
            state.pending = false;
            return;
        }
        Self::emit_pending_locked(&self.call_id, self.emit_fn.as_ref(), &mut state);
    }

    fn schedule_flush(&self) {
        let weak = self.self_weak.clone();
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            // Synchronous callers still get the final merged snapshot from finalize().
            return;
        };
        runtime.spawn(async move {
            tokio::time::sleep(COALESCE_INTERVAL).await;
            Self::flush_from_timer(weak);
        });
    }

    fn flush_from_timer(weak: Weak<Self>) {
        let Some(this) = weak.upgrade() else {
            return;
        };
        let mut state = this.state.lock().expect("tool progress mutex poisoned");
        state.timer_scheduled = false;
        if state.terminal || this.abort.aborted() {
            state.terminal |= this.abort.aborted();
            state.pending = false;
            return;
        }
        Self::emit_pending_locked(&this.call_id, this.emit_fn.as_ref(), &mut state);
    }

    fn emit_pending_locked(call_id: &str, emit_fn: &ToolUpdateEmitFn, state: &mut ProgressState) {
        if !state.pending {
            return;
        }
        let patch = snapshot(&state.display);
        state.pending = false;
        emit_fn(call_id, &patch);
    }

    fn apply_update(&self, mut patch: ToolUpdatePatch) {
        // Tools cannot control terminal truth. Keep useful fields from a patch
        // that also attempted to set Completed/Failed.
        if matches!(
            patch.status,
            Some(ToolStatus::Completed | ToolStatus::Failed)
        ) {
            patch.status = None;
        }
        if patch_is_empty(&patch) {
            return;
        }

        let mut schedule_timer = false;
        {
            let mut state = self.state.lock().expect("tool progress mutex poisoned");
            if state.terminal || self.abort.aborted() {
                state.terminal |= self.abort.aborted();
                state.pending = false;
                return;
            }

            state.display.apply(patch);
            if !state.pending && !state.timer_scheduled {
                // No rate-limit window is active, so emit first/new-cycle state promptly.
                state.pending = true;
                Self::emit_pending_locked(&self.call_id, self.emit_fn.as_ref(), &mut state);
                state.timer_scheduled = true;
                schedule_timer = true;
            } else {
                state.pending = true;
            }
        }

        if schedule_timer {
            self.schedule_flush();
        }
    }
}

impl ToolProgress for RuntimeToolProgress {
    fn update(&self, patch: ToolUpdatePatch) {
        self.apply_update(patch);
    }
}

fn patch_is_empty(patch: &ToolUpdatePatch) -> bool {
    patch.markdown.is_none()
        && patch.title.is_none()
        && patch.status.is_none()
        && patch.content.is_none()
        && patch.kind.is_none()
        && patch.locations.is_none()
        && patch.usage.is_none()
}

fn snapshot(state: &ToolDisplayState) -> ToolUpdatePatch {
    ToolUpdatePatch {
        markdown: state.markdown.clone(),
        title: state.title.clone(),
        status: state.status,
        content: state.content.clone(),
        kind: state.kind,
        locations: state.locations.clone(),
        usage: state.usage.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harnx_core::abort::create_abort_signal;

    struct ProgressFixture {
        progress: Arc<RuntimeToolProgress>,
        updates: Arc<Mutex<Vec<(String, ToolUpdatePatch)>>>,
        abort: AbortSignal,
    }

    fn recording_progress() -> ProgressFixture {
        let updates = Arc::new(Mutex::new(Vec::new()));
        let capture = Arc::clone(&updates);
        let emit_fn: Arc<ToolUpdateEmitFn> = Arc::new(move |call_id, patch| {
            capture
                .lock()
                .expect("updates mutex poisoned")
                .push((call_id.to_string(), patch.clone()));
        });
        let abort = create_abort_signal();
        ProgressFixture {
            progress: RuntimeToolProgress::new("call-1".to_string(), emit_fn, Arc::clone(&abort)),
            updates,
            abort,
        }
    }

    #[tokio::test(start_paused = true)]
    async fn first_update_is_prompt_and_later_updates_coalesce() {
        let ProgressFixture {
            progress, updates, ..
        } = recording_progress();

        progress.update(ToolUpdatePatch {
            title: Some("first".to_string()),
            ..Default::default()
        });
        progress.update(ToolUpdatePatch {
            title: Some("second".to_string()),
            ..Default::default()
        });
        progress.update(ToolUpdatePatch {
            locations: Some(vec![]),
            ..Default::default()
        });

        assert_eq!(updates.lock().unwrap().len(), 1);
        tokio::task::yield_now().await;
        tokio::time::advance(COALESCE_INTERVAL).await;
        tokio::task::yield_now().await;

        let updates = updates.lock().unwrap();
        assert_eq!(updates.len(), 2);
        assert_eq!(updates[0].0, "call-1");
        assert_eq!(updates[0].1.title.as_deref(), Some("first"));
        assert_eq!(updates[1].1.title.as_deref(), Some("second"));
        assert_eq!(updates[1].1.locations, Some(vec![]));
    }

    #[tokio::test(start_paused = true)]
    async fn finalize_flushes_once_before_rejecting_late_updates() {
        let ProgressFixture {
            progress, updates, ..
        } = recording_progress();
        progress.update(ToolUpdatePatch {
            title: Some("first".to_string()),
            ..Default::default()
        });
        progress.update(ToolUpdatePatch {
            title: Some("final".to_string()),
            ..Default::default()
        });

        progress.finalize();
        progress.update(ToolUpdatePatch {
            title: Some("late".to_string()),
            ..Default::default()
        });
        tokio::time::advance(COALESCE_INTERVAL).await;
        tokio::task::yield_now().await;

        let updates = updates.lock().unwrap();
        assert_eq!(updates.len(), 2);
        assert_eq!(updates[1].1.title.as_deref(), Some("final"));
        assert!(updates
            .iter()
            .all(|(_, patch)| patch.title.as_deref() != Some("late")));
    }

    #[tokio::test(start_paused = true)]
    async fn cancellation_rejects_pending_and_future_updates() {
        let ProgressFixture {
            progress,
            updates,
            abort,
        } = recording_progress();
        progress.update(ToolUpdatePatch {
            title: Some("first".to_string()),
            ..Default::default()
        });
        progress.update(ToolUpdatePatch {
            title: Some("pending".to_string()),
            ..Default::default()
        });
        abort.set_ctrlc();

        tokio::time::advance(COALESCE_INTERVAL).await;
        tokio::task::yield_now().await;
        progress.update(ToolUpdatePatch {
            title: Some("late".to_string()),
            ..Default::default()
        });
        progress.finalize();

        let updates = updates.lock().unwrap();
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].1.title.as_deref(), Some("first"));
    }

    #[tokio::test(start_paused = true)]
    async fn terminal_only_and_empty_patches_do_not_emit() {
        let ProgressFixture {
            progress, updates, ..
        } = recording_progress();
        progress.update(ToolUpdatePatch::default());
        progress.update(ToolUpdatePatch {
            status: Some(ToolStatus::Completed),
            ..Default::default()
        });
        progress.finalize();
        assert!(updates.lock().unwrap().is_empty());
    }
}
