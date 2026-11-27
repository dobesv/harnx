//! The session_load refit ladder (plan #2970, T3).
//!
//! The wrapper's mandatory routing/status identity, the pending-interaction
//! identity and the page are fitted against `read.max_output_bytes` together:
//! attempt 1 keeps full bounded metadata and full optional progress; each
//! later attempt removes optional progress, then reduces metadata to its
//! `{"truncated": true}` marker, then omits it — always refitting the SAME
//! loaded window, consuming no cursor. An attempt that stalls (delivers no
//! advancing content in either cursor mode) or overflows is discarded; only
//! a mandatory-only page that still cannot deliver usable content returns the
//! bounded recoverable ask for a larger `read.max_output_bytes`.

use super::super::merge_page_into_wrapper;
use super::read::LoadMetadata;
use crate::nats_session_log::NatsSessionLog;
use crate::nats_worker::subagent_toolset::async_session::observation;
use crate::nats_worker::subagent_toolset::async_session::SessionPromptStatus;
use crate::nats_worker::subagent_toolset::session_read::{self, LoadedWindow, ResolvedRead};
use harnx_core::pending_interaction::PendingInteractionSummary;
use harnx_toolset::ToolInvokeError;
use serde_json::{json, Value};

/// Mandatory identity one prompt-scoped load carries, beyond the page.
pub(in crate::nats_worker::subagent_toolset) struct LoadScope {
    pub session_id: String,
    pub prompt_id: Option<String>,
    pub status: Option<SessionPromptStatus>,
    pub pending: Option<PendingInteractionSummary>,
}

/// Load the durable window once, then fit it under the refit ladder.
pub(in crate::nats_worker::subagent_toolset) async fn compose_load_page(
    log: &NatsSessionLog,
    read: &ResolvedRead,
    scope: &LoadScope,
    metadata: &LoadMetadata,
) -> Result<Value, ToolInvokeError> {
    let window = session_read::load_read_window(log, read)
        .await
        .map_err(ToolInvokeError::Recoverable)?;
    fit_loaded_window(&window, read, scope, metadata)
}

/// The ladder over an already-loaded window. Pure: refits never re-read the
/// log, so no cursor is consumed and concurrent appends cannot make two
/// attempts disagree about the window.
pub(in crate::nats_worker::subagent_toolset) fn fit_loaded_window(
    window: &LoadedWindow,
    read: &ResolvedRead,
    scope: &LoadScope,
    metadata: &LoadMetadata,
) -> Result<Value, ToolInvokeError> {
    // Request errors (a missing entry, a mid-character byte offset) must
    // surface as errors; the ladder below only shrinks optional data and
    // would otherwise bury them behind its budget message.
    session_read::validate_read_window(window, read).map_err(ToolInvokeError::Recoverable)?;
    for attempt in Attempt::ladder() {
        if let Some(result) = attempt.fit(window, read, scope, metadata) {
            return Ok(result);
        }
    }
    Err(ToolInvokeError::Recoverable(format!(
        "session_load result does not fit read.max_output_bytes {}: mandatory \
         session, prompt and pending-interaction identity leaves no room for \
         usable page content; retry with a larger read.max_output_bytes",
        read.max_output_bytes
    )))
}

#[derive(Clone, Copy)]
enum Attempt {
    /// Full bounded metadata and full optional progress.
    Full,
    /// Full metadata, optional progress reduced to the pending identity.
    IdentityProgress,
    /// Metadata reduced to its truncation marker, pending identity only.
    MarkerMetadata,
    /// Metadata omitted, pending identity only.
    NoMetadata,
}

impl Attempt {
    fn ladder() -> [Attempt; 4] {
        [
            Attempt::Full,
            Attempt::IdentityProgress,
            Attempt::MarkerMetadata,
            Attempt::NoMetadata,
        ]
    }

    fn fit(
        self,
        window: &LoadedWindow,
        read: &ResolvedRead,
        scope: &LoadScope,
        metadata: &LoadMetadata,
    ) -> Option<Value> {
        let wrapper = load_wrapper(scope, metadata_field(self, metadata));
        let reserve = serialized(&wrapper)
            .saturating_sub(1)
            .saturating_add(progress_reserve(self, scope));
        let outcome = session_read::fit_read_window(window, read, reserve).ok()?;
        let mut result = merge_page_into_wrapper(wrapper, outcome.value).ok()?;
        attach_progress(self, &mut result, scope, read.max_output_bytes);
        if outcome.stalled || serialized(&result) > read.max_output_bytes {
            return None;
        }
        Some(result)
    }
}

/// The `metadata` value an attempt carries. An absent record stays null in
/// every attempt; a projection that cannot fit degrades to the marker and
/// finally to omission, but never to `null`, which is reserved for a
/// genuinely absent record.
fn metadata_field(attempt: Attempt, metadata: &LoadMetadata) -> Option<Value> {
    match (attempt, metadata) {
        (_, LoadMetadata::Absent) => Some(Value::Null),
        (Attempt::NoMetadata, _) => None,
        (Attempt::MarkerMetadata, _) => Some(json!({"truncated": true})),
        (_, LoadMetadata::Omitted) => None,
        (_, LoadMetadata::Bounded(view)) => Some(view.clone()),
    }
}

fn load_wrapper(scope: &LoadScope, metadata: Option<Value>) -> Value {
    let mut wrapper = match (&scope.prompt_id, scope.status) {
        (Some(prompt_id), Some(status)) => json!({
            "session_id": scope.session_id,
            "prompt_id": prompt_id,
            "status": status,
            "terminal": status.is_terminal(),
        }),
        _ => json!({ "session_id": scope.session_id }),
    };
    if let Some(metadata) = metadata {
        wrapper["metadata"] = metadata;
    }
    wrapper
}

fn progress_reserve(attempt: Attempt, scope: &LoadScope) -> usize {
    let Some(pending) = scope.pending.as_ref() else {
        return 0;
    };
    match attempt {
        Attempt::Full => {
            let fields = observation::pending_progress_fields(
                pending,
                scope.prompt_id.as_deref().unwrap_or_default(),
                None,
            );
            observation::progress_fields_reserve(&fields)
        }
        _ => observation::pending_identity_reserve(pending),
    }
}

fn attach_progress(attempt: Attempt, result: &mut Value, scope: &LoadScope, max_bytes: usize) {
    let Some(pending) = scope.pending.clone() else {
        return;
    };
    match attempt {
        Attempt::Full => observation::attach_pending_progress(result, pending, None, max_bytes),
        _ => observation::attach_pending_identity(result, pending, max_bytes),
    }
}

fn serialized(value: &Value) -> usize {
    serde_json::to_vec(value)
        .map(|bytes| bytes.len())
        .unwrap_or(usize::MAX)
}

#[cfg(test)]
#[path = "load_page_tests.rs"]
mod load_page_tests;
