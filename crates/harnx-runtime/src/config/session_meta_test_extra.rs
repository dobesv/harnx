#[cfg(test)]
mod tests {
    use super::super::*;
    use std::fs;
    use std::time::{Duration, UNIX_EPOCH};
    use tempfile::TempDir;

    fn session_meta(name: &str) -> SessionMeta {
        SessionMeta {
            id: name.to_string(),
            session_id: None,
            working_dir: None,
            git_branch: None,
            git_remote: None,
            terminal_session_id: None,
            agent_name: None,
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

    #[test]
    fn test_read_header_large_but_within_64kb() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("large.yaml");

        // Build a valid header with a long agent_instructions field (but total < 64KB).
        let long_instructions: String = "x".repeat(5000);
        let content = format!(
            "type: header\nmodel: gpt-4o\nagent_name: large-agent\nagent_instructions: {}\n---\nnext doc\n",
            long_instructions
        );

        fs::write(&path, &content).unwrap();

        // Should successfully parse despite having a long field
        let meta = parse_session_meta("large", &path).unwrap();
        assert_eq!(meta.agent_name.as_deref(), Some("large-agent"));
    }
}
