//! Per-invocation timeout and token-budget integration tests.

use super::*;
use crate::nats_session::test_support::InheritedTestTool;
use std::sync::atomic::AtomicUsize;

fn budget_boundary_call_fn(call_count: Arc<AtomicUsize>) -> crate::agent_loop::AgentCallFn {
    Arc::new(move |_input, _config, _abort| {
        let call_index = call_count.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            let usage = crate::client::CompletionTokenUsage {
                input_tokens: 2,
                output_tokens: 1,
                cached_tokens: 0,
                cache_write_tokens: 0,
            };
            harnx_core::sink::emit_agent_event(AgentEvent::Model(
                harnx_core::event::ModelEvent::Usage {
                    input: usage.input_tokens,
                    output: usage.output_tokens,
                    cached: usage.cached_tokens,
                    cache_write: usage.cache_write_tokens,
                    session_label: None,
                },
            ));
            let output = if call_index == 0 {
                (
                    "calling tool before budget boundary".to_string(),
                    None,
                    vec![ToolCall::new(
                        "missing_tool".to_string(),
                        json!({}),
                        Some("budget-tool-call".to_string()),
                        None,
                    )],
                    usage,
                )
            } else {
                (
                    "same-session retry completed".to_string(),
                    None,
                    vec![],
                    usage,
                )
            };
            Ok(output)
        })
    })
}

fn timeout_then_reply_call_fn(call_count: Arc<AtomicUsize>) -> crate::agent_loop::AgentCallFn {
    Arc::new(move |input, _config, abort| {
        call_count.fetch_add(1, Ordering::SeqCst);
        // The deadline can win before G1 reaches the model. G2 must not inherit
        // the first-call stall merely because startup was slower under load.
        let timed_prompt = input.text() == "work until the invocation deadline";
        Box::pin(async move {
            if timed_prompt {
                harnx_core::abort::wait_abort_signal(&abort).await;
                bail!("timed-out child call aborted")
            }
            Ok((
                "same-session retry after timeout completed".to_string(),
                None,
                vec![],
                crate::client::CompletionTokenUsage::default(),
            ))
        })
    })
}

async fn run_budget_test_turn(
    session: &NatsSession,
    prompt: &str,
    options: crate::RunTurnOptions,
) -> crate::NatsTurnResult {
    tokio::time::timeout(
        NATS_TEST_CONDITION_TIMEOUT,
        session
            .clone()
            .with_external_admission()
            .run_turn_with_options(prompt, Arc::new(NoopEventSink), None, options),
    )
    .await
    .expect("budget test turn timed out")
    .expect("budget test turn transport failed")
}

fn assert_budget_terminal_transcript(entries: &[(u64, SessionLogEntry)]) {
    let error_message = entries.iter().find_map(|(_, entry)| match entry {
        SessionLogEntry::Error { message, .. } => Some(message),
        _ => None,
    });
    assert_eq!(
        error_message.map(String::as_str),
        Some(crate::budget_terminal_message(3, 1).as_str())
    );
    assert!(entries
        .iter()
        .any(|(_, entry)| matches!(entry, SessionLogEntry::ToolCalls { .. })));
    assert!(entries
        .iter()
        .any(|(_, entry)| matches!(entry, SessionLogEntry::ToolResults { .. })));
    assert!(reconstruct_state_from_nats(entries).resumable_ctx.is_none());
}

async fn invoke_subagent_prompt(
    toolset: &super::super::subagent_toolset::SubagentToolset,
    arguments: serde_json::Value,
) -> serde_json::Value {
    tokio::time::timeout(
        NATS_TEST_CONDITION_TIMEOUT,
        toolset.invoke_inherited("session_prompt", arguments, CancellationToken::new()),
    )
    .await
    .expect("bounded sub-agent tool call did not return")
    .expect("bounded stop must be returned as an Ok tool result")
}

fn assert_timeout_result(stopped: &serde_json::Value, session_id: &str) {
    assert_eq!(
        (
            stopped["session_id"].clone(),
            stopped["termination"]["kind"].clone(),
            stopped["termination"]["session_id"].clone(),
            stopped["termination"]["usage"]["budgeted"].clone(),
            stopped["sub_agent_progress"]["status"].clone(),
        ),
        (
            json!(session_id),
            json!("timeout"),
            json!(session_id),
            json!(0),
            json!("cancelled"),
        )
    );
    assert_eq!(stopped["termination"]["scope"], "local_invocation");
    assert!(stopped["termination"]["deadline"].as_str().is_some());
    assert_eq!(
        stopped["termination"]["public_progress"]["available"],
        false
    );
    assert_eq!(
        stopped["termination"]["public_progress"]["references"],
        json!([])
    );
    assert!(stopped["termination"]["retry_hint"]
        .as_str()
        .unwrap()
        .contains("Do not retry unchanged"));
    let response = stopped["response"]
        .as_str()
        .expect("timeout result has synthesized response text");
    assert!(
        response.contains("stopped after reaching its time limit")
            && response.contains("No thinking text was captured")
            && response.contains(&format!("same session id `{session_id}`"))
            && response.contains("Usage: used 0 budgeted tokens.")
    );
}

