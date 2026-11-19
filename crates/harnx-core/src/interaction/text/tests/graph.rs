use super::*;
use crate::interaction::surface::{MAX_COMPONENT_DEPTH, MAX_COMPONENT_EXPANSION};

#[test]
fn render_truncates_stored_self_reference() {
    let mut form = payload();
    form["createSurface"]["components"][0]["child"] = json!("root");
    let text = render(&form);
    assert!(text.contains("surface truncated"), "{text}");
    assert!(text.contains("Actions: deploy (Deploy), deny"), "{text}");
}

#[test]
fn render_truncates_stored_two_node_cycle_and_keeps_siblings() {
    let mut form = payload();
    form["createSurface"]["components"][1]["children"] = json!(["root", "title"]);
    let text = render(&form);
    assert!(text.contains("surface truncated"), "{text}");
    assert!(text.contains("Pick a deployment target"), "{text}");
    assert_eq!(text.matches("surface truncated").count(), 1);
}

fn deep_form(depth: usize) -> Value {
    let mut nodes = vec![json!({"id": "root", "component": "Card", "child": "n1"})];
    for index in 1..depth - 1 {
        nodes.push(json!({
            "id": format!("n{index}"), "component": "Card", "child": format!("n{}", index + 1)
        }));
    }
    nodes.push(json!({"id": format!("n{}", depth - 1), "component": "Text", "text": "Deep leaf"}));
    json!({"version": "1.0", "createSurface": {"components": nodes}})
}

#[test]
fn render_enforces_stored_depth_bound() {
    let text = render(&deep_form(MAX_COMPONENT_DEPTH));
    assert_eq!(text, "Deep leaf");
    let text = render(&deep_form(MAX_COMPONENT_DEPTH + 1));
    assert!(text.contains("surface truncated"), "{text}");
    assert!(!text.contains("Deep leaf"), "{text}");
}

#[test]
fn render_handles_very_deep_stored_graph_without_stack_overflow() {
    // Component references are flat JSON, so this can exist in old logs even
    // though today's validator refuses it. Never follow all 20,000 references.
    let text = render(&deep_form(20_000));
    assert!(text.contains("surface truncated"), "{text}");
    assert!(!text.contains("Deep leaf"), "{text}");
}

#[test]
fn render_preserves_shared_dag_layout_order() {
    let mut form = payload();
    form["createSurface"]["components"][1]["children"] = json!(["title", "name", "title"]);
    let text = render(&form);
    let lines: Vec<_> = text.lines().collect();
    assert_eq!(
        &lines[..3],
        &[
            "Pick a deployment target",
            "Release name: rel-0",
            "Pick a deployment target"
        ]
    );
    assert!(!text.contains("surface truncated"), "{text}");
}

#[test]
fn render_enforces_stored_expansion_bound() {
    for (visits, truncated) in [
        (MAX_COMPONENT_EXPANSION, false),
        (MAX_COMPONENT_EXPANSION + 1, true),
    ] {
        let form = json!({"createSurface": {"components": [
            {"id": "root", "component": "Column", "children": vec!["leaf"; visits - 1]},
            {"id": "leaf", "component": "Text", "text": "Shared"}
        ]}});
        let text = render(&form);
        assert_eq!(text.contains("surface truncated"), truncated);
        assert_eq!(
            text.lines().filter(|line| *line == "Shared").count(),
            MAX_COMPONENT_EXPANSION - 1
        );
    }
}

#[test]
fn render_caps_exponential_stored_dag() {
    let mut nodes = vec![json!({"id": "root", "component": "Column", "children": ["n1", "n1"]})];
    for index in 1..30 {
        let child = format!("n{}", index + 1);
        nodes.push(
            json!({"id": format!("n{index}"), "component": "Column", "children": [&child, &child]}),
        );
    }
    nodes.push(json!({"id": "n30", "component": "Text", "text": "Shared"}));
    let text = render(&json!({"createSurface": {"components": nodes}}));
    assert!(text.contains("surface truncated"));
    assert!(text.lines().count() <= MAX_COMPONENT_EXPANSION);
}
