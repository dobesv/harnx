//! Assemble a wait result: the final reply once the prompt settles, the
//! activity sample while it runs. Identity and lifecycle fields are reserved
//! first; only the reply and the sample give way to the byte budget.
use super::{
    budget::{Budget, Stop},
    scope::Scope,
    serialized_len,
    snapshot::{Snapshot, LIVE_UNPROVABLE},
    Activity, DispatchConfirmation, LatestStep, Observation, Observer, PromptHandle,
    SessionPromptResult, SessionPromptStatus, WaitStop,
};
use crate::nats_session::NatsSession;
use crate::nats_session_log::NatsSessionLog;
use harnx_core::event::SubAgentProgress;
use harnx_core::text::strip_think_tag;
use harnx_toolset::ToolInvokeError;
use serde_json::Value;

/// Rows before the terminal a settled observation reads to find the final
/// reply. The reply is normally the row right before the `TurnEnd`, so a
/// short window keeps the read cheap however long the turn ran.
const REPLY_SCAN_ROWS: u64 = 32;
/// Extra bytes cut from an oversized reply so the `response_truncated` flag
/// it adds still fits; the fitting loop takes care of the rest.
const TRUNCATION_SLACK: usize = 32;

pub(super) struct Frame {
    pub(super) result: SessionPromptResult,
    max_bytes: usize,
    observed_base: u64,
    running_base: Option<u64>,
    error: Option<&'static str>,
    complete: bool,
    reply_loaded: bool,
    latest: Option<LatestStep>,
    live: Option<SubAgentProgress>,
}

impl Frame {
    pub(super) fn new(handle: &PromptHandle, max_bytes: usize) -> Self {
        Self {
            result: SessionPromptResult::empty(handle),
            max_bytes,
            observed_base: chrono::Utc::now()
                .timestamp_millis()
                .try_into()
                .unwrap_or(0),
            running_base: None,
            error: None,
            complete: false,
            reply_loaded: false,
            latest: None,
            live: None,
        }
    }

    pub(super) fn preflight(&mut self, invocation: Option<&str>) -> Result<(), ToolInvokeError> {
        self.result.invocation_id = invocation.map(str::to_owned);
        let reserved = self.reserve();
        // Reject before send mutates, never after losing an accepted handle.
        if reserved + 256 > self.max_bytes {
            return Err(ToolInvokeError::Recoverable(format!(
                "observation metadata ({reserved} bytes) leaves too little of the {} byte result budget for a reply",
                self.max_bytes
            )));
        }
        Ok(())
    }

    pub(super) fn set_invocation(&mut self, invocation: Option<&str>) {
        self.result.invocation_id = invocation.map(str::to_owned);
        if self.reserve() + 256 > self.max_bytes {
            // Diagnostic identity is optional. A long legacy identity must not
            // cost a valid handle, lifecycle status or observation metadata.
            self.result.invocation_id = None;
        }
    }

    pub(super) fn bind(&mut self, scope: &Scope) {
        self.set_invocation(Some(scope.admission.invocation_id.as_str()));
        self.running_base = Some(
            (chrono::Utc::now() - scope.admission.admitted_at)
                .num_milliseconds()
                .max(0) as u64,
        );
    }

    pub(super) fn update(&mut self, snapshot: &Snapshot) {
        self.result.status = snapshot.status();
        self.result.terminal = snapshot.terminal.is_some();
        self.complete = snapshot.complete && self.result.status != SessionPromptStatus::Unknown;
        if let Some(error) = snapshot.evidence_error() {
            self.error = Some(error);
        } else if let Some(error) = snapshot.live_error() {
            self.error = Some(error);
        } else if self.error == Some(LIVE_UNPROVABLE) {
            // The state became provable again, for example after a terminal.
            self.error = None;
        }
    }

    fn observation(&self, elapsed: u64) -> Observation {
        Observation {
            observed_at_ms: self.observed_base.saturating_add(elapsed),
            running_ms: self.running_base.map(|base| base.saturating_add(elapsed)),
            complete: self.complete,
            error: self.error,
        }
    }

    /// Bytes of the widest envelope this frame can produce without a reply
    /// or an activity sample.
    fn reserve(&self) -> usize {
        let worst = SessionPromptResult {
            invocation_id: self
                .result
                .invocation_id
                .clone()
                .or_else(|| Some("0".repeat(36))),
            dispatch: Some(DispatchConfirmation::Unconfirmed),
            status: SessionPromptStatus::Cancelled,
            stopped_by: Some(WaitStop::WIDEST),
            elapsed_ms: u64::MAX,
            observation: Observation::widest(),
            ..SessionPromptResult::empty(&PromptHandle {
                session_id: self.result.session_id.clone(),
                prompt_id: self.result.prompt_id.clone(),
            })
        };
        serialized_len(&worst)
    }

