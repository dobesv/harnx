#[cfg(test)]
mod tests {
    use super::super::session_meta::*;
    use std::time::{Duration, UNIX_EPOCH};
    use tempfile::TempDir;
    use std::fs;

    fn session_meta(name: &str) -> SessionMeta {
        SessionMeta {
            name: name.to_string(),
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
        assert_eq!(sorted[0].name, "new-session");
    }

    #[test]
    fn test_read_header_64kb_limit() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("large.yaml");
        
        let mut content = String::from("type: header\nagent_name: large-agent\ninstruction: |");
        for _ in 0..65000 {
            content.push('x');
        }
        content.push_str("\n---\nnext doc\n");
        
        fs::write(&path, &content).unwrap();
        
        // Should successfully parse despite being near 64KB
        let meta = parse_session_meta("large", &path).unwrap();
        assert_eq!(meta.agent_name.as_deref(), Some("large-agent"));
    }

    #[test]
    fn test_read_header_exceeds_64kb_limit() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("too_large.yaml");
        
        let mut content = String::from("type: header\nagent_name: too-large\ninstruction: |");
        for _ in 0..70000 {
            content.push('x');
        }
        content.push_str("\n---\nnext doc\n");
        
        fs::write(&path, &content).unwrap();
        
        // parse_session_meta should still work but it will only read first 64KB.
        // If the document boundary is after 64KB, it might fail to find the boundary or fail to parse YAML.
        let meta = parse_session_meta("too_large", &path);
        // In the current implementation, it reads 64KB and if no boundary found, it uses the whole buffer.
        // If 64KB is in the middle of a YAML block, serde_yaml will fail.
        assert!(meta.is_none(), "Should fail to parse if header document boundary is beyond 64KB");
    }
}
