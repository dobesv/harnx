//! History projection, task filtering and context-scoped pagination.
use a2a_lf::{A2AError, ListTasksRequest, ListTasksResponse, Task};

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

pub(super) fn task_matches(task: &Task, req: &ListTasksRequest) -> bool {
    if req
        .status
        .as_ref()
        .is_some_and(|state| *state != task.status.state)
    {
        return false;
    }
    !req.status_timestamp_after.is_some_and(|timestamp| {
        task.status
            .timestamp
            .is_none_or(|status| status <= timestamp)
    })
}

pub(super) fn paginate(
    tasks: Vec<Task>,
    req: &ListTasksRequest,
) -> Result<ListTasksResponse, A2AError> {
    // Validate the cursor only within the authorized, filtered context.
    let total_size = tasks.len() as i32;
    let offset = match req.page_token.as_deref().filter(|token| !token.is_empty()) {
        Some(token) => tasks
            .iter()
            .position(|task| task.id == token)
            .map(|index| index + 1)
            .ok_or_else(|| A2AError::invalid_params("invalid pageToken"))?,
        None => 0,
    };
    let page_size = a2a_server_lf::pagination::resolve_page_size(req.page_size);
    let next_page_token = if offset + page_size < tasks.len() {
        tasks[offset + page_size - 1].id.clone()
    } else {
        String::new()
    };
    Ok(ListTasksResponse {
        tasks: tasks.into_iter().skip(offset).take(page_size).collect(),
        next_page_token,
        page_size: page_size as i32,
        total_size,
    })
}

pub(super) fn project_task(task: &mut Task, req: &ListTasksRequest) -> Result<(), A2AError> {
    history(task, req.history_length)?;
    if req.include_artifacts == Some(false) {
        task.artifacts = None;
    }
    Ok(())
}
