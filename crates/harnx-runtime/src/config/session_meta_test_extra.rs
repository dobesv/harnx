#[cfg(test)]
mod tests {
    use super::super::*;
    use std::time::{Duration, UNIX_EPOCH};

    fn session_meta(name: &str) -> SessionMeta {
        SessionMeta {
            id: name.to_string(),
            session_id: None,
            working_dir: None,
            git_branch: None,
            git_remote: None,
            terminal_session_id: None,
            agent_name: None,
            title: None,
            modified: None,
        }
    }

    #[test]
    fn test_sort_modified_fallback() {
        let mut older = session_meta("old-session");
        older.modified = Some(UNIX_EPOCH + Duration::from_secs(10));

        let mut newer = session_meta("new-session");
        newer.modified = Some(UNIX_EPOCH + Duration::from_secs(20));

        let context = PickerContext {
            current_terminal_id: None,
            current_branch: None,
            current_dir: "/nowhere".to_string(),
            current_remote: None,
        };

        let sorted = sort_sessions_for_picker(vec![older.clone(), newer.clone()], &context);
        assert_eq!(sorted[0].id, "new-session");
    }
}