async fn assert_subagent_retry(
    toolset: &super::super::subagent_toolset::SubagentToolset,
    arguments: serde_json::Value,
    expected_response: &str,
    call_count: &AtomicUsize,
) {
    let before = call_count.load(Ordering::SeqCst);
    let retry = invoke_subagent_prompt(toolset, arguments).await;
    assert_eq!(retry["response"], expected_response);
    assert!(retry.get("termination").is_none());
    assert_eq!(call_count.load(Ordering::SeqCst), before + 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn token_budget_stops_at_round_boundary_and_resets_for_next_activation() {
    let _env_guard = env_lock().await;

    let Some((url, mut nats, _store_dir)) = spawn_test_nats().await else {
        return;
    };
    let seeded = seed_remote_config(&url);
    let _env = subagent_test_env(&url, &seeded);
    let call_count = Arc::new(AtomicUsize::new(0));
    let daemon =
        spawn_metis_worker_with_call_fn(&url, budget_boundary_call_fn(Arc::clone(&call_count)));
    subagent_discovery_tests::wait_for_cluster_worker(&seeded.parent_config, "local")
        .await
        .expect("budget test worker should register");

    let client = async_nats::connect(&url)
        .await
        .expect("connect budget test NATS client");
    let session_id = crate::nats_worker::new_remote_session_id();
    let session = NatsSession::new(
        cluster_shared_session_config("local", session_id.clone()),
        client.clone(),
        async_nats::jetstream::new(client.clone()),
        harnx_core::abort::create_abort_signal(),
    )
    .await
    .expect("create budget test NATS session");
    let options = crate::RunTurnOptions {
        token_budget: Some(1),
        ..Default::default()
    };

    let first = run_budget_test_turn(&session, "run one tool round", options).await;
    let terminal = first
        .error
        .as_deref()
        .and_then(crate::parse_budget_terminal)
        .unwrap_or_else(|| panic!("worker error was not a budget terminal: {:?}", first.error));
    assert_eq!((terminal.budgeted, terminal.budget), (3, 1));
    assert_eq!(call_count.load(Ordering::SeqCst), 1);

    let log = NatsSessionLog::for_agent(
        async_nats::jetstream::new(client.clone()),
        "metis",
        &session_id,
    )
    .with_replicas(1);
    assert_budget_terminal_transcript(
        &log.load_events_async()
            .await
            .expect("load budget-limited transcript"),
    );

    let retry = run_budget_test_turn(&session, "retry in the same session", options).await;
    assert_eq!(
        retry.response.as_deref(),
        Some("same-session retry completed")
    );
    assert!(retry.error.is_none());
    assert_eq!(call_count.load(Ordering::SeqCst), 2);

    daemon.abort();
    let _ = daemon.await;
    let _ = nats.kill();
    let _ = nats.wait();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn subagent_timeout_returns_synthesized_result_and_same_session_retry_succeeds() {
    let _env_guard = env_lock().await;
    let Some((url, mut nats, _store_dir)) = spawn_test_nats().await else {
        return;
    };
    let seeded = seed_remote_config(&url);
    let _env = subagent_test_env(&url, &seeded);
    let call_count = Arc::new(AtomicUsize::new(0));
    let daemon =
        spawn_metis_worker_with_call_fn(&url, timeout_then_reply_call_fn(Arc::clone(&call_count)));
    subagent_discovery_tests::wait_for_cluster_worker(&seeded.parent_config, "local")
        .await
        .expect("timeout test worker should register");
    let toolset = test_subagent_toolset(&url).await;
    let session_id = crate::nats_worker::new_remote_session_id();

    let stopped = invoke_subagent_prompt(
        &toolset,
        json!({
            "message": "work until the invocation deadline",
            "session_id": session_id,
            "timeout_secs": 1
        }),
    )
    .await;
    assert_timeout_result(&stopped, &session_id);

    // The bounded stop is one `Cancel` in the child's own log, naming the
    // caller that imposed the deadline. Nothing else records the termination,
    // and the turn it ended is over rather than resumable.
    let js = seeded.parent_config.nats_jetstream("local").await.unwrap();
    let storage_key = harnx_core::session_identity::session_key(Some("metis"), &session_id);
    let entries = NatsSessionLog::new_with_replicas(js, &storage_key, 1)
        .load_events_latest_async()
        .await
        .unwrap();
    assert!(
        entries.iter().any(|(_, entry)| matches!(
            entry,
            SessionLogEntry::Cancel {
                requested_by: Some(_),
                ..
            }
        )),
        "the timed-out child's log records who stopped it: {entries:?}"
    );
    assert!(reconstruct_state_from_nats(&entries)
        .resumable_ctx
        .is_none());

    assert_subagent_retry(
        &toolset,
        json!({
            "message": "retry after the bounded stop",
            "session_id": session_id
        }),
        "same-session retry after timeout completed",
        &call_count,
    )
    .await;

    daemon.abort();
    let _ = daemon.await;
    let _ = nats.kill();
    let _ = nats.wait();
}

#[tokio::test]
async fn expired_child_admission_returns_scoped_advice_without_model_dispatch() {
    use crate::nats_session_metadata::{CallTimeoutOverride, RunLimitsRecord};
    use harnx_toolset::{AutonomousRunContext, ToolInvocation, ToolInvocationContext, Toolset};

    let _env_guard = env_lock().await;
    let (url, mut nats, _store_dir) = spawn_test_nats().await.expect("nats-server required");
    let seeded = seed_remote_config(&url);
    let _env = subagent_test_env(&url, &seeded);
    let calls = Arc::new(AtomicUsize::new(0));
    let daemon = spawn_metis_worker_with_call_fn(&url, timeout_then_reply_call_fn(calls.clone()));
    subagent_discovery_tests::wait_for_cluster_worker(&seeded.parent_config, "local")
        .await
        .expect("worker registration");
    let toolset = test_subagent_toolset(&url).await;
    let original = chrono::Utc::now() - chrono::Duration::seconds(30);
    for (scope, parent_allowance, local_allowance) in [
        ("inherited_deadline", 1, 60),
        ("local_invocation", 86400, 1),
    ] {
        let parent = RunLimitsRecord::admit_root(
            Default::default(),
            Default::default(),
            original,
            Default::default(),
            None,
            CallTimeoutOverride::from_optional(Some(parent_allowance)),
        )
        .unwrap();
        let session_id = crate::nats_worker::new_remote_session_id();
        let call_id = uuid::Uuid::new_v4().to_string();
        let result = tokio::time::timeout(NATS_TEST_CONDITION_TIMEOUT, toolset.invoke_with_context(ToolInvocation {
            tool: "session_prompt".into(),
            args: json!({"message": "work until the invocation deadline", "session_id": session_id, "timeout_secs": local_allowance}),
            cancel: CancellationToken::new(),
            context: ToolInvocationContext {
                call_id: call_id.clone(),
                run_context: Some(AutonomousRunContext {
                    snapshot: serde_json::to_value(&parent).unwrap(),
                    started_at_ms: original.timestamp_millis().try_into().unwrap(),
                }),
                ..Default::default()
            },
        })).await.expect("admission result timeout").expect("expired admission returns a tool result");
        assert_eq!(result["session_id"], session_id);
        assert_eq!(result["termination"]["session_id"], session_id);
        assert_eq!(result["termination"]["kind"], "timeout");
        assert_eq!(result["termination"]["scope"], scope);
        assert_eq!(result["termination"]["run_id"], parent.run_id.as_str());
        assert_eq!(result["termination"]["invocation_id"], call_id);
        assert_eq!(result["termination"]["usage"]["budgeted"], 0);
        assert_eq!(
            result["termination"]["thinking_excerpt"],
            serde_json::Value::Null
        );
        assert_eq!(result["termination"]["public_progress"]["available"], false);
        assert_eq!(result["sub_agent_progress"]["status"], "cancelled");
        let advice = result["termination"]["retry_hint"].as_str().unwrap();
        assert!(result["response"].as_str().unwrap().contains(advice));
        assert!(result["response"]
            .as_str()
            .unwrap()
            .contains("Public progress unavailable"));
        if scope == "inherited_deadline" {
            assert!(advice.contains("The inherited deadline expired."));
            assert!(advice.contains("Do not retry:"));
            assert!(advice.contains("Return to the user to confirm continuation"));
        } else {
            assert!(advice.contains("revise or narrow instructions"));
            assert!(advice.contains("only while the outer run remains live"));
            assert!(advice.contains(&session_id));
        }
    }
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "expired admissions must never call the model"
    );
    daemon.abort();
    let _ = daemon.await;
    let _ = nats.kill();
    let _ = nats.wait();
}
