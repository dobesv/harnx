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
