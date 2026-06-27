//! Tests for remote-agent ACP/handoff config behavior extracted from tests.rs.
#![cfg(test)]

use super::test_support::{env_lock, EnvGuard, PackageServer};
use super::*;

#[test]
fn acp_tool_declarations_respect_use_tools_selector_whitelist() {
    let mut config = Config {
        acp_servers: vec![PackageServer::new("metis", "local").into_acp()],
        ..Config::default()
    };

    // Global scope keeps package-prefixed ACP tool names, matching the current
    // auto-registered package agent shape in issue #913.
    config.reinit_managers_for_agent(None);

    let declarations = config
        .tool_declarations_for_use_tools(Some("some_unrelated_tool"), None)
        .0;
    let leaked: Vec<String> = declarations
        .iter()
        .map(|d| d.name.clone())
        .filter(|name| name.starts_with("local__metis_session_"))
        .collect();

    assert!(
        leaked.is_empty(),
        "unselected ACP agent tools must not leak into use_tools-filtered declarations; got {leaked:?}"
    );
}

#[test]
fn acp_tool_declarations_keep_selected_agent_and_wildcard_tools() {
    let mut config = Config {
        acp_servers: vec![PackageServer::new("metis", "local").into_acp()],
        ..Config::default()
    };

    config.reinit_managers_for_agent(None);

    let selected_names: Vec<String> = config
        .tool_declarations_for_use_tools(Some("local__metis_session_*"), None)
        .0
        .into_iter()
        .map(|d| d.name)
        .filter(|name| name.starts_with("local__metis_session_"))
        .collect();
    assert_eq!(
        selected_names,
        vec![
            "local__metis_session_cancel".to_string(),
            "local__metis_session_load".to_string(),
            "local__metis_session_new".to_string(),
            "local__metis_session_prompt".to_string(),
        ]
    );

    let wildcard_names: Vec<String> = config
        .tool_declarations_for_use_tools(Some("*"), None)
        .0
        .into_iter()
        .map(|d| d.name)
        .filter(|name| name.starts_with("local__metis_session_"))
        .collect();
    assert_eq!(
        wildcard_names,
        vec![
            "local__metis_session_cancel".to_string(),
            "local__metis_session_load".to_string(),
            "local__metis_session_new".to_string(),
            "local__metis_session_prompt".to_string(),
        ]
    );
}

#[test]
fn auto_register_agents_populates_catalog_descriptions() {
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

    let config = Config::load_from_file(&temp.path().join("config.yaml")).unwrap();

    let server_description = |name: &str| {
        config
            .acp_servers
            .iter()
            .find(|server| server.name == name)
            .and_then(|server| server.description.clone())
    };

    assert_eq!(
        server_description("metis@local").as_deref(),
        Some("Handles heavy planning")
    );
    assert_eq!(
        server_description("local-helper").as_deref(),
        Some("Local helper description")
    );
    assert_eq!(server_description("atlas@local"), None);
    assert_eq!(server_description("no-description"), None);
}

#[test]
fn auto_register_agents_preserves_raw_remote_spawn_args() {
    use std::fs;

    let _env_guard = env_lock();
    let temp = tempfile::TempDir::new().unwrap();
    let _config_dir = EnvGuard::new("HARNX_CONFIG_DIR", temp.path());
    fs::create_dir_all(temp.path().join("nats_servers")).unwrap();
    fs::write(
        temp.path().join("nats_servers/local.yaml"),
        concat!(
            "url: nats://localhost:4222\n",
            "agents:\n",
            "  - name: metis\n",
            "    description: Handles heavy planning\n"
        ),
    )
    .unwrap();

    let config = Config::load_from_file(&temp.path().join("config.yaml")).unwrap();
    let remote_server = config
        .acp_servers
        .iter()
        .find(|server| server.name == "metis@local")
        .expect("auto-registered remote ACP server");

    assert_eq!(remote_server.args, vec!["metis@local"]);
}

fn metis_remote_acp_names(config: &Config) -> Vec<String> {
    let manager = config
        .acp_manager
        .as_ref()
        .expect("acp_manager should be initialized for auto-registered remote ACP server");
    let mut manager_names: Vec<String> = manager
        .get_all_tools_blocking()
        .into_iter()
        .filter(|d| d.name.starts_with("metis__at__local_session_"))
        .map(|d| d.name)
        .collect();
    manager_names.sort();
    manager_names
}

