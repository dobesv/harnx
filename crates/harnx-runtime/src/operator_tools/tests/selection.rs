use super::*;

fn selection(names: &[&str]) -> Vec<String> {
    names.iter().map(|name| (*name).to_string()).collect()
}

/// All five selection-matrix rows share `bash_selection_selectors`. Claims
/// only decide which tool servers a command asks to start; admission is the
/// authorization that `inner_declarations_for` resolves against discovery.
#[test]
fn bash_selection_matrix_rows_claim_and_admit() {
    let use_tools = Some(selection(&["u1", "u2"]));
    let bash_tools = Some(selection(&["b1"]));

    // Named inferred: claims the literal names only, admits U ∪ B. The
    // inferred names themselves never widen admission.
    let (claims, admitted) = bash_selection_selectors(
        &BashToolSelection::Inferred {
            names: selection(&["l1", "l2"]),
        },
        use_tools.clone(),
        bash_tools.clone(),
    );
    assert_eq!(claims, selection(&["l1", "l2"]));
    assert_eq!(admitted, selection(&["u1", "u2", "b1"]));

    // Named explicit: claims and admits the deduplicated union U ∪ B ∪ E.
    let (claims, admitted) = bash_selection_selectors(
        &BashToolSelection::Explicit {
            selectors: selection(&["e1"]),
        },
        use_tools.clone(),
        bash_tools.clone(),
    );
    assert_eq!(claims, selection(&["u1", "u2", "b1", "e1"]));
    assert_eq!(admitted, selection(&["u1", "u2", "b1", "e1"]));

    // Explicit presence always disables inference, even with empty selectors.
    let (claims, admitted) = bash_selection_selectors(
        &BashToolSelection::Explicit {
            selectors: Vec::new(),
        },
        use_tools.clone(),
        bash_tools.clone(),
    );
    assert_eq!(claims, selection(&["u1", "u2", "b1"]));
    assert_eq!(admitted, selection(&["u1", "u2", "b1"]));

    // TUI session: claims and admits U ∪ B.
    let (claims, admitted) =
        bash_selection_selectors(&BashToolSelection::Session, use_tools, bash_tools);
    assert_eq!(claims, selection(&["u1", "u2", "b1"]));
    assert_eq!(admitted, selection(&["u1", "u2", "b1"]));
}

#[test]
fn bash_selection_claims_stay_literal_without_effective_toolsets() {
    // Without U or B, inferred commands claim their literals and admit
    // nothing; the literal names are exact commands, not selectors.
    let (claims, admitted) = bash_selection_selectors(
        &BashToolSelection::Inferred {
            names: selection(&["fs_read", "fs_*"]),
        },
        None,
        None,
    );
    assert_eq!(claims, selection(&["fs_read", "fs_*"]));
    assert!(admitted.is_empty());

    // Whitespace and duplicates are trimmed and deduplicated across parts.
    let (claims, admitted) = bash_selection_selectors(
        &BashToolSelection::Explicit {
            selectors: selection(&[" e1 ", ""]),
        },
        Some(selection(&["u1", " e1"])),
        Some(selection(&["b1"])),
    );
    assert_eq!(claims, selection(&["u1", "e1", "b1"]));
    assert_eq!(admitted, selection(&["u1", "e1", "b1"]));
}

/// Admitted inner declarations come from one place: the resolved admission
/// names intersected with eligible discovered declarations. Discovery alone
/// grants nothing and control tools stay out of the script catalog.
#[test]
fn inner_declarations_intersect_discovered_and_drop_control_tools() {
    let discovered: Vec<ToolDeclaration> = [
        "fs_read",
        "fs_write",
        crate::config::BASH_TOOL_NAME,
        "switch_agent",
        "ask_user",
        "metis_session_handoff",
    ]
    .iter()
    .map(|name| ToolDeclaration {
        name: (*name).to_string(),
        description: String::new(),
        parameters: serde_json::from_value(json!({"type":"object","properties":{}})).unwrap(),
        mcp_tool_name: None,
        mcp_server_name: None,
        call_template: None,
        result_template: None,
        idempotent_hint: None,
        read_only_hint: None,
        kind: None,
    })
    .collect();
    let resolved: HashSet<String> = [
        "fs_read",
        crate::config::BASH_TOOL_NAME,
        "switch_agent",
        "absent",
    ]
    .iter()
    .map(|name| (*name).to_string())
    .collect();
    let admitted = inner_declarations_for(&resolved, &discovered);
    assert_eq!(
        admitted
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>(),
        ["fs_read"]
    );
    // An empty admission set is valid: pure shell keeps the outer bash.
    assert!(inner_declarations_for(&HashSet::new(), &discovered).is_empty());
}

/// #2655 finding B: the named/session worker path admits selected in-process
/// built-ins (session history, session metadata) alongside NATS-discovered
/// tools, prefers the discovered declaration when a tool server advertises
/// the same name, and still drops control tools and unbacked names.
#[test]
fn operator_inner_declarations_include_selected_in_process_tools() {
    let nats_declaration = |name: &str, description: &str| ToolDeclaration {
        name: name.to_string(),
        description: description.to_string(),
        parameters: serde_json::from_value(json!({"type":"object","properties":{}})).unwrap(),
        mcp_tool_name: None,
        mcp_server_name: None,
        call_template: None,
        result_template: None,
        idempotent_hint: None,
        read_only_hint: None,
        kind: None,
    };
    let discovered = vec![
        nats_declaration("fs_read", "nats"),
        nats_declaration(crate::session_meta_tool::READ_TOOL_NAME, "nats copy"),
    ];
    let resolved: HashSet<String> = [
        "fs_read",
        crate::session_meta_tool::READ_TOOL_NAME,
        crate::session_history::TOOL_NAME,
        "switch_agent",
        "absent",
    ]
    .iter()
    .map(|name| (*name).to_string())
    .collect();
    let admitted = operator_bash_inner_declarations(&resolved, &discovered);
    assert_eq!(
        admitted
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>(),
        [
            "fs_read",
            crate::session_history::TOOL_NAME,
            crate::session_meta_tool::READ_TOOL_NAME,
        ]
    );
    // A discovered declaration wins over the built-in of the same name,
    // matching provider order and the context declaration map.
    let shadowed = admitted
        .iter()
        .find(|tool| tool.name == crate::session_meta_tool::READ_TOOL_NAME)
        .unwrap();
    assert_eq!(shadowed.description, "nats copy");
    // The built-in fallback carries the real built-in declaration.
    let history = admitted
        .iter()
        .find(|tool| tool.name == crate::session_history::TOOL_NAME)
        .unwrap();
    assert_eq!(
        history.description,
        crate::session_history::tool_declaration().description
    );
    // Nothing admitted that is not selected, backed, or allowed.
    assert!(admitted
        .iter()
        .all(|tool| crate::config::is_bash_inner_tool_allowed(&tool.name)));
}
