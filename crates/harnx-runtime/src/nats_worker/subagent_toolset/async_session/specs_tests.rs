use super::session_wait_spec;
use crate::nats_worker::subagent_toolset::async_session::{
    SessionPromptResult, SessionPromptStatus,
};
use harnx_core::tool::{render_tool_call_template, render_tool_result_template};
use serde_json::{json, Value};

fn render_result(result: &Value) -> String {
    let spec = session_wait_spec("pkg/helper");
    let template = spec.meta.as_ref().unwrap()["result_template"]
        .as_str()
        .unwrap();
    let rendered = render_tool_result_template(template, result, "raw\nfallback").unwrap();
    assert_eq!(rendered.lines().count(), 1, "{rendered}");
    rendered
}

#[test]
fn wait_call_is_one_line_for_packaged_and_bare_agents() {
    for agent in ["pkg/helper", "helper"] {
        let spec = session_wait_spec(agent);
        let template = spec.meta.as_ref().unwrap()["call_template"]
            .as_str()
            .unwrap();
        let args = json!({
            "session_id": "abcdefgh-long-session-id",
            "prompt_id": "long-prompt-id",
            "seconds": 240,
            "read": {"max_output_bytes": 65536}
        });
        let rendered = render_tool_call_template(template, &args, "raw fallback").unwrap();
        assert_eq!(rendered, format!("@ {agent} [abcdefgh] wait long-pro"));
    }
}

#[test]
fn wait_results_summarize_every_status_without_dumping_output() {
    for (status, expected) in [
        (SessionPromptStatus::Pending, "pending"),
        (SessionPromptStatus::Blocked, "blocked"),
        (SessionPromptStatus::Completed, "completed"),
        (SessionPromptStatus::Failed, "failed"),
        (SessionPromptStatus::Cancelled, "cancelled"),
        (SessionPromptStatus::TimedOut, "timed_out"),
        (SessionPromptStatus::Unknown, "unknown"),
    ] {
        let result = serde_json::to_value(SessionPromptResult {
            session_id: "abcdefgh-long-session-id".into(),
            prompt_id: "long-prompt-id".into(),
            invocation_id: None,
            dispatch: None,
            status,
            terminal: status.is_terminal(),
            wait_expired: false,
            stopped_by: None,
            elapsed_ms: 240_000,
            output: json!({"text": "child output\n".repeat(100), "observation": {"error": null}}),
        })
        .unwrap();
        assert_eq!(render_result(&result), expected);
    }
}

#[test]
fn wait_result_keeps_expiry_stop_reasons_and_observation_errors_visible() {
    for (fields, expected) in [
        (
            json!({"wait_expired": true, "output": {"observation": {"error": "soft_wait_expired"}}}),
            "pending (wait expired)",
        ),
        (
            json!({"stopped_by": "new_message"}),
            "pending (new_message)",
        ),
        (
            json!({"stopped_by": "caller_deadline"}),
            "pending (caller_deadline)",
        ),
        (
            json!({"output": {"observation": {"error": "durable_observation_unavailable"}}}),
            "pending — durable_observation_unavailable",
        ),
    ] {
        let mut result = json!({"status": "pending"});
        result
            .as_object_mut()
            .unwrap()
            .extend(fields.as_object().unwrap().clone());
        assert_eq!(render_result(&result), expected);
    }
}

#[test]
fn wait_result_handles_recoverable_errors_and_missing_envelopes() {
    assert_eq!(
        render_result(&json!({"is_error": true, "error": "access denied\r\ninvalid handle"})),
        "access denied  invalid handle"
    );
    assert_eq!(render_result(&json!({"is_error": true})), "raw fallback");
    let long_error =
        render_result(&json!({"is_error": true, "error": "large error\n".repeat(100)}));
    assert_eq!(long_error.chars().count(), 100);
    assert!(long_error.ends_with("..."));
    for result in [json!({}), Value::Null] {
        assert_eq!(render_result(&result), "unknown");
    }
}

#[test]
fn wait_terminal_failure_includes_a_short_single_line_diagnostic() {
    let result = json!({
        "status": "failed", "terminal": true,
        "output": {"events": [
            {"type": "message", "text": "unrelated output"},
            {"type": "error", "text": "worker failed\nnetwork unavailable"}
        ]}
    });
    assert_eq!(
        render_result(&result),
        "failed — worker failed network unavailable"
    );
    let result = json!({"status": "failed", "terminal": true, "output": {}});
    assert_eq!(render_result(&result), "failed");
}
