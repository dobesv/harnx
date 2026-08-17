#![cfg(test)]

use super::*;

#[test]
fn expand_use_tools_wildcard_returns_concrete_names() {
    let temp = tempfile::tempdir().unwrap();
    let _lock = super::test_support::env_lock();
    let _config_dir = super::test_support::EnvGuard::new("HARNX_CONFIG_DIR", temp.path());
    let _data_dir = super::test_support::EnvGuard::new("HARNX_DATA_DIR", temp.path());
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

// ── expand_use_tools regression tests (#886 filtering) ──────────────────────

/// Regression test for #886: explicit selector must return ONLY that tool,
/// not ALL builtin tools (the bug was that tool_declarations_for_use_tools
/// starts from ALL builtins and only ADDS MCP/handoff tools, never filtering).
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
