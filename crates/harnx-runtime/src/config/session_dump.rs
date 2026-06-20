use super::*;

use crate::client::{Message, MessageRole};
use anyhow::{bail, Context, Result};
use std::path::PathBuf;

pub fn render_session_dump(agent_name: Option<&str>, session_id: &str) -> Result<String> {
    let mut config = load_config_for_session_dump()?;
    let session_path = resolve_session_path(agent_name, session_id);

    if !session_path.is_file() {
        let scope = agent_name.unwrap_or("<top-level>");
        bail!("session '{session_id}' not found for agent '{scope}'");
    }

    config.sessions_dir_override = Some(
        session_path
            .parent()
            .map(|path| path.to_path_buf())
            .context("Session file missing parent directory")?,
    );

    if let Some(meta) = parse_session_meta(session_id, &session_path) {
        if meta.agent_name.as_deref() != agent_name {
            if let Some(header_agent_name) = meta.agent_name.as_deref() {
                log::warn!(
                    "session '{}' header agent_name '{}' does not match requested agent '{:?}'",
                    session_id,
                    header_agent_name,
                    agent_name
                );
            }
        }
    }

    let session = self::session::load(&config, session_id, &session_path)
        .with_context(|| format!("Failed to render session dump for '{session_id}'"))?;

    build_session_dump(&session)
}

fn resolve_session_path(agent_name: Option<&str>, session_id: &str) -> PathBuf {
    let config = Config {
        sessions_dir_override: Some(harnx_core::config_paths::sessions_dir(agent_name)),
        ..Config::default()
    };
    config.session_file(session_id)
}

fn load_config_for_session_dump() -> Result<Config> {
    Config::load_from_file(&Config::config_file()).context("Failed to load config for session dump")
}

fn build_session_dump(session: &Session) -> Result<String> {
    let mut data = serde_json::Map::new();
    data.insert("model".to_string(), serde_json::json!(session.model().id()));
    data.insert(
        "tokens".to_string(),
        serde_json::Value::Object(build_session_tokens(session)),
    );
    data.insert(
        "agent_variables".to_string(),
        serde_json::to_value(session.agent_variables())?,
    );
    data.insert(
        "messages".to_string(),
        serde_json::to_value(build_session_messages(session))?,
    );
    data.insert(
        "snapshot".to_string(),
        serde_json::Value::Object(build_session_snapshot(session)),
    );

    serde_yaml::to_string(&serde_json::Value::Object(data))
        .context("Unable to render state-only session dump")
}

fn build_session_tokens(session: &Session) -> serde_json::Map<String, serde_json::Value> {
    let mut tokens = serde_json::Map::new();
    let (total_tokens, percent) = session.tokens_usage();
    tokens.insert("total_tokens".to_string(), serde_json::json!(total_tokens));
    if let Some(max_input_tokens) = session.model().max_input_tokens() {
        tokens.insert(
            "max_input_tokens".to_string(),
            serde_json::json!(max_input_tokens),
        );
    }
    if percent != 0.0 {
        tokens.insert(
            "total/max".to_string(),
            serde_json::json!(format!("{percent}%")),
        );
    }
    tokens
}

fn build_session_messages(session: &Session) -> Vec<Message> {
    session
        .compressed_messages
        .iter()
        .chain(session.messages.iter())
        .filter(|message| !matches!(message.role, MessageRole::System))
        .cloned()
        .collect()
}

fn build_session_snapshot(session: &Session) -> serde_json::Map<String, serde_json::Value> {
    let mut snapshot = serde_json::Map::new();
    snapshot.insert("id".to_string(), serde_json::json!(session.id()));
    snapshot.insert("path".to_string(), serde_json::json!(session.path));
    snapshot.insert(
        "session_id".to_string(),
        serde_json::json!(session.session_id),
    );
    snapshot.insert(
        "working_dir".to_string(),
        serde_json::json!(session.working_dir),
    );
    snapshot.insert(
        "git_branch".to_string(),
        serde_json::json!(session.git_branch),
    );
    snapshot.insert(
        "git_remote".to_string(),
        serde_json::json!(session.git_remote),
    );
    snapshot.insert(
        "terminal_session_id".to_string(),
        serde_json::json!(session.terminal_session_id),
    );
    snapshot.insert(
        "agent_name".to_string(),
        serde_json::json!(session.agent_name),
    );
    snapshot.insert(
        "save_session".to_string(),
        serde_json::json!(session.save_session),
    );
    snapshot.insert(
        "compress_threshold".to_string(),
        serde_json::json!(session.compress_threshold),
    );
    snapshot.insert(
        "model_fallbacks".to_string(),
        serde_json::json!(session.model_fallbacks),
    );
    snapshot.insert(
        "compaction_agent".to_string(),
        serde_json::json!(session.compaction_agent),
    );
    snapshot.insert(
        "log_entry_count".to_string(),
        serde_json::json!(session.log_entry_count),
    );
    snapshot
}

