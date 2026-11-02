//! Glob `use_tools` selectors resolved through `Config`.

use super::*;
use harnx_core::agent_config::AgentConfig;

fn tool(name: &str) -> ToolDeclaration {
    ToolDeclaration {
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

fn config_with_tools() -> Config {
    Config {
        tools: Tools::init_from_mcp(Some(vec![
            tool("fs_read"),
            tool("fs_write"),
            tool("bash_exec"),
            tool("git_status"),
        ])),
        ..Config::default()
    }
}

fn agent_using_fs_glob_and_bash() -> AgentConfig {
    let mut agent = AgentConfig::from_prompt("test");
    agent.set_use_tools(Some(vec!["fs_*".to_string(), "bash_exec".to_string()]));
    agent
}

#[test]
fn select_tools_expands_glob_selectors() {
    let config = config_with_tools();

    let mut names: Vec<String> = config
        .select_tools(&agent_using_fs_glob_and_bash())
        .unwrap_or_default()
        .into_iter()
        .map(|declaration| declaration.name)
        .collect();
    names.sort();

    assert_eq!(names, vec!["bash_exec", "fs_read", "fs_write"]);
}

#[test]
fn active_tool_names_expands_glob_selectors() {
    let mut config = config_with_tools();
    config.agent = Some(agent::Agent::new(agent_using_fs_glob_and_bash()));

    let mut names: Vec<String> = config.active_tool_names().into_iter().collect();
    names.sort();

    assert_eq!(names, vec!["bash_exec", "fs_read", "fs_write"]);
}

fn agent_with(use_tools: Option<Vec<String>>, bash_tools: Option<Vec<String>>) -> AgentConfig {
    let mut agent = AgentConfig::from_prompt("test");
    agent.set_use_tools(use_tools);
    agent.set_bash_tools(bash_tools);
    agent
}

fn declaration_names(config: &Config, agent: &AgentConfig) -> Vec<String> {
    config
        .select_tools(agent)
        .unwrap_or_default()
        .into_iter()
        .map(|declaration| declaration.name)
        .collect()
}

#[test]
fn bash_tool_names_resolves_glob_and_explicit_selectors() {
    let config = config_with_tools();
    for (selectors, expected) in [
        (
            &["fs_*"][..],
            vec!["fs_read".to_string(), "fs_write".to_string()],
        ),
        (
            &["fs_read", "bash_exec"][..],
            vec!["bash_exec".to_string(), "fs_read".to_string()],
        ),
    ] {
        let agent = agent_with(
            None,
            Some(
                selectors
                    .iter()
                    .map(|selector| selector.to_string())
                    .collect(),
            ),
        );
        assert_eq!(
            config.bash_tool_names(&agent),
            expected,
            "selectors: {selectors:?}"
        );
    }
}

#[test]
fn bash_inner_set_is_independent_of_use_tools() {
    let config = config_with_tools();
    // fs_write is bash-only: absent from use_tools, present in bash_tools.
    let agent = agent_with(
        Some(vec!["fs_read".to_string()]),
        Some(vec!["fs_*".to_string()]),
    );

    assert_eq!(config.bash_tool_names(&agent), vec!["fs_read", "fs_write"]);
    assert_eq!(declaration_names(&config, &agent), vec!["fs_read", "bash"]);
}

#[test]
fn bash_tools_without_use_tools_exposes_only_bash() {
    let config = config_with_tools();
    let agent = agent_with(None, Some(vec!["fs_*".to_string()]));

    assert_eq!(declaration_names(&config, &agent), vec!["bash"]);
}

#[test]
fn session_bash_tools_do_not_bleed_into_the_compaction_summarizer() {
    // Session-only config: `extract_agent` resolves to the unnamed `%%`
    // agent — exactly the name the default compaction summarizer gets from
    // `AgentConfig::from_prompt(DEFAULT_COMPACT_SYSTEM_PROMPT)`. The session
    // override must follow the explicit tool-selection resolution
    // (`tool_selection_agent`), never that name collision.
    let mut config = config_with_tools();
    let mut session = Session::default();
    session.set_bash_tools(Some(vec!["fs_*".to_string()]));
    config.session = Some(session);

    let summarizer =
        AgentConfig::from_prompt(crate::config::compaction::DEFAULT_COMPACT_SYSTEM_PROMPT);
    assert_eq!(
        summarizer.name(),
        config.extract_agent().name(),
        "both resolve to `%%`; documents why a name compare cannot identify the active agent"
    );
    assert!(config.select_tools(&summarizer).is_none());
    assert_eq!(config.bash_tool_names(&summarizer), Vec::<String>::new());

    // The active agent still gets script mode from the session's record.
    assert!(config.bash_tools_active());
    assert_eq!(
        config.bash_tool_names(&config.tool_selection_agent()),
        vec!["fs_read".to_string(), "fs_write".to_string()]
    );
}

#[test]
fn bash_inner_set_excludes_control_tools() {
    let config = Config {
        tools: Tools::init_from_mcp(Some(vec![
            tool("fs_read"),
            tool("bash"),
            tool("switch_agent"),
            tool("ask_user"),
            tool("atlas_session_handoff"),
        ])),
        ..Config::default()
    };
    let agent = agent_with(
        None,
        Some(vec![
            "fs_read".to_string(),
            "bash".to_string(),
            "switch_agent".to_string(),
            "ask_user".to_string(),
            "atlas_session_handoff".to_string(),
        ]),
    );

    assert_eq!(config.bash_tool_names(&agent), vec!["fs_read"]);
}

#[test]
fn bash_declaration_has_exec_mirroring_params_and_hints() {
    let config = config_with_tools();
    let agent = agent_with(
        Some(vec!["fs_read".to_string()]),
        Some(vec!["fs_*".to_string()]),
    );

    let bash = config
        .select_tools(&agent)
        .unwrap_or_default()
        .into_iter()
        .find(|declaration| declaration.name == "bash")
        .expect("bash declaration synthesized");

    assert_eq!(bash.idempotent_hint, Some(false));
    assert_eq!(bash.read_only_hint, Some(false));
    // PR3 review F3: script `bash` is an execution tool, not a read.
    assert_eq!(
        bash.kind,
        Some(harnx_core::event::ToolKind::Execute),
        "synthesized bash declaration must declare ToolKind::Execute"
    );

    let schema = bash.parameters.as_value();
    let properties = schema
        .get("properties")
        .and_then(|value| value.as_object())
        .expect("properties object");
    assert_eq!(
        schema.get("required"),
        Some(&serde_json::json!(["script"])),
        "only script is required"
    );
    for (param, expected_type) in [
        ("script", "string"),
        ("timeout_secs", "integer"),
        ("head_lines", "integer"),
        ("tail_lines", "integer"),
        ("max_output_bytes", "integer"),
    ] {
        let property = properties
            .get(param)
            .unwrap_or_else(|| panic!("missing param {param}"));
        assert_eq!(
            property.get("type").and_then(|value| value.as_str()),
            Some(expected_type),
            "{param} type"
        );
    }
    assert_eq!(
        properties
            .get("timeout_secs")
            .and_then(|value| value.get("default")),
        Some(&serde_json::json!(86400))
    );

    let description = &bash.description;
    assert!(
        description.contains("- fs_read: tool fs_read"),
        "description lists inner tool name + summary: {description}"
    );
    assert!(
        description.contains("- fs_write: tool fs_write"),
        "bash-only tools appear in the description: {description}"
    );
}

#[test]
fn bash_declaration_absent_when_bash_tools_unset_empty_or_unmatched() {
    let config = config_with_tools();
    for bash_tools in [None, Some(vec![]), Some(vec!["missing_*".to_string()])] {
        let agent = agent_with(Some(vec!["fs_read".to_string()]), bash_tools.clone());
        assert_eq!(
            declaration_names(&config, &agent),
            vec!["fs_read"],
            "no bash declaration for bash_tools {bash_tools:?}"
        );
        assert!(
            config.bash_tool_names(&agent).is_empty(),
            "empty inner set for bash_tools {bash_tools:?}"
        );
    }
}

#[test]
fn use_tools_glob_never_adds_bash_declaration() {
    let config = config_with_tools();

    // `*` and `bash*` must not synthesize or match a `bash` meta-tool; only
    // the real `bash_exec` declaration matches `bash*`.
    for use_tools in [
        vec!["*".to_string()],
        vec!["bash*".to_string()],
        vec!["bash".to_string()],
    ] {
        let agent = agent_with(Some(use_tools.clone()), None);
        let names = declaration_names(&config, &agent);
        assert!(
            !names.contains(&"bash".to_string()),
            "use_tools {use_tools:?} must not add a bash declaration: {names:?}"
        );
    }

    let star = agent_with(Some(vec!["*".to_string()]), None);
    let names = declaration_names(&config, &star);
    assert!(names.contains(&"bash_exec".to_string()));
}

#[test]
fn bash_name_is_reserved_when_bash_tools_active() {
    let config = Config {
        tools: Tools::init_from_mcp(Some(vec![tool("fs_read"), tool("bash")])),
        ..Config::default()
    };
    // A tool server registers a tool literally named `bash`. While script
    // mode is active the name is reserved: the synthesized declaration
    // replaces the external one instead of being suppressed by it.
    let agent = agent_with(
        Some(vec!["bash".to_string()]),
        Some(vec!["fs_*".to_string()]),
    );

    let declarations = config.select_tools(&agent).expect("declarations");
    let bash_declarations: Vec<_> = declarations
        .iter()
        .filter(|declaration| declaration.name == "bash")
        .collect();
    assert_eq!(bash_declarations.len(), 1, "exactly one bash declaration");
    assert_ne!(
        bash_declarations[0].description, "tool bash",
        "the synthesized declaration wins, not the tool server's"
    );
    assert_eq!(config.bash_tool_names(&agent), vec!["fs_read"]);

    // Without bash_tools the external `bash` tool is exposed unchanged.
    let plain = agent_with(Some(vec!["bash".to_string()]), None);
    let declarations = config.select_tools(&plain).expect("declarations");
    let external = declarations
        .iter()
        .find(|declaration| declaration.name == "bash")
        .expect("external bash tool selected");
    assert_eq!(external.description, "tool bash");
}

#[test]
fn bash_provider_claims_only_active_bash_name() {
    use crate::bash_provider::BashProvider;
    use harnx_core::tool::{ToolError, ToolProvider};

    let active = BashProvider::new(true, Vec::new(), Vec::new(), None, None);
    assert!(active.has_tool("bash"));
    assert!(!active.has_tool("bash_exec"));
    assert!(!active.has_tool("fs_read"));

    let inactive = BashProvider::new(false, Vec::new(), Vec::new(), None, None);
    assert!(!inactive.has_tool("bash"));

    // An active provider without a round context slot must fail closed:
    // recoverable error, never a silent dispatch to a raw-alias external
    // `bash` route.
    let result = tokio::runtime::Runtime::new().unwrap().block_on(async {
        active
            .call_tool(
                "bash",
                serde_json::json!({"script": "true"}),
                &harnx_core::abort::create_abort_signal(),
            )
            .await
    });
    let error = match result {
        Err(error) => error,
        // `ToolError` is not `Debug`, so a manual match replaces `expect_err`.
        Ok(_) => panic!("provider without a slot must return an error"),
    };
    match error {
        ToolError::Recoverable(error) => assert!(
            error
                .to_string()
                .contains("cannot run scripts in this execution context"),
            "advisory error: {error}"
        ),
        ToolError::Fatal(_) => panic!("expected a recoverable error, got a fatal one"),
    }
}

#[test]
fn bash_tools_respect_tool_use_disabled() {
    let mut config = config_with_tools();
    config.tool_use = false;
    let agent = agent_with(
        Some(vec!["fs_read".to_string()]),
        Some(vec!["fs_*".to_string()]),
    );

    assert!(config.bash_tool_names(&agent).is_empty());
    assert!(config.select_tools(&agent).is_none());
}
