//! Per-invocation timeout and token-budget integration tests.

use super::*;
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
    Arc::new(move |_input, _config, abort| {
        let call_index = call_count.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            if call_index == 0 {
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
        session.run_turn_with_options(prompt, Arc::new(NoopEventSink), None, options),
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
        toolset.invoke("session_prompt", arguments, CancellationToken::new()),
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
            json!("done"),
        )
    );
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

fn assert_budget_result(stopped: &serde_json::Value, session_id: &str) {
    assert_eq!(
        (
            stopped["session_id"].clone(),
            stopped["termination"]["kind"].clone(),
            stopped["termination"]["session_id"].clone(),
            stopped["termination"]["usage"].clone(),
            stopped["sub_agent_progress"]["status"].clone(),
        ),
        (
            json!(session_id),
            json!("budget_exceeded"),
            json!(session_id),
            json!({"input_uncached": 2, "cache_write": 0, "output": 1, "budgeted": 3}),
            json!("done"),
        )
    );
    let response = stopped["response"]
        .as_str()
        .expect("budget result has synthesized response text");
    assert!(
        response.contains("reached its token budget (used 3 of 1 budgeted tokens)")
            && response.contains(&format!("same session id `{session_id}`"))
    );
}

async fn assert_subagent_retry(
    toolset: &super::super::subagent_toolset::SubagentToolset,
    arguments: serde_json::Value,
    expected_response: &str,
    call_count: &AtomicUsize,
) {
    let retry = invoke_subagent_prompt(toolset, arguments).await;
    assert_eq!(retry["response"], expected_response);
    assert!(retry.get("termination").is_none());
    assert_eq!(call_count.load(Ordering::SeqCst), 2);
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
    };

    let first = run_budget_test_turn(&session, "run one tool round", options).await;
    let terminal = first
        .error
        .as_deref()
        .and_then(crate::parse_budget_terminal)
        .unwrap_or_else(|| panic!("worker error was not a budget terminal: {:?}", first.error));
    assert_eq!((terminal.budgeted, terminal.budget), (3, 1));
    assert_eq!(call_count.load(Ordering::SeqCst), 1);

    let log = NatsSessionLog::new(async_nats::jetstream::new(client.clone()), session_id);
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

    let log = NatsSessionLog::new(
        seeded
            .parent_config
            .nats_jetstream("local")
            .await
            .expect("timeout child log jetstream"),
        session_id.clone(),
    );
    let entries = log
        .load_events_async()
        .await
        .expect("load timeout child log");
    assert!(entries
        .iter()
        .any(|(_, entry)| matches!(entry, SessionLogEntry::Cancel { .. })));

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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn subagent_token_budget_returns_synthesized_result() {
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
        .expect("sub-agent budget test worker should register");
    let toolset = test_subagent_toolset(&url).await;
    let session_id = crate::nats_worker::new_remote_session_id();

    let stopped = invoke_subagent_prompt(
        &toolset,
        json!({
            "message": "run one tool round within a tiny budget",
            "session_id": session_id,
            "token_budget": 1
        }),
    )
    .await;
    assert_budget_result(&stopped, &session_id);

    assert_subagent_retry(
        &toolset,
        json!({
            "message": "retry after the budget stop",
            "session_id": session_id,
            "token_budget": 1
        }),
        "same-session retry completed",
        &call_count,
    )
    .await;

    daemon.abort();
    let _ = daemon.await;
    let _ = nats.kill();
    let _ = nats.wait();
}
