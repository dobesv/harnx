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
        vec![
            "*",
            "alpha_tool",
            "beta_tool",
            crate::session_history::TOOL_NAME,
            crate::session_meta_tool::READ_TOOL_NAME,
            crate::session_meta_tool::WRITE_TOOL_NAME,
        ]
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

/// Wildcard '*' must still list all locally known tools, and must stay in the
/// output itself: tool-server declarations are not available offline, so
/// dropping the selector would silently turn "all tools" into "just builtins".
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
    assert!(expanded.contains(&"*".to_string()));
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

// ── expand_use_tools offline/tool-server behavior (#2628) ──────────────────

/// Explicit tool names with no local declaration (tool-server tools such as
/// bash_exec/fs_* run on workers, not in this process) must survive expansion
/// instead of being filtered out (#2628).
#[test]
fn expand_use_tools_keeps_unknown_explicit_tool_names() {
    // Frontend-like config: no MCP tools, no NATS declarations, just builtins.
    let config = Config::default();

    let expanded = config.expand_use_tools(
        Some(&[
            "bash_exec".to_string(),
            crate::session_history::TOOL_NAME.to_string(),
            "fs_read".to_string(),
        ]),
        None,
    );

    // Order preserved, nothing dropped just because it is unknown here.
    assert_eq!(
        expanded,
        vec![
            "bash_exec".to_string(),
            crate::session_history::TOOL_NAME.to_string(),
            "fs_read".to_string(),
        ]
    );
}

/// Toolsets still expand to their contents; unresolvable names stay verbatim.
#[test]
fn expand_use_tools_expands_toolsets_alongside_unknown_names() {
    let mut config = Config::default();
    config.toolsets.insert(
        "filesystem".to_string(),
        vec!["fs_read".into(), "fs_ls".into()],
    );

    let expanded = config.expand_use_tools(
        Some(&["filesystem".to_string(), "bash_exec".to_string()]),
        None,
    );

    assert_eq!(
        expanded,
        vec![
            "fs_read".to_string(),
            "fs_ls".to_string(),
            "bash_exec".to_string(),
        ]
    );
}

/// A glob selector stays verbatim and additionally lists the declarations known
/// locally, so offline dumps show what it resolves to here (#2628).
#[test]
fn expand_use_tools_glob_keeps_selector_alongside_known_matches() {
    let config = Config {
        tools: crate::tool::Tools::init_from_mcp(Some(vec![
            make_tool_decl("fs_read"),
            make_tool_decl("fs_write"),
            make_tool_decl("bash_exec"),
        ])),
        ..Config::default()
    };

    let expanded = config.expand_use_tools(Some(&["fs_*".to_string()]), None);

    assert_eq!(
        expanded,
        vec![
            "fs_*".to_string(),
            "fs_read".to_string(),
            "fs_write".to_string(),
        ]
    );
}

/// A glob that matches nothing locally still keeps the selector itself.
#[test]
fn expand_use_tools_glob_without_local_matches_keeps_selector() {
    let config = Config::default();

    let expanded = config.expand_use_tools(Some(&["bash_*".to_string()]), None);

    assert_eq!(expanded, vec!["bash_*".to_string()]);
}

/// Duplicate selectors (across toolsets or repeated entries) dedupe while
/// preserving first-seen order.
#[test]
fn expand_use_tools_dedupes_preserving_order() {
    let mut config = Config::default();
    config
        .toolsets
        .insert("filesystem".to_string(), vec!["fs_read".into()]);

    let expanded = config.expand_use_tools(
        Some(&[
            "fs_read".to_string(),
            "filesystem".to_string(),
            "bash_exec".to_string(),
            "bash_exec".to_string(),
        ]),
        None,
    );

    assert_eq!(
        expanded,
        vec!["fs_read".to_string(), "bash_exec".to_string()]
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
        kind: None,
    }
}
