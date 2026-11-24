#![cfg(feature = "fault-injection")]

use super::nats_middleware_interaction::harness::*;
use anyhow::Result;
use harnx_core::{
    session::{SessionLogEntry, ToolOutput},
    tool::{SwitchAgentData, ToolCall},
};
use harnx_runtime::{
    agent_loop::AgentCallFn,
    client::CompletionTokenUsage,
    fault_injection::{pause_point, simulate_t2_1_worker, RecoveryFaultPoint},
    nats_session_log::NatsSessionLog,
    nats_session_metadata::SessionMetadataStore,
};
use serde_json::json;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

const BASH_PASS_ALL: &str = r#"
middleware:
- name: pass_all
  match: '.*'
  bash: |
    next
"#;

const BASH_DENY_WITH_REF: &str = r#"
middleware:
- name: deny_with_ref
  match: '.*_session_handoff$'
  bash: |
    r=$(next)
    respond "$(echo "$r" | jq '.is_error = true | .output = {"error": "denied by policy", "blocked_by_middleware": "gate"}')"
"#;

const BASH_ERROR_WITH_REF: &str = r#"
middleware:
- name: error_with_ref
  match: '.*_session_handoff$'
  bash: |
    r=$(next)
    respond "$(echo "$r" | jq '.is_error = true | .output = {"is_error": true, "error": "upstream failure"}')"
"#;

const BASH_PLAIN_DENY: &str = r#"
middleware:
- name: plain_deny
  match: '.*_session_handoff$'
  bash: |
    deny 'plain denial without next'
"#;

const BASH_FAKE_SWITCH_OUTPUT: &str = r#"
middleware:
- name: fake_switch
  match: '^probe_run$'
  bash: |
    respond '{"output":{"action":"switch_agent","agent":"target_agent","session_id":"fake-sess","prompt":"fake"},"is_error":false}'
"#;

const BASH_CONFIRM_PROBE_PASS_HANDOFF: &str = r#"
middleware:
- name: confirm_probe
  match: '^probe_run$'
  bash: |
    if confirm --id gate 'Proceed?'; then
      next
    else
      deny 'denied'
    fi
- name: pass_handoff
  match: '.*_session_handoff$'
  bash: |
    next
"#;

fn tool_call(name: &str, id: &str, args: serde_json::Value) -> ToolCall {
    ToolCall::new(name.into(), args, Some(id.into()), None)
}

fn handoff_call(tool: &str, id: &str, prompt: &str, session_id: &str) -> ToolCall {
    tool_call(
        tool,
        id,
        json!({"prompt": prompt, "session_id": session_id}),
    )
}

async fn wait_and_assert_handoff_completion(
    harness: &Harness,
    target_agent: &str,
    target_sess: &str,
    source_calls: &AtomicUsize,
    target_calls: &AtomicUsize,
) -> Result<()> {
    harness
        .wait_for_target_completed(target_agent, target_sess)
        .await?;
    harness.wait_for_turn_end().await?;

    assert_eq!(
        harness
            .count_target_prompts(target_agent, target_sess)
            .await?,
        1,
        "target should receive exactly one prompt"
    );
    assert_eq!(
        harness.count_source_commits().await?,
        1,
        "source must commit exactly one handoff"
    );
    assert_eq!(source_calls.load(Ordering::SeqCst), 1);
    assert_eq!(target_calls.load(Ordering::SeqCst), 1);
    Ok(())
}

