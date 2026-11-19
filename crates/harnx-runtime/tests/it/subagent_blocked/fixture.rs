use anyhow::Result;
use harnx_core::{pending_interaction::ApprovalRef, session::SessionLogEntry, tool::ToolCall};
use harnx_runtime::nats_session_log::NatsSessionLog;
use serde_json::json;

pub use crate::nats_middleware_interaction::harness::*;

pub async fn seed_child_legacy_hitl(
    harness: &Harness,
    tool_call_id: &str,
    tool_name: &str,
    summary_message: &str,
) -> Result<ApprovalRef> {
    let storage_key = harness.session.storage_key();
    let log = NatsSessionLog::new_with_replicas(harness.js.clone(), storage_key, 1);
    let prompt_id = uuid::Uuid::new_v4().to_string();
    crate::worker::append_admitted_fixture_user(&log, &prompt_id, "run legacy tool").await?;
    let tool_round_seq = log
        .append_event_async(&SessionLogEntry::ToolCalls {
            text: "activation completed".to_string(),
            thought: None,
            calls: vec![ToolCall::new(
                tool_name.to_string(),
                json!({}),
                Some(tool_call_id.to_string()),
                None,
            )],
            timestamp: None,
            fence_token: None,
        })
        .await?;
    let req_seq = log
        .append_event_async(&SessionLogEntry::HitlApprovalRequested {
            tool_call_id: tool_call_id.to_string(),
            summary: summary_message.to_string(),
            fence_token: 0,
        })
        .await?;
    Ok(ApprovalRef {
        invocation_id: prompt_id,
        tool_round_seq,
        request_seq: req_seq,
        tool_call_id: tool_call_id.to_string(),
    })
}

pub fn assert_blocked_summary(
    value: &serde_json::Value,
    expected: &harnx_core::pending_interaction::PendingInteractionSummary,
) {
    assert_eq!(value["status"], "blocked");
    assert_eq!(value["terminal"], false);
    assert_eq!(value["sub_agent_progress"]["status"], "running");
    let expected = serde_json::to_value(expected).unwrap();
    assert_eq!(value["pending_interaction"], expected);
    assert_eq!(value["sub_agent_progress"]["pending_interaction"], expected);
}