#[cfg(all(test, unix))]
mod tests {
    use super::render_session_dump;
    use crate::config::{paths, test_support::EnvGuard};
    use harnx_core::config_paths::{agent_data_dir, state_dir};
    use std::fs;
    use std::sync::{Mutex, OnceLock};
    use tempfile::TempDir;

    fn normalize_session_dump_for_snapshot(dump: &str) -> String {
        let dump = dump.to_string();
        let temp_dir = std::env::temp_dir().display().to_string();
        let dump = dump.replace(&temp_dir, "[TMP]");
        let dump = dump
            .lines()
            .filter(|line| !line.starts_with("assertion_line: "))
            .collect::<Vec<_>>()
            .join("\n");
        let start = dump.find("path: [TMP]/").unwrap_or(0);
        if let Some(rel_end) = dump[start..].find("/agents/") {
            let prefix_end = start + rel_end;
            let mut normalized = dump.clone();
            normalized.replace_range(start + "path: ".len()..prefix_end, "[TMP]");
            normalized
        } else {
            dump
        }
    }

    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        match LOCK.get_or_init(|| Mutex::new(())).lock() {
            Ok(g) => g,
            Err(e) => e.into_inner(),
        }
    }

    fn write_test_config() {
        let config_dir = harnx_core::config_paths::config_dir();
        fs::create_dir_all(config_dir.join(paths::CLIENTS_DIR_NAME)).unwrap();
        fs::write(
            config_dir.join("config.yaml"),
            "show_timestamps: false\nshow_sequence_numbers: false\n",
        )
        .unwrap();
        fs::write(
            config_dir.join(paths::CLIENTS_DIR_NAME).join("openai.yaml"),
            "type: openai\napi_key: sk-test\nmodels:\n  - name: test-model\n    type: chat\n    max_input_tokens: 4096\n",
        )
        .unwrap();
    }

    #[test]
    fn render_session_dump_resolves_scope_and_omits_system_prompt() {
        let _lock = env_lock();
        let tmp = TempDir::new().unwrap();
        let _data = EnvGuard::new("HARNX_DATA_DIR", tmp.path());
        let _state = EnvGuard::new("HARNX_STATE_DIR", tmp.path());
        write_test_config();

        let sessions_dir = agent_data_dir("smith").join("sessions");
        fs::create_dir_all(sessions_dir.join("sub")).unwrap();
        let session_path = sessions_dir.join("sub/demo.yaml");
        fs::write(
            &session_path,
            r#"---
type: header
model: openai:test-model
session_id: session-123
working_dir: /tmp/work
git_branch: main
git_remote: origin
terminal_session_id: term-1
agent_name: smith
agent_variables:
  topic: forge
---
type: message
role: user
content: hello
---
type: compress
prompt: SECRET SYSTEM PROMPT
---
type: message
role: assistant
content: hi there
"#,
        )
        .unwrap();

        let dump = render_session_dump(Some("smith"), "sub/demo").unwrap();
        assert!(dump.contains("model: openai:test-model"));
        assert!(dump.contains("topic: forge"));
        assert!(dump.contains("content: hello"));
        assert!(dump.contains("content: hi there"));
        assert!(dump.contains("snapshot:"));
        assert!(!dump.contains("SECRET SYSTEM PROMPT"));
    }

    #[test]
    fn render_session_dump_snapshot_is_stable_and_omits_system_prompt() {
        let _lock = env_lock();
        let tmp = TempDir::new().unwrap();
        let _data = EnvGuard::new("HARNX_DATA_DIR", tmp.path());
        let _state = EnvGuard::new("HARNX_STATE_DIR", tmp.path());
        write_test_config();

        let sessions_dir = agent_data_dir("smith").join("sessions");
        fs::create_dir_all(sessions_dir.join("snapshots")).unwrap();
        let session_path = sessions_dir.join("snapshots/render.yaml");
        fs::write(
            &session_path,
            r#"---
type: header
model: openai:test-model
session_id: session-123
working_dir: /tmp/work
git_branch: main
git_remote: origin
terminal_session_id: term-1
agent_name: smith
save_session: true
compress_threshold: 42
model_fallbacks:
  - openai:fallback
compaction_agent: smith-compactor
agent_variables:
  topic: forge
  phase: alpha
---
type: message
role: system
content: SECRET SYSTEM PROMPT
---
type: message
role: user
content: hello
---
type: message
role: assistant
content: hi there
"#,
        )
        .unwrap();

        let dump = render_session_dump(Some("smith"), "snapshots/render").unwrap();
        assert!(!dump.contains("SECRET SYSTEM PROMPT"));

        insta::assert_snapshot!(
            "render_session_dump_snapshot",
            normalize_session_dump_for_snapshot(&dump)
        );
    }

    #[test]
    fn render_session_dump_reports_missing_session_clearly() {
        let _lock = env_lock();
        let tmp = TempDir::new().unwrap();
        let _data = EnvGuard::new("HARNX_DATA_DIR", tmp.path());
        let _state = EnvGuard::new("HARNX_STATE_DIR", tmp.path());
        write_test_config();

        let err = render_session_dump(Some("smith"), "missing").unwrap_err();
        assert_eq!(
            err.to_string(),
            "session 'missing' not found for agent 'smith'"
        );
    }

    #[test]
    fn render_session_dump_warns_on_agent_mismatch_and_continues() {
        let _lock = env_lock();
        let tmp = TempDir::new().unwrap();
        let _data = EnvGuard::new("HARNX_DATA_DIR", tmp.path());
        let _state = EnvGuard::new("HARNX_STATE_DIR", tmp.path());
        write_test_config();

        let sessions_dir = state_dir().join("sessions");
        fs::create_dir_all(&sessions_dir).unwrap();
        let session_path = sessions_dir.join("mismatch.yaml");
        fs::write(
            &session_path,
            r#"---
type: header
model: openai:test-model
session_id: session-top
working_dir: /tmp/work
agent_name: other-agent
---
type: message
role: user
content: mismatch ok
"#,
        )
        .unwrap();

        let dump = render_session_dump(None, "mismatch").unwrap();
        assert!(dump.contains("content: mismatch ok"));
        assert!(dump.contains("agent_name: other-agent"));
    }

    #[test]
    fn render_session_dump_warns_on_requested_agent_mismatch_and_continues() {
        let _lock = env_lock();
        let tmp = TempDir::new().unwrap();
        let _data = EnvGuard::new("HARNX_DATA_DIR", tmp.path());
        let _state = EnvGuard::new("HARNX_STATE_DIR", tmp.path());
        write_test_config();

        let sessions_dir = agent_data_dir("smith").join("sessions");
        fs::create_dir_all(&sessions_dir).unwrap();
        let session_path = sessions_dir.join("mismatch-agent.yaml");
        fs::write(
            &session_path,
            r#"---
type: header
model: openai:test-model
session_id: session-abc
working_dir: /tmp/work
agent_name: other-agent
---
type: message
role: assistant
content: still loads
"#,
        )
        .unwrap();

        let dump = render_session_dump(Some("smith"), "mismatch-agent").unwrap();
        assert!(dump.contains("content: still loads"));
        assert!(!dump.contains("system_prompt"));
    }

    #[test]
    fn render_session_dump_loads_clients_from_disk_without_mcp_init() {
        let _lock = env_lock();
        let tmp = TempDir::new().unwrap();
        let _data = EnvGuard::new("HARNX_DATA_DIR", tmp.path());
        let _state = EnvGuard::new("HARNX_STATE_DIR", tmp.path());
        write_test_config();

        let sessions_dir = agent_data_dir("smith").join("sessions");
        fs::create_dir_all(&sessions_dir).unwrap();
        let session_path = sessions_dir.join("disk-config.yaml");
        fs::write(
            &session_path,
            r#"---
type: header
model: openai:test-model
session_id: session-disk
working_dir: /tmp/work
agent_name: smith
---
type: message
role: user
content: disk config path
"#,
        )
        .unwrap();

        let dump = render_session_dump(Some("smith"), "disk-config").unwrap();
        assert!(dump.contains("model: openai:test-model"));
        assert!(dump.contains("content: disk config path"));
    }
}
