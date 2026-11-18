use super::harness::*;
use anyhow::Result;
use serde_json::json;

const BASH_D4_SAVE_RESTORE: &str = r#"
middleware:
- name: d4_save_restore
  match: '^probe_run$'
  bash: |
    if saved result > /tmp/r; then :; else
      next --id run > /tmp/r
      save result "$(cat /tmp/r)"
    fi
    if confirm --id keep 'Keep this result?'; then
      respond "$(cat /tmp/r)"
    else
      deny 'Result discarded. The command already ran.'
    fi
"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn d4_save_restore_script_approved() -> Result<()> {
    let model = single_call_model("probe_run", json!({"value": "original"}), "call-run");
    let (harness, req_seq, interaction_id, _) =
        Harness::start_and_suspend(BASH_D4_SAVE_RESTORE, model, "start d4 save/restore test")
            .await?;

    // In D4, next runs before confirm and result is stashed
    harness.probe.assert_single_call("probe_run");

    // Approve the suspension
    assert!(harness
        .approve(&interaction_id, req_seq)
        .await?
        .is_applied());

    let entries = harness.wait_for_turn_end().await?;

    // On resume: probe_run must NOT rerun; stash is used
    assert_eq!(
        harness.probe.total_calls(),
        1,
        "probe_run must not rerun after approval"
    );

    let results = extract_tool_results(&entries);
    assert_eq!(results.len(), 1);
    assert_ne!(results[0].output.get("is_error"), Some(&json!(true)));
    assert_eq!(results[0].output["value"], "original");

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn d4_save_restore_script_denied() -> Result<()> {
    let model = single_call_model("probe_run", json!({"value": "original"}), "call-run");
    let (harness, req_seq, interaction_id, _) =
        Harness::start_and_suspend(BASH_D4_SAVE_RESTORE, model, "start d4 denial test").await?;

    harness.probe.assert_single_call("probe_run");

    // Deny the suspension
    assert!(harness
        .deny(&interaction_id, req_seq, Some("denied by operator"))
        .await?
        .is_applied());

    let entries = harness.wait_for_turn_end().await?;

    // On resume: probe_run must NOT rerun
    assert_eq!(
        harness.probe.total_calls(),
        1,
        "probe_run must not rerun after denial"
    );

    let results = extract_tool_results(&entries);
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].output["is_error"], true);
    assert_eq!(
        results[0].output["error"],
        "Result discarded. The command already ran."
    );
    assert_eq!(
        results[0].output["blocked_by_middleware"],
        "d4_save_restore"
    );

    Ok(())
}
