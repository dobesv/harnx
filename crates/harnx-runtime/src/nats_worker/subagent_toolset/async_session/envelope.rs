//! The send/wait result envelope and the byte budget it must fit.

use super::{Activity, DispatchConfirmation, PromptHandle, SessionPromptStatus, WaitStop};
use serde::Serialize;

/// Serialized budget of one send/wait result. The final reply is cut to fit;
/// identifiers and lifecycle fields always fit.
pub(crate) const RESULT_MAX_BYTES: usize =
    super::super::session_read::DEFAULT_MAX_OUTPUT_BYTES as usize;

/// Shared send/wait result envelope, sized to fit [`RESULT_MAX_BYTES`]. The
/// envelope's own metadata is reserved first so a dispatched handle can never
/// be lost to the reply. A settled prompt carries its final reply (and the
/// failure that ended a failed turn); a running one carries the activity
/// sample. Neither carries the child's transcript: the parent asked for an
/// answer, and `session_load` reads the turn when it wants more.
#[derive(Debug, Serialize)]
pub(crate) struct SessionPromptResult {
    pub(crate) session_id: String,
    pub(crate) prompt_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) invocation_id: Option<String>,
    pub(crate) dispatch: Option<DispatchConfirmation>,
    pub(crate) status: SessionPromptStatus,
    pub(crate) terminal: bool,
    pub(crate) wait_expired: bool,
    /// What ended the wait early, when it was not the wait's own limit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) stopped_by: Option<WaitStop>,
    pub(crate) elapsed_ms: u64,
    /// The child's last assistant reply in this prompt's turn, thinking
    /// stripped, as the synchronous `session_prompt` returns it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) response: Option<String>,
    /// The reply was cut to fit the result budget.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub(crate) response_truncated: bool,
    /// The worker's error for a `failed` turn.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) failure: Option<String>,
    /// What a pending or blocked child is doing, when `include_partial`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) activity: Option<Activity>,
    pub(crate) observation: Observation,
}

/// How this observation went, apart from the child's own state.
#[derive(Debug, Clone, Default, Serialize)]
pub(crate) struct Observation {
    pub(crate) observed_at_ms: u64,
    pub(crate) running_ms: Option<u64>,
    pub(crate) complete: bool,
    pub(crate) error: Option<&'static str>,
}

impl Observation {
    /// The widest observation, which result metadata reserves room for.
    pub(crate) fn widest() -> Self {
        Self {
            observed_at_ms: u64::MAX,
            running_ms: Some(u64::MAX),
            complete: false,
            error: Some("durable_observation_unavailable"),
        }
    }
}

/// Bytes of a result envelope as the tool returns it.
pub(crate) fn serialized_len(result: &SessionPromptResult) -> usize {
    serde_json::to_vec(result)
        .expect("result envelope serializes")
        .len()
}

impl SessionPromptResult {
    /// An envelope with no reply, activity or observation details yet.
    pub(crate) fn empty(handle: &PromptHandle) -> Self {
        Self {
            session_id: handle.session_id.clone(),
            prompt_id: handle.prompt_id.clone(),
            invocation_id: None,
            dispatch: None,
            status: SessionPromptStatus::Unknown,
            terminal: false,
            wait_expired: false,
            stopped_by: None,
            elapsed_ms: 0,
            response: None,
            response_truncated: false,
            failure: None,
            activity: None,
            observation: Observation::default(),
        }
    }
}

/// Serialized size of the result envelope without a reply or activity sample —
/// the bytes that must be reserved before fitting either.
pub(super) fn result_metadata_bytes(session_id: &str, prompt_id: &str) -> usize {
    let envelope = SessionPromptResult {
        stopped_by: Some(WaitStop::WIDEST),
        elapsed_ms: u64::MAX,
        observation: Observation::widest(),
        ..SessionPromptResult::empty(&PromptHandle {
            session_id: session_id.to_string(),
            prompt_id: prompt_id.to_string(),
        })
    };
    serialized_len(&envelope)
}

/// Reject identifiers that cannot fit their own handle metadata before any
/// dispatch side effect, so a dispatched handle is never lost to the output
/// budget.
pub(crate) fn ensure_result_metadata_fits(
    session_id: &str,
    prompt_id: &str,
    budget: usize,
) -> Result<(), String> {
    let reserved = result_metadata_bytes(session_id, prompt_id);
    if reserved > budget {
        Err(format!(
            "result metadata ({reserved} bytes for session_id and prompt_id) exceeds \
             the {budget} byte result budget; the handle could not be returned"
        ))
    } else {
        Ok(())
    }
}
