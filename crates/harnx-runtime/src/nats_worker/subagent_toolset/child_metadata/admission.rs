//! Durable admission proof and the pinned patch application.

use super::{recoverable, storage_key, SubagentToolset};
use crate::nats_session_metadata::{ChildPatchOutcome, ChildSessionMetadataPatch};
use harnx_core::session::SessionLogEntry;
use harnx_toolset::ToolInvokeError;

/// Durable admission proof for this invocation: an accepted busy-join
/// record or binding, an accepted admission head, or terminal proof — the
/// `replay_saved_dispatch` distinction. A saved candidate admission alone
/// is NOT proof.
pub(super) async fn admitted_before(
    toolset: &SubagentToolset,
    storage: &str,
    call_id: &str,
) -> Result<bool, ToolInvokeError> {
    let store = &toolset.session_metadata;
    if store
        .joined_dispatch(storage, call_id)
        .await
        .map_err(recoverable)?
        .is_some()
    {
        return Ok(true);
    }
    if store
        .prompt_admission_binding(storage, call_id)
        .await
        .map_err(recoverable)?
        .is_some()
    {
        return Ok(true);
    }
    let Some(admission) = store
        .admission(storage, call_id)
        .await
        .map_err(recoverable)?
    else {
        return Ok(false);
    };
    if admission.prompt_content.is_none() {
        return Ok(false);
    }
    if store
        .admission_head(storage)
        .await
        .map_err(recoverable)?
        .as_deref()
        == Some(call_id)
    {
        return Ok(true);
    }
    let entries = load_entries(toolset, storage).await?;
    store
        .admission_is_terminal(storage, call_id, &entries)
        .await
        .map_err(recoverable)
}

pub(super) async fn load_entries(
    toolset: &SubagentToolset,
    storage: &str,
) -> Result<Vec<(u64, SessionLogEntry)>, ToolInvokeError> {
    crate::nats_session_log::NatsSessionLog::new(toolset.jetstream.clone(), storage)
        .load_events_async()
        .await
        .map_err(recoverable)
}

/// Whether a user row with this prompt id is already committed.
pub(super) fn user_row_present(entries: &[(u64, SessionLogEntry)], id: &str) -> bool {
    entries.iter().any(|(_, entry)| {
        matches!(
            entry,
            SessionLogEntry::Message {
                id: Some(found),
                role,
                ..
            } if found == id && role.is_user()
        )
    })
}

/// Apply the patch at the pinned revision. `AlreadyHeld` proceeds (a retry
/// must not write over what the record gained since); a conflict rechecks
/// durable admission — proven admission means the original call already
/// applied its patch and a later child edit must survive, anything else is a
/// recoverable conflict with no append.
pub(super) async fn apply_pinned(
    toolset: &SubagentToolset,
    storage: &str,
    call_id: &str,
    patch: &ChildSessionMetadataPatch,
    revision: u64,
) -> Result<(), ToolInvokeError> {
    match toolset
        .session_metadata
        .apply_child_patch(storage, patch, Some(revision))
        .await
    {
        Ok(ChildPatchOutcome::Applied(_)) | Ok(ChildPatchOutcome::AlreadyHeld(_)) => Ok(()),
        Ok(ChildPatchOutcome::Conflict { .. }) => {
            if admitted_before(toolset, storage, call_id).await? {
                Ok(())
            } else {
                Err(ToolInvokeError::Recoverable(format!(
                    "child session metadata conflict: the session moved past the pinned \
                     revision {revision} without carrying the requested update; re-check \
                     the session and issue a fresh call"
                )))
            }
        }
        Err(error) => Err(recoverable(error)),
    }
}

/// Whether a patch-bearing `session_send` must compose its original content
/// before the metadata gate runs. Admitted replays deliver durable content,
/// so their (possibly vanished) attachments are never fetched again; every
/// fresh or candidate-only call, including a generated create, validates its
/// attachments before the gate can mutate or create anything.
pub(in crate::nats_worker::subagent_toolset) async fn fresh_send_composes_content(
    toolset: &SubagentToolset,
    target: Option<&str>,
    call_id: &str,
) -> Result<bool, ToolInvokeError> {
    let Some(session_id) = target else {
        return Ok(true);
    };
    let storage = storage_key(toolset, session_id);
    Ok(!admitted_before(toolset, &storage, call_id).await?)
}
