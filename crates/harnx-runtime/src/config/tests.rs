//! Tests for the config module (extracted from mod.rs for code health).
#![cfg(test)]

use super::test_support::env_lock;
use super::test_support::env_lock_async;
use super::test_support::EnvGuard;
use super::*;
use harnx_core::message::MessageRole;

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

    let session = super::session::new(&config, "my-session", None).unwrap();
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
    let session3 = super::session::new(&config3, "session3", None).unwrap();
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
    let session_no_agent = super::session::new(&config2, "my-session2", None).unwrap();
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

// ── handoff session emptying tests ─────────────────────────────────────

/// Verify that empty_session clears messages from a session that was loaded
/// with an existing name (simulating the handoff path with session_id).
/// This is the unit-level guarantee behind the #291 fix: after handoff the
/// new agent starts with a blank session even when a session_id was provided.
#[test]
fn test_new_session_has_session_id() {
    let config = Config::default();
    let session = self::session::new(&config, "metadata-check", None).unwrap();

    assert!(session.session_id.is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_new_session_has_short_id() {
    let config = Arc::new(RwLock::new(Config::default()));
    let session_id = Config::reserve_new_session_id(&config).await.unwrap();
    config.write().use_session(Some(&session_id)).unwrap();

    let guard = config.read();
    let session = guard.session.as_ref().unwrap();
    assert_eq!(
        session.id.len(),
        6,
        "anonymous session ID should be 6-char short ID"
    );
    assert!(
        crate::utils::session_name::decode_timestamp_session_id(&session.id).is_some(),
        "anonymous session ID should be a valid base64url timestamp short ID"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_anonymous_session_id_collision_retries() {
    let config = Arc::new(RwLock::new(Config::default()));
    let id1 = Config::reserve_new_session_id(&config).await.unwrap();
    let id2 = Config::reserve_new_session_id(&config).await.unwrap();
    assert_ne!(
        id1, id2,
        "concurrent anonymous sessions must get unique IDs"
    );
    assert_eq!(id1.len(), 6);
    assert_eq!(id2.len(), 6);
}

#[test]
fn empty_session_after_persisted_clear_clears_named_session_with_messages() {
    let mut config = Config::default();
    let mut session = self::session::new(&config, "handoff-target", None).unwrap();
    session.push_message_for_test(MessageRole::System, "You are agent A.".to_string());
    session.push_message_for_test(MessageRole::User, "Hello from old session".to_string());
    session.push_message_for_test(MessageRole::Assistant, "Response from agent A".to_string());
    assert!(!session.is_empty());
    config.session = Some(session);

    config.empty_session_after_persisted_clear().unwrap();

    let session = config.session.as_ref().unwrap();
    assert!(
        session.is_empty(),
        "session should be empty after empty_session"
    );
}

#[test]
fn empty_session_keeps_messages_when_clear_cannot_be_persisted() {
    let mut config = Config::default();
    let mut session = self::session::new(&config, "handoff-target", None).unwrap();
    session.push_message_for_test(MessageRole::User, "keep me".to_string());
    config.session = Some(session);

    assert!(config.empty_session().is_err());
    assert!(!config.session.as_ref().unwrap().is_empty());
}

// ── after_chat_completion incremental persistence tests ─────────────────

/// Verify that after_chat_completion persists intermediate rounds
/// (non-empty tool_results) to the session, not just the final round.
#[test]
fn after_chat_completion_saves_intermediate_tool_rounds() {
    use crate::tool::{ToolCall, ToolResult};
    use serde_json::json;

    let _tmp = tempfile::TempDir::new().unwrap();
    let mut config = Config {
        data: ConfigData {
            stream: false,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut session = self::session::new(&config, "test-intermediate", None).unwrap();
    self::session::attach_memory_log(&mut session);
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

/// Regression test for the non-interactive failure where `use_agent_by_name`
/// followed by `use_session` bailed with "agent variables are required"
/// for an agent whose variables use `path:` (file-backed defaults).  The
/// async `agent::init` resolves these defaults, but the synchronous
/// `retrieve_agent` does not — `use_agent_by_name` must do so itself,
/// otherwise `init_agent_session_variables` (called from `use_session`)
/// finds no defaults and bails in non-interactive contexts.
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
    let _env_lock = env_lock_async().await;
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

#[test]
fn select_tools_merges_cached_nats_declarations() {
    use harnx_core::agent_config::AgentConfig;

    let config = Config::default();
    config
        .nats_tool_declarations
        .write()
        .push(make_tool_decl("fs_read"));
    let mut agent = AgentConfig::from_prompt("test");
    agent.set_use_tools(Some(vec!["fs_read".to_string()]));

    let declarations = config
        .select_tools(&agent)
        .expect("cached NATS tool selected");
    assert_eq!(
        declarations
            .iter()
            .map(|declaration| declaration.name.as_str())
            .collect::<Vec<_>>(),
        vec!["fs_read"]
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
fn selector_could_match_server_sanitizes_remote_ref_selector_forward() {
    for selector in ["metis@local", "metis__at__local", "metis__at__local_*"] {
        assert!(
            selector_could_match_server(selector, "metis__at__local"),
            "selector should match forward-sanitized remote server: {selector}"
        );
    }
    assert!(!selector_could_match_server(
        "atlas@local",
        "metis__at__local"
    ));
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
    let _lock = env_lock();
    let tmp = tempfile::tempdir().unwrap();
    let _config_dir = EnvGuard::new("HARNX_CONFIG_DIR", tmp.path());
    let _provider = EnvGuard::new("HARNX_PROVIDER", "claude:some-model");

    let config = tokio_test::block_on(Config::init(WorkingMode::Cmd, false))
        .expect("dynamic config should load");

    assert_eq!(config.clients.len(), 1);
    assert_eq!(config.clients[0].effective_name(), "claude");
}
#[tokio::test]
async fn use_agent_routes_remote_refs_to_nats_cluster_validation() {
    use crate::client::TestStateGuard;
    use harnx_core::{abort::create_abort_signal, working_mode::WorkingMode};

    let _guard = TestStateGuard::new(None).await;
    let _env_lock = env_lock_async().await;

    let temp = tempfile::TempDir::new().unwrap();
    let _env = EnvGuard::new("HARNX_CONFIG_DIR", temp.path());
    let _provider = EnvGuard::new("HARNX_PROVIDER", "claude:some-model");

    let config = Config::init(WorkingMode::Cmd, false)
        .await
        .expect("config init");
    let config = std::sync::Arc::new(parking_lot::RwLock::new(config));

    let err = Config::use_agent(&config, "atlas@prod", None, create_abort_signal())
        .await
        .expect_err("remote ref with an unknown cluster must fail cluster validation");

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
