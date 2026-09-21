//! Tests for remote-agent handoff config behavior extracted from tests.rs.
#![cfg(test)]

use super::test_support::{env_lock, EnvGuard};
use super::*;
#[test]
fn remote_handoff_selector_logic_resolves_raw_agent_without_reverse_sanitize() {
    use std::fs;

    let _env_guard = env_lock();
    let temp = tempfile::TempDir::new().unwrap();
    let _config_dir = EnvGuard::new("HARNX_CONFIG_DIR", temp.path());
    fs::create_dir_all(temp.path().join("nats_servers")).unwrap();
    fs::write(
        temp.path().join("nats_servers/local.yaml"),
        "url: nats://localhost:4222\nagents:\n  - name: metis\n",
    )
    .unwrap();

    let (declarations, handoff_targets) = handoff_tool_declarations_for_agents(None);
    assert!(declarations
        .iter()
        .any(|tool| tool.name == "metis__at__local_session_handoff"));
    assert_eq!(
        handoff_targets.get("metis__at__local").map(String::as_str),
        Some("metis@local")
    );

    let bare_target = "metis__at__local_session_handoff"
        .strip_suffix("_session_handoff")
        .unwrap();
    assert_eq!(bare_target, "metis__at__local");
    assert_eq!(
        handoff_targets.get(bare_target).map(String::as_str),
        Some("metis@local")
    );
}

#[test]
fn handoff_tool_declarations_filter_per_agent_and_keep_targets_in_sync() {
    use std::fs;

    let _env_guard = env_lock();
    let temp = tempfile::TempDir::new().unwrap();
    let _config_dir = EnvGuard::new("HARNX_CONFIG_DIR", temp.path());
    fs::create_dir_all(temp.path().join("agents")).unwrap();
    fs::create_dir_all(temp.path().join("nats_servers")).unwrap();
    fs::write(
        temp.path().join("nats_servers/local.yaml"),
        "url: nats://localhost:4222\nagents:\n  - name: metis\n  - name: atlas\n",
    )
    .unwrap();

    let config = Config::default();

    let (selected_declarations, selected_targets) =
        config.tool_declarations_for_use_tools(Some("metis__at__local_session_handoff"), None);
    let selected_handoff_names: Vec<String> = selected_declarations
        .into_iter()
        .map(|d| d.name)
        .filter(|name| name.ends_with("_session_handoff"))
        .collect();
    assert_eq!(
        selected_handoff_names,
        vec!["metis__at__local_session_handoff".to_string()]
    );
    assert_eq!(selected_targets.len(), 1);
    assert_eq!(
        selected_targets.get("metis__at__local").map(String::as_str),
        Some("metis@local")
    );
    assert!(
        !selected_targets.contains_key("atlas__at__local"),
        "handoff target map must only retain selected agents: {selected_targets:?}"
    );

    let (wildcard_declarations, wildcard_targets) =
        config.tool_declarations_for_use_tools(Some("*"), None);
    let wildcard_handoff_names: Vec<String> = wildcard_declarations
        .into_iter()
        .map(|d| d.name)
        .filter(|name| name.ends_with("_session_handoff"))
        .collect();
    assert!(
        wildcard_handoff_names.contains(&"metis__at__local_session_handoff".to_string()),
        "wildcard should keep metis handoff: {wildcard_handoff_names:?}"
    );
    assert!(
        wildcard_handoff_names.contains(&"atlas__at__local_session_handoff".to_string()),
        "wildcard should keep atlas handoff: {wildcard_handoff_names:?}"
    );
    assert_eq!(
        wildcard_targets.get("metis__at__local").map(String::as_str),
        Some("metis@local")
    );
    assert_eq!(
        wildcard_targets.get("atlas__at__local").map(String::as_str),
        Some("atlas@local")
    );
}