fn metis_remote_tool_names(config: &Config, selector: &str) -> Vec<String> {
    let mut names: Vec<String> = config
        .tool_declarations_for_use_tools(Some(selector), None)
        .0
        .into_iter()
        .map(|d| d.name)
        .filter(|name| name.starts_with("metis__at__local_session_"))
        .collect();
    names.sort();
    names
}

fn metis_remote_full_family() -> Vec<String> {
    vec![
        "metis__at__local_session_cancel".to_string(),
        "metis__at__local_session_handoff".to_string(),
        "metis__at__local_session_load".to_string(),
        "metis__at__local_session_new".to_string(),
        "metis__at__local_session_prompt".to_string(),
    ]
}

fn metis_remote_acp_family() -> Vec<String> {
    vec![
        "metis__at__local_session_cancel".to_string(),
        "metis__at__local_session_load".to_string(),
        "metis__at__local_session_new".to_string(),
        "metis__at__local_session_prompt".to_string(),
    ]
}

struct MetisRemoteConfigFixture {
    temp: tempfile::TempDir,
    config_dir: EnvGuard,
    config: Config,
}

fn load_metis_remote_config() -> MetisRemoteConfigFixture {
    use std::fs;

    let temp = tempfile::TempDir::new().unwrap();
    let config_dir = EnvGuard::new("HARNX_CONFIG_DIR", temp.path());
    fs::create_dir_all(temp.path().join("nats_servers")).unwrap();
    fs::write(
        temp.path().join("nats_servers/local.yaml"),
        concat!(
            "url: nats://localhost:4222\n",
            "agents:\n",
            "  - name: metis\n",
            "    description: Handles heavy planning\n"
        ),
    )
    .unwrap();

    let mut config = Config::load_from_file(&temp.path().join("config.yaml")).unwrap();
    config.reinit_managers_for_agent(None);
    MetisRemoteConfigFixture {
        temp,
        config_dir,
        config,
    }
}

#[test]
fn remote_selector_bare_ref_exposes_full_family() {
    let _env_guard = env_lock();
    let fixture = load_metis_remote_config();
    let _keep_temp_alive = &fixture.temp;
    let _keep_config_dir_alive = &fixture.config_dir;
    let config = &fixture.config;

    assert_eq!(
        metis_remote_acp_names(config),
        metis_remote_acp_family(),
        "auto-registered remote ACP server must contribute full delegation family"
    );
    assert_eq!(
        metis_remote_tool_names(config, "metis@local"),
        metis_remote_full_family(),
        "bare remote selector should keep full sanitized family"
    );
}

#[test]
fn remote_selector_sanitized_ref_exposes_full_family() {
    let _env_guard = env_lock();
    let fixture = load_metis_remote_config();
    let _keep_temp_alive = &fixture.temp;
    let _keep_config_dir_alive = &fixture.config_dir;
    let config = &fixture.config;

    assert_eq!(
        metis_remote_tool_names(config, "metis__at__local"),
        metis_remote_full_family(),
        "sanitized remote selector should keep full sanitized family"
    );
}

#[test]
fn remote_selector_wildcard_exposes_full_family() {
    let _env_guard = env_lock();
    let fixture = load_metis_remote_config();
    let _keep_temp_alive = &fixture.temp;
    let _keep_config_dir_alive = &fixture.config_dir;
    let config = &fixture.config;

    assert_eq!(
        metis_remote_tool_names(config, "*"),
        metis_remote_full_family()
    );
}

#[test]
fn remote_selector_specific_tool_stays_narrow() {
    let _env_guard = env_lock();
    let fixture = load_metis_remote_config();
    let _keep_temp_alive = &fixture.temp;
    let _keep_config_dir_alive = &fixture.config_dir;
    let config = &fixture.config;

    let specific_tool_names = metis_remote_tool_names(config, "metis__at__local_session_prompt");
    let remote_acp_names: Vec<String> = specific_tool_names
        .iter()
        .filter(|name| !name.ends_with("_handoff"))
        .cloned()
        .collect();
    assert_eq!(
        remote_acp_names,
        vec!["metis__at__local_session_prompt".to_string()],
        "specific ACP tool selector must not broaden to ACP remote family"
    );
}

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
