use super::harness::*;
use anyhow::Result;
use harnx_core::session::SessionLogEntry;
use harnx_runtime::fault_injection::{pause_point, RecoveryFaultPoint};
use serde_json::json;

const BASH_SAVE_RESTORE: &str = r#"
middleware:
- name: save_mw
  match: '^probe_run$'
  bash: |
    if saved result > /tmp/r.json; then
      :
    else
      next --id run > /tmp/r.json
      save result "$(cat /tmp/r.json)"
    fi
    respond "$(cat /tmp/r.json)"
"#;

const BASH_ALLOWLISTED_CALL_TOOL: &str = r#"
middleware:
- name: allowlisted_mw
  match: '^probe_run$'
  tools: [probe_aux]
  bash: |
    probe_aux --json '{"value": "aux_before_confirm"}'
    if confirm --id gate 'Proceed?'; then
      next
    else
      deny 'user denied'
    fi
"#;

async fn run_passthrough_matrix_case(point: RecoveryFaultPoint) -> Result<()> {
    let model = single_call_model("probe_run", json!({"value": "run"}), "call-matrix");
    let mut harness = Harness::required_start(BASH_PASSTHROUGH, model).await?;

    let mut pause = pause_point(harness.session.storage_key(), point);
    harness.activate("start matrix test").await?;
    pause.wait_reached().await?;

    harness.kill_and_replace_worker("worker-recovery-b").await?;
    pause.crash();
    harness.reactivate().await?;

    let entries = harness.wait_for_turn_end().await?;
    assert_eq!(
        harness.probe.call_count("probe_run"),
        1,
        "tool should run once and succeed"
    );

    let results = assert_single_tool_results(&entries);
    assert_eq!(results.len(), 1);
    assert_ne!(results[0].output.get("is_error"), Some(&json!(true)));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_crash_attempt_start_no_dispatch_reruns_once() -> Result<()> {
    run_passthrough_matrix_case(RecoveryFaultPoint::AttemptStartAfter).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_crash_after_all_completed_writes_tool_results() -> Result<()> {
    run_passthrough_matrix_case(RecoveryFaultPoint::FinalToolResultsBefore).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_crash_after_dispatch_intent_ends_interrupted_no_redispatch() -> Result<()> {
    let model = single_call_model("probe_run", json!({"value": "run"}), "call-intent");
    let mut harness = Harness::required_start(BASH_PASSTHROUGH, model).await?;

    let mut pause = pause_point(
        harness.session.storage_key(),
        RecoveryFaultPoint::DispatchIntentAfter,
    );
    harness.activate("start dispatch intent crash test").await?;
    pause.wait_reached().await?;

    assert_eq!(
        harness.probe.call_count("probe_run"),
        0,
        "downstream leaf execution not yet completed before crash"
    );
    harness.kill_and_replace_worker("worker-recovery-b").await?;
    pause.crash();
    harness.reactivate().await?;

    let entries = harness.wait_for_turn_end().await?;
    assert_eq!(
        harness.probe.call_count("probe_run"),
        0,
        "crashed dispatch intent must never be redispatched"
    );

    let results = assert_single_tool_results(&entries);
    assert_eq!(results.len(), 1);
    assert_eq!(
        results[0].output.get("is_error"),
        Some(&json!(true)),
        "interrupted attempt must end as error result"
    );
    let err_msg = results[0].output["error"].as_str().unwrap_or_default();
    assert!(
        err_msg.contains("interrupted") || err_msg.contains("unknown"),
        "error message must report interrupted outcome: {err_msg}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_crash_after_leaf_reply_before_save_ends_interrupted() -> Result<()> {
    let model = single_call_model("probe_run", json!({"value": "run"}), "call-save");
    let mut harness = Harness::required_start(BASH_SAVE_RESTORE, model).await?;

    let mut pause = pause_point(
        harness.session.storage_key(),
        RecoveryFaultPoint::DownstreamCompletionAfter,
    );
    harness.activate("start leaf reply crash test").await?;
    pause.wait_reached().await?;

    harness.kill_and_replace_worker("worker-recovery-b").await?;
    pause.crash();
    harness.reactivate().await?;

    let entries = harness.wait_for_turn_end().await?;
    assert_eq!(
        harness.probe.call_count("probe_run"),
        1,
        "downstream tool should not be re-executed"
    );

    let results = assert_single_tool_results(&entries);
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].output.get("is_error"), Some(&json!(true)));
    let err_msg = results[0].output["error"].as_str().unwrap_or_default();
    assert!(
        err_msg.contains("the downstream tool completed but the middleware didn't finish")
            || err_msg.contains("downstream completed but the middleware didn't finish"),
        "error must report downstream completed but middleware unfinished: {err_msg}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_crash_after_interaction_requested_commits_suspension_ready_on_resume() -> Result<()> {
    let model = single_call_model("probe_run", json!({"value": "run"}), "call-req");
    let mut harness = Harness::required_start(BASH_CONFIRM, model).await?;

    let mut pause = pause_point(
        harness.session.storage_key(),
        RecoveryFaultPoint::InteractionRequestedAfter,
    );
    harness.activate("start req crash test").await?;
    pause.wait_reached().await?;

    harness.kill_and_replace_worker("worker-recovery-b").await?;
    pause.crash();
    harness.reactivate().await?;

    let (req_seq, interaction_id, _) = harness.wait_for_suspension().await?;
    assert!(harness
        .approve(&interaction_id, req_seq)
        .await?
        .is_applied());

    let entries = harness.wait_for_turn_end().await?;
    let results = assert_single_tool_results(&entries);
    assert_eq!(results.len(), 1);
    assert_ne!(results[0].output.get("is_error"), Some(&json!(true)));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_crash_after_accepted_response_reruns_once() -> Result<()> {
    let (mut harness, mut pause, req_seq, interaction_id) =
        start_confirm_fault(RecoveryFaultPoint::AcceptedResponseAfter, "call-resp").await?;

    let approval_handle = harness.spawn_approve(&interaction_id, req_seq);
    pause.wait_reached().await?;

    let log_entries = harness.entries().await?;
    assert!(
        log_entries
            .iter()
            .any(|(_, e)| matches!(e, SessionLogEntry::InteractionResponse { .. })),
        "InteractionResponse must be committed to durable log"
    );

    harness.kill_and_replace_worker("worker-recovery-b").await?;
    pause.crash();
    let _ = approval_handle.await;
    harness.reactivate().await?;

    let entries = harness.wait_for_turn_end().await?;
    assert_eq!(
        harness.probe.call_count("probe_run"),
        1,
        "answered call should rerun once and complete"
    );

    let results = assert_single_tool_results(&entries);
    assert_eq!(results.len(), 1);
    assert_ne!(results[0].output.get("is_error"), Some(&json!(true)));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_crash_after_sibling_completed_preserves_result_and_observation() -> Result<()> {
    let (model, seen_messages) = probe_test_two_call_model();
    let mut harness = Harness::required_start(BASH_CONFIRM, model).await?;

    let mut pause = pause_point(
        harness.session.storage_key(),
        RecoveryFaultPoint::OuterCompletionRecordAfter,
    );
    harness.activate("start sibling crash test").await?;
    pause.wait_reached().await?;

    harness.kill_and_replace_worker("worker-recovery-b").await?;
    pause.crash();
    harness.reactivate().await?;

    let (req_seq, interaction_id, _) = harness.wait_for_suspension().await?;
    assert_eq!(
        harness.probe.call_count("probe_aux"),
        1,
        "completed sibling must not be re-executed"
    );

    assert!(harness
        .approve(&interaction_id, req_seq)
        .await?
        .is_applied());
    let entries = harness.wait_for_turn_end().await?;
    assert_eq!(
        harness.probe.call_count("probe_aux"),
        1,
        "completed sibling count remains 1 at turn end"
    );

    let contexts = harness.metadata_execution_contexts().await?;
    assert!(
        contexts
            .iter()
            .any(|c| c.workspace_root.contains("/workspace/probe_b")),
        "private execution observation must survive crash recovery"
    );

    for (_, entry) in &entries {
        let serialized = serde_json::to_string(entry)?;
        assert!(
            !serialized.contains("/workspace/probe_b"),
            "session log entry must not leak private observation: {serialized}"
        );
    }

    let msgs = seen_messages.lock().clone();
    assert!(!msgs.is_empty(), "model messages captured on second turn");
    for msg in &msgs {
        let serialized = serde_json::to_string(msg)?;
        assert!(
            !serialized.contains("/workspace/probe_b"),
            "model prompt messages must not leak private observation: {serialized}"
        );
    }

    let tool_results_entries: Vec<_> = entries
        .iter()
        .filter(|(_, e)| matches!(e, SessionLogEntry::ToolResults { .. }))
        .collect();
    assert_eq!(
        tool_results_entries.len(),
        1,
        "exactly one ToolResults entry"
    );
    assert_eq!(extract_tool_results(&entries).len(), 2);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_crash_after_allowlisted_call_tool_ends_interrupted() -> Result<()> {
    let model = single_call_model("probe_run", json!({"value": "run"}), "call-allowlisted");
    let mut harness = Harness::required_start(BASH_ALLOWLISTED_CALL_TOOL, model).await?;

    let mut pause = pause_point(
        harness.session.storage_key(),
        RecoveryFaultPoint::CheckpointBefore,
    );
    harness
        .activate("start allowlisted call_tool crash test")
        .await?;
    pause.wait_reached().await?;

    assert_eq!(
        harness.probe.call_count("probe_aux"),
        1,
        "allowlisted tool should run before confirm"
    );
    assert_eq!(
        harness.probe.call_count("probe_run"),
        0,
        "wrapped tool has not yet run"
    );

    harness.kill_and_replace_worker("worker-recovery-b").await?;
    pause.crash();
    harness.reactivate().await?;

    let entries = harness.wait_for_turn_end().await?;
    assert_eq!(
        harness.probe.call_count("probe_aux"),
        1,
        "allowlisted tool must not be re-executed on recovery"
    );
    assert_eq!(
        harness.probe.call_count("probe_run"),
        0,
        "wrapped tool must not be executed on interrupted recovery"
    );

    let results = assert_single_tool_results(&entries);
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].output.get("is_error"), Some(&json!(true)));
    Ok(())
}
