use anyhow::{Context, Result};
use harnx_core::{
    event::SubAgentProgressStatus, pending_interaction::PendingInteractionSummary, require_nextest,
    session::SessionLogEntry,
};
use harnx_runtime::{nats_session::completed_subagent_progress, nats_session_log::NatsSessionLog};
use serde_json::json;

use super::fixture::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subagent_blocked_persisted_parent_results_and_catchup() -> Result<()> {
    require_nextest();
    let (harness, _req_seq, _interaction_id, prompt_id, provider) =
        suspended_child_case("call_p", "call-p").await?;

    let wait_out = call_subagent_tool(
        &provider,
        "session_wait",
        json!({"session_id": SESSION_ID, "prompt_id": prompt_id, "seconds": 1}),
    )
    .await?;
    assert_eq!(wait_out["status"], "blocked");
    let expected_summary: PendingInteractionSummary =
        serde_json::from_value(wait_out["pending_interaction"].clone())?;

    // Parent session records actual tool execution output into ToolResults
    let parent_key =
        harnx_core::session_identity::session_key(Some("parent-agent"), "parent-waiter");
    let parent_log = NatsSessionLog::new_with_replicas(harness.js.clone(), &parent_key, 1);

    let tool_output = harnx_core::session::ToolOutput {
        user_surfaces: Vec::new(),
        id: Some("parent-call-1".into()),
        name: format!("{AGENT_NAME}_session_wait"),
        output: wait_out.clone(),
        markdown: None,
        content: vec![],
        switch_agent: None,
    };
    parent_log
        .append_event_async(&SessionLogEntry::ToolResults {
            handoff_selection: None,
            results: vec![tool_output],
            timestamp: None,
        })
        .await?;

    // Reconnecting parent frontend reads parent log and catches up
    let parent_entries = parent_log.load_events_async().await?;
    let completed = completed_subagent_progress(&parent_entries, 0);
    assert!(
        !completed.is_empty(),
        "catch-up via completed_subagent_progress must include blocked subagent progress"
    );
    let caught_up = completed
        .iter()
        .find(|p| p.tool_call_id.as_deref() == Some("parent-call-1"))
        .context("find caught-up progress entry")?;
    assert_eq!(
        caught_up.pending_interaction.as_ref(),
        Some(&expected_summary),
        "caught-up progress must retain the pending_interaction summary"
    );
    assert_eq!(
        caught_up.status,
        SubAgentProgressStatus::Running,
        "blocked subagent progress remains Running"
    );
    Ok(())
}