    /// Settled: read the final reply once. Running: note the latest step,
    /// which the snapshot already holds, so a poll costs no extra read.
    pub(super) async fn render(&mut self, observer: &Observer<'_>) -> Result<(), Stop> {
        let snapshot = &observer.snapshot;
        self.update(snapshot);
        if snapshot.evidence_error().is_some() {
            return Ok(());
        }
        if self.result.terminal {
            return self.load_reply(observer).await;
        }
        self.latest = observer
            .wait
            .include_partial
            .then(|| snapshot.latest_step())
            .flatten();
        Ok(())
    }

    async fn load_reply(&mut self, observer: &Observer<'_>) -> Result<(), Stop> {
        let snapshot = &observer.snapshot;
        let Some(prompt) = snapshot.prompt.filter(|_| !self.reply_loaded) else {
            return Ok(());
        };
        let end = snapshot.output_end();
        let start = prompt
            .saturating_add(1)
            .max(end.saturating_sub(REPLY_SCAN_ROWS - 1));
        let log = NatsSessionLog::new(observer.toolset.jetstream.clone(), &observer.scope.storage);
        let window = observer
            .budget
            .io(log.load_window_async(Some(start), Some(end)))
            .await?;
        observer.budget.check()?;
        let Ok(window) = window else {
            self.error = Some("presentation_unavailable");
            return Ok(());
        };
        let (reply, failure) =
            NatsSession::extract_turn_outcome_bounded(&window.entries, prompt, end);
        // Strip at the delegation boundary, as session_prompt does.
        self.result.response = reply.map(|reply| strip_think_tag(&reply).into_owned());
        self.result.failure = failure.filter(|_| self.result.status == SessionPromptStatus::Failed);
        self.reply_loaded = true;
        self.error = snapshot.live_error();
        Ok(())
    }

    /// The watcher's live counters for the activity sample.
    pub(super) fn set_live(&mut self, live: Option<SubAgentProgress>) {
        self.live = live;
    }

    /// Repair republishes an activation for a prompt that is provably pending.
    /// While the live state is unprovable (or any error is set) it stays off.
    pub(super) fn permits_repair(&self) -> bool {
        self.error.is_none()
    }

    pub(super) fn stopped(&mut self, reason: Stop) {
        // An expired wait over an unprovable live state keeps saying why the
        // state is unknown, which is more useful than that the wait ended.
        let keep_live_error = reason == Stop::Expired && self.error == Some(LIVE_UNPROVABLE);
        self.error = Some(match reason {
            // Not an error: the result is the last complete observation, and
            // the caller waits again after reading its message.
            Stop::NewMessage => {
                self.result.stopped_by = Some(WaitStop::NewMessage);
                return;
            }
            Stop::CallerDeadline => {
                // A terminal result was not cut short; the work was done.
                if !self.result.terminal {
                    self.result.stopped_by = Some(WaitStop::CallerDeadline);
                }
                return;
            }
            Stop::Expired if keep_live_error => LIVE_UNPROVABLE,
            Stop::Expired => "soft_wait_expired",
            Stop::Cancelled => "observer_cancelled",
            Stop::Unavailable => "durable_observation_unavailable",
        });
        self.result.wait_expired = reason == Stop::Expired && !self.result.terminal;
        // Presentation failure cannot revoke a proven durable terminal.
        if reason == Stop::Unavailable && !self.result.terminal {
            self.result.status = SessionPromptStatus::Unknown;
            self.complete = false;
        }
    }

    pub(super) fn finish(mut self, budget: &Budget) -> Value {
        let elapsed = budget.elapsed_ms();
        self.result.elapsed_ms = elapsed;
        self.result.observation = self.observation(elapsed);
        if !self.result.terminal {
            self.result.activity = Activity::new(self.live.as_ref(), self.latest.take());
        }
        self.fit();
        serde_json::to_value(self.result).expect("result JSON")
    }

    fn size(&self) -> usize {
        serialized_len(&self.result)
    }

    /// Preflight and bind reserve the identity, lifecycle and observation
    /// bytes even on error paths. What gives way is the activity sample, then
    /// the tail of the reply, then the failure text.
    fn fit(&mut self) {
        if self.size() <= self.max_bytes {
            return;
        }
        self.result.activity = None;
        loop {
            let size = self.size();
            if size <= self.max_bytes {
                return;
            }
            let over = size - self.max_bytes;
            let Some(reply) = self.result.response.as_mut().filter(|r| !r.is_empty()) else {
                break;
            };
            let keep = reply.len().saturating_sub(over + TRUNCATION_SLACK);
            reply.truncate(reply.floor_char_boundary(keep));
            self.result.response_truncated = true;
        }
        self.result.failure = None;
    }
}
