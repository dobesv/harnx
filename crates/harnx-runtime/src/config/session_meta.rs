use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, PartialEq)]
pub struct SessionMeta {
    pub id: String,
    pub session_id: Option<String>,
    pub agent_name: Option<String>,
    pub title: Option<String>,
    pub modified: Option<SystemTime>,
}

pub(crate) fn session_recency_key(session: &SessionMeta) -> u128 {
    // Prefer the file's modification time — it reflects when the session was
    // last active, not when it was created.
    if let Some(modified_ms) = session
        .modified
        .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis())
    {
        return u128::MAX - modified_ms;
    }

    // Fall back to the creation timestamp embedded in the session ID.
    if let Some(seconds) = crate::utils::session_name::decode_timestamp_session_id(&session.id) {
        return u128::MAX - (seconds as u128 * 1_000);
    }

    if let Ok(uuid) = uuid::Uuid::parse_str(&session.id) {
        if uuid.get_version_num() == 7 {
            if let Some(timestamp) = uuid.get_timestamp() {
                let (seconds, nanos) = timestamp.to_unix();
                let timestamp_ms = (seconds as u128 * 1_000) + (nanos as u128 / 1_000_000);
                return u128::MAX - timestamp_ms;
            }
        }
    }

    u128::MAX
}

/// For CLI auto-session: return the most recent session for the active agent.
/// Workspace, repository, branch, and terminal state are intentionally not
/// persisted or used for ranking.
pub fn find_matching_session(sessions: &[SessionMeta], agent_name: &str) -> Option<String> {
    let mut candidates: Vec<&SessionMeta> = sessions
        .iter()
        .filter(|s| s.agent_name.as_deref() == Some(agent_name))
        .collect();
    candidates.sort_by_key(|s| session_recency_key(s));
    candidates.first().map(|s| s.id.clone())
}

pub fn sort_sessions_for_picker(mut sessions: Vec<SessionMeta>) -> Vec<SessionMeta> {
    sessions.sort_by_key(session_recency_key);
    sessions
}

#[cfg(test)]
mod tests {
    use super::{find_matching_session, sort_sessions_for_picker, SessionMeta};
    use std::time::{Duration, UNIX_EPOCH};

    fn session_meta(name: &str) -> SessionMeta {
        SessionMeta {
            id: name.to_string(),
            session_id: None,
            agent_name: None,
            title: None,
            modified: None,
        }
    }

    #[test]
    fn test_sort_recency_fallback() {
        let older = session_meta("018f0d1c-5b2a-7000-8000-000000000000");
        let newer = session_meta("018f0d1c-5b2b-7000-8000-000000000000");
        let sorted = sort_sessions_for_picker(vec![older, newer.clone()]);
        assert_eq!(sorted[0].id, newer.id);
    }

    #[test]
    fn test_sort_modified_beats_id_timestamp() {
        // Session with a NEWER UUIDv7 ID (created later) but OLDER mtime loses
        // to a session with an OLDER UUIDv7 ID but NEWER mtime.
        // This verifies that mtime takes precedence over creation time.
        let mut newer_id_older_mtime = session_meta("018f0d1c-5b2b-7000-8000-000000000000");
        newer_id_older_mtime.modified = Some(UNIX_EPOCH + Duration::from_secs(10));

        let mut older_id_newer_mtime = session_meta("018f0d1c-5b2a-7000-8000-000000000000");
        older_id_newer_mtime.modified = Some(UNIX_EPOCH + Duration::from_secs(20));

        let sorted =
            sort_sessions_for_picker(vec![newer_id_older_mtime, older_id_newer_mtime.clone()]);
        assert_eq!(
            sorted[0].id, older_id_newer_mtime.id,
            "session with newer mtime should sort first, even if its ID was created earlier"
        );
    }

    #[test]
    fn test_find_matching_session_filters_agent_and_picks_most_recent() {
        let mut older = session_meta("018f0d1c-5b2a-7000-8000-000000000000");
        older.agent_name = Some("smith".to_string());
        older.modified = Some(UNIX_EPOCH + Duration::from_secs(1));

        let mut newer = older.clone();
        newer.id = "018f0d1c-5b2b-7000-8000-000000000000".to_string();
        newer.modified = Some(UNIX_EPOCH + Duration::from_secs(2));

        let mut other_agent = newer.clone();
        other_agent.id = "newest-other-agent".to_string();
        other_agent.agent_name = Some("neo".to_string());
        other_agent.modified = Some(UNIX_EPOCH + Duration::from_secs(3));

        let found = find_matching_session(&[older, newer.clone(), other_agent], "smith");
        assert_eq!(found.as_deref(), Some(newer.id.as_str()));
    }
}
