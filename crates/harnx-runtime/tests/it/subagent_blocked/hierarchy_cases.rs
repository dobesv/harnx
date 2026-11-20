use anyhow::Result;
use harnx_core::{
    event::{SubAgentProgress, SubAgentProgressStatus},
    pending_interaction::{PendingInteractionRequest, PendingInteractionSummary},
    require_nextest,
    session::{SessionLogEntry, ToolOutput},
};
use serde_json::json;

use super::fixture::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subagent_blocked_parent_continues_and_grandchild_nonpropagation() -> Result<()> {
    require_nextest();
    let (_harness, _req_seq, _interaction_id, prompt_id, provider) =
        suspended_child_case("call_cont", "call-cont").await?;

    // 1. Parent tool call returns blocked promptly; parent doesn't hang
    let wait_out = call_subagent_tool(
        &provider,
        "session_wait",
        json!({"session_id": SESSION_ID, "prompt_id": prompt_id, "seconds": 1}),
    )
    .await?;
    assert_eq!(wait_out["status"], "blocked");
    assert_eq!(wait_out["terminal"], false);

    // 2. Verify summary points only to the direct child, not any grandchild
    let summary: PendingInteractionSummary =
        serde_json::from_value(wait_out["pending_interaction"].clone())?;
    assert_eq!(summary.agent, AGENT_NAME, "must address direct child agent");
    assert_eq!(
        summary.cluster, "local",
        "must address direct child cluster"
    );
    assert_eq!(
        summary.session_id, SESSION_ID,
        "must address direct child session"
    );

    // 3. Genuine non-propagation: child log containing a grandchild's blocked ToolResults
    // must NOT produce a pending interaction summary for the child session.
    let grandchild_summary = PendingInteractionSummary {
        agent: "grandchild-worker".into(),
        cluster: "local".into(),
        session_id: "grandchild-session-99".into(),
        request: PendingInteractionRequest::Interaction {
            interaction_id: "int-grandchild".into(),
            request_seq: 7,
            interaction_kind: "confirm".into(),
        },
        tool_name: "grandchild_bash".into(),
        message: "Grandchild confirm?".into(),
        pending_count: 0,
    };
    let grandchild_progress = SubAgentProgress {
        invocation_id: "gc-inv-1".into(),
        agent: "grandchild-worker".into(),
        session_id: "grandchild-session-99".into(),
        status: SubAgentProgressStatus::Running,
        elapsed_ms: 150,
        usage: Default::default(),
        tool_call_count: 1,
        title: None,
        tool_call_id: Some("call-grandchild".into()),
        pending_interaction: Some(grandchild_summary),
    };
    let child_entries_with_grandchild = vec![
        (
            1,
            SessionLogEntry::Message {
                id: Some("child-inv-1".into()),
                role: harnx_core::message::MessageRole::User,
                content: harnx_core::message::MessageContent::Text("run child".into()),
                timestamp: None,
                fence_token: None,
            },
        ),
        (
            2,
            SessionLogEntry::ToolResults {
                handoff_selection: None,
                results: vec![ToolOutput {
                    user_surfaces: Vec::new(),
                    id: Some("call-grandchild".into()),
                    name: "grandchild_session_wait".into(),
                    output: json!({
                        "status": "blocked",
                        "terminal": false,
                        "sub_agent_progress": grandchild_progress,
                    }),
                    markdown: None,
                    content: vec![],
                    switch_agent: None,
                }],
                timestamp: None,
            },
        ),
    ];

    let derived = harnx_runtime::interactions::derive_pending_interaction_summary(
        &child_entries_with_grandchild,
        AGENT_NAME,
        "local",
        SESSION_ID,
        "child-inv-1",
    )?;
    assert_eq!(
        derived, None,
        "grandchild pending interaction in ToolResults must never propagate to parent summary"
    );

    Ok(())
}