#[test]
fn handoff_tool_declarations_append_catalog_description_for_local_and_remote_agents() {
    use std::fs;

    let _env_guard = env_lock();
    let temp = tempfile::TempDir::new().unwrap();
    let _config_dir = EnvGuard::new("HARNX_CONFIG_DIR", temp.path());
    fs::create_dir_all(temp.path().join("agents")).unwrap();
    fs::create_dir_all(temp.path().join("nats_servers")).unwrap();
    fs::write(
        temp.path().join("agents/local-helper.md"),
        "---\ndescription: Local helper description\n---\nPrompt\n",
    )
    .unwrap();
    fs::write(
        temp.path().join("agents/no-description.md"),
        "---\ndescription: \"\"\n---\nPrompt\n",
    )
    .unwrap();
    fs::write(
        temp.path().join("nats_servers/local.yaml"),
        concat!(
            "url: nats://localhost:4222\n",
            "agents:\n",
            "  - name: metis\n",
            "    description: Handles heavy planning\n",
            "  - name: atlas\n",
            "    description: \"\"\n"
        ),
    )
    .unwrap();

    let (declarations, _) = handoff_tool_declarations_for_agents(None);
    let descriptions: std::collections::HashMap<&str, &str> = declarations
        .iter()
        .map(|tool| (tool.name.as_str(), tool.description.as_str()))
        .collect();

    assert!(
        descriptions["local-helper_session_handoff"].contains("Local helper description"),
        "local handoff tool should include local description: {:?}",
        descriptions["local-helper_session_handoff"]
    );
    assert!(
        descriptions["metis__at__local_session_handoff"].contains("Handles heavy planning"),
        "remote handoff tool should include remote description: {:?}",
        descriptions["metis__at__local_session_handoff"]
    );
    assert_eq!(
        descriptions["no-description_session_handoff"],
        "Finish the current agent session and hand off to the 'no-description' agent. Omit `session_id` (or pass an empty value) to create a generated target session. Pass an unused ID to create that exact target session, or the exact ID of an existing session owned by this target to continue its transcript. Do not invent a session ID when you want a generated session. Include enough context in `prompt` for a new session."
    );
    assert_eq!(
        descriptions["atlas__at__local_session_handoff"],
        "Finish the current agent session and hand off to the 'atlas@local' agent. Omit `session_id` (or pass an empty value) to create a generated target session. Pass an unused ID to create that exact target session, or the exact ID of an existing session owned by this target to continue its transcript. Do not invent a session ID when you want a generated session. Include enough context in `prompt` for a new session."
    );
}

#[test]
fn agent_switches_drop_the_old_session_and_preserve_new_remote_identity() {
    let mut config = Config::default();
    let mut local = Agent::default();
    local.set_name("alpha");
    config.use_agent_obj(local.clone()).unwrap();
    config.use_session(Some("review-12345")).unwrap();
    let alpha_key = config.session.as_ref().unwrap().storage_key();
    let mut clients: Vec<ClientConfig> = serde_yaml::from_str(
        "- type: openai\n  models:\n  - name: test-embedding\n    type: embedding\n",
    )
    .unwrap();
    clients[0].set_name("openai".into());
    config.rag = Some(
        harnx_rag::Rag::create(
            &clients,
            "alpha-rag",
            std::path::Path::new("unused.yaml"),
            harnx_rag::RagData::new("openai:test-embedding".into(), 256, 0, None, 5, None),
        )
        .unwrap()
        .into(),
    );
    config.set_remote_agent("beta".into(), "remote".into());
    assert!(config.session.is_none());
    assert!(config.agent.is_none());
    assert!(config.rag.is_none());
    config.use_session(Some("review-12345")).unwrap();
    let session = config.session.as_ref().unwrap();
    assert_eq!(session.agent_name(), Some("beta"));
    assert_ne!(session.storage_key(), alpha_key);
    assert_eq!(
        session.storage_key(),
        crate::SessionInitializer::from_config(&config)
            .unwrap()
            .session_key("review-12345")
    );
    config.use_agent_obj(local).unwrap();
    assert!(config.session.is_none());
    assert!(config.remote_agent.is_none());
    config.use_session(Some("review-12345")).unwrap();
    assert_eq!(config.session.as_ref().unwrap().storage_key(), alpha_key);
}

#[tokio::test]
async fn cluster_routing_activates_bare_agent_as_remote() {
    harnx_core::require_nextest();
    let config = Config {
        nats_servers: vec![serde_yaml::from_str(
            "name: remote\nurl: nats://127.0.0.1:65535\nagents:\n  - name: reviewer\n",
        )
        .unwrap()],
        nats_routing: NatsRouting::Cluster("remote".to_string()),
        ..Config::default()
    };
    let config = Arc::new(RwLock::new(config));

    Config::use_agent(
        &config,
        "reviewer",
        None,
        crate::utils::create_abort_signal(),
    )
    .await
    .unwrap();

    let config = config.read();
    assert_eq!(config.default_cluster_key(), "remote");
    assert_eq!(
        config.remote_agent.as_ref(),
        Some(&("reviewer".to_string(), "remote".to_string()))
    );
    assert!(config.agent.is_none());
}
