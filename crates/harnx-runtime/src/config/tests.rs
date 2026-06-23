//! Tests for the config module (extracted from mod.rs for code health).
#![cfg(test)]

use super::test_support::EnvGuard;
use super::*;
use harnx_core::message::MessageRole;
use std::sync::{Mutex, OnceLock};

#[test]
fn test_render_status_line() {
    let mut config = Config {
        model: harnx_client::Model::new("test", "test-model"),
        ..Default::default()
    };

    // When agent and session are missing:
    assert_eq!(config.render_status_line(true), "");

    let mut agent = Agent::new(AgentConfig::from_markdown("my-agent", "prompt").unwrap());
    agent.set_model(crate::client::Model::new("test", "agent-model"));
    config.agent = Some(agent);

    // Agent + Model (no session)
    assert_eq!(
        config.render_status_line(true),
        "🤖 my-agent ▸ test:agent-model"
    );
    assert_eq!(
        config.render_status_line(false),
        "my-agent ▸ test:agent-model"
    );

    let session = super::session::new(&config, "my-session").unwrap();
    let session_id = session.id().to_string();
    config.session = Some(session);

    // Agent + Model + Session
    assert_eq!(
        config.render_status_line(true),
        format!("🤖 my-agent ▸ test:agent-model ▸ {}", session_id)
    );
    assert_eq!(
        config.render_status_line(false),
        format!("my-agent ▸ test:agent-model ▸ {}", session_id)
    );

    // Agent + Session (No Model ID)
    let mut config3 = Config::default();
    let mut agent3 = Agent::new(AgentConfig::from_markdown("agent3", "prompt").unwrap());
    agent3.set_model(crate::client::Model::new("", ""));
    config3.agent = Some(agent3);
    let session3 = super::session::new(&config3, "session3").unwrap();
    let session_id3 = session3.id().to_string();
    config3.session = Some(session3);

    assert_eq!(
        config3.render_status_line(true),
        format!("🤖 agent3 ▸ {}", session_id3)
    );
    assert_eq!(
        config3.render_status_line(false),
        format!("agent3 ▸ {}", session_id3)
    );

    // Session only (create a session without an agent)
    let mut config2 = Config::default();
    let session_no_agent = super::session::new(&config2, "my-session2").unwrap();
    let session_id2 = session_no_agent.id().to_string();
    config2.session = Some(session_no_agent);
    assert_eq!(
        config2.render_status_line(true),
        format!("💬 {}", session_id2)
    );
    assert_eq!(config2.render_status_line(false), session_id2);
}
#[test]
fn test_split_tool_selectors_simple() {
    assert_eq!(split_tool_selectors("a,b,c"), vec!["a", "b", "c"]);
}

#[test]
fn test_split_tool_selectors_braces() {
    assert_eq!(
        split_tool_selectors("fs_{read_file,write_file},bash_exec"),
        vec!["fs_{read_file,write_file}", "bash_exec"]
    );
}

#[test]
fn test_split_tool_selectors_single() {
    assert_eq!(split_tool_selectors("*"), vec!["*"]);
}

#[test]
fn test_split_tool_selectors_nested_braces() {
    assert_eq!(
        split_tool_selectors("a_{b_{c,d},e},f"),
        vec!["a_{b_{c,d},e}", "f"]
    );
}

#[test]
fn test_split_tool_selectors_empty() {
    assert_eq!(split_tool_selectors(""), vec![""]);
}

#[test]
fn test_init_mcp_manager_with_roots() {
    // This test asserts cwd is prepended as a root, which depends on HOME
    // (cwd is skipped when it equals or is an ancestor of $HOME). Other tests
    // mutate HOME under `env_lock`, so hold the same lock to avoid racing
    // with them and observing a transient HOME that suppresses the cwd root.
    #[cfg(unix)]
    let _env_guard = env_lock();

    let mut config = Config::default();
    let server = McpServerConfig {
        name: "test".to_string(),
        command: "ls".to_string(),
        args: vec![],
        env: HashMap::new(),
        roots: vec!["/existing".to_string()],
        enabled: true,
        description: None,
        rename_tools: HashMap::new(),
        tool_templates: HashMap::new(),
        package: None,
        hooks: None,
    };
    config.mcp_servers = vec![server];
    config.mcp_root = vec!["/extra".to_string()];

    config.init_mcp_manager();

    let manager = config.mcp_manager.expect("Manager should be initialized");
    let client = manager.get_client("test").expect("Client should exist");
    let roots = client.get_roots();

    // Roots should be: [cwd, /extra, /existing]
    assert_eq!(roots.len(), 3);
    let cwd = env::current_dir()
        .unwrap()
        .into_os_string()
        .into_string()
        .unwrap();
    assert_eq!(roots[0], cwd);
    assert_eq!(roots[1], "/extra");
    assert_eq!(roots[2], "/existing");
}

// ── handoff session emptying tests ─────────────────────────────────────

