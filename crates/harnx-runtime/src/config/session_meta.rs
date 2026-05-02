use harnx_core::session::SessionLogEntry;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};
use std::{fs::File, io::Read};

#[derive(Debug, Clone, PartialEq)]
pub struct SessionMeta {
    pub name: String,
    pub session_id: Option<String>,
    pub working_dir: Option<String>,
    pub git_branch: Option<String>,
    pub git_remote: Option<String>,
    pub terminal_session_id: Option<String>,
    pub agent_name: Option<String>,
    pub modified: Option<SystemTime>,
}

#[derive(Debug, Clone)]
pub struct PickerContext {
    pub current_terminal_id: Option<String>,
    pub current_branch: Option<String>,
    pub current_dir: String,
    pub current_remote: Option<String>,
}

pub fn build_picker_context() -> PickerContext {
    let current_branch = crate::utils::session_name::git_branch();

    PickerContext {
        current_terminal_id: crate::utils::terminal_session_id(),
        current_branch: (!current_branch.is_empty()).then_some(current_branch),
        current_dir: std::env::current_dir()
            .ok()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default(),
        current_remote: crate::utils::session_name::git_remote(),
    }
}

fn session_recency_key(session: &SessionMeta) -> u128 {
    if let Ok(uuid) = uuid::Uuid::parse_str(&session.name) {
        if uuid.get_version_num() == 7 {
            if let Some(timestamp) = uuid.get_timestamp() {
                let (seconds, nanos) = timestamp.to_unix();
                let timestamp_ms = (seconds as u128 * 1_000) + (nanos as u128 / 1_000_000);
                return u128::MAX - timestamp_ms;
            }
        }
    }

    if let Some(modified_ms) = session
        .modified
        .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis())
    {
        return u128::MAX - modified_ms;
    }

    u128::MAX
}

pub fn sort_sessions_for_picker(
    mut sessions: Vec<SessionMeta>,
    context: &PickerContext,
) -> Vec<SessionMeta> {
    sessions.sort_by_key(|session| {
        let terminal_match_score = if session.terminal_session_id.is_some()
            && context.current_terminal_id.is_some()
            && session.terminal_session_id == context.current_terminal_id
        {
            0
        } else {
            1
        };

        let branch_match_score = if session
            .git_branch
            .as_deref()
            .is_some_and(|branch| !branch.is_empty())
            && context
                .current_branch
                .as_deref()
                .is_some_and(|branch| !branch.is_empty())
            && session.git_branch == context.current_branch
        {
            0
        } else {
            1
        };

        let dir_match_score = if session.working_dir.as_deref() == Some(context.current_dir.as_str()) {
            0
        } else {
            1
        };

        let remote_match_score = if session.git_remote.is_some()
            && context.current_remote.is_some()
            && session.git_remote == context.current_remote
        {
            0
        } else {
            1
        };

        (
            terminal_match_score,
            branch_match_score,
            dir_match_score,
            remote_match_score,
            session_recency_key(session),
        )
    });
    sessions
}

// Maximum bytes to read when looking for the YAML document boundary.
// 64KB is generous enough to accommodate headers with long agent instructions.
const MAX_HEADER_BYTES: usize = 65536;

fn read_session_header_bytes(path: &Path) -> Option<String> {
    let mut file = File::open(path).ok()?;
    let mut buffer = vec![0_u8; MAX_HEADER_BYTES];
    let read_len = file.read(&mut buffer).ok()?;
    let bytes = &buffer[..read_len];

    // Skip an optional leading `---\n` or `---\r\n` document-start marker.
    // Standard YAML multi-doc files often omit it, but hand-edited files may include it.
    let content_start = if bytes.starts_with(b"---\n") {
        4
    } else if bytes.starts_with(b"---\r\n") {
        5
    } else {
        0
    };
    let content = &bytes[content_start..];

    // Find the next document separator — this marks the end of the header document.
    let boundary = if content.starts_with(b"---\n") || content.starts_with(b"---\r\n") {
        0
    } else {
        content
            .windows(5)
            .position(|w| w == b"\n---\n")
            .map(|pos| pos + 1)
            .or_else(|| {
                content.windows(6).position(|w| w == b"\n---\r\n").map(|pos| pos + 1)
            })
            .unwrap_or(content.len())
    };


    String::from_utf8(content[..boundary].to_vec()).ok()
}