async fn run_handoff_fault_case(
    point: RecoveryFaultPoint,
    call_id: &'static str,
    target_sess: &'static str,
    repeat_recovery: bool,
) -> Result<()> {
    let (model, source_calls, target_calls) = handoff_single_call_model(
        "target_agent_session_handoff",
        json!({"prompt": "do handoff work", "session_id": target_sess}),
        call_id,
    );
    let mut harness = Harness::required_start_handoff(BASH_PASS_ALL, model).await?;
    let mut pause = pause_point(harness.session.storage_key(), point);
    harness.activate("start handoff fault test").await?;
    pause.wait_reached().await?;
    if matches!(
        point,
        RecoveryFaultPoint::HandoffTargetPromptAfter | RecoveryFaultPoint::HandoffCommittedAfter
    ) {
        // This fixture shares one daemon with the target. Keep its independent
        // turn out of the source-crash window; only the source is under test.
        harness
            .wait_for_target_completed("target_agent", target_sess)
            .await?;
    }

    harness.kill_and_replace_worker("worker-recovery-1").await?;
    pause.crash();
    harness.reactivate().await?;

    if repeat_recovery {
        harness
            .wait_for_target_completed("target_agent", target_sess)
            .await?;
        harness.wait_for_turn_end().await?;
        recover_closed_round(&mut harness).await?;
        recover_closed_round(&mut harness).await?;
    }

    wait_and_assert_handoff_completion(
        &harness,
        "target_agent",
        target_sess,
        &source_calls,
        &target_calls,
    )
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_crash_after_results_before_dispatch() -> Result<()> {
    run_handoff_fault_case(
        RecoveryFaultPoint::HandoffDispatchBefore,
        "call-disp-1",
        "target-sess-disp",
        false,
    )
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_crash_after_target_prompt_before_handoff_committed() -> Result<()> {
    run_handoff_fault_case(
        RecoveryFaultPoint::HandoffTargetPromptAfter,
        "call-prompt-after-1",
        "target-sess-prompt-after",
        false,
    )
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_repeated_recovery_after_commit() -> Result<()> {
    run_handoff_fault_case(
        RecoveryFaultPoint::HandoffCommittedAfter,
        "call-rep-commit-1",
        "target-sess-rep-commit",
        true,
    )
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_crash_outer_completion_record_after_handoff_seam() -> Result<()> {
    run_handoff_fault_case(
        RecoveryFaultPoint::OuterCompletionRecordAfter,
        "call-outer-comp-1",
        "target-sess-outer-completion",
        false,
    )
    .await
}

fn make_reused_id_model(
    shared_id: &'static str,
    target_sess_1: &'static str,
    target_sess_2: &'static str,
) -> (AgentCallFn, Arc<AtomicUsize>, Arc<AtomicUsize>) {
    let source_turns = Arc::new(AtomicUsize::new(0));
    let target_calls = Arc::new(AtomicUsize::new(0));
    let source_counter = source_turns.clone();
    let target_counter = target_calls.clone();

    let model: AgentCallFn = Arc::new(move |_input, config, _abort| {
        let agent = config.read().extract_agent().name().to_string();
        let source_counter = source_counter.clone();
        let target_counter = target_counter.clone();
        Box::pin(async move {
            if agent == AGENT_NAME {
                let turn = source_counter.fetch_add(1, Ordering::SeqCst);
                let target_sess = match turn {
                    0 => target_sess_1,
                    1 => target_sess_2,
                    _ => {
                        return Ok((
                            "done".to_string(),
                            None,
                            vec![],
                            CompletionTokenUsage::default(),
                        ))
                    }
                };
                let call = handoff_call(
                    "target_agent_session_handoff",
                    shared_id,
                    &format!("round {} work", turn + 1),
                    target_sess,
                );
                Ok((
                    "handoff requested".to_string(),
                    None,
                    vec![call],
                    CompletionTokenUsage::default(),
                ))
            } else {
                target_counter.fetch_add(1, Ordering::SeqCst);
                Ok((
                    "target completed".to_string(),
                    None,
                    vec![],
                    CompletionTokenUsage::default(),
                ))
            }
        })
    });
    (model, source_turns, target_calls)
}

async fn wait_for_source_commit_and_terminal(
    harness: &Harness,
    expected_commits: usize,
) -> Result<Vec<(u64, SessionLogEntry)>> {
    let storage_key = harness.session.storage_key();
    let log = NatsSessionLog::new_with_replicas(harness.js.clone(), storage_key, 1);
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        let entries = log.load_events_async().await?;
        let commit_count = entries
            .iter()
            .filter(|(_, entry)| matches!(entry, SessionLogEntry::HandoffCommitted { .. }))
            .count();
        let turn_ends = entries
            .iter()
            .filter(|(_, entry)| matches!(entry, SessionLogEntry::TurnEnd { .. }))
            .count();
        if commit_count >= expected_commits && turn_ends >= expected_commits {
            return Ok(entries);
        }
        anyhow::ensure!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {expected_commits} commits and turn ends"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

async fn execute_reused_round(
    harness: &mut Harness,
    worker_id: &'static str,
    prompt: &str,
    target_sess: &str,
    expected_commits: usize,
) -> Result<()> {
    let mut pause = pause_point(
        harness.session.storage_key(),
        RecoveryFaultPoint::HandoffDispatchBefore,
    );
    harness.activate(prompt).await?;
    pause.wait_reached().await?;

    harness.kill_and_replace_worker(worker_id).await?;
    pause.crash();
    harness.reactivate().await?;
    harness.reactivate().await?;

    harness
        .wait_for_target_completed("target_agent", target_sess)
        .await?;
    wait_for_source_commit_and_terminal(harness, expected_commits).await?;
    recover_closed_round(harness).await?;
    recover_closed_round(harness).await?;
    assert_eq!(
        harness
            .count_target_prompts("target_agent", target_sess)
            .await?,
        1,
        "target must receive exactly one prompt"
    );
    Ok(())
}

async fn verify_distinct_invocations(
    harness: &Harness,
    target_sess_1: &str,
    target_sess_2: &str,
) -> Result<()> {
    let store = SessionMetadataStore::ensure(&harness.js, 1).await?;
    let key1 = harnx_core::session_identity::session_key(Some("target_agent"), target_sess_1);
    let key2 = harnx_core::session_identity::session_key(Some("target_agent"), target_sess_2);

    let entries1 = harness
        .target_entries("target_agent", target_sess_1)
        .await?;
    let entries2 = harness
        .target_entries("target_agent", target_sess_2)
        .await?;

    let prompt_id_1 = entries1
        .iter()
        .find_map(|(_, entry)| match entry {
            SessionLogEntry::Message {
                id: Some(id),
                role,
                content,
                ..
            } if role.is_user()
                && !harnx_core::timing_identity::is_timing_notice(&content.to_text()) =>
            {
                Some(id.as_str())
            }
            _ => None,
        })
        .expect("target 1 prompt id");

    let prompt_id_2 = entries2
        .iter()
        .find_map(|(_, entry)| match entry {
            SessionLogEntry::Message {
                id: Some(id),
                role,
                content,
                ..
            } if role.is_user()
                && !harnx_core::timing_identity::is_timing_notice(&content.to_text()) =>
            {
                Some(id.as_str())
            }
            _ => None,
        })
        .expect("target 2 prompt id");

    let adm1 = store
        .prompt_admission(&key1, prompt_id_1)
        .await?
        .expect("adm 1");
    let adm2 = store
        .prompt_admission(&key2, prompt_id_2)
        .await?
        .expect("adm 2");
    assert_ne!(
        adm1.invocation_id, adm2.invocation_id,
        "targets must have independent invocation IDs"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_reused_tool_call_id_across_rounds_round_scoped_commit() -> Result<()> {
    let shared_id = "call-shared-handoff-id";
    let target_sess_1 = "target-sess-round-1";
    let target_sess_2 = "target-sess-round-2";

    let (model, source_turns, target_calls) =
        make_reused_id_model(shared_id, target_sess_1, target_sess_2);
    let mut harness = Harness::required_start_handoff(BASH_PASS_ALL, model).await?;

    execute_reused_round(
        &mut harness,
        "worker-recovery-r1",
        "start round 1",
        target_sess_1,
        1,
    )
    .await?;
    assert_eq!(harness.count_source_commits().await?, 1);

    execute_reused_round(
        &mut harness,
        "worker-recovery-r2",
        "start round 2",
        target_sess_2,
        2,
    )
    .await?;
    assert_eq!(harness.count_source_commits().await?, 2);

    verify_distinct_invocations(&harness, target_sess_1, target_sess_2).await?;
    assert_eq!(source_turns.load(Ordering::SeqCst), 2);
    assert_eq!(target_calls.load(Ordering::SeqCst), 2);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_script_switch_shaped_json_without_ref_no_handoff() -> Result<()> {
    let (model, model_calls) =
        counting_single_call_model("probe_run", json!({"value": "call_fake"}), "call-fake-1");
    let mut harness = Harness::required_start_handoff(BASH_FAKE_SWITCH_OUTPUT, model).await?;
    let entries =
        recover_non_handoff_results(&mut harness, "start fake switch script test").await?;

    assert_eq!(
        harness
            .count_target_prompts("target_agent", "fake-sess")
            .await?,
        0,
        "script switch JSON without handoff ref must not start target (404/no stream)"
    );
    assert_eq!(harness.count_source_commits().await?, 0);
    assert_eq!(model_calls.load(Ordering::SeqCst), 2);
    let results = assert_single_tool_results(&entries);
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].output["action"], "switch_agent");
    assert!(results[0].switch_agent.is_none());
    assert!(harness.probe.calls.lock().is_empty());
    Ok(())
}

async fn assert_ref_carrying_failure_no_handoff(
    middleware: &str,
    target_sess: &'static str,
    call_id: &'static str,
    expected_error: &str,
) -> Result<()> {
    let (model, source_calls, target_calls) = handoff_single_call_model(
        "target_agent_session_handoff",
        json!({"prompt": "handoff with ref", "session_id": target_sess}),
        call_id,
    );
    let mut harness = Harness::required_start_handoff(middleware, model).await?;
    let entries = recover_non_handoff_results(&mut harness, "start failure test").await?;

    assert_eq!(
        harness
            .count_target_prompts("target_agent", target_sess)
            .await?,
        0,
        "failed handoff with ref must not dispatch to target"
    );
    assert_eq!(harness.count_source_commits().await?, 0);
    assert_eq!(target_calls.load(Ordering::SeqCst), 0);
    assert_eq!(source_calls.load(Ordering::SeqCst), 2);
    let results = assert_single_tool_results(&entries);
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].output["is_error"], true);
    assert!(
        results[0].output.to_string().contains(expected_error),
        "unexpected result: {:?}",
        results[0].output
    );
    assert!(results[0].switch_agent.is_none());
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_denied_ref_carrying_handoff_no_handoff() -> Result<()> {
    assert_ref_carrying_failure_no_handoff(
        BASH_DENY_WITH_REF,
        "target-sess-denied-ref",
        "call-denied-ref-1",
        "denied by policy",
    )
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_error_ref_carrying_handoff_no_handoff() -> Result<()> {
    assert_ref_carrying_failure_no_handoff(
        BASH_ERROR_WITH_REF,
        "target-sess-error-ref",
        "call-error-ref-1",
        "upstream failure",
    )
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_plain_denial_no_handoff() -> Result<()> {
    assert_ref_carrying_failure_no_handoff(
        BASH_PLAIN_DENY,
        "target-sess-plain-deny",
        "call-plain-deny-1",
        "plain denial without next",
    )
    .await
}

async fn seed_pre_t2_2_legacy_log(
    log: &NatsSessionLog,
    target_sess: &str,
    call_id: &str,
) -> Result<()> {
    crate::worker::append_admitted_fixture_user(log, "user-pre-t22", "run prompt").await?;
    let calls_entry = SessionLogEntry::ToolCalls {
        text: "calling tool".into(),
        thought: None,
        calls: vec![handoff_call(
            "target_agent_session_handoff",
            call_id,
            "pre-t2.2 prompt",
            target_sess,
        )],
        timestamp: Some(chrono::Utc::now()),
        fence_token: None,
    };
    log.append_event_async(&calls_entry).await?;

    let legacy_results = SessionLogEntry::ToolResults {
        results: vec![ToolOutput {
            id: Some(call_id.to_string()),
            name: "target_agent_session_handoff".to_string(),
            output: json!({"status": "ok"}),
            markdown: None,
            content: vec![],
            switch_agent: Some(SwitchAgentData {
                agent: "target_agent".to_string(),
                session_id: Some(target_sess.to_string()),
                prompt: "pre-t2.2 prompt".to_string(),
                tool_call_id: Some(call_id.to_string()),
            }),
        }],
        handoff_selection: None,
        timestamp: Some(chrono::Utc::now()),
    };
    log.append_event_async(&legacy_results).await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_pre_t2_2_switch_metadata_without_selection_no_handoff() -> Result<()> {
    let target_sess = "target-sess-pre-t22";
    let model_calls = Arc::new(AtomicUsize::new(0));
    let model_counter = model_calls.clone();
    let model: AgentCallFn = Arc::new(move |_input, _config, _abort| {
        model_counter.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            Ok((
                "continuation completed".to_string(),
                None,
                vec![],
                CompletionTokenUsage::default(),
            ))
        })
    });
    let harness = Harness::required_start_handoff(BASH_PASS_ALL, model).await?;
    let log =
        NatsSessionLog::new_with_replicas(harness.js.clone(), harness.session.storage_key(), 1);

    seed_pre_t2_2_legacy_log(&log, target_sess, "call-pre-t22").await?;

    harness.reactivate().await?;
    let entries = harness.wait_for_turn_end().await?;

    assert!(
        entries
            .iter()
            .any(|(_, e)| matches!(e, SessionLogEntry::TurnEnd { .. })),
        "must record physical TurnEnd"
    );
    assert_eq!(
        harness
            .count_target_prompts("target_agent", target_sess)
            .await?,
        0,
        "pre-T2.2 non-null switch with handoff_selection=None must not dispatch"
    );
    assert_eq!(harness.count_source_commits().await?, 0);
    assert_eq!(
        model_calls.load(Ordering::SeqCst),
        1,
        "model continuation must run"
    );
    Ok(())
}

async fn assert_t2_1_parked_state(
    harness: &Harness,
    pre_len: usize,
    target_sess: &str,
    source_calls: &AtomicUsize,
) -> Result<()> {
    let mid_entries = harness.entries().await?;
    assert_eq!(
        mid_entries.len(),
        pre_len,
        "log must remain unchanged while parked"
    );
    assert!(
        !mid_entries.iter().any(|(_, e)| matches!(
            e,
            SessionLogEntry::TurnEnd { .. } | SessionLogEntry::Cancel { .. }
        )),
        "no TurnEnd or Cancel while parked"
    );
    assert_eq!(
        harness
            .count_target_prompts("target_agent", target_sess)
            .await?,
        0
    );
    assert_eq!(harness.count_source_commits().await?, 0);
    assert_eq!(source_calls.load(Ordering::SeqCst), 1);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_mixed_t2_1_post_results_park_then_t2_2_finish() -> Result<()> {
    let target_sess = "target-sess-mixed";
    let (model, source_calls, target_calls) = handoff_single_call_model(
        "target_agent_session_handoff",
        json!({"prompt": "mixed handoff work", "session_id": target_sess}),
        "call-mixed-1",
    );
    let mut harness = Harness::required_start_handoff(BASH_PASS_ALL, model).await?;
    let storage_key = harness.session.storage_key().to_string();
    let mut pause = pause_point(&storage_key, RecoveryFaultPoint::HandoffDispatchBefore);
    harness.activate("start mixed t2.1 test").await?;
    pause.wait_reached().await?;

    let pre_crash_entries = harness.entries().await?;

    harness
        .kill_and_replace_worker("worker-t2-1-replacement")
        .await?;
    pause.crash();

    simulate_t2_1_worker(&storage_key, true);
    harness.reactivate().await?;

    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while harnx_runtime::fault_injection::t2_1_preflight_count(&storage_key) == 0 {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await?;
    assert_t2_1_parked_state(
        &harness,
        pre_crash_entries.len(),
        target_sess,
        &source_calls,
    )
    .await?;

    simulate_t2_1_worker(&storage_key, false);
    harness
        .kill_and_replace_worker("worker-t2-2-replacement")
        .await?;
    harness.reactivate().await?;

    wait_and_assert_handoff_completion(
        &harness,
        "target_agent",
        target_sess,
        &source_calls,
        &target_calls,
    )
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_sibling_waiting_confirm_prevents_target_start() -> Result<()> {
    let target_sess = "target-sess-sibling";
    let calls = vec![
        tool_call(
            "probe_run",
            "call-sibling-probe",
            json!({"value": "confirm_val"}),
        ),
        handoff_call(
            "target_agent_session_handoff",
            "call-sibling-handoff",
            "sibling handoff",
            target_sess,
        ),
    ];
    let (model, source_calls, target_calls) = handoff_multi_call_model(calls);
    let harness = Harness::required_start_handoff(BASH_CONFIRM_PROBE_PASS_HANDOFF, model).await?;

    harness.activate("start sibling confirm test").await?;
    let (req_seq, interaction_id, _) = harness.wait_for_suspension().await?;
    let suspended = harness.entries().await?;
    assert!(suspended.iter().any(|(_, entry)| matches!(
        entry,
        SessionLogEntry::ToolCallCompleted { tool_call_id, result, .. }
            if tool_call_id == "call-sibling-handoff" && result.switch_agent.is_some()
    )));
    assert!(!suspended
        .iter()
        .any(|(_, entry)| matches!(entry, SessionLogEntry::ToolResults { .. })));
    assert_eq!(target_calls.load(Ordering::SeqCst), 0);
    assert_eq!(source_calls.load(Ordering::SeqCst), 1);

    assert_eq!(
        harness
            .count_target_prompts("target_agent", target_sess)
            .await?,
        0,
        "target must not start while sibling is waiting for confirmation"
    );
    assert_eq!(harness.count_source_commits().await?, 0);

    let outcome = harness.spawn_approve(&interaction_id, req_seq).await??;
    assert_eq!(
        outcome,
        harnx_runtime::interactions::decision::InteractionDecisionOutcome::Applied
    );

    wait_and_assert_handoff_completion(
        &harness,
        "target_agent",
        target_sess,
        &source_calls,
        &target_calls,
    )
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_multiple_handoff_results_existing_first_selection() -> Result<()> {
    let first_sess = "target-sess-multi-first";
    let second_sess = "other-sess-multi-second";
    let calls = vec![
        handoff_call(
            "target_agent_session_handoff",
            "call-multi-1",
            "first target prompt",
            first_sess,
        ),
        handoff_call(
            "other_agent_session_handoff",
            "call-multi-2",
            "second other prompt",
            second_sess,
        ),
    ];
    let (model, source_calls, target_calls) = handoff_multi_call_model(calls);
    let harness = Harness::required_start_handoff(BASH_PASS_ALL, model).await?;
    harness
        .activate("start multiple handoff results test")
        .await?;

    wait_and_assert_handoff_completion(
        &harness,
        "target_agent",
        first_sess,
        &source_calls,
        &target_calls,
    )
    .await?;

    assert_eq!(
        harness
            .count_target_prompts("other_agent", second_sess)
            .await?,
        0,
        "second handoff must not receive prompt"
    );
    Ok(())
}

async fn recover_non_handoff_results(
    harness: &mut Harness,
    prompt: &str,
) -> Result<Vec<(u64, SessionLogEntry)>> {
    let mut pause = pause_point(
        harness.session.storage_key(),
        RecoveryFaultPoint::FinalToolResultsAfter,
    );
    harness.activate(prompt).await?;
    pause.wait_reached().await?;
    let entries = harness.entries().await?;
    assert!(entries.iter().any(|(_, entry)| matches!(
        entry,
        SessionLogEntry::ToolResults {
            handoff_selection: None,
            ..
        }
    )));
    harness
        .kill_and_replace_worker("worker-non-handoff-recovery")
        .await?;
    pause.crash();
    harness.reactivate().await?;
    harness.wait_for_turn_end().await
}

async fn recover_closed_round(harness: &mut Harness) -> Result<()> {
    use async_nats::jetstream::consumer::pull;
    use harnx_runtime::nats_worker::{publish_session_activate, SessionActivate};
    harness
        .kill_and_replace_worker("closed-round-recovery")
        .await?;
    let seq = publish_session_activate(
        &harness.js,
        "local",
        &SessionActivate::new(harness.session.storage_key()),
        1,
    )
    .await?;
    let stream = harness.js.get_stream("WORK_NOTIFY_local").await?;
    let mut consumer = stream
        .get_consumer::<pull::Config>("workers")
        .await
        .map_err(|error| anyhow::anyhow!("read activation consumer: {error}"))?;
    // A settled activation can be acknowledged without taking a lease. Wait
    // for its durable ACK, not the TurnEnd left by the previous activation.
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if consumer.info().await?.ack_floor.stream_sequence >= seq {
                return Ok::<_, anyhow::Error>(());
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await??;
    Ok(())
}
