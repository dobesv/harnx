use super::harness::*;
use anyhow::Result;
use harnx_core::{session::SessionLogEntry, tool::ToolCall};
use harnx_engine::tool::ToolUseConfirmation;
use harnx_runtime::client::CompletionTokenUsage;
use serde_json::json;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

const BASH_CONFIRM: &str = r#"
middleware:
- name: gate
  match: '^probe_run$'
  bash: |
    if confirm --id gate 'Proceed?'; then
      next
    else
      deny 'user denied'
    fi
"#;

use super::harness::probe_test_two_call_model;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_suspends_b_completes_handler_count_one_and_combined_tool_results_on_approve(
) -> Result<()> {
    let (model, _) = probe_test_two_call_model();
    let (harness, req_seq, interaction_id, _) =
        Harness::start_and_suspend(BASH_CONFIRM, model, "start two calls").await?;

    // While suspended: Tool B (probe_aux) has executed once; Tool A (probe_run) suspended before execution
    assert_eq!(
        harness.probe.call_count("probe_aux"),
        1,
        "Tool B must complete immediately"
    );
    assert_eq!(
        harness.probe.call_count("probe_run"),
        0,
        "Tool A must be suspended"
    );

    // Approve the suspension
    assert!(harness
        .approve(&interaction_id, req_seq)
        .await?
        .is_applied());

    // Wait for the round and turn to complete
    let entries = harness.wait_for_turn_end().await?;

    // Tool B must NOT have been re-executed: total calls to probe_aux is still 1
    assert_eq!(
        harness.probe.call_count("probe_aux"),
        1,
        "Tool B handler count must remain 1"
    );
    assert_eq!(
        harness.probe.call_count("probe_run"),
        1,
        "Tool A must execute once on approve"
    );

    // Exactly one combined ToolResults entry with results for both calls
    let results = extract_tool_results(&entries);
    assert_eq!(
        results.len(),
        2,
        "ToolResults must contain results for both A and B"
    );
    assert!(results.iter().any(
        |r| r.id.as_deref() == Some("call-a") && r.output.get("is_error") != Some(&json!(true))
    ));
    assert!(results.iter().any(
        |r| r.id.as_deref() == Some("call-b") && r.output.get("is_error") != Some(&json!(true))
    ));

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn different_worker_b_execution_context_observation_persists_not_transcript() -> Result<()> {
    let (model, seen_messages) = probe_test_two_call_model();
    let (mut harness, req_seq, interaction_id, _) =
        Harness::start_and_suspend(BASH_CONFIRM, model, "start observation test").await?;

    // Restart daemon under worker identity B while suspended
    harness
        .restart_worker_with_identity("worker-middleware-worker-b")
        .await?;

    assert!(harness
        .approve(&interaction_id, req_seq)
        .await?
        .is_applied());

    let entries = harness.wait_for_turn_end().await?;

    // 1. Tool B must NOT have been re-executed by replacement worker
    assert_eq!(
        harness.probe.call_count("probe_aux"),
        1,
        "Tool B must not be re-executed by replacement worker"
    );
    assert_eq!(
        harness.probe.call_count("probe_run"),
        1,
        "Tool A must execute once on replacement worker"
    );

    // 2. Tool B completed with private observation snapshot in log
    assert!(
        entries.iter().any(|(_, e)| matches!(
            e,
            SessionLogEntry::ToolCallCompleted { tool_call_id, private_snapshot, .. }
                if tool_call_id == "call-b" && !private_snapshot.is_empty()
        )),
        "ToolCallCompleted for call-b must persist private observation snapshot"
    );

    // 3. Session metadata store holds the observed execution context
    let store =
        harnx_runtime::nats_session_metadata::SessionMetadataStore::ensure(&harness.js, 1).await?;
    let record = store
        .get(harness.session.storage_key())
        .await?
        .expect("session metadata record");
    let contexts = harnx_runtime::nats_session_metadata::execution_contexts(&record.metadata)?;
    assert!(
        contexts.iter().any(|c| c.workspace_root.contains("/workspace/probe_b")),
        "observed execution context /workspace/probe_b must survive into session metadata: {contexts:?}"
    );

    // 4. Public ToolResults does NOT contain private observation
    for (_, entry) in &entries {
        if let SessionLogEntry::ToolResults { results, .. } = entry {
            let serialized = serde_json::to_string(results).unwrap();
            assert!(
                !serialized.contains("/workspace/probe_b"),
                "public ToolResults must not leak private observation: {serialized}"
            );
        }
    }

    // 5. Model wire messages built on turn 2 do NOT contain private observation
    let msgs = seen_messages.lock().clone();
    assert!(
        !msgs.is_empty(),
        "model messages must be captured on second turn"
    );
    for msg in &msgs {
        let serialized = serde_json::to_string(msg).unwrap();
        assert!(
            !serialized.contains("/workspace/probe_b"),
            "model wire messages must not leak private observation: {serialized}"
        );
    }

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_callback_log_before_rerun() -> Result<()> {
    let turns = Arc::new(AtomicUsize::new(0));
    let t = turns.clone();
    let model: harnx_runtime::agent_loop::AgentCallFn = Arc::new(move |_input, config, _abort| {
        let first = t.fetch_add(1, Ordering::SeqCst) == 0;
        if first {
            config
                .write()
                .set_tui_confirm_tool_use(Some(Arc::new(|_, _, _| ToolUseConfirmation::Approve)));
        }
        Box::pin(async move {
            let calls = if first {
                vec![ToolCall::new(
                    "probe_run".into(),
                    json!({"value": "call_a"}),
                    Some("call-live".into()),
                    None,
                )]
            } else {
                vec![]
            };
            Ok((
                "activation completed".to_string(),
                None,
                calls,
                CompletionTokenUsage::default(),
            ))
        })
    });

    let harness = Harness::required_start(BASH_CONFIRM, model).await?;
    harness.activate("start live callback log test").await?;

    // Turn completes automatically through live callback without external decide_interaction call
    let entries = harness.wait_for_turn_end().await?;

    let req_seq = entries
        .iter()
        .find_map(|(seq, e)| match e {
            SessionLogEntry::InteractionRequested { tool_call_id, .. }
                if tool_call_id == "call-live" =>
            {
                Some(*seq)
            }
            _ => None,
        })
        .expect("InteractionRequested must exist in durable log");

    let ready_seq = entries
        .iter()
        .find_map(|(seq, e)| match e {
            SessionLogEntry::SuspensionReady { .. } => Some(*seq),
            _ => None,
        })
        .expect("SuspensionReady must exist in durable log");

    let resp_seq = entries
        .iter()
        .find_map(|(seq, e)| match e {
            SessionLogEntry::InteractionResponse { tool_call_id, .. }
                if tool_call_id == "call-live" =>
            {
                Some(*seq)
            }
            _ => None,
        })
        .expect("InteractionResponse must exist in durable log");

    let completed_seq = entries
        .iter()
        .find_map(|(seq, e)| match e {
            SessionLogEntry::ToolCallCompleted { tool_call_id, .. }
                if tool_call_id == "call-live" =>
            {
                Some(*seq)
            }
            _ => None,
        })
        .expect("ToolCallCompleted must exist in durable log");

    assert!(
        req_seq < ready_seq,
        "InteractionRequested must precede SuspensionReady"
    );
    assert!(
        ready_seq < resp_seq,
        "SuspensionReady must precede InteractionResponse"
    );
    assert!(
        resp_seq < completed_seq,
        "InteractionResponse must be durable before ToolCallCompleted"
    );

    let invocations = harness.probe.invocations.lock().clone();
    assert_eq!(
        invocations.len(),
        1,
        "exactly 1 physical invocation executed"
    );
    assert_eq!(invocations[0].0, "run");

    Ok(())
}
