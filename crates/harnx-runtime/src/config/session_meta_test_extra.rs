#[cfg(test)]
mod tests {
    use super::super::*;
    use anyhow::{anyhow, Result};
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

    #[tokio::test]
    async fn test_read_header_large_but_within_64kb() -> Result<()> {
        let tmp = TempDir::new()?;
        let path = tmp.path().join("large.yaml");

        let header_prefix = "type: header
model: gpt-4o
agent_name: large-agent
agent_instructions: ";
        let header_suffix = "
---
next doc
";
        let repeat_count = 63 * 1024 - header_prefix.len() - header_suffix.len();
        let long_instructions: String = "x".repeat(repeat_count);
        let content = format!("{header_prefix}{long_instructions}{header_suffix}");

        tokio::fs::write(&path, &content).await?;

        let meta = parse_session_meta("large", &path)
            .ok_or_else(|| anyhow!("parse_session_meta returned None"))?;
        assert_eq!(meta.agent_name.as_deref(), Some("large-agent"));
        Ok(())
    }
}
