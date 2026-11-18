use super::harness::*;
use anyhow::Result;
use harnx_core::session::SessionLogEntry;
use harnx_runtime::{
    fault_injection::{
        pause_point, simulate_store_error, simulate_t2_1_worker, store_error_count,
        t2_1_preflight_count, RecoveryFaultPoint,
    },
    interaction_checkpoint::{test_guard_is_parked, test_purge_checkpoints},
};
use harnx_toolset_server::invocation_journal::InvocationJournal;
use serde_json::json;
use std::sync::atomic::Ordering;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_checkpoint_store_unreachable_parks_until_restored() -> Result<()> {
    let (model, model_calls) =
        counting_single_call_model("probe_run", json!({"value": "run"}), "call-store-err");
    let mut harness = Harness::required_start(BASH_PASSTHROUGH, model).await?;

    let storage_key = harness.session.storage_key().to_owned();
    let mut pause = pause_point(&storage_key, RecoveryFaultPoint::RoundStartAfter);
    harness.activate("start store unreachable test").await?;
    pause.wait_reached().await?;

    assert_eq!(
        model_calls.load(Ordering::SeqCst),
        1,
        "model called once on initial turn activation"
    );

    // Isolate checkpoint store failure before attempt-start while leaving NATS logs writable
    simulate_store_error(&storage_key, true);

    let initial_entries = harness.entries().await?;

    harness.kill_and_replace_worker("worker-recovery-b").await?;
    pause.crash();
    harness.reactivate().await?;

    // Wait for confirmed store error read by recovery worker
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    while store_error_count(&storage_key) == 0 && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(
        store_error_count(&storage_key) > 0,
        "recovery worker must have encountered simulated store error"
    );

    // Verify session log and model calls remain unchanged while store is unreachable
    let mid_entries = harness.entries().await?;
    assert_eq!(
        mid_entries.len(),
        initial_entries.len(),
        "log must not be modified while checkpoint store is unreachable"
    );
    assert!(
        !mid_entries
            .iter()
            .any(|(_, e)| matches!(e, SessionLogEntry::ToolResults { .. })),
        "ToolResults must not be written while store is unreachable"
    );
    assert_eq!(
        model_calls.load(Ordering::SeqCst),
        1,
        "model must not be re-called while checkpoint store is unreachable"
    );
    assert_eq!(
        harness.probe.call_count("probe_run"),
        0,
        "tool must not run while store is unreachable"
    );

    // Restore store and reactivate: recovery completes normally
    simulate_store_error(&storage_key, false);
    harness.reactivate().await?;

    let entries = harness.wait_for_turn_end().await?;
    assert_eq!(
        harness.probe.call_count("probe_run"),
        1,
        "tool runs once store is restored"
    );

    let results = assert_single_tool_results(&entries);
    assert_eq!(results.len(), 1);
    assert_ne!(results[0].output.get("is_error"), Some(&json!(true)));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_mixed_version_round_start_parks_t2_1_worker_then_t2_2_completes() -> Result<()> {
    let (model, model_calls) =
        counting_single_call_model("probe_run", json!({"value": "run"}), "call-mixed");
    let mut harness = Harness::required_start(BASH_PASSTHROUGH, model).await?;

    let storage_key = harness.session.storage_key().to_owned();
    let mut pause = pause_point(&storage_key, RecoveryFaultPoint::RoundStartAfter);
    harness.activate("start mixed version test").await?;
    pause.wait_reached().await?;

    // Worker wrote RoundStart to checkpoint store and crashed before writing any log record
    harness
        .kill_and_replace_worker("worker-t2-1-replacement")
        .await?;
    pause.crash();

    let initial_entries = harness.entries().await?;

    // Enable simulated T2.1 worker mode: worker runs original guard::is_parked preflight
    simulate_t2_1_worker(&storage_key, true);
    harness.reactivate().await?;

    // Wait for T2.1 preflight guard check to park
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    while t2_1_preflight_count(&storage_key) == 0 && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(
        t2_1_preflight_count(&storage_key) > 0,
        "T2.1 worker must run preflight guard"
    );

    // Also assert directly via guard::is_parked helper for defense-in-depth
    assert!(
        test_guard_is_parked(&harness.js, &storage_key, &initial_entries).await?,
        "T2.1 read-side guard must confirm parked state"
    );

    // Verify log is unchanged, no results written, probe not called, model counter is 1
    let mid_entries = harness.entries().await?;
    assert_eq!(
        mid_entries.len(),
        initial_entries.len(),
        "log must remain unchanged while T2.1 worker is parked"
    );
    assert!(
        !mid_entries
            .iter()
            .any(|(_, e)| matches!(e, SessionLogEntry::ToolResults { .. })),
        "ToolResults must not be written while T2.1 worker is parked"
    );
    assert_eq!(
        harness.probe.call_count("probe_run"),
        0,
        "tool must not run while T2.1 worker is parked"
    );
    assert_eq!(
        model_calls.load(Ordering::SeqCst),
        1,
        "model must not be called while T2.1 worker is parked"
    );

    // Disable T2.1 mode and reactivate as actual T2.2 worker: completes the round
    simulate_t2_1_worker(&storage_key, false);
    harness
        .kill_and_replace_worker("worker-t2-2-replacement")
        .await?;
    harness.reactivate().await?;

    let entries = harness.wait_for_turn_end().await?;
    assert_eq!(
        harness.probe.call_count("probe_run"),
        1,
        "replacement T2.2 worker completes the round"
    );

    let results = assert_single_tool_results(&entries);
    assert_eq!(results.len(), 1);
    assert_ne!(results[0].output.get("is_error"), Some(&json!(true)));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_attempt_start_with_missing_checkpoint_ends_interrupted() -> Result<()> {
    let (mut harness, mut pause, req_seq, interaction_id) =
        start_confirm_fault(RecoveryFaultPoint::AttemptStartAfter, "call-1").await?;
    assert!(harness
        .approve(&interaction_id, req_seq)
        .await?
        .is_applied());
    pause.wait_reached().await?;

    let storage_key = harness.session.storage_key().to_owned();

    // Derive actual round sequence from durable session log
    let entries = harness.entries().await?;
    let round_seq = entries
        .iter()
        .find_map(|(seq, e)| matches!(e, SessionLogEntry::ToolCalls { .. }).then_some(*seq))
        .expect("round sequence from ToolCalls");

    // Purge exact existing checkpoint manifest and chunk keys via test helper
    let cp_ref = test_purge_checkpoints(&harness.js, &storage_key, round_seq, "call-1")
        .await?
        .expect("rerun attempt must reference suspension checkpoint");

    let kv = harness
        .js
        .get_key_value(harnx_runtime::interaction_checkpoint::ROUND_START_BUCKET)
        .await?;
    assert!(
        kv.get(&cp_ref).await?.is_none(),
        "checkpoint manifest must be confirmed missing after purge"
    );

    // Crash after AttemptStart written with missing checkpoint data
    harness.kill_and_replace_worker("worker-recovery-b").await?;
    pause.crash();
    harness.reactivate().await?;

    let entries = harness.wait_for_turn_end().await?;
    assert_eq!(
        harness.probe.call_count("probe_run"),
        0,
        "tool must never run when checkpoint is missing"
    );

    let results = assert_single_tool_results(&entries);
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].output.get("is_error"), Some(&json!(true)));
    let err_msg = results[0].output["error"].as_str().unwrap_or_default();
    assert!(
        err_msg.contains("interrupted") || err_msg.contains("unknown"),
        "error must indicate interrupted/unknown outcome: {err_msg}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_pre_t2_2_round_ignores_journal_for_matching_middleware() -> Result<()> {
    let (model, _) = probe_test_two_call_model();
    let mut harness = Harness::required_start(BASH_PASSTHROUGH, model).await?;
    let storage_key = harness.session.storage_key().to_owned();

    // Register TWO pauses before OuterCompletionRecordBefore for both calls in the round
    let mut pause_a = pause_point(
        &storage_key,
        RecoveryFaultPoint::OuterCompletionRecordBefore,
    );
    let mut pause_b = pause_point(
        &storage_key,
        RecoveryFaultPoint::OuterCompletionRecordBefore,
    );

    harness.activate("start pre t2.2 test").await?;
    tokio::try_join!(pause_a.wait_reached(), pause_b.wait_reached())?;

    // Hard kill worker before either call appends ToolCallCompleted to session log
    harness.kill_and_replace_worker("worker-recovery-b").await?;
    pause_a.crash();
    pause_b.crash();

    let entries = harness.entries().await?;
    let round_seq = entries
        .iter()
        .find_map(|(seq, e)| matches!(e, SessionLogEntry::ToolCalls { .. }).then_some(*seq))
        .expect("round sequence from ToolCalls");

    // This broker owns only this fixture. Remove the T2.2 store to model an old
    // worker: KV purge markers would still prove that attempts once existed.
    harness
        .js
        .delete_key_value(harnx_runtime::interaction_checkpoint::ROUND_START_BUCKET)
        .await?;

    // Assert: no ToolCallCompleted / InteractionRequested / SuspensionReady in session log
    assert!(
        !entries.iter().any(|(_, e)| matches!(
            e,
            SessionLogEntry::ToolCallCompleted { .. }
                | SessionLogEntry::InteractionRequested { .. }
                | SessionLogEntry::SuspensionReady { .. }
        )),
        "no completion or interaction log records before recovery"
    );

    // Assert: both tools executed on downstream provider exactly once
    assert_eq!(harness.probe.call_count("probe_run"), 1);
    assert_eq!(harness.probe.call_count("probe_aux"), 1);

    // Assert: raw journal entries exist in NATS invocation journal
    let journal = InvocationJournal::ensure(&harness.js, 1).await?;
    assert!(
        journal
            .find(&storage_key, round_seq, "call-a")
            .await?
            .is_some(),
        "call-a must be recorded in invocation journal"
    );
    assert!(
        journal
            .find(&storage_key, round_seq, "call-b")
            .await?
            .is_some(),
        "call-b must be recorded in invocation journal"
    );

    // Reactivate replacement worker: recovering with canonical session log
    harness.reactivate().await?;

    let final_entries = harness.wait_for_turn_end().await?;
    let results = assert_single_tool_results(&final_entries);
    assert_eq!(results.len(), 2);

    // Matching call: raw journal reply ignored, call ends interrupted, 0 extra effects
    let matching = results
        .iter()
        .find(|r| r.id.as_deref() == Some("call-a"))
        .expect("call-a");
    assert_eq!(
        matching.output.get("is_error"),
        Some(&json!(true)),
        "matching middleware must end interrupted without replaying raw journal"
    );

    // Unmatched sibling: legacy partition successfully replayed from raw journal
    let unmatched = results
        .iter()
        .find(|r| r.id.as_deref() == Some("call-b"))
        .expect("call-b");
    assert_ne!(
        unmatched.output.get("is_error"),
        Some(&json!(true)),
        "unmatched sibling must complete successfully via legacy partition: {unmatched:?}"
    );

    // Probe call counts remain 1 (neither tool re-executed)
    assert_eq!(harness.probe.call_count("probe_run"), 1);
    assert_eq!(harness.probe.call_count("probe_aux"), 1);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_crashed_pending_dispatch_is_not_rerun_on_recovery() -> Result<()> {
    let model = single_call_model("probe_run", json!({"value": "run"}), "call-pending");
    let mut harness = Harness::required_start(BASH_PASSTHROUGH, model).await?;

    // Block provider inside leaf invoke AFTER incrementing call count
    let _unblock = harness.probe.block_reply_on();

    harness.activate("start pending dispatch test").await?;

    // Wait until probe tool effect has been recorded by provider
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    while harness.probe.call_count("probe_run") == 0 && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert_eq!(
        harness.probe.call_count("probe_run"),
        1,
        "downstream provider must have started invocation"
    );

    // Hard kill worker while provider is blocked executing the leaf tool
    harness.kill_and_replace_worker("worker-recovery-b").await?;

    // Reactivate replacement worker: discovers pending dispatch intent in checkpoint store
    harness.reactivate().await?;

    let entries = harness.wait_for_turn_end().await?;

    // Unblock the original tool task on cleanup
    harness.probe.unblock_reply();

    // Probe call count must remain EXACTLY 1 (must NOT rerun the pending dispatch!)
    assert_eq!(
        harness.probe.call_count("probe_run"),
        1,
        "already-started pending tool must not be rerun on recovery"
    );

    let results = assert_single_tool_results(&entries);
    assert_eq!(results.len(), 1);
    assert_eq!(
        results[0].output.get("is_error"),
        Some(&json!(true)),
        "interrupted recovery must classify pending uncompleted dispatch as error"
    );
    let err_msg = results[0].output["error"].as_str().unwrap_or_default();
    assert!(
        err_msg.contains("interrupted") || err_msg.contains("unknown"),
        "error must indicate interrupted outcome: {err_msg}"
    );

    Ok(())
}
