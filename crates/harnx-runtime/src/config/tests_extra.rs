#![cfg(test)]

use super::test_support::EnvGuard;
use super::*;

#[test]
fn expand_use_tools_wildcard_returns_concrete_names() {
    let config = Config {
        tools: crate::tool::Tools::init_from_mcp(Some(vec![
            make_tool_decl("alpha_tool"),
            make_tool_decl("beta_tool"),
        ])),
        ..Config::default()
    };

    let expanded = config.expand_use_tools(Some(&["*".to_string()]), None);

    assert_eq!(
        expanded,
        vec!["alpha_tool", "beta_tool", crate::session_history::TOOL_NAME,]
    );
}

#[test]
fn expand_use_tools_empty_is_graceful() {
    let config = Config::default();

    let expanded = config.expand_use_tools(None, None);

    assert!(expanded.is_empty());
}

#[test]
fn expand_use_tools_mcp_failure_logs_warning_and_continues() {
    let _log_guard = lock_log_capture();
    let temp = tempfile::TempDir::new().unwrap();
    let log_file = temp.path().join("expand-use-tools.log");
    let _log_path = EnvGuard::new_file("HARNX_LOG_PATH", &log_file);
    let prev_level = std::env::var_os("HARNX_LOG_LEVEL");
    // SAFETY: test-only; this test serializes env + logger-adjacent state.
    unsafe { std::env::set_var("HARNX_LOG_LEVEL", "debug") };
    let _ = crate::bootstrap::setup_logger(false);

    let mut config = Config {
        tools: crate::tool::Tools::init_from_mcp(Some(vec![make_tool_decl("local_tool")])),
        mcp_servers: vec![harnx_mcp::McpServerConfig {
            name: "broken".to_string(),
            command: std::env::current_exe().unwrap().display().to_string(),
            args: vec!["--definitely-not-a-valid-harnx-flag".to_string()],
            env: std::collections::HashMap::new(),
            roots: vec![],
            enabled: true,
            description: None,
            rename_tools: std::collections::HashMap::new(),
            tool_templates: std::collections::HashMap::new(),
            hooks: None,
            package: None,
        }],
        ..Config::default()
    };
    config.reinit_managers_for_agent(None);

    let expanded = config.expand_use_tools(Some(&["*".to_string()]), None);

    match prev_level {
        Some(value) => unsafe { std::env::set_var("HARNX_LOG_LEVEL", value) },
        None => unsafe { std::env::remove_var("HARNX_LOG_LEVEL") },
    }

    assert_eq!(
        expanded,
        vec!["local_tool", crate::session_history::TOOL_NAME]
    );
    let log = std::fs::read_to_string(log_file).unwrap_or_default();
    assert!(
        log.contains("MCP server 'broken' connection failed"),
        "expected MCP warning in log, got: {log}"
    );
}

// ── expand_use_tools regression tests (#886 filtering) ──────────────────────

/// Regression test for #886: explicit selector must return ONLY that tool,
/// not ALL builtin tools (the bug was that tool_declarations_for_use_tools
/// starts from ALL builtins and only ADDS MCP/ACP/handoff tools, never filtering).
#[test]
fn expand_use_tools_explicit_selector_returns_only_that_tool() {
    let config = Config {
        tools: crate::tool::Tools::init_from_mcp(Some(vec![
            make_tool_decl("fs_read"),
            make_tool_decl("fs_write"),
            make_tool_decl("bash_exec"),
            make_tool_decl("fetch_fetch_markdown"),
        ])),
        ..Config::default()
    };

    // Explicit selector => only fs_read, NOT all builtins
    let expanded = config.expand_use_tools(Some(&["fs_read".to_string()]), None);

    // Should have ONLY fs_read (bug was: would have ALL builtins)
    assert_eq!(expanded, vec!["fs_read"]);
}

/// Wildcard '*' must still return all available tools.
#[test]
fn expand_use_tools_wildcard_returns_all_tools() {
    let config = Config {
        tools: crate::tool::Tools::init_from_mcp(Some(vec![
            make_tool_decl("alpha_tool"),
            make_tool_decl("beta_tool"),
        ])),
        ..Config::default()
    };

    let expanded = config.expand_use_tools(Some(&["*".to_string()]), None);

    // Wildcard returns all tools
    assert!(expanded.contains(&"alpha_tool".to_string()));
    assert!(expanded.contains(&"beta_tool".to_string()));
    assert!(expanded.contains(&crate::session_history::TOOL_NAME.to_string()));
}

#[test]
fn log_path_template_is_absolutized_and_pid_expanded() {
    let _lock = crate::config::test_support::env_lock();
    let prev_cwd = std::env::current_dir().unwrap();
    let prev_path = std::env::var_os("HARNX_LOG_PATH");
    let temp = tempfile::TempDir::new().unwrap();
    std::env::set_current_dir(temp.path()).unwrap();
    unsafe { std::env::set_var("HARNX_LOG_PATH", "logs/harnx-{pid}.log") };

    let (_, log_path) = Config::log_config(false).unwrap();

    std::env::set_current_dir(prev_cwd).unwrap();
    match prev_path {
        Some(value) => unsafe { std::env::set_var("HARNX_LOG_PATH", value) },
        None => unsafe { std::env::remove_var("HARNX_LOG_PATH") },
    }

    let log_path = log_path.expect("log path");
    let rendered = log_path.to_string_lossy().into_owned();
    assert!(rendered.starts_with(temp.path().join("logs").to_string_lossy().as_ref()));
    assert!(rendered.ends_with(&format!("harnx-{}.log", std::process::id())));
}

