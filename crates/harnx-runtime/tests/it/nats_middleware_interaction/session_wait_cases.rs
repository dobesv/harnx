use super::harness::*;
use anyhow::Result;
use harnx_core::session::SessionLogEntry;
use serde_json::json;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn middleware_suspended_child_observed_by_parent_session_wait() -> Result<()> {
    let (harness, req_seq, interaction_id, prompt_id, provider) =
        suspended_child_case("call_a", "call-a").await?;

    let wait_out = call_subagent_tool(
        &provider,
        "session_wait",
        json!({
            "session_id": SESSION_ID,
            "prompt_id": prompt_id,
            "seconds": 1,
        }),
    )
    .await?;
    assert_eq!(
        wait_out["status"], "blocked",
        "session_wait must report blocked while child middleware interaction is pending"
    );
    assert_eq!(
        wait_out["terminal"], false,
        "session_wait must report non-terminal while child middleware interaction is pending"
    );
    assert!(
        harness
            .approve(&interaction_id, req_seq)
            .await?
            .is_applied(),
        "interaction approval must be applied"
    );
    let end_entries = harness.wait_for_turn_end().await?;
    assert!(
        end_entries
            .iter()
            .any(|(_, e)| matches!(e, SessionLogEntry::TurnEnd { .. })),
        "child turn must reach TurnEnd"
    );

    let completed_out = call_subagent_tool(
        &provider,
        "session_wait",
        json!({
            "session_id": SESSION_ID,
            "prompt_id": prompt_id,
            "seconds": 5,
        }),
    )
    .await?;
    assert_eq!(
        completed_out["status"], "completed",
        "session_wait must report completed after child turn settles"
    );
    assert_eq!(
        completed_out["terminal"], true,
        "session_wait must report terminal: true after child turn settles"
    );
    Ok(())
}
