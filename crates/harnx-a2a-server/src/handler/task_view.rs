//! History projection, task filtering and context-scoped pagination.
use crate::store::to_index_state;
use a2a_lf::{A2AError, ListTasksRequest, Task};
use harnx_runtime::nats_session_metadata::TaskIndexEntry;

pub(super) fn validate_history(length: Option<i32>) -> Result<(), A2AError> {
    if length.is_some_and(|length| length < 0) {
        return Err(A2AError::invalid_params(
            "historyLength must be nonnegative",
        ));
    }
    Ok(())
}

pub(super) fn history(task: &mut Task, length: Option<i32>) -> Result<(), A2AError> {
    validate_history(length)?;
    if let Some(length) = length {
        if let Some(messages) = &mut task.history {
            let keep = length as usize;
            messages.drain(..messages.len().saturating_sub(keep));
        }
    }
    Ok(())
}

pub(super) fn project_task(task: &mut Task, req: &ListTasksRequest) -> Result<(), A2AError> {
    history(task, req.history_length)?;
    if req.include_artifacts == Some(false) {
        task.artifacts = None;
    }
    Ok(())
}

/// Check if an index entry matches the ListTasksRequest filter criteria.
pub fn entry_matches(entry: &TaskIndexEntry, req: &ListTasksRequest) -> bool {
    if let Some(status) = &req.status {
        if to_index_state(status.clone()) != entry.state {
            return false;
        }
    }
    !req.status_timestamp_after.is_some_and(|timestamp| {
        entry
            .status_timestamp
            .is_none_or(|status| status <= timestamp)
    })
}

/// Resolve pagination offset from pageToken.
pub fn resolve_offset(
    entries: &[TaskIndexEntry],
    page_token: Option<&str>,
) -> Result<usize, A2AError> {
    match page_token.filter(|token| !token.is_empty()) {
        Some(token) => entries
            .iter()
            .position(|entry| entry.task_id == token)
            .map(|index| index + 1)
            .ok_or_else(|| A2AError::invalid_params("invalid pageToken")),
        None => Ok(0),
    }
}
