pub fn session_prefix(session_id: &str) -> String {
    format!("sessions/{session_id}")
}

pub fn metadata_key(session_id: &str) -> String {
    format!("{}/meta", session_prefix(session_id))
}

pub fn activity_key(session_id: &str) -> String {
    format!("{}/activity", session_prefix(session_id))
}

pub fn read_cursor_key(storage_key: &str, viewer: &str) -> String {
    format!("{}/read/{viewer}", session_prefix(storage_key))
}

/// Key for run limits record under a session.
/// Format: sessions/<storage_key>/runs/<run_id>
pub fn run_limits_key(storage_key: &str, run_id: &str) -> String {
    format!("{}/runs/{}", session_prefix(storage_key), run_id)
}

/// Key for invocation limits record under a session.
/// Format: sessions/<storage_key>/invocations/<invocation_id>
pub fn invocation_limits_key(storage_key: &str, invocation_id: &str) -> String {
    format!(
        "{}/invocations/{}",
        session_prefix(storage_key),
        invocation_id
    )
}

pub fn invalidation_subject(session_id: &str) -> String {
    format!("harnx.session.{session_id}.metadata.invalidated")
}

pub fn read_invalidation_subject(storage_key: &str) -> String {
    format!("harnx.session.{storage_key}.read.invalidated")
}
