//! Canonical task identifiers and context ID validation.
use anyhow::{ensure, Context, Result};

/// Parse a task ID into its local session ID and UUID components.
/// Format: `{local_id}.{uuid}`, with a lowercase hyphenated UUID.
pub fn parse_task_id(task_id: &str) -> Result<(&str, &str)> {
    let dot_pos = task_id
        .find('.')
        .ok_or_else(|| anyhow::anyhow!("task ID must contain '.'"))?;
    let local_id = &task_id[..dot_pos];
    let uuid = &task_id[dot_pos + 1..];
    ensure!(!local_id.is_empty(), "local ID must not be empty");
    let parsed = uuid::Uuid::parse_str(uuid).context("invalid task UUID")?;
    // UUID text is also the KV suffix; aliases must not select different keys.
    ensure!(
        parsed.hyphenated().to_string() == uuid,
        "task UUID must be lowercase and hyphenated"
    );
    ensure!(
        local_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
        "invalid local session ID"
    );
    Ok((local_id, uuid))
}

/// Format a task ID from local session ID and UUID.
pub fn format_task_id(local_id: &str, uuid: &str) -> String {
    format!("{}.{}", local_id, uuid)
}

/// Assert local_id has no `.` (valid per base64url alphabet).
pub fn assert_local_id_no_dot(local_id: &str) -> Result<()> {
    ensure!(
        !local_id.contains('.'),
        "local session ID must not contain '.' (got '{}')",
        local_id
    );
    Ok(())
}

/// Create a new task ID with a fresh UUID.
pub fn new_task_id(local_id: &str) -> String {
    assert_local_id_no_dot(local_id).expect("local_id validated at session creation");
    let uuid = uuid::Uuid::new_v4();
    format_task_id(local_id, &uuid.hyphenated().to_string())
}