/// Empty selectors list should return empty (no tools).
#[test]
fn expand_use_tools_empty_list_returns_empty() {
    let config = Config {
        tools: crate::tool::Tools::init_from_mcp(Some(vec![make_tool_decl("fs_read")])),
        ..Config::default()
    };

    let expanded = config.expand_use_tools(Some(&[]), None);
    assert!(expanded.is_empty());
}

/// Multiple explicit selectors return only those selected (no others).
#[test]
fn expand_use_tools_multiple_explicit_selectors_returns_only_those() {
    let config = Config {
        tools: crate::tool::Tools::init_from_mcp(Some(vec![
            make_tool_decl("fs_read"),
            make_tool_decl("fs_write"),
            make_tool_decl("bash_exec"),
        ])),
        ..Config::default()
    };

    let expanded = config.expand_use_tools(
        Some(&["fs_read".to_string(), "bash_exec".to_string()]),
        None,
    );

    // Should have exactly fs_read and bash_exec
    assert_eq!(expanded.len(), 2);
    assert!(expanded.contains(&"fs_read".to_string()));
    assert!(expanded.contains(&"bash_exec".to_string()));
}

// ── SessionHistoryProvider::call_tool end-to-end ─────────────────────────

/// Exercises the full `call_tool` path of `SessionHistoryProvider`: build a
/// real on-disk log, wire it up as the active session's `path`, then call
/// through the provider and verify the envelope shape.
#[tokio::test]
async fn session_history_provider_call_tool_returns_message_entries() {
    use crate::session_history::SessionHistoryProvider;
    use harnx_core::abort::create_abort_signal;
    use harnx_core::tool::ToolProvider as _;

    let tmp = tempfile::TempDir::new().unwrap();

    // Build a minimal config with a session that has a real on-disk log.
    let mut config = Config {
        data: ConfigData {
            stream: false,
            save_session: Some(true),
            ..Default::default()
        },
        ..Default::default()
    };
    let mut session = self::session::new(&config, "call-tool-test", None).unwrap();
    session.set_sessions_dir(tmp.path().to_path_buf());

    // Append a message entry so the log file is initialized and contains data.
    let wrote = self::session::append_event(
        &mut session,
        &harnx_core::session::SessionLogEntry::Message {
            id: None,
            role: crate::client::MessageRole::User,
            content: crate::client::MessageContent::Text("what is 2+2?".to_string()),
            timestamp: None,
            fence_token: None,
        },
    );
    assert!(wrote, "append_event must write the log file");

    config.session = Some(session);
    let global_config: GlobalConfig = Arc::new(RwLock::new(config));

    let provider = SessionHistoryProvider::new(global_config.clone());
    let abort = create_abort_signal();

    // has_tool assertions
    assert!(
        provider.has_tool(crate::session_history::TOOL_NAME),
        "provider must report the session-history tool as owned"
    );
    assert!(
        !provider.has_tool("other_tool"),
        "provider must not claim unrelated tools"
    );

    // call_tool with a type filter so only message entries come back.
    let result = provider
        .call_tool(
            crate::session_history::TOOL_NAME,
            serde_json::json!({"type": "message"}),
            &abort,
        )
        .await
        .map_err(|e| match e {
            harnx_core::tool::ToolError::Recoverable(e) => e,
            harnx_core::tool::ToolError::Fatal(e) => e,
        })
        .unwrap();

    // Verify the content envelope: [{type:"text", text:"[...]"}]
    let text = result["content"][0]["text"]
        .as_str()
        .expect("content[0].text must be a string");
    assert_eq!(
        result["content"][0]["type"], "text",
        "content type must be 'text'"
    );

    // The text must deserialize as a JSON array containing at least one object
    // with type == "message".
    let rows: serde_json::Value =
        serde_json::from_str(text).expect("content text must be valid JSON");
    let arr = rows.as_array().expect("rows must be a JSON array");
    assert!(
        arr.iter().any(|r| r["type"] == "message"),
        "at least one entry with type 'message' must be present; got: {arr:?}"
    );
}

fn make_tool_decl(name: &str) -> crate::tool::ToolDeclaration {
    crate::tool::ToolDeclaration {
        name: name.to_string(),
        description: "desc".to_string(),
        parameters: serde_json::from_value(serde_json::json!({"type": "object"}))
            .expect("tool schema must parse"),
        mcp_tool_name: None,
        mcp_server_name: None,
        call_template: None,
        result_template: None,
        idempotent_hint: None,
        read_only_hint: None,
    }
}

fn lock_log_capture() -> std::sync::MutexGuard<'static, ()> {
    super::tests::LOG_CAPTURE_LOCK
        .get_or_init(|| std::sync::Mutex::new(()))
        .lock()
        .unwrap_or_else(|err| err.into_inner())
}
