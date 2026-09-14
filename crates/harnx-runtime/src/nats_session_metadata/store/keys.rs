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

pub fn invalidation_subject(session_id: &str) -> String {
    format!("harnx.session.{session_id}.metadata.invalidated")
}

pub fn read_invalidation_subject(storage_key: &str) -> String {
    format!("harnx.session.{storage_key}.read.invalidated")
}
