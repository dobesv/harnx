//! Reserve the whole result, including live metadata, before fitting a page.
use super::{
    budget::{Budget, Stop},
    scope::Scope,
    snapshot::Snapshot,
    DispatchConfirmation, Observer, PromptHandle, SessionPromptResult, SessionPromptStatus,
};
use crate::nats_session_log::NatsSessionLog;
use crate::nats_worker::subagent_toolset::session_read::{
    read_session_log_window, ReadMode, ResolvedRead,
};
use harnx_toolset::ToolInvokeError;
use serde_json::{json, Value};

pub(super) struct Frame {
    pub(super) result: SessionPromptResult,
    max_bytes: usize,
    observed_base: u64,
    running_base: Option<u64>,
    error: Option<&'static str>,
    complete: bool,
    rendered_through: Option<u64>,
}

impl Frame {
    pub(super) fn new(handle: &PromptHandle, max_bytes: usize) -> Self {
        Self {
            result: SessionPromptResult {
                session_id: handle.session_id.clone(),
                prompt_id: handle.prompt_id.clone(),
                invocation_id: None,
                dispatch: None,
                status: SessionPromptStatus::Unknown,
                terminal: false,
                wait_expired: false,
                elapsed_ms: 0,
                output: json!({}),
            },
            max_bytes,
            observed_base: chrono::Utc::now()
                .timestamp_millis()
                .try_into()
                .unwrap_or(0),
            running_base: None,
            error: None,
            complete: false,
            rendered_through: None,
        }
    }

    pub(super) fn preflight(&mut self, invocation: Option<&str>) -> Result<(), ToolInvokeError> {
        self.result.invocation_id = invocation.map(str::to_owned);
        let reserved = self.reserve();
        // Reject before send mutates, never after losing an accepted handle.
        if reserved + 256 > self.max_bytes {
            return Err(ToolInvokeError::Recoverable(format!("observation metadata ({reserved} bytes) leaves insufficient max_output_bytes budget {} for a bounded page", self.max_bytes)));
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
        }
    }

    fn observation(&self, elapsed: u64) -> Value {
        json!({"observed_at_ms": self.observed_base.saturating_add(elapsed),
            "running_ms": self.running_base.map(|base| base.saturating_add(elapsed)),
            "complete": self.complete, "error": self.error})
    }

    fn reserve(&self) -> usize {
        let worst = SessionPromptResult {
            session_id: self.result.session_id.clone(),
            prompt_id: self.result.prompt_id.clone(),
            invocation_id: self
                .result
                .invocation_id
                .clone()
                .or_else(|| Some("0".repeat(36))),
            dispatch: Some(DispatchConfirmation::Unconfirmed),
            status: SessionPromptStatus::Cancelled,
            terminal: false,
            wait_expired: false,
            elapsed_ms: u64::MAX,
            output: Value::Null,
        };
        let extra = json!({"observation": {"observed_at_ms": u64::MAX, "running_ms": u64::MAX,
            "complete": false, "error": "durable_observation_unavailable"}});
        serde_json::to_vec(&worst).expect("envelope JSON").len() - 4
            + serde_json::to_vec(&extra).expect("observation JSON").len()
            - 1
    }

    pub(super) async fn render(&mut self, observer: &Observer<'_>) -> Result<(), Stop> {
        let snapshot = &observer.snapshot;
        self.update(snapshot);
        if self.rendered_through == Some(snapshot.through) {
            return Ok(());
        }
        if !self.should_render(observer) {
            return Ok(());
        }
        let read = match scoped_read(observer) {
            Ok(read) => read,
            Err(error) => {
                self.error = Some(error);
                return Ok(());
            }
        };
        let log = NatsSessionLog::new(observer.toolset.jetstream.clone(), &observer.scope.storage);
        let page = observer
            .budget
            .io(read_session_log_window(&log, &read, self.reserve()))
            .await?;
        observer.budget.check()?;
        match page {
            Ok(page) => {
                self.result.output = page;
                self.rendered_through = Some(snapshot.through);
                self.error = None;
            }
            Err(_) => {
                self.error = Some("presentation_unavailable");
            }
        }
        Ok(())
    }

    fn should_render(&self, observer: &Observer<'_>) -> bool {
        let allowed = observer.wait.include_partial || self.result.terminal;
        allowed && observer.snapshot.evidence_error().is_none()
    }

    pub(super) fn permits_repair(&self) -> bool {
        self.error.is_none()
    }

    pub(super) fn stopped(&mut self, reason: Stop) {
        self.error = Some(match reason {
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
        let observation = self.observation(elapsed);
        self.result
            .output
            .as_object_mut()
            .expect("page object")
            .insert("observation".into(), observation.clone());
        let mut value = serde_json::to_value(self.result).expect("result JSON");
        // Preflight and bind reserve this metadata even on error paths. Only
        // presentation is expendable; identifiers and lifecycle proof stay intact.
        if serde_json::to_vec(&value).expect("result JSON").len() > self.max_bytes {
            value["output"] = json!({"observation": observation});
        }
        value
    }
}

fn scoped_read(observer: &Observer<'_>) -> Result<ResolvedRead, &'static str> {
    let start = observer
        .snapshot
        .prompt
        .ok_or("original_prompt_not_found")?;
    let end = observer.snapshot.output_end();
    let mut read = observer.wait.read.clone();
    match &mut read.mode {
        ReadMode::Sequential(seq) => {
            seq.index_min = Some(seq.index_min.unwrap_or(start).max(start));
            seq.index_max = Some(seq.index_max.unwrap_or(end).min(end));
        }
        ReadMode::Continuation { entry_index, .. }
            if *entry_index < start || *entry_index > end =>
        {
            return Err("entry_outside_prompt_scope");
        }
        _ => {}
    }
    Ok(read)
}
