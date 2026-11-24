use super::harness::*;
use anyhow::Result;
use harnx_runtime::fault_injection::{pause_point, RecoveryFaultPoint};
use serde_json::json;

const LEAF_REPLY_ERR: &str = "the downstream tool completed but the middleware didn't finish";

async fn run_passthrough_test(
    point: RecoveryFaultPoint,
    expected_runs: usize,
    expect_error: bool,
    err_substring: Option<&str>,
) -> Result<()> {
    let model = single_call_model("probe_run", json!({"value": "run"}), "call-boundary");
    let mut harness = Harness::required_start(BASH_PASSTHROUGH, model).await?;

    let mut pause = pause_point(harness.session.storage_key(), point);
    harness.activate("start boundary test").await?;
    pause.wait_reached().await?;

    harness.kill_and_replace_worker("worker-recovery-b").await?;
    pause.crash();
    harness.reactivate().await?;

    let entries = harness.wait_for_turn_end().await?;
    assert_eq!(
        harness.probe.call_count("probe_run"),
        expected_runs,
        "probe handler call count mismatch for {point:?}"
    );

    let results = assert_single_tool_results(&entries);
    assert_eq!(results.len(), 1);

    if expect_error {
        assert_eq!(
            results[0].output.get("is_error"),
            Some(&json!(true)),
            "expected error result for {point:?}"
        );
        if let Some(expected_txt) = err_substring {
            let actual_err = results[0].output["error"].as_str().unwrap_or_default();
            assert!(
                actual_err.contains(expected_txt),
                "error '{actual_err}' must contain exact expected text '{expected_txt}' for {point:?}"
            );
        }
    } else {
        assert_ne!(
            results[0].output.get("is_error"),
            Some(&json!(true)),
            "expected successful result for {point:?}"
        );
    }
    Ok(())
}

