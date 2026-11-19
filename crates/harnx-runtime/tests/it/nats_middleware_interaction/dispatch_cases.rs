use super::harness::*;
use anyhow::Result;
use harnx_core::session::SessionLogEntry;
use serde_json::json;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bare_repeated_next_rerun_twice_distinct_physical_request_ids() -> Result<()> {
    const BASH_BARE_REPEAT: &str = r#"
middleware:
- name: bare_repeat
  match: '^probe_run$'
  bash: |
    next
    if confirm --id gate 'Proceed?'; then
      respond '{"output":{"done":true}}'
    fi
"#;
    let model = single_call_model(
        "probe_run",
        json!({"value": "original"}),
        "call-bare-repeat",
    );
    let (harness, req_seq, interaction_id, _) =
        Harness::start_and_suspend(BASH_BARE_REPEAT, model, "run bare next repeat").await?;

    // First attempt executed bare next once before suspension
    assert_eq!(harness.probe.invocations.lock().len(), 1);

    // Approve the suspension
    assert!(harness
        .approve(&interaction_id, req_seq)
        .await?
        .is_applied());

    harness.wait_for_turn_end().await?;

    // On resume, bare next reruns, generating a second physical request ID
    let invocations = harness.probe.invocations.lock().clone();
    assert_eq!(invocations.len(), 2, "bare next must rerun after resume");
    let (tool_1, phys_1) = &invocations[0];
    let (tool_2, phys_2) = &invocations[1];
    assert_eq!(tool_1, "run");
    assert_eq!(tool_2, "run");

    assert_ne!(
        phys_1, phys_2,
        "repeated next dispatches must mint distinct physical request IDs"
    );
    assert_ne!(
        phys_1, "call-bare-repeat",
        "physical ID must differ from transcript tool_call_id"
    );
    assert_ne!(
        phys_2, "call-bare-repeat",
        "physical ID must differ from transcript tool_call_id"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn outer_next_twice_inner_confirm_same_msg_distinct_interactions() -> Result<()> {
    const BASH_NESTED_INTERACTIONS: &str = r#"
middleware:
- name: outer
  match: '^probe_run$'
  bash: |
    if saved first > /tmp/r; then :; else
      next --id first > /tmp/r
      save first "$(cat /tmp/r)"
    fi
    next --id second > /tmp/r
    respond "$(cat /tmp/r)"
- name: inner
  match: '^probe_run$'
  bash: |
    confirm 'Proceed?'
    next
"#;
    let model = single_call_model("probe_run", json!({"value": "original"}), "call-nested");
    let harness = Harness::required_start(BASH_NESTED_INTERACTIONS, model).await?;
    harness.activate("run nested interactions").await?;

    // First inner confirm suspension
    let (req_seq_1, interaction_id_1, _) = harness.wait_for_suspension().await?;
    assert!(harness
        .approve(&interaction_id_1, req_seq_1)
        .await?
        .is_applied());

    // Second inner confirm suspension
    let (req_seq_2, interaction_id_2) = {
        let storage_key = harness.session.storage_key();
        let log = harnx_runtime::nats_session_log::NatsSessionLog::new_with_replicas(
            harness.js.clone(),
            storage_key,
            1,
        );
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(20);
        loop {
            let entries = log.load_events_async().await?;
            let second_req = entries.iter().find_map(|(seq, e)| match e {
                SessionLogEntry::InteractionRequested { interaction_id, .. }
                    if interaction_id != &interaction_id_1 =>
                {
                    Some((*seq, interaction_id.clone()))
                }
                _ => None,
            });
            if let Some((seq, id)) = second_req {
                if entries.iter().any(|(_, e)| matches!(e, SessionLogEntry::SuspensionReady { pending, .. } if pending.contains(&id))) {
                    break (seq, id);
                }
            }
            anyhow::ensure!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for second suspension"
            );
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    };

    assert_ne!(
        interaction_id_1, interaction_id_2,
        "identical confirm messages on separate dispatches must generate distinct interaction_ids"
    );

    assert!(harness
        .approve(&interaction_id_2, req_seq_2)
        .await?
        .is_applied());
    harness.wait_for_turn_end().await?;

    Ok(())
}

async fn assert_source_change_after_dispatch_blocks_rerun(
    initial_yaml: &str,
    mutated_yaml: &str,
    tool_dispatched_before_suspension: &str,
    call_id: &'static str,
    activation_msg: &str,
) -> Result<()> {
    let model = single_call_model("probe_run", json!({}), call_id);
    let (harness, req_seq, interaction_id, _) =
        Harness::start_and_suspend(initial_yaml, model, activation_msg).await?;

    // Tool ran once before confirm
    harness
        .probe
        .assert_single_call(tool_dispatched_before_suspension);

    // Mutate middleware source on disk while suspended
    harness.update_agent_middleware(mutated_yaml)?;

    // Approve the suspension
    assert!(harness
        .approve(&interaction_id, req_seq)
        .await?
        .is_applied());

    let entries = harness.wait_for_turn_end().await?;

    // Tool must NOT rerun after source change
    assert_eq!(
        harness.probe.call_count(tool_dispatched_before_suspension),
        1,
        "tool must not rerun after source change"
    );

    // Because downstream effects already happened, middleware reports error
    assert_middleware_changed_error(&entries);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn middleware_source_change_with_dispatch_errors_and_does_not_rerun_helper() -> Result<()> {
    const BASH_WITH_DISPATCH: &str = r#"
middleware:
- name: brancher
  match: '^probe_run$'
  tools: [probe_aux]
  bash: |
    probe_aux --json '{"step":"helper"}'
    confirm 'Proceed with branch?'
    next --id br_a '{"branch":"A"}'
"#;
    const BASH_MUTATED: &str = r#"
middleware:
- name: brancher
  match: '^probe_run$'
  tools: [probe_aux]
  bash: |
    next --id br_changed '{"branch":"mutated"}'
"#;
    assert_source_change_after_dispatch_blocks_rerun(
        BASH_WITH_DISPATCH,
        BASH_MUTATED,
        "probe_aux",
        "call-branch",
        "run branch helper",
    )
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn middleware_source_change_after_next_dispatch_errors_and_does_not_rerun_tool() -> Result<()>
{
    const BASH_AFTER_NEXT: &str = r#"
middleware:
- name: next_gate
  match: '^probe_run$'
  bash: |
    next --id first '{"branch":"A"}'
    confirm 'Keep output?'
"#;
    const BASH_MUTATED: &str = r#"
middleware:
- name: next_gate
  match: '^probe_run$'
  bash: |
    next --id second '{"branch":"B"}'
"#;
    assert_source_change_after_dispatch_blocks_rerun(
        BASH_AFTER_NEXT,
        BASH_MUTATED,
        "probe_run",
        "call-next-dispatch",
        "run next dispatch change",
    )
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn middleware_source_change_without_dispatch_reprompts_cleanly() -> Result<()> {
    const BASH_WITHOUT_DISPATCH: &str = r#"
middleware:
- name: gate
  match: '^probe_run$'
  bash: |
    confirm 'Original prompt?'
    next
"#;
    let model = single_call_model("probe_run", json!({"value": "clean"}), "call-clean-change");
    let (harness, req_seq, interaction_id, _) =
        Harness::start_and_suspend(BASH_WITHOUT_DISPATCH, model, "run clean change").await?;

    // No dispatches occurred before suspension
    assert_eq!(harness.probe.calls.lock().len(), 0);

    // Mutate middleware source on disk while suspended
    harness.update_agent_middleware(
        r#"
middleware:
- name: gate
  match: '^probe_run$'
  bash: |
    confirm 'Mutated prompt?'
    next
"#,
    )?;

    // Approve original request (triggers resume; new chain reprompts)
    let _ = harness.approve(&interaction_id, req_seq).await?;

    // Wait for NEW InteractionRequested with "Mutated prompt?" and SuspensionReady
    let (new_req_seq, new_interaction_id) = {
        let storage_key = harness.session.storage_key();
        let log = harnx_runtime::nats_session_log::NatsSessionLog::new_with_replicas(
            harness.js.clone(),
            storage_key,
            1,
        );
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(20);
        loop {
            let entries = log.load_events_async().await?;
            let reprompt = entries.iter().find_map(|(seq, e)| match e {
                SessionLogEntry::InteractionRequested {
                    interaction_id: id,
                    summary,
                    ..
                } if id != &interaction_id && summary == "Mutated prompt?" => {
                    Some((*seq, id.clone()))
                }
                _ => None,
            });
            if let Some((seq, id)) = reprompt {
                if entries.iter().any(|(_, e)| matches!(e, SessionLogEntry::SuspensionReady { pending, .. } if pending.contains(&id))) {
                    break (seq, id);
                }
            }
            anyhow::ensure!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for mutated reprompt"
            );
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    };

    // Old answer does not authorize new prompt; probe calls remain 0
    assert_eq!(harness.probe.calls.lock().len(), 0);

    // Approve the new request
    assert!(harness
        .approve(&new_interaction_id, new_req_seq)
        .await?
        .is_applied());

    // Finish turn and assert tool executed exactly once
    let entries = harness.wait_for_turn_end().await?;
    assert_eq!(harness.probe.calls.lock().len(), 1);
    let results = extract_tool_results(&entries);
    assert_eq!(results.len(), 1);
    assert_ne!(results[0].output.get("is_error"), Some(&json!(true)));
    assert_eq!(results[0].output["value"], "clean");

    Ok(())
}
