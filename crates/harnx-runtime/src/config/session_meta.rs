use harnx_core::session::SessionLogEntry;
use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::time::SystemTime;

#[derive(Debug, Clone)]
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

fn read_session_header_bytes(path: &Path) -> Option<String> {
    let mut file = File::open(path).ok()?;
    let mut buffer = [0_u8; 4096];
    let read_len = file.read(&mut buffer).ok()?;
    let bytes = &buffer[..read_len];

    let boundary = bytes
        .windows(4)
        .position(|window| window == b"---\n")
        .unwrap_or(bytes.len());

    String::from_utf8(bytes[..boundary].to_vec()).ok()
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
    use super::parse_session_meta;
    use std::fs;
    use tempfile::TempDir;

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
}