async fn run_confirm_test(point: RecoveryFaultPoint) -> Result<()> {
    let model = single_call_model(
        "probe_run",
        json!({"value": "run"}),
        "call-confirm-boundary",
    );
    let mut harness = Harness::required_start(BASH_CONFIRM, model).await?;

    let mut pause = pause_point(harness.session.storage_key(), point);
    harness.activate("start confirm test").await?;
    pause.wait_reached().await?;

    harness.kill_and_replace_worker("worker-recovery-b").await?;
    pause.crash();
    harness.reactivate().await?;

    let (req_seq, int_id, _) = harness.wait_for_suspension().await?;
    assert!(harness.approve(&int_id, req_seq).await?.is_applied());

    let entries = harness.wait_for_turn_end().await?;
    assert_eq!(
        harness.probe.call_count("probe_run"),
        1,
        "probe runs once after confirmation"
    );

    let results = assert_single_tool_results(&entries);
    assert_eq!(results.len(), 1);
    assert_ne!(results[0].output.get("is_error"), Some(&json!(true)));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_fault_point_round_start_before() -> Result<()> {
    run_passthrough_test(RecoveryFaultPoint::RoundStartBefore, 0, true, None).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_fault_point_round_start_after() -> Result<()> {
    run_passthrough_test(RecoveryFaultPoint::RoundStartAfter, 1, false, None).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_fault_point_attempt_start_before() -> Result<()> {
    run_passthrough_test(RecoveryFaultPoint::AttemptStartBefore, 1, false, None).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_fault_point_attempt_start_after() -> Result<()> {
    run_passthrough_test(RecoveryFaultPoint::AttemptStartAfter, 1, false, None).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_fault_point_dispatch_intent_before() -> Result<()> {
    run_passthrough_test(RecoveryFaultPoint::DispatchIntentBefore, 1, false, None).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_fault_point_dispatch_intent_after() -> Result<()> {
    run_passthrough_test(RecoveryFaultPoint::DispatchIntentAfter, 0, true, None).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_fault_point_downstream_snapshot_before() -> Result<()> {
    run_passthrough_test(
        RecoveryFaultPoint::DownstreamSnapshotBefore,
        1,
        true,
        Some(LEAF_REPLY_ERR),
    )
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_fault_point_downstream_snapshot_after() -> Result<()> {
    run_passthrough_test(
        RecoveryFaultPoint::DownstreamSnapshotAfter,
        1,
        true,
        Some(LEAF_REPLY_ERR),
    )
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_fault_point_downstream_completion_before() -> Result<()> {
    run_passthrough_test(
        RecoveryFaultPoint::DownstreamCompletionBefore,
        1,
        true,
        Some(LEAF_REPLY_ERR),
    )
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_fault_point_downstream_completion_after() -> Result<()> {
    run_passthrough_test(
        RecoveryFaultPoint::DownstreamCompletionAfter,
        1,
        true,
        Some(LEAF_REPLY_ERR),
    )
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_fault_point_outer_completion_record_before() -> Result<()> {
    run_passthrough_test(
        RecoveryFaultPoint::OuterCompletionRecordBefore,
        1,
        true,
        Some(LEAF_REPLY_ERR),
    )
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_fault_point_outer_completion_record_after() -> Result<()> {
    run_passthrough_test(
        RecoveryFaultPoint::OuterCompletionRecordAfter,
        1,
        false,
        None,
    )
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_fault_point_final_tool_results_before() -> Result<()> {
    run_passthrough_test(RecoveryFaultPoint::FinalToolResultsBefore, 1, false, None).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_fault_point_final_tool_results_after() -> Result<()> {
    run_passthrough_test(RecoveryFaultPoint::FinalToolResultsAfter, 1, false, None).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_fault_point_checkpoint_before() -> Result<()> {
    run_confirm_test(RecoveryFaultPoint::CheckpointBefore).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_fault_point_checkpoint_after() -> Result<()> {
    run_confirm_test(RecoveryFaultPoint::CheckpointAfter).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_fault_point_interaction_requested_before() -> Result<()> {
    run_confirm_test(RecoveryFaultPoint::InteractionRequestedBefore).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_fault_point_interaction_requested_after() -> Result<()> {
    run_confirm_test(RecoveryFaultPoint::InteractionRequestedAfter).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_fault_point_suspension_ready_before() -> Result<()> {
    run_confirm_test(RecoveryFaultPoint::SuspensionReadyBefore).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_fault_point_suspension_ready_after() -> Result<()> {
    run_confirm_test(RecoveryFaultPoint::SuspensionReadyAfter).await
}

async fn run_rerun_fault_test(point: RecoveryFaultPoint) -> Result<()> {
    let (mut harness, mut pause, req_seq, interaction_id) =
        start_confirm_fault(point, "call-rerun-boundary").await?;
    assert!(harness
        .approve(&interaction_id, req_seq)
        .await?
        .is_applied());

    pause.wait_reached().await?;
    harness.kill_and_replace_worker("worker-recovery-b").await?;
    pause.crash();
    harness.reactivate().await?;

    let entries = harness.wait_for_turn_end().await?;
    assert_eq!(harness.probe.call_count("probe_run"), 1);

    let results = assert_single_tool_results(&entries);
    assert_eq!(results.len(), 1);
    assert_ne!(results[0].output.get("is_error"), Some(&json!(true)));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_fault_point_accepted_response_before() -> Result<()> {
    let (mut harness, mut pause, req_seq, interaction_id) = start_confirm_fault(
        RecoveryFaultPoint::AcceptedResponseBefore,
        "call-resp-boundary",
    )
    .await?;
    let approval_handle = harness.spawn_approve(&interaction_id, req_seq);

    pause.wait_reached().await?;
    harness.kill_and_replace_worker("worker-recovery-b").await?;
    pause.crash();
    let _ = approval_handle.await;
    harness.reactivate().await?;

    // Crashed before response CAS appended: must re-approve pending interaction
    let (req_seq2, int_id2, _) = harness.wait_for_suspension().await?;
    assert!(harness.approve(&int_id2, req_seq2).await?.is_applied());

    let entries = harness.wait_for_turn_end().await?;
    assert_eq!(harness.probe.call_count("probe_run"), 1);

    let results = assert_single_tool_results(&entries);
    assert_eq!(results.len(), 1);
    assert_ne!(results[0].output.get("is_error"), Some(&json!(true)));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_fault_point_accepted_response_after() -> Result<()> {
    let (mut harness, mut pause, req_seq, interaction_id) = start_confirm_fault(
        RecoveryFaultPoint::AcceptedResponseAfter,
        "call-resp-boundary",
    )
    .await?;
    let approval_handle = harness.spawn_approve(&interaction_id, req_seq);

    pause.wait_reached().await?;
    harness.kill_and_replace_worker("worker-recovery-b").await?;
    pause.crash();
    let _ = approval_handle.await;
    harness.reactivate().await?;

    // Crashed after response CAS appended: recovery continues directly without second approve
    let entries = harness.wait_for_turn_end().await?;
    assert_eq!(harness.probe.call_count("probe_run"), 1);

    let results = assert_single_tool_results(&entries);
    assert_eq!(results.len(), 1);
    assert_ne!(results[0].output.get("is_error"), Some(&json!(true)));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_fault_point_rerun_before() -> Result<()> {
    run_rerun_fault_test(RecoveryFaultPoint::RerunBefore).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_fault_point_rerun_after() -> Result<()> {
    run_rerun_fault_test(RecoveryFaultPoint::RerunAfter).await
}
