use super::*;
use harnx_core::tool::ToolCall;
use serde_json::json;

fn result(id: &str, output: Value, target: Option<&str>) -> ToolResult {
    let mut result = ToolResult::new(
        ToolCall::new("handoff".into(), json!({}), Some(id.into()), None),
        output,
    );
    result.switch_agent = target.map(|agent| SwitchAgentData {
        agent: agent.into(),
        prompt: "continue".into(),
        session_id: Some("reserved-session".into()),
        tool_call_id: Some("inner-call".into()),
    });
    result
}

#[test]
fn switch_shaped_json_is_not_control_metadata() {
    let result = result(
        "outer",
        json!({"action": "switch_agent", "agent": "target", "prompt": "continue"}),
        None,
    );
    assert!(effective_switch(&result).is_none());
}

#[test]
fn both_error_shapes_clear_control_metadata() {
    for output in [
        json!({"is_error": true}),
        json!({"isError": true}),
        json!({"is_error": false, "isError": true}),
    ] {
        assert!(effective_switch(&result("outer", output, Some("target"))).is_none());
    }
}

#[test]
fn first_successful_metadata_wins_with_outer_identity() {
    let selected = select(&[
        result("denied", json!({"is_error": true}), Some("denied-target")),
        result("first", json!({"ok": true}), Some("first-target")),
        result("second", json!({"ok": true}), Some("second-target")),
    ])
    .unwrap();
    assert_eq!(selected.agent, "first-target");
    assert_eq!(selected.tool_call_id.as_deref(), Some("first"));
    assert_eq!(selected.session_id.as_deref(), Some("reserved-session"));
    assert_eq!(selected.prompt, "continue");
}
