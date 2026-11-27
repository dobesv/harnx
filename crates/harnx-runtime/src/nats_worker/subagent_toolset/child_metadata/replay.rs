//! The pre-composition admission check for a patch-bearing sync prompt.

use super::admission::{load_entries, user_row_present};
use super::recoverable;
use super::SubagentToolset;
use crate::nats_session_metadata::InvocationAdmission;
use harnx_toolset::ToolInvokeError;

/// What a sync `session_prompt` retry learns from durable state before any
/// fresh attachment composition: admitted replays must not re-fetch
/// attachments that may have disappeared.
pub(in crate::nats_worker::subagent_toolset) enum ReplayCheck {
    /// No admission proof: fresh or candidate-only, the gate applies and the
    /// original content must be composed.
    Fresh,
    /// Admitted, and durable content covers delivery (a committed row or
    /// the saved admission content): skip fresh composition.
    AdmittedDurableContent,
    /// Admitted through a busy-join binding whose delivery still needs the
    /// original content; compose it as before.
    AdmittedNeedsCompose,
}

impl ReplayCheck {
    pub(in crate::nats_worker::subagent_toolset) fn admitted(&self) -> bool {
        !matches!(self, Self::Fresh)
    }

    pub(in crate::nats_worker::subagent_toolset) fn skips_fresh_compose(&self) -> bool {
        matches!(self, Self::AdmittedDurableContent)
    }
}

pub(in crate::nats_worker::subagent_toolset) async fn sync_replay_check(
    toolset: &SubagentToolset,
    storage: &str,
    call_id: &str,
) -> Result<ReplayCheck, ToolInvokeError> {
    let store = &toolset.session_metadata;
    if store
        .joined_dispatch(storage, call_id)
        .await
        .map_err(recoverable)?
        .is_some()
    {
        return Ok(ReplayCheck::AdmittedDurableContent);
    }
    match store
        .admission(storage, call_id)
        .await
        .map_err(recoverable)?
    {
        Some(admission) => admitted_replay(toolset, storage, call_id, &admission).await,
        None => joined_replay(toolset, storage, call_id).await,
    }
}

/// An admission record proves replay only with durable delivery or terminal
/// proof; a candidate admission is never proof.
async fn admitted_replay(
    toolset: &SubagentToolset,
    storage: &str,
    call_id: &str,
    admission: &InvocationAdmission,
) -> Result<ReplayCheck, ToolInvokeError> {
    let store = &toolset.session_metadata;
    let entries = load_entries(toolset, storage).await?;
    if user_row_present(&entries, call_id) {
        return Ok(ReplayCheck::AdmittedDurableContent);
    }
    if admission.prompt_content.is_none() {
        return Ok(ReplayCheck::Fresh);
    }
    let owns_head = store
        .admission_head(storage)
        .await
        .map_err(recoverable)?
        .as_deref()
        == Some(call_id);
    let terminal = store
        .admission_is_terminal(storage, call_id, &entries)
        .await
        .map_err(recoverable)?;
    if owns_head || terminal {
        Ok(ReplayCheck::AdmittedDurableContent)
    } else {
        // Candidate-only: never proof.
        Ok(ReplayCheck::Fresh)
    }
}

/// A busy-join binding proves the call joined a running turn. Its committed
/// user row makes delivery durable — without it the original content still
/// has to be composed for the join.
async fn joined_replay(
    toolset: &SubagentToolset,
    storage: &str,
    call_id: &str,
) -> Result<ReplayCheck, ToolInvokeError> {
    let store = &toolset.session_metadata;
    if store
        .prompt_admission_binding(storage, call_id)
        .await
        .map_err(recoverable)?
        .is_none()
    {
        return Ok(ReplayCheck::Fresh);
    }
    let entries = load_entries(toolset, storage).await?;
    if user_row_present(&entries, call_id) {
        Ok(ReplayCheck::AdmittedDurableContent)
    } else {
        Ok(ReplayCheck::AdmittedNeedsCompose)
    }
}
