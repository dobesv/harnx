use anyhow::{Context, Result};
use harnx_core::{
    pending_interaction::{PendingInteractionRequest, PendingInteractionSummary},
    require_nextest,
    session::SessionLogEntry,
};
use serde_json::json;

use super::fixture::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subagent_blocked_async_send_wait_load_middleware_confirm() -> Result<()> {
    require_nextest();
    let harness = Harness::required_start(
        BASH_CONFIRM,
        single_call_model("probe_run", json!({"value": "call_a"}), "call-a"),
    )
    .await?;
    let provider = discover_parent_tools(&harness).await?;
    let send_out = call_subagent_tool(
        &provider,
        "session_send",
        json!({
            "session_id": SESSION_ID, "message": "start blocked child", "wait": {"seconds": 10}
        }),
    )
    .await?;
    assert_eq!(send_out["status"], "blocked");
    let prompt_id = send_out["prompt_id"]
        .as_str()
        .context("send prompt_id")?
        .to_owned();
    let (req_seq, interaction_id, _) = harness.wait_for_suspension().await?;

    // 1. session_wait returns blocked with top-level and sub_agent_progress summaries
    let wait_out = call_subagent_tool(
        &provider,
        "session_wait",
        json!({"session_id": SESSION_ID, "prompt_id": prompt_id, "seconds": 1}),
    )
    .await?;
    assert_eq!(wait_out["status"], "blocked");
    assert_eq!(wait_out["terminal"], false);
    let top_summary: PendingInteractionSummary =
        serde_json::from_value(wait_out["pending_interaction"].clone())
            .context("decode top-level pending_interaction in session_wait")?;
    assert_eq!(top_summary.agent, AGENT_NAME);
    assert_eq!(top_summary.cluster, "local");
    assert_eq!(top_summary.session_id, SESSION_ID);
    assert_eq!(top_summary.tool_name, "probe_run");
    assert_eq!(top_summary.message, "Proceed?");
    match &top_summary.request {
        PendingInteractionRequest::Interaction {
            interaction_id: id,
            request_seq: seq,
            interaction_kind,
        } => {
            assert_eq!(id, &interaction_id);
            assert_eq!(*seq, req_seq);
            assert_eq!(interaction_kind, "confirm");
        }
        _ => panic!("expected Interaction request variant"),
    }
    let prog_summary: PendingInteractionSummary =
        serde_json::from_value(wait_out["sub_agent_progress"]["pending_interaction"].clone())
            .context("decode sub_agent_progress.pending_interaction in session_wait")?;
    assert_eq!(prog_summary, top_summary);
    assert_blocked_summary(&wait_out, &top_summary);

    // 2. session_load returns blocked with matching summary
    let load_out = call_subagent_tool(
        &provider,
        "session_load",
        json!({"session_id": SESSION_ID, "prompt_id": prompt_id}),
    )
    .await?;
    assert_eq!(load_out["status"], "blocked");
    assert_eq!(load_out["terminal"], false);
    let load_summary: PendingInteractionSummary =
        serde_json::from_value(load_out["pending_interaction"].clone())
            .context("decode top-level pending_interaction in session_load")?;
    assert_eq!(load_summary, top_summary);
    assert_blocked_summary(&load_out, &top_summary);

    // Initial session_send carries the same exact request as subsequent reads.
    assert_eq!(send_out["status"], "blocked");
    assert_eq!(send_out["terminal"], false);
    let send_summary: PendingInteractionSummary =
        serde_json::from_value(send_out["pending_interaction"].clone())
            .context("decode top-level pending_interaction in session_send")?;
    assert_eq!(send_summary, top_summary);
    assert_blocked_summary(&send_out, &top_summary);

    // 4. Approve child interaction and verify subsequent session_wait completes
    assert!(harness
        .approve(&interaction_id, req_seq)
        .await?
        .is_applied());
    let end_entries = harness.wait_for_turn_end().await?;
    assert!(end_entries
        .iter()
        .any(|(_, e)| matches!(e, SessionLogEntry::TurnEnd { .. })));

    let completed_out = call_subagent_tool(
        &provider,
        "session_wait",
        json!({"session_id": SESSION_ID, "prompt_id": prompt_id, "seconds": 5}),
    )
    .await?;
    assert_eq!(completed_out["status"], "completed");
    assert_eq!(completed_out["terminal"], true);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subagent_blocked_async_send_wait_load_legacy_hitl() -> Result<()> {
    require_nextest();
    let harness =
        Harness::required_start("", single_call_model("probe_run", json!({}), "call-1")).await?;
    let approval_ref = seed_child_legacy_hitl(
        &harness,
        "danger-call-1",
        "probe_run",
        "Approve dangerous action?",
    )
    .await?;
    let provider = discover_parent_tools(&harness).await?;

    // 1. session_wait
    let wait_out = call_subagent_tool(
        &provider,
        "session_wait",
        json!({
            "session_id": SESSION_ID,
            "prompt_id": approval_ref.invocation_id,
            "seconds": 1,
        }),
    )
    .await?;
    assert_eq!(wait_out["status"], "blocked");
    assert_eq!(wait_out["terminal"], false);
    let top_summary: PendingInteractionSummary =
        serde_json::from_value(wait_out["pending_interaction"].clone())
            .context("decode top-level pending_interaction in session_wait")?;
    assert_eq!(top_summary.agent, AGENT_NAME);
    assert_eq!(top_summary.cluster, "local");
    assert_eq!(top_summary.session_id, SESSION_ID);
    assert_eq!(top_summary.tool_name, "probe_run");
    assert_eq!(top_summary.message, "Approve dangerous action?");
    match &top_summary.request {
        PendingInteractionRequest::LegacyApproval { request } => {
            assert_eq!(request, &approval_ref);
        }
        _ => panic!("expected LegacyApproval request variant"),
    }
    let prog_summary: PendingInteractionSummary =
        serde_json::from_value(wait_out["sub_agent_progress"]["pending_interaction"].clone())
            .context("decode sub_agent_progress.pending_interaction in session_wait")?;
    assert_eq!(prog_summary, top_summary);
    assert_blocked_summary(&wait_out, &top_summary);

    // 2. session_load
    let load_out = call_subagent_tool(
        &provider,
        "session_load",
        json!({
            "session_id": SESSION_ID,
            "prompt_id": approval_ref.invocation_id,
        }),
    )
    .await?;
    assert_eq!(load_out["status"], "blocked");
    assert_eq!(load_out["terminal"], false);
    let load_summary: PendingInteractionSummary =
        serde_json::from_value(load_out["pending_interaction"].clone())
            .context("decode top-level pending_interaction in session_load")?;
    assert_eq!(load_summary, top_summary);
    assert_blocked_summary(&load_out, &top_summary);

    // 3. Genuine legacy session_send with wait returns blocked with matching summary
    let send_out = call_subagent_tool(
        &provider,
        "session_send",
        json!({
            "session_id": SESSION_ID,
            "message": "check legacy blocked child",
            "wait": { "seconds": 1 },
        }),
    )
    .await?;
    assert_eq!(send_out["status"], "blocked");
    assert_eq!(send_out["terminal"], false);
    let send_summary: PendingInteractionSummary =
        serde_json::from_value(send_out["pending_interaction"].clone())
            .context("decode top-level pending_interaction in session_send")?;
    assert_eq!(send_summary, top_summary);
    assert_blocked_summary(&send_out, &top_summary);

    Ok(())
}