/// Verify that empty_session clears messages from a session that was loaded
/// with an existing name (simulating the handoff path with session_id).
/// This is the unit-level guarantee behind the #291 fix: after handoff the
/// new agent starts with a blank session even when a session_id was provided.
#[test]
fn test_new_session_has_session_id() {
    let config = Config::default();
    let session = self::session::new(&config, "metadata-check").unwrap();

    assert!(session.session_id.is_some());
}

#[test]
fn test_new_session_has_short_id_filename() {
    let tmp = tempfile::TempDir::new().unwrap();
    let mut config = Config {
        sessions_dir_override: Some(tmp.path().to_path_buf()),
        ..Config::default()
    };

    config.use_session(None).unwrap();

    let session = config.session.as_ref().unwrap();
    assert_eq!(
        session.id.len(),
        6,
        "anonymous session ID should be 6-char short ID"
    );
    assert!(
        crate::utils::session_name::decode_timestamp_session_id(&session.id).is_some(),
        "anonymous session ID should be a valid base64url timestamp short ID"
    );
    assert_eq!(
        session
            .sessions_dir
            .as_ref()
            .unwrap()
            .join(format!("{}.yaml", session.id)),
        tmp.path().join(format!("{}.yaml", session.id))
    );
    // Claim stub file must exist immediately after use_session returns
    assert!(
        tmp.path().join(format!("{}.yaml", session.id)).exists(),
        "claim stub file should exist on disk immediately after use_session(None)"
    );
}

#[test]
fn test_anonymous_session_id_collision_retries() {
    let tmp = tempfile::TempDir::new().unwrap();
    let mut config1 = Config {
        sessions_dir_override: Some(tmp.path().to_path_buf()),
        ..Config::default()
    };
    let mut config2 = Config {
        sessions_dir_override: Some(tmp.path().to_path_buf()),
        ..Config::default()
    };

    config1.use_session(None).unwrap();
    config2.use_session(None).unwrap();

    let id1 = config1.session.as_ref().unwrap().id.clone();
    let id2 = config2.session.as_ref().unwrap().id.clone();
    assert_ne!(
        id1, id2,
        "concurrent anonymous sessions must get unique IDs"
    );
    assert_eq!(id1.len(), 6);
    assert_eq!(id2.len(), 6);
}

#[test]
fn empty_session_clears_named_session_with_messages() {
    let mut config = Config::default();
    let mut session = self::session::new(&config, "handoff-target").unwrap();
    session.push_message_for_test(MessageRole::System, "You are agent A.".to_string());
    session.push_message_for_test(MessageRole::User, "Hello from old session".to_string());
    session.push_message_for_test(MessageRole::Assistant, "Response from agent A".to_string());
    assert!(!session.is_empty());
    config.session = Some(session);

    config.empty_session().unwrap();

    let session = config.session.as_ref().unwrap();
    assert!(
        session.is_empty(),
        "session should be empty after empty_session"
    );
}

// ── after_chat_completion incremental persistence tests ─────────────────

/// Verify that after_chat_completion persists intermediate rounds
/// (non-empty tool_results) to the session, not just the final round.
#[test]
fn after_chat_completion_saves_intermediate_tool_rounds() {
    use crate::tool::{ToolCall, ToolResult};
    use serde_json::json;

    let tmp = tempfile::TempDir::new().unwrap();
    let mut config = Config {
        data: ConfigData {
            stream: false,
            save_session: Some(true),
            ..Default::default()
        },
        ..Default::default()
    };
    let mut session = self::session::new(&config, "test-intermediate").unwrap();
    session.set_sessions_dir(tmp.path().to_path_buf());
    config.session = Some(session);

    let _agent = config.extract_agent();
    let global_config: GlobalConfig = Arc::new(RwLock::new(config));
    let input = crate::config::input::from_str(&global_config, "do something", None);

    let tool_results = vec![ToolResult::new(
        ToolCall {
            name: "my_tool".to_string(),
            arguments: json!({"key": "val"}),
            id: Some("tc1".to_string()),
            thought_signature: None,
        },
        json!("tool output"),
    )];

    // Call after_chat_completion with non-empty tool_results.
    // Previously this returned early without saving; now it should persist.
    global_config
        .write()
        .after_chat_completion(
            &input,
            "intermediate output",
            None,
            &tool_results,
            &Default::default(),
        )
        .unwrap();

    let config_guard = global_config.read();
    let session = config_guard.session.as_ref().unwrap();
    assert!(
        !session.is_empty(),
        "session should have messages after intermediate round"
    );
    // Verify content via the session's export (which serializes messages).
    let export = session.export().unwrap();
    assert!(
        export.contains("intermediate output"),
        "session export should contain assistant output; got:\n{export}"
    );
    assert!(
        export.contains("my_tool"),
        "session export should contain tool call info; got:\n{export}"
    );
}

