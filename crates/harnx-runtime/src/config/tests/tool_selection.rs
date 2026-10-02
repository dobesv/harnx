use crate::config::test_support::{env_lock, EnvGuard};
use crate::config::*;

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
        kind: None,
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

// NatsToolProvider discovery caches names relative to the requested package.
fn config_with_package_tool_view() -> Config {
    let config = Config::default();
    config.nats_tool_declarations.write().extend([
        make_tool_decl("fs_read"),
        make_tool_decl("fs_write"),
        make_tool_decl("otherpkg__fs_read"),
        make_tool_decl("otherpkg__db_query"),
    ]);
    config
}

fn selected_names_for_package(
    config: &Config,
    selectors: &[&str],
    package: Option<&str>,
) -> Vec<String> {
    let selectors = selectors.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    let mut names = config
        .select_tools_for_package(&selectors, package)
        .into_iter()
        .map(|d| d.name)
        .collect::<Vec<_>>();
    names.sort();
    names
}

#[test]
fn select_tools_for_package_selects_same_package_bare_names() {
    let config = config_with_package_tool_view();
    assert!(config.agent.is_none());
    assert_eq!(
        selected_names_for_package(&config, &["fs_read"], Some("pkg")),
        vec!["fs_read"]
    );
}

#[test]
fn select_tools_for_package_selects_cross_package_qualified_names() {
    let config = config_with_package_tool_view();
    assert_eq!(
        selected_names_for_package(&config, &["otherpkg__fs_read"], Some("pkg")),
        vec!["otherpkg__fs_read"]
    );
    assert_eq!(
        selected_names_for_package(&config, &["otherpkg__fs_read"], None),
        vec!["otherpkg__fs_read"]
    );
}

#[test]
fn select_tools_for_package_expands_toolset_aliases() {
    let mut config = config_with_package_tool_view();
    config.toolsets.insert(
        "file_tools".to_string(),
        vec![
            "fs_read".to_string(),
            "fs_write".to_string(),
            "otherpkg__fs_read".to_string(),
            "missing_tool".to_string(),
        ],
    );
    assert_eq!(
        selected_names_for_package(&config, &["file_tools", "fs_read"], Some("pkg")),
        vec!["fs_read", "fs_write", "otherpkg__fs_read"]
    );
}

#[test]
fn select_tools_for_package_expands_globs() {
    let config = config_with_package_tool_view();
    assert_eq!(
        selected_names_for_package(&config, &["fs_*", "otherpkg__fs_*"], Some("pkg")),
        vec!["fs_read", "fs_write", "otherpkg__fs_read"]
    );
}

#[test]
fn select_tools_for_package_returns_empty_for_zero_matches() {
    let config = config_with_package_tool_view();
    for selectors in [&["missing_*"][..], &["pkg__fs_read"][..], &[][..]] {
        assert!(selected_names_for_package(&config, selectors, Some("pkg")).is_empty());
    }
    let mut agent = harnx_core::agent_config::AgentConfig::from_prompt("test");
    agent.set_use_tools(Some(vec!["missing_*".to_string()]));
    assert!(config.select_tools(&agent).is_none());
}

#[test]
fn select_tools_for_package_returns_empty_when_tool_use_disabled() {
    let mut config = config_with_package_tool_view();
    config.tool_use = false;
    assert!(selected_names_for_package(&config, &["fs_read"], Some("pkg")).is_empty());
    let mut agent = harnx_core::agent_config::AgentConfig::from_prompt("test");
    agent.set_use_tools(Some(vec!["fs_read".to_string()]));
    assert!(config.select_tools(&agent).is_none());
}

#[test]
fn select_tools_delegates_with_agent_package_for_handoffs() {
    let _lock = env_lock();
    let temp = tempfile::tempdir().unwrap();
    let agents_dir = temp.path().join("packages/pkg/agents");
    std::fs::create_dir_all(&agents_dir).unwrap();
    std::fs::write(agents_dir.join("helper.md"), "You are a helper.").unwrap();
    let _config_dir = EnvGuard::new("HARNX_CONFIG_DIR", temp.path());
    let config = Config::default();

    for (agent_name, package, tool_name) in [
        ("pkg/caller", Some("pkg"), "helper_session_handoff"),
        (
            "otherpkg/caller",
            Some("otherpkg"),
            "pkg__helper_session_handoff",
        ),
        ("caller", None, "pkg__helper_session_handoff"),
    ] {
        let selectors = vec![tool_name.to_string()];
        let explicit = config.select_tools_for_package(&selectors, package);
        assert_eq!(
            explicit.iter().map(|d| d.name.as_str()).collect::<Vec<_>>(),
            vec![tool_name]
        );
        let mut agent = harnx_core::agent_config::AgentConfig::from_prompt("test");
        agent.set_name(agent_name);
        agent.set_use_tools(Some(selectors));
        let delegated = config.select_tools(&agent).expect("handoff tool selected");
        assert_eq!(
            delegated.iter().map(|d| &d.name).collect::<Vec<_>>(),
            explicit.iter().map(|d| &d.name).collect::<Vec<_>>()
        );
    }
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