pub fn parse_session_meta(name: &str, path: &Path) -> Option<SessionMeta> {
    let header_str = read_session_header_bytes(path)?;
    let modified = std::fs::metadata(path).ok()?.modified().ok();

    match serde_yaml::from_str::<SessionLogEntry>(&header_str).ok()? {
        SessionLogEntry::Header {
            session_id,
            working_dir,
            git_branch,
            git_remote,
            terminal_session_id,
            agent_name,
            ..
        } => Some(SessionMeta {
            name: name.to_string(),
            session_id,
            working_dir,
            git_branch,
            git_remote,
            terminal_session_id,
            agent_name,
            modified,
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        build_picker_context, parse_session_meta, sort_sessions_for_picker, PickerContext,
        SessionMeta,
    };
    use std::fs;
    use std::time::{Duration, UNIX_EPOCH};
    use tempfile::TempDir;

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
    fn parse_session_meta_reads_single_header_doc() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("session.yaml");
        fs::write(
            &path,
            "type: header\nmodel: test-model\nsession_id: sess-123\nworking_dir: /tmp/work\ngit_branch: main\ngit_remote: origin\nterminal_session_id: term-1\nagent_name: smith\n",
        )
        .unwrap();

        let meta = parse_session_meta("session", &path).unwrap();
        assert_eq!(meta.name, "session");
        assert_eq!(meta.session_id.as_deref(), Some("sess-123"));
        assert_eq!(meta.working_dir.as_deref(), Some("/tmp/work"));
        assert_eq!(meta.git_branch.as_deref(), Some("main"));
        assert_eq!(meta.git_remote.as_deref(), Some("origin"));
        assert_eq!(meta.terminal_session_id.as_deref(), Some("term-1"));
        assert_eq!(meta.agent_name.as_deref(), Some("smith"));
        assert!(meta.modified.is_some());
    }

    #[test]
    fn test_session_meta_round_trip() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("test-session.yaml");
        fs::write(
            &path,
            "type: header\nmodel: gpt-4o\nsession_id: \"01234567-89ab-cdef-0123-456789abcdef\"\nworking_dir: \"/tmp/test\"\ngit_branch: \"main\"\ngit_remote: \"https://github.com/test/repo\"\nterminal_session_id: \"TERM_SESSION_ID:abc123\"\n",
        )
        .unwrap();

        let meta = parse_session_meta("test-session", &path).unwrap();
        assert_eq!(meta.name, "test-session");
        assert_eq!(
            meta.session_id.as_deref(),
            Some("01234567-89ab-cdef-0123-456789abcdef")
        );
        assert_eq!(meta.working_dir.as_deref(), Some("/tmp/test"));
        assert_eq!(meta.git_branch.as_deref(), Some("main"));
        assert_eq!(
            meta.git_remote.as_deref(),
            Some("https://github.com/test/repo")
        );
        assert_eq!(
            meta.terminal_session_id.as_deref(),
            Some("TERM_SESSION_ID:abc123")
        );
    }

    #[test]
    fn parse_session_meta_stops_at_next_yaml_doc() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("multi.yaml");
        fs::write(
            &path,
            "type: header\nmodel: test-model\nsession_id: sess-456\nworking_dir: /repo\n---\ntype: message\nrole: user\ncontent: hello\n",
        )
        .unwrap();

        let meta = parse_session_meta("multi", &path).unwrap();
        assert_eq!(meta.name, "multi");
        assert_eq!(meta.session_id.as_deref(), Some("sess-456"));
        assert_eq!(meta.working_dir.as_deref(), Some("/repo"));
        assert_eq!(meta.git_branch, None);
        assert_eq!(meta.git_remote, None);
        assert_eq!(meta.terminal_session_id, None);
        assert_eq!(meta.agent_name, None);
        assert!(meta.modified.is_some());
    }

    #[test]
    fn parse_session_meta_returns_none_for_malformed_or_empty_file() {
        let tmp = TempDir::new().unwrap();

        let malformed = tmp.path().join("bad.yaml");
        fs::write(&malformed, "type: message\nrole: user\ncontent: nope\n").unwrap();
        assert!(parse_session_meta("bad", &malformed).is_none());

        let empty = tmp.path().join("empty.yaml");
        fs::write(&empty, "").unwrap();
        assert!(parse_session_meta("empty", &empty).is_none());
    }

    #[test]
    fn test_sort_terminal_match_first() {
        let mut matching = session_meta("11111111-1111-7111-8000-000000000001");
        matching.terminal_session_id = Some("term-1".to_string());
        matching.modified = Some(UNIX_EPOCH + Duration::from_secs(1));

        let mut other = session_meta("11111111-1111-7111-8000-000000000002");
        other.terminal_session_id = Some("term-2".to_string());
        other.modified = Some(UNIX_EPOCH + Duration::from_secs(2));

        let context = PickerContext {
            current_terminal_id: Some("term-1".to_string()),
            current_branch: None,
            current_dir: String::new(),
            current_remote: None,
        };

        let sorted = sort_sessions_for_picker(vec![other, matching.clone()], &context);
        assert_eq!(sorted[0].name, matching.name);
    }

    #[test]
    fn test_sort_branch_match_second() {
        let mut matching = session_meta("22222222-2222-7222-8000-000000000001");
        matching.git_branch = Some("main".to_string());
        matching.modified = Some(UNIX_EPOCH + Duration::from_secs(1));

        let mut other = session_meta("22222222-2222-7222-8000-000000000002");
        other.git_branch = Some("feature".to_string());
        other.modified = Some(UNIX_EPOCH + Duration::from_secs(2));

        let context = PickerContext {
            current_terminal_id: Some("term-x".to_string()),
            current_branch: Some("main".to_string()),
            current_dir: String::new(),
            current_remote: None,
        };

        let sorted = sort_sessions_for_picker(vec![other, matching.clone()], &context);
        assert_eq!(sorted[0].name, matching.name);
    }

    #[test]
    fn test_sort_recency_fallback() {
        let older = session_meta("018f0d1c-5b2a-7000-8000-000000000000");
        let newer = session_meta("018f0d1c-5b2b-7000-8000-000000000000");
        let context = PickerContext {
            current_terminal_id: None,
            current_branch: None,
            current_dir: "/nowhere".to_string(),
            current_remote: None,
        };

        let sorted = sort_sessions_for_picker(vec![older, newer.clone()], &context);
        assert_eq!(sorted[0].name, newer.name);
    }

    #[test]
    fn test_sort_cwd_match_third() {
        // Neither terminal nor branch match; CWD match should win.
        let mut matching = session_meta("33333333-3333-7333-8000-000000000001");
        matching.working_dir = Some("/home/user/projects/foo".to_string());
        matching.modified = Some(UNIX_EPOCH + Duration::from_secs(1));

        let mut other = session_meta("33333333-3333-7333-8000-000000000002");
        other.working_dir = Some("/home/user/projects/bar".to_string());
        other.modified = Some(UNIX_EPOCH + Duration::from_secs(2));

        let context = PickerContext {
            current_terminal_id: Some("term-x".to_string()),
            current_branch: Some("other-branch".to_string()),
            current_dir: "/home/user/projects/foo".to_string(),
            current_remote: None,
        };

        let sorted = sort_sessions_for_picker(vec![other, matching.clone()], &context);
        assert_eq!(sorted[0].name, matching.name, "CWD match should sort first");
    }

    #[test]
    fn test_sort_remote_match_fourth() {
        // Neither terminal, branch, nor cwd match; remote match should win.
        let mut matching = session_meta("44444444-4444-7444-8000-000000000001");
        matching.git_remote = Some("https://github.com/org/repo".to_string());
        matching.modified = Some(UNIX_EPOCH + Duration::from_secs(1));

        let mut other = session_meta("44444444-4444-7444-8000-000000000002");
        other.git_remote = Some("https://github.com/org/other".to_string());
        other.modified = Some(UNIX_EPOCH + Duration::from_secs(2));

        let context = PickerContext {
            current_terminal_id: Some("term-x".to_string()),
            current_branch: Some("other-branch".to_string()),
            current_dir: "/tmp/unrelated".to_string(),
            current_remote: Some("https://github.com/org/repo".to_string()),
        };

        let sorted = sort_sessions_for_picker(vec![other, matching.clone()], &context);
        assert_eq!(sorted[0].name, matching.name, "Remote match should sort first");
    }


    #[test]
    fn test_parse_session_meta_multiline_yaml_separator() {
        use std::io::Write;
        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("test_session.yaml");
        let mut file = std::fs::File::create(&file_path).unwrap();
        let yaml_content = r#"---
type: header
model: gpt-4o
session_id: "test-session-123"
agent_instructions: |
  This is a test agent.
  It includes a literal:
  ---
  inside the string.
---
Some other content here"#;
        file.write_all(yaml_content.as_bytes()).unwrap();
        
        let meta = super::parse_session_meta("test_session", &file_path).expect("Failed to parse");
        assert_eq!(meta.session_id.as_deref(), Some("test-session-123"));
    }
    #[test]
    fn test_build_picker_context_no_panic() {
        let _ = build_picker_context();
    }
}

#[cfg(test)]
#[path = "session_meta_test_extra.rs"]
mod tests_extra;