/// Regression test for the ACP-server failure where `use_agent_by_name`
/// followed by `use_session` bailed with "agent variables are required"
/// for an agent whose variables use `path:` (file-backed defaults).  The
/// async `agent::init` resolves these defaults, but the synchronous
/// `retrieve_agent` does not — `use_agent_by_name` must do so itself,
/// otherwise `init_agent_session_variables` (called from `use_session`)
/// finds no defaults and bails in non-interactive contexts like ACP.
#[tokio::test]
async fn test_use_agent_by_name_resolves_file_backed_variable_defaults() {
    use crate::client::TestStateGuard;

    let temp = tempfile::TempDir::new().unwrap();
    let agents_dir = temp.path().join("agents");
    std::fs::create_dir_all(agents_dir.join("shared")).unwrap();
    std::fs::write(
            agents_dir.join("file-backed-vars.md"),
            "---\nvariables:\n  - name: prompt_body\n    description: Shared prompt\n    path: shared/prompt.md\n---\n{{prompt_body}}\n",
        )
        .unwrap();
    std::fs::write(agents_dir.join("shared/prompt.md"), "Loaded body").unwrap();

    // Hold the global test lock so concurrent tests can't race on the
    // shared HARNX_CONFIG_DIR env var.
    let _guard = TestStateGuard::new(None).await;
    let _env = EnvGuard::new("HARNX_CONFIG_DIR", temp.path());

    // Drive use_session in non-interactive mode so the inquire prompt
    // that would otherwise hang in CI is suppressed.  The fix must still
    // produce populated shared_variables under no_interaction.
    let mut config = Config {
        info_flag: true,
        ..Default::default()
    };
    config
        .use_agent_by_name("file-backed-vars")
        .expect("use_agent_by_name must resolve path-backed variable defaults");
    config
        .use_session(Some("file-backed-vars-session"))
        .expect("use_session must succeed once defaults are resolved");

    let agent = config.agent.as_ref().expect("agent should be set");
    assert_eq!(
        agent
            .shared_variables()
            .get("prompt_body")
            .map(String::as_str),
        Some("Loaded body"),
        "shared_variables should be populated from the file-backed default"
    );
}
// ── Tests for HOME boundary guard in reinit_managers_for_agent ──

#[cfg(unix)]
/// Helper: make a minimal MCP server config for testing roots.
fn make_test_mcp_server(name: &str) -> McpServerConfig {
    McpServerConfig {
        name: name.to_string(),
        command: "echo".to_string(),
        args: vec![],
        env: HashMap::new(),
        roots: vec![],
        enabled: true,
        description: None,
        rename_tools: HashMap::new(),
        tool_templates: HashMap::new(),
        hooks: None,
        package: None,
    }
}

#[cfg(unix)]
/// Serialize env-mutating tests to prevent HOME from racing.
fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    match LOCK.get_or_init(|| std::sync::Mutex::new(())).lock() {
        Ok(g) => g,
        Err(e) => e.into_inner(),
    }
}

#[cfg(unix)]
/// Helper: RAII guard for HOME env var (holds the env_lock while alive).
struct HomeGuard {
    prev: Option<std::ffi::OsString>,
    _lock: std::sync::MutexGuard<'static, ()>,
}
#[cfg(unix)]
impl HomeGuard {
    fn set(value: &str) -> Self {
        let _lock = env_lock();
        let prev = std::env::var_os("HOME");
        unsafe { std::env::set_var("HOME", value) };
        Self { prev, _lock }
    }
}
#[cfg(unix)]
impl Drop for HomeGuard {
    fn drop(&mut self) {
        match &self.prev {
            Some(v) => unsafe { std::env::set_var("HOME", v) },
            None => unsafe { std::env::remove_var("HOME") },
        }
    }
}

#[cfg(unix)]
#[test]
fn test_cwd_equals_home_not_added_as_root() {
    // Set HOME to the actual CWD so they match.
    let cwd = env::current_dir().unwrap();
    let cwd_str = cwd.to_string_lossy().to_string();
    let _home = HomeGuard::set(&cwd_str);

    let mut config = Config::default();
    let server = make_test_mcp_server("test_eq");
    config.mcp_servers = vec![server];
    config.mcp_root = vec![];
    config.init_mcp_manager();

    let manager = config.mcp_manager.expect("Manager should be initialized");
    let client = manager.get_client("test_eq").expect("Client should exist");
    let roots = client.get_roots();
    assert!(
        !roots.contains(&cwd_str),
        "CWD = $HOME must not appear as MCP root, but got: {roots:?}"
    );
}

#[cfg(unix)]
#[test]
fn test_cwd_above_home_not_added_as_root() {
    // Set HOME to CWD/subdir — making CWD an ancestor of $HOME.
    // CWD must not be added as a root.
    let cwd = env::current_dir().unwrap();
    let cwd_str = cwd.to_string_lossy().to_string();
    let fake_home = format!("{cwd_str}/harnx-test-fake-home-above");
    let _home = HomeGuard::set(&fake_home);

    let mut config = Config::default();
    let server = make_test_mcp_server("test_above");
    config.mcp_servers = vec![server];
    config.mcp_root = vec![];
    config.init_mcp_manager();

    let manager = config.mcp_manager.expect("Manager should be initialized");
    let client = manager
        .get_client("test_above")
        .expect("Client should exist");
    let roots = client.get_roots();
    assert!(
        !roots.contains(&cwd_str),
        "CWD that is ancestor of $HOME must not appear as root, but got: {roots:?}"
    );
}

