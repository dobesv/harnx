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

    let mut config = Config::default();
    config.reinit_managers_for_agent(None);

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
        "Exit the current agent session and hand off to the 'no-description' agent, which starts fresh. Prior conversation history is not carried over — it is intentionally cleared on handoff. Only the `prompt` argument provides context to the target agent, so include everything it needs there."
    );
    assert_eq!(
        descriptions["atlas__at__local_session_handoff"],
        "Exit the current agent session and hand off to the 'atlas@local' agent, which starts fresh. Prior conversation history is not carried over — it is intentionally cleared on handoff. Only the `prompt` argument provides context to the target agent, so include everything it needs there."
    );
}
