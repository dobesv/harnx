use anyhow::{Context, Result};
use harnx_core::{pending_interaction::PendingInteractionSummary, require_nextest};
use serde_json::json;

use super::fixture::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subagent_blocked_sync_session_prompt_and_wait_after_answer() -> Result<()> {
    require_nextest();
    let model = single_call_model("probe_run", json!({"value": "sync_call"}), "sync-call-1");
    let harness = Harness::required_start(BASH_CONFIRM, model).await?;
    let provider = discover_parent_tools(&harness).await?;

    // Call sync session_prompt on child that will suspend on confirm
    let prompt_out = call_subagent_tool(
        &provider,
        "session_prompt",
        json!({
            "session_id": SESSION_ID,
            "message": "run sync prompt",
        }),
    )
    .await?;

    assert_eq!(
        prompt_out["status"], "blocked",
        "sync session_prompt on blocked child must return status: blocked"
    );
    assert_eq!(
        prompt_out["terminal"], false,
        "sync session_prompt on blocked child must return terminal: false"
    );
    assert_eq!(prompt_out["session_id"], SESSION_ID);
    assert!(
        prompt_out["prompt_id"].is_string(),
        "sync session_prompt blocked result must include prompt_id"
    );
    let prompt_id = prompt_out["prompt_id"].as_str().unwrap().to_string();

    let top_summary: PendingInteractionSummary =
        serde_json::from_value(prompt_out["pending_interaction"].clone())
            .context("decode pending_interaction in sync session_prompt")?;
    assert_eq!(top_summary.agent, AGENT_NAME);
    assert_eq!(top_summary.cluster, "local");
    assert_eq!(top_summary.session_id, SESSION_ID);
    assert_eq!(top_summary.tool_name, "probe_run");

    assert_ne!(
        prompt_out["sub_agent_progress"]["status"], "failed",
        "sync session_prompt on blocked child must not report failed progress"
    );
    let prog_summary: PendingInteractionSummary =
        serde_json::from_value(prompt_out["sub_agent_progress"]["pending_interaction"].clone())
            .context("decode sub_agent_progress.pending_interaction in sync session_prompt")?;
    assert_eq!(prog_summary, top_summary);

    // Answer the child's interaction
    let (req_seq, interaction_id, _) = harness.wait_for_suspension().await?;
    assert!(harness
        .approve(&interaction_id, req_seq)
        .await?
        .is_applied());
    let _ = harness.wait_for_turn_end().await?;

    // Now session_wait recovers the final completed response
    let wait_out = call_subagent_tool(
        &provider,
        "session_wait",
        json!({
            "session_id": SESSION_ID,
            "prompt_id": prompt_id,
            "seconds": 5,
        }),
    )
    .await?;
    assert_eq!(wait_out["status"], "completed");
    assert_eq!(wait_out["terminal"], true);
    assert!(wait_out.get("sub_agent_progress").is_some());
    Ok(())
}