#[cfg(unix)]
#[test]
fn test_cwd_below_home_added_as_root() {
    // Set HOME to parent of CWD so CWD is a child of $HOME — should be allowed.
    let cwd = env::current_dir().unwrap();
    let cwd_str = cwd.to_string_lossy().to_string();
    let parent = cwd.parent().unwrap_or(&cwd);
    let parent_str = parent.to_string_lossy().to_string();
    let _home = HomeGuard::set(&parent_str);

    let mut config = Config::default();
    let server = make_test_mcp_server("test_below");
    config.mcp_servers = vec![server];
    config.mcp_root = vec![];
    config.init_mcp_manager();

    let manager = config.mcp_manager.expect("Manager should be initialized");
    let client = manager
        .get_client("test_below")
        .expect("Client should exist");
    let roots = client.get_roots();
    assert!(
        roots.contains(&cwd_str),
        "CWD below $HOME should be added as root, but not found in: {roots:?}"
    );
}

// ── select_tools whitelist tests (#624) ──────────────────────────────────

fn make_tool_decl(name: &str) -> harnx_core::tool::ToolDeclaration {
    harnx_core::tool::ToolDeclaration {
        name: name.to_string(),
        description: format!("tool {name}"),
        parameters: Default::default(),
        mcp_tool_name: None,
        mcp_server_name: None,
        call_template: None,
        result_template: None,
        idempotent_hint: None,
        read_only_hint: None,
    }
}

/// Regression test for #624: when an agent has a `use_tools` whitelist and
/// `self.agent` is populated with all MCP tools, `select_tools` must return
/// only the whitelisted subset — not every tool known to the agent.
#[test]
fn select_tools_respects_use_tools_whitelist() {
    use harnx_core::{agent_config::AgentConfig, tool::Tools};

    // Set up config with three available tools (tool_use defaults to true).
    let mut config = Config {
        tools: Tools::init_from_mcp(Some(vec![
            make_tool_decl("fs_read"),
            make_tool_decl("fs_write"),
            make_tool_decl("bash_exec"),
        ])),
        ..Config::default()
    };

    // Active agent also has all three tools (as happens at runtime via init_from_mcp).
    let mut agent_config = AgentConfig::from_prompt("test agent");
    agent_config.set_tools(Tools::init_from_mcp(Some(vec![
        make_tool_decl("fs_read"),
        make_tool_decl("fs_write"),
        make_tool_decl("bash_exec"),
    ])));
    config.agent = Some(crate::config::agent::Agent::new(agent_config));

    // Agent's use_tools only requests fs_read.
    let mut agent_config2 = AgentConfig::from_prompt("test");
    agent_config2.set_use_tools(Some(vec!["fs_read".to_string()]));

    let result = config.select_tools(&agent_config2);

    let names: Vec<String> = result
        .unwrap_or_default()
        .into_iter()
        .map(|t| t.name)
        .collect();

    assert_eq!(
        names,
        vec!["fs_read".to_string()],
        "select_tools should honour use_tools and not leak fs_write or bash_exec: got {names:?}"
    );
}

/// When use_tools is not set, select_tools should return None (no tools).
#[test]
fn select_tools_returns_none_without_use_tools() {
    use harnx_core::{agent_config::AgentConfig, tool::Tools};

    let config = Config {
        tools: Tools::init_from_mcp(Some(vec![make_tool_decl("fs_read")])),
        ..Config::default()
    };

    let agent_config = AgentConfig::from_prompt("no tools");
    // use_tools is not set
    let result = config.select_tools(&agent_config);
    assert!(
        result.is_none(),
        "select_tools should return None when use_tools is unset"
    );
}

use super::apply_mcp_server_patch;
use harnx_mcp::McpServerConfig;

fn make_server(name: &str, command: &str) -> McpServerConfig {
    McpServerConfig {
        name: name.to_string(),
        command: command.to_string(),
        args: vec![],
        env: Default::default(),
        roots: vec![],
        enabled: true,
        description: None,
        rename_tools: Default::default(),
        tool_templates: Default::default(),
        package: Some("mypkg".to_string()), // Important: test that this is preserved
        hooks: None,
    }
}

#[test]
fn apply_mcp_server_patch_with_identity_expression_leaves_config_unchanged() {
    let mut server = make_server("test-server", "mcp-test");
    let original_name = server.name.clone();
    let original_command = server.command.clone();

    let result = apply_mcp_server_patch(&mut server, &[".".to_string()]);

    assert!(result.is_ok());
    assert_eq!(server.name, original_name);
    assert_eq!(server.command, original_command);
}

#[test]
fn apply_mcp_server_patch_sets_field_via_jq_expression() {
    let mut server = make_server("test-server", "mcp-original");

    let result = apply_mcp_server_patch(
        &mut server,
        &[
            r#".command = "mcp-patched""#.to_string(),
            r#".args = ["--verbose"]"#.to_string(),
        ],
    );

    assert!(result.is_ok());
    assert_eq!(server.command, "mcp-patched");
    assert_eq!(server.args, vec!["--verbose"]);
}

#[test]
fn apply_mcp_server_patch_with_empty_patches_is_noop() {
    let mut server = make_server("test-server", "mcp-test");
    let original_name = server.name.clone();

    let result = apply_mcp_server_patch(&mut server, &[]);

    assert!(result.is_ok());
    assert_eq!(server.name, original_name);
}

#[test]
fn apply_mcp_server_patch_preserves_server_package_field() {
    let mut server = make_server("test-server", "mcp-test");
    assert_eq!(server.package, Some("mypkg".to_string()));

    // Apply a patch that would serialize and deserialize
    let result = apply_mcp_server_patch(
        &mut server,
        &[r#".description = "Updated description""#.to_string()],
    );

    assert!(result.is_ok());
    // The package field should be preserved even though it has #[serde(skip)]
    assert_eq!(server.package, Some("mypkg".to_string()));
    assert_eq!(server.description, Some("Updated description".to_string()));
}

#[test]
fn apply_mcp_server_patch_with_invalid_jq_expression_returns_err() {
    let mut server = make_server("test-server", "mcp-test");
    let original_command = server.command.clone();

    // Invalid expression — unclosed string
    let result = apply_mcp_server_patch(&mut server, &[r#".command = "unclosed"#.to_string()]);

    assert!(result.is_err());
    // Server should be unchanged
    assert_eq!(server.command, original_command);
}

use super::apply_client_patch;
use harnx_client::ClientConfig;

fn make_openai_client() -> ClientConfig {
    let mut client: ClientConfig = serde_yaml::from_str("type: openai\napi_key: sk-original\n")
        .expect("should parse openai client config");
    client.set_name("openai".to_string());
    client
}

fn make_claude_client() -> ClientConfig {
    let mut client: ClientConfig = serde_yaml::from_str("type: claude\napi_key: sk-original\n")
        .expect("should parse claude client config");
    client.set_name("claude".to_string());
    client
}

#[test]
fn apply_client_patch_with_identity_expression_leaves_config_unchanged() {
    let mut client = make_openai_client();
    let before = serde_json::to_value(&client).expect("serialize");
    let result = apply_client_patch(&mut client, &[".".to_string()]);
    let after = serde_json::to_value(&client).expect("serialize");
    assert!(result.is_ok());
    assert_eq!(before, after);
}

#[test]
fn apply_client_patch_with_empty_patches_is_noop() {
    let mut client = make_openai_client();
    let before = serde_json::to_value(&client).expect("serialize");
    let result = apply_client_patch(&mut client, &[]);
    let after = serde_json::to_value(&client).expect("serialize");
    assert!(result.is_ok());
    assert_eq!(before, after);
}

#[test]
fn apply_client_patch_sets_field_via_jq_expression() {
    let mut client = make_openai_client();
    let result = apply_client_patch(&mut client, &[r#".api_key = "sk-patched""#.to_string()]);
    assert!(result.is_ok());
    if let ClientConfig::OpenAIConfig(c) = &client {
        assert_eq!(c.api_key.as_deref(), Some("sk-patched"));
    } else {
        panic!("expected OpenAI client, got: {client:?}");
    }
}

#[test]
fn apply_client_patch_name_filter_matches_and_preserves_name() {
    let mut client = make_claude_client();
    client.set_package(Some("pkg".to_string()));

    let result = apply_client_patch(
        &mut client,
        &[r#"if .name == "claude" then .api_key = "patched-key" else . end"#.to_string()],
    );

    assert!(result.is_ok());
    assert_eq!(client.effective_name(), "claude");
    if let ClientConfig::ClaudeConfig(c) = &client {
        assert_eq!(c.api_key.as_deref(), Some("patched-key"));
        assert_eq!(c.package.as_deref(), Some("pkg"));
    } else {
        panic!("expected Claude client, got: {client:?}");
    }
}

#[test]
fn apply_client_patch_with_invalid_jq_expression_returns_err() {
    let mut client = make_openai_client();
    let before = serde_json::to_value(&client).expect("serialize");
    let result = apply_client_patch(&mut client, &[r#".api_key = "unclosed"#.to_string()]);
    let after = serde_json::to_value(&client).expect("serialize");
    assert!(result.is_err());
    assert_eq!(before, after);
}

#[test]
fn handoff_tool_declarations_are_package_aware_and_valid() {
    let fixture_agents = [
        "pantheon/atlas".to_string(),
        "otherpkg/helper".to_string(),
        "global".to_string(),
    ];

    let declarations = fixture_agents
        .iter()
        .map(|agent_name| {
            let display_name =
                harnx_core::package_namespace::handoff_display_name(agent_name, Some("pantheon"));
            (
                format!("{display_name}_session_handoff"),
                display_name,
                agent_name.clone(),
            )
        })
        .collect::<Vec<_>>();
    let declaration_names: std::collections::HashSet<String> = declarations
        .iter()
        .map(|(name, _, _)| name.clone())
        .collect();
    let handoff_targets: std::collections::HashMap<String, String> = declarations
        .iter()
        .map(|(_, display_name, agent_name)| (display_name.clone(), agent_name.clone()))
        .collect();

    assert!(declaration_names.iter().all(|name| !name.contains('/')));
    assert!(declaration_names.iter().all(|name| name
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')));

    assert!(declaration_names.contains("atlas_session_handoff"));
    assert!(declaration_names.contains("otherpkg__helper_session_handoff"));
    assert!(declaration_names.contains("__global_session_handoff"));

    assert_eq!(
        handoff_targets.get("atlas").map(String::as_str),
        Some("pantheon/atlas")
    );
    assert_eq!(
        handoff_targets.get("otherpkg__helper").map(String::as_str),
        Some("otherpkg/helper")
    );
    assert_eq!(
        handoff_targets.get("__global").map(String::as_str),
        Some("global")
    );
}

#[test]
fn session_history_tool_declaration_is_gated_by_use_tools() {
    let config = Config::default();
    let history_name = crate::session_history::TOOL_NAME;

    let selected = config
        .tool_declarations_for_use_tools(Some(history_name), None)
        .0;
    assert!(
        selected.iter().any(|d| d.name == history_name),
        "explicitly selecting the tool should include its declaration"
    );

    let unrelated = config
        .tool_declarations_for_use_tools(Some("some_unrelated_tool"), None)
        .0;
    assert!(
        !unrelated.iter().any(|d| d.name == history_name),
        "an unrelated selector must not include the session-history declaration"
    );

    let wildcard = config.tool_declarations_for_use_tools(Some("*"), None).0;
    assert!(
        wildcard.iter().any(|d| d.name == history_name),
        "a wildcard selector should include the session-history declaration"
    );
}

#[test]
fn dynamic_provider_model_init_sets_client_name_from_provider() {
    struct ProviderGuard(Option<std::ffi::OsString>);
    impl Drop for ProviderGuard {
        fn drop(&mut self) {
            match self.0.take() {
                Some(value) => unsafe { std::env::set_var("HARNX_PROVIDER", value) },
                None => unsafe { std::env::remove_var("HARNX_PROVIDER") },
            }
        }
    }

    #[cfg(unix)]
    let _lock = env_lock();
    let tmp = tempfile::tempdir().unwrap();
    let _config_dir = EnvGuard::new("HARNX_CONFIG_DIR", tmp.path());
    let _provider_guard = ProviderGuard(std::env::var_os("HARNX_PROVIDER"));
    unsafe { std::env::set_var("HARNX_PROVIDER", "claude:some-model") };

    let config = tokio_test::block_on(Config::init(WorkingMode::Cmd, false, vec![]))
        .expect("dynamic config should load");

    assert_eq!(config.clients.len(), 1);
    assert_eq!(config.clients[0].effective_name(), "claude");
}

// ── Regression tests for #826: package agent delegation/MCP tools must be
//    scoped to the active agent's package ───────────────────────────────────

/// A package-loaded server identity: the bare yaml stem plus the package it
/// belongs to. Mirrors what `load_package_servers` records on disk.
struct PackageServer<'a> {
    stem: &'a str,
    package: &'a str,
}

impl<'a> PackageServer<'a> {
    fn new(stem: &'a str, package: &'a str) -> Self {
        Self { stem, package }
    }

    /// Build an ACP server config for this package server.
    fn into_acp(self) -> AcpServerConfig {
        AcpServerConfig {
            name: self.stem.to_string(),
            command: "echo".to_string(),
            args: vec![],
            env: HashMap::new(),
            enabled: true,
            description: None,
            idle_timeout_secs: 300,
            operation_timeout_secs: 3600,
            package: Some(self.package.to_string()),
        }
    }

    /// Build an MCP server config for this package server.
    ///
    /// Built inline (rather than via the `#[cfg(unix)]` `make_test_mcp_server`
    /// helper) so these regression tests compile on all platforms.
    fn into_mcp(self) -> McpServerConfig {
        McpServerConfig {
            name: self.stem.to_string(),
            command: "echo".to_string(),
            args: vec![],
            env: HashMap::new(),
            roots: vec![],
            enabled: true,
            description: None,
            rename_tools: HashMap::new(),
            tool_templates: HashMap::new(),
            hooks: None,
            package: Some(self.package.to_string()),
        }
    }
}

/// #826 regression: when the active agent belongs to package `pantheon`, its
/// own package ACP server (e.g. `pantheon/atlas`) must surface its delegation
/// tool under the BARE name `atlas_session_prompt` — the name the agent's
/// `use_tools` allow-list references. If the managers are left in the global
/// (`None`) scope, the tool is emitted as `pantheon__atlas_session_prompt`,
/// which does not match the allow-list and is filtered out, so the agent has
/// NO delegation tools (the observed bug).
#[test]
fn package_agent_acp_delegation_tool_uses_bare_name_when_scoped() {
    let mut config = Config {
        acp_servers: vec![PackageServer::new("atlas", "pantheon").into_acp()],
        ..Config::default()
    };

    // Scope managers to the active agent's package, as the agent-activation
    // path must do before tools are read.
    config.reinit_managers_for_agent(Some("pantheon"));

    let manager = config
        .acp_manager
        .as_ref()
        .expect("acp_manager should be initialized for a package ACP server");
    let names: Vec<String> = manager
        .get_all_tools_blocking()
        .into_iter()
        .map(|t| t.name)
        .collect();

    assert!(
        names.contains(&"atlas_session_prompt".to_string()),
        "same-package ACP server must surface bare `atlas_session_prompt`; got {names:?}"
    );
    assert!(
        !names.iter().any(|n| n.starts_with("pantheon__")),
        "same-package ACP tools must NOT be prefixed with `pantheon__`; got {names:?}"
    );
}

/// #826 regression: documents the BROKEN scope. When the managers are left in
/// the global (`None`) scope — as the async `use_agent` path does, because it
/// never calls `reinit_managers_for_agent` — a `pantheon` package ACP server is
/// emitted under the prefixed name `pantheon__atlas_session_prompt`. This is the
/// mismatch that makes delegation tools "disappear" for a package agent. This
/// test pins the contrast so a future change that fixes the async path (by
/// scoping the managers) is matched by the scoped-name assertion above.
#[test]
fn unscoped_managers_emit_prefixed_acp_tool_name() {
    let mut config = Config {
        acp_servers: vec![PackageServer::new("atlas", "pantheon").into_acp()],
        ..Config::default()
    };

    // Global scope — exactly the state left by `Config::init` /
    // `init_mcp_manager()` before any package-scoped reinit.
    config.reinit_managers_for_agent(None);

    let manager = config.acp_manager.as_ref().expect("acp_manager");
    let names: Vec<String> = manager
        .get_all_tools_blocking()
        .into_iter()
        .map(|t| t.name)
        .collect();

    assert!(
        names.contains(&"pantheon__atlas_session_prompt".to_string()),
        "unscoped managers prefix the package ACP tool; got {names:?}"
    );
    assert!(
        !names.contains(&"atlas_session_prompt".to_string()),
        "unscoped managers must NOT emit the bare name; got {names:?}"
    );
}

/// #826 regression (MCP side): when scoped to the active agent's package,
/// same-package MCP servers use bare names while OTHER packages stay prefixed.
/// In the broken (`None`) scope every package server is prefixed, so the agent
/// sees both `pantheon__*` AND `coding__*` namespaced tools instead of its bare
/// same-package tools — the "extra tools that shouldn't be available" symptom.
#[test]
fn package_agent_mcp_tools_scoped_to_active_package() {
    let mut config = Config {
        mcp_servers: vec![
            PackageServer::new("fs", "pantheon").into_mcp(),
            PackageServer::new("db", "coding").into_mcp(),
        ],
        mcp_root: vec![],
        ..Config::default()
    };

    // Active agent is in `pantheon`.
    config.reinit_managers_for_agent(Some("pantheon"));
    let manager = config.mcp_manager.as_ref().expect("mcp_manager");
    let servers = manager.list_servers();

    assert!(
        servers.contains(&"fs".to_string()),
        "same-package MCP server must use its bare name `fs`; got {servers:?}"
    );
    assert!(
        servers.contains(&"coding__db".to_string()),
        "other-package MCP server stays prefixed as `coding__db`; got {servers:?}"
    );
    assert!(
        !servers.contains(&"pantheon__fs".to_string()),
        "same-package MCP server must NOT be prefixed `pantheon__fs`; got {servers:?}"
    );
}

/// #826 end-to-end regression: activating a package agent through the ASYNC
/// `Config::use_agent` path (used by `--agent`, handoff `switch_agent`, and the
/// ACP server) must scope the MCP/ACP managers to the agent's package, so the
/// agent's own auto-registered ACP delegation tool is surfaced under the BARE
/// name `atlas_session_prompt` (matching its `use_tools` allow-list) — not the
/// `pantheon__atlas_session_prompt` form that gets filtered out, leaving the
/// agent unable to delegate.
///
/// Before the fix, `use_agent` never called `reinit_managers_for_agent`, so the
/// managers stayed in the global (`None`) scope left by `Config::init` and the
/// tool was emitted prefixed.
#[tokio::test]
async fn use_agent_scopes_managers_to_package_for_delegation_tools() {
    use crate::client::TestStateGuard;
    use harnx_core::abort::create_abort_signal;

    // Serialize on the shared test state lock (guards HARNX_* env vars).
    let _guard = TestStateGuard::new(None).await;

    let temp = tempfile::TempDir::new().unwrap();
    // Package layout: packages/pantheon/agents/{atlas,hermes}.md
    let pkg_agents = temp.path().join("packages/pantheon/agents");
    std::fs::create_dir_all(&pkg_agents).unwrap();
    std::fs::write(
        pkg_agents.join("atlas.md"),
        "---\nuse_tools: hermes_session_prompt\n---\nYou are Atlas. Delegate to hermes.\n",
    )
    .unwrap();
    std::fs::write(pkg_agents.join("hermes.md"), "---\n---\nYou are Hermes.\n").unwrap();

    let _env = EnvGuard::new("HARNX_CONFIG_DIR", temp.path());
    let provider_prev = std::env::var_os("HARNX_PROVIDER");
    // SAFETY: test-only; the shared test lock is held for the duration.
    unsafe { std::env::set_var("HARNX_PROVIDER", "claude:some-model") };

    let config = Config::init(WorkingMode::Cmd, false, vec![])
        .await
        .expect("config init");
    let config = std::sync::Arc::new(parking_lot::RwLock::new(config));

    // Sanity: the package agent was auto-registered as an ACP server, and at
    // this point (no agent active) the managers are in the global scope so the
    // tool name is prefixed.
    {
        let cfg = config.read();
        let manager = cfg.acp_manager.as_ref().expect("acp_manager after init");
        let names: Vec<String> = manager
            .get_all_tools_blocking()
            .into_iter()
            .map(|t| t.name)
            .collect();
        assert!(
            names.contains(&"pantheon__atlas_session_prompt".to_string()),
            "pre-activation (global scope) should expose the prefixed tool; got {names:?}"
        );
    }

    // Activate the package agent through the async path.
    Config::use_agent(&config, "pantheon/atlas", None, create_abort_signal())
        .await
        .expect("use_agent should activate the package agent");

    // After activation the managers must be scoped to `pantheon`, so the
    // sibling `hermes` delegation tool is surfaced under its bare name.
    let cfg = config.read();
    let manager = cfg
        .acp_manager
        .as_ref()
        .expect("acp_manager should remain initialized after use_agent");
    let names: Vec<String> = manager
        .get_all_tools_blocking()
        .into_iter()
        .map(|t| t.name)
        .collect();

    // SAFETY: test-only; restore provider env.
    unsafe {
        match &provider_prev {
            Some(v) => std::env::set_var("HARNX_PROVIDER", v),
            None => std::env::remove_var("HARNX_PROVIDER"),
        }
    }

    assert!(
        names.contains(&"hermes_session_prompt".to_string()),
        "after use_agent, same-package delegation tool must be bare `hermes_session_prompt`; got {names:?}"
    );
    assert!(
        !names.iter().any(|n| n.starts_with("pantheon__")),
        "after use_agent, same-package ACP tools must NOT be `pantheon__`-prefixed; got {names:?}"
    );
}

#[tokio::test]
async fn use_agent_routes_remote_refs_to_nats_cluster_validation() {
    use crate::client::TestStateGuard;
    use harnx_core::{abort::create_abort_signal, working_mode::WorkingMode};

    let _guard = TestStateGuard::new(None).await;

    let temp = tempfile::TempDir::new().unwrap();
    let _env = EnvGuard::new("HARNX_CONFIG_DIR", temp.path());
    let provider_prev = std::env::var_os("HARNX_PROVIDER");
    // SAFETY: test-only; shared test lock held for duration.
    unsafe { std::env::set_var("HARNX_PROVIDER", "claude:some-model") };

    let config = Config::init(WorkingMode::Cmd, false, vec![])
        .await
        .expect("config init");
    let config = std::sync::Arc::new(parking_lot::RwLock::new(config));

    let err = Config::use_agent(&config, "atlas@prod", None, create_abort_signal())
        .await
        .expect_err("remote ref with an unknown cluster must fail cluster validation");

    // SAFETY: test-only; restore provider env.
    unsafe {
        match &provider_prev {
            Some(v) => std::env::set_var("HARNX_PROVIDER", v),
            None => std::env::remove_var("HARNX_PROVIDER"),
        }
    }

    // Remote refs are no longer stubbed out — they route into NATS thin-client
    // mode, which first validates the cluster. With no nats_servers/prod.yaml
    // the activation fails on cluster lookup, proving the remote path is wired.
    let msg = err.to_string();
    assert!(
        msg.contains("prod") && msg.contains("nats_servers/prod.yaml"),
        "expected unknown-cluster validation error, got: {msg}"
    );
    assert!(
        !msg.contains("not yet implemented"),
        "remote refs must no longer be stubbed: {msg}"
    );
}

pub(super) static LOG_CAPTURE_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
