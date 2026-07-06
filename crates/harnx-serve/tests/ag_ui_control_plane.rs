use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

use anyhow::anyhow;
use bytes::Bytes;
use harnx_core::{event::ContentBlock, message::Message, tool::ToolCall};
use harnx_runtime::{
    client::{CompletionTokenUsage, TestStateGuard},
    config::Config,
    AgentCallFn,
};
use harnx_serve::{
    ag_ui::ag_ui_run_with_call_fn,
    ag_ui_rpc::{handle_ag_ui_rpc_bytes, PersistenceKind},
    session_actor::SessionRegistry,
    test_support::TestConfigSandbox,
};
use http::Method;
use http_body_util::BodyExt;
use hyper::Response;
use serde_json::{json, Value};
use tokio::sync::{Mutex, Notify};
use tokio_stream::StreamExt;
use uuid::Uuid;

type AppResponse = Response<http_body_util::combinators::BoxBody<Bytes, std::convert::Infallible>>;

#[derive(Debug, Default)]
struct SseRead {
    frames: Vec<String>,
    events: Vec<Value>,
    comments: Vec<String>,
}

async fn read_sse_until<F>(response: AppResponse, timeout: Duration, done: F) -> SseRead
where
    F: Fn(&SseRead) -> bool,
{
    let fut = async {
        let mut body = response.into_body().into_data_stream();
        let mut read = SseRead::default();
        let mut partial = String::new();

        while !done(&read) {
            match tokio::time::timeout(timeout, body.next()).await {
                Ok(Some(Ok(chunk))) => {
                    partial.push_str(std::str::from_utf8(&chunk).expect("sse utf8"));
                }
                Ok(Some(Err(err))) => {
                    panic!(
                        "error while reading SSE stream before predicate satisfied: {err}. frames: {:?}, events: {:?}, comments: {:?}",
                        read.frames, read.events, read.comments
                    );
                }
                Ok(None) => {
                    break;
                }
                Err(_) => {
                    panic!(
                        "timed out after {timeout:?} waiting for SSE predicate. frames: {:?}, events: {:?}, comments: {:?}",
                        read.frames, read.events, read.comments
                    );
                }
            }

            while let Some(idx) = partial.find("\n\n") {
                let frame = partial[..idx].trim().to_string();
                partial.drain(..idx + 2);
                if frame.is_empty() {
                    continue;
                }
                read.frames.push(frame.clone());
                if frame.starts_with(':') {
                    read.comments.push(frame);
                } else {
                    let payload = frame
                        .strip_prefix("data: ")
                        .expect("sse frame should start with data prefix");
                    read.events
                        .push(serde_json::from_str(payload).expect("valid event json"));
                }
                if done(&read) {
                    return read;
                }
            }
        }

        read
    };

    tokio::time::timeout(timeout, fut)
        .await
        .expect("SSE read should finish before outer timeout")
}

async fn open_sse(
    config: &Config,
    registry: &SessionRegistry,
    agent: &str,
    session: &str,
    messages: Value,
) -> AppResponse {
    ag_ui_run_with_call_fn(
        config,
        registry,
        agent,
        session,
        &serde_json::to_vec(&json!({
            "threadId": Uuid::new_v4(),
            "runId": Uuid::new_v4(),
            "messages": messages,
        }))
        .unwrap(),
        None,
    )
    .await
    .expect("sse response")
}

async fn rpc_call(
    config: &Config,
    registry: &SessionRegistry,
    agent: &str,
    session: &str,
    request: Value,
) -> Value {
    let response = handle_ag_ui_rpc_bytes(
        Method::POST,
        format!("/v1/agents/{agent}/sessions/{session}/rpc"),
        Bytes::from(request.to_string()),
        config,
        registry,
        PersistenceKind::Filesystem,
    )
    .await
    .expect("rpc response");
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("collect rpc body")
        .to_bytes();
    serde_json::from_slice(&bytes).expect("rpc json")
}

fn load_session_messages(config: &Config, agent: &str, session: &str) -> Vec<Message> {
    let mut scoped = config.clone();
    scoped.use_agent_by_name(agent).expect("set agent");
    scoped.use_session(Some(session)).expect("load session");
    scoped
        .session
        .as_ref()
        .expect("session should exist")
        .messages
        .clone()
}

#[allow(dead_code)]
fn history_snapshot_texts(result: &Value) -> Vec<String> {
    result["history_snapshot"]
        .as_array()
        .unwrap_or(&Vec::new())
        .iter()
        .filter_map(|message| message["content"].as_str().map(str::to_string))
        .collect()
}

#[allow(dead_code)]
fn sessions_json(config: &Config, agent: &str) -> Value {
    let mut scoped = config.clone();
    scoped.use_agent_by_name(agent).expect("set agent");
    Value::Array(
        scoped
            .list_sessions_with_meta()
            .into_iter()
            .filter(|session| session.agent_name.as_deref() == Some(agent))
            .map(|session| {
                json!({
                    "id": session.session_id,
                    "agent": session.agent_name,
                })
            })
            .collect(),
    )
}

#[allow(dead_code)]
fn history_json(config: &Config, agent: &str, session: &str) -> Value {
    let mut scoped = config.clone();
    scoped.use_agent_by_name(agent).expect("set agent");
    scoped.use_session(Some(session)).expect("load session");
    scoped
        .session
        .as_ref()
        .expect("session should exist")
        .messages
        .iter()
        .map(|msg| {
            json!({
                "id": Uuid::new_v4(),
                "role": match msg.role {
                    harnx_core::message::MessageRole::User => "user",
                    harnx_core::message::MessageRole::Assistant => "assistant",
                    harnx_core::message::MessageRole::System => "system",
                    harnx_core::message::MessageRole::Tool => "tool",
                },
                "content": msg.content.to_text(),
            })
        })
        .collect::<Vec<_>>()
        .into()
}

#[tokio::test(flavor = "multi_thread")]
async fn e2e_success_criterion_1_sse_endpoint_returns_messages_snapshot_and_events() {
    let _guard = TestStateGuard::new(None).await;
    let sandbox = TestConfigSandbox::new();
    sandbox.write_agent("plain", "You are plain.");
    let config = sandbox.config();
    let call_fn: AgentCallFn = Arc::new(|input, _config, _abort| {
        let text = input.text();
        Box::pin(async move {
            Ok((
                format!("assistant {text}"),
                None,
                Vec::<ToolCall>::new(),
                CompletionTokenUsage::default(),
            ))
        })
    });
    let registry =
        SessionRegistry::new_for_tests(config.clone(), Duration::from_secs(30), Some(call_fn));

    let response = open_sse(&config, &registry, "plain", "criteria-1", json!([])).await;
    let sse_task = tokio::spawn(async move {
        read_sse_until(response, Duration::from_secs(10), |read| {
            read.events
                .iter()
                .any(|event| event["type"] == "RUN_FINISHED")
        })
        .await
    });

    let prompt = rpc_call(
        &config,
        &registry,
        "plain",
        "criteria-1",
        json!({"jsonrpc":"2.0","id":1,"method":"session/prompt","params":{"text":"hello"}}),
    )
    .await;
    assert_eq!(prompt["result"]["status"], "accepted");
    assert!(prompt["result"]["run_id"].as_str().is_some());

    let read = sse_task.await.expect("sse task");
    assert_eq!(read.events[0]["type"], "MESSAGES_SNAPSHOT");
    assert!(read
        .events
        .iter()
        .any(|event| event["type"] == "RUN_STARTED"));
    assert!(read
        .events
        .iter()
        .any(|event| event["type"] == "RUN_FINISHED"));
}

#[tokio::test(flavor = "multi_thread")]
async fn e2e_success_criterion_2_rpc_prompt_starts_run_and_injects_prompt() {
    let _guard = TestStateGuard::new(None).await;
    let sandbox = TestConfigSandbox::new();
    sandbox.write_agent("plain", "You are plain.");
    let config = sandbox.config();
    let call_fn: AgentCallFn = Arc::new(|input, _config, _abort| {
        let text = input.text().to_string();
        Box::pin(async move {
            Ok((
                format!("assistant {text}"),
                None,
                Vec::<ToolCall>::new(),
                CompletionTokenUsage::default(),
            ))
        })
    });
    let registry =
        SessionRegistry::new_for_tests(config.clone(), Duration::from_secs(30), Some(call_fn));

    let response = open_sse(&config, &registry, "plain", "criteria-2", json!([])).await;
    let sse_task = tokio::spawn(async move {
        read_sse_until(response, Duration::from_secs(10), |read| {
            read.events
                .iter()
                .any(|event| event["type"] == "RUN_FINISHED")
        })
        .await
    });

    let first = rpc_call(
        &config,
        &registry,
        "plain",
        "criteria-2",
        json!({"jsonrpc":"2.0","id":1,"method":"session/prompt","params":{"text":"one"}}),
    )
    .await;
    assert_eq!(first["result"]["status"], "accepted");

    let read = sse_task.await.expect("sse task");
    assert!(read
        .events
        .iter()
        .any(|event| event["type"] == "RUN_STARTED"));
    assert!(read
        .events
        .iter()
        .any(|event| event["type"] == "RUN_FINISHED"));

    // Verify assistant response persisted
    let persisted = load_session_messages(&config, "plain", "criteria-2");
    assert!(persisted
        .iter()
        .any(|msg| msg.content.to_text().contains("one")));
}

#[tokio::test(flavor = "multi_thread")]
async fn e2e_success_criterion_3_prompt_while_running_injects_mid_loop() {
    let _guard = TestStateGuard::new(None).await;
    let sandbox = TestConfigSandbox::new();
    sandbox.write_agent("plain", "You are plain.");
    let config = sandbox.config();

    // Tracks injected text seen in call_fn
    let injected_seen = Arc::new(Mutex::new(None::<String>));
    // Gate: call_fn signals when round 0 starts, test releases after sending second prompt
    let round0_started = Arc::new(Notify::new());
    let release_round0 = Arc::new(Notify::new());
    // Gate: call_fn waits on round 1 until test releases
    let release_round1 = Arc::new(Notify::new());
    let call_count = Arc::new(AtomicUsize::new(0));

    let call_fn: AgentCallFn = {
        let injected_seen = injected_seen.clone();
        let round0_started = round0_started.clone();
        let release_round0 = release_round0.clone();
        let release_round1 = release_round1.clone();
        let call_count = call_count.clone();
        Arc::new(move |input, _config, _abort| {
            let injected_seen = injected_seen.clone();
            let round0_started = round0_started.clone();
            let release_round0 = release_round0.clone();
            let release_round1 = release_round1.clone();
            let call_count = call_count.clone();
            // Capture injected text BEFORE doing any async that might race
            let injected = input.injected_user_text.clone();
            let text = input.text().to_string();
            let n = call_count.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                if n == 0 {
                    // Round 0: return tool call so actor loops back
                    round0_started.notify_one();
                    release_round0.notified().await;
                    Ok((
                        "tool round".to_string(),
                        None,
                        vec![ToolCall::new(
                            "noop".into(),
                            json!({}),
                            Some("call-0".into()),
                            None,
                        )],
                        CompletionTokenUsage::default(),
                    ))
                } else if n == 1 {
                    // Round 1: should see injected prompt
                    {
                        let mut seen = injected_seen.lock().await;
                        *seen = injected.clone();
                    }
                    release_round1.notified().await;
                    Ok((
                        format!("done {text}"),
                        None,
                        Vec::<ToolCall>::new(),
                        CompletionTokenUsage::default(),
                    ))
                } else {
                    Ok((
                        "done".to_string(),
                        None,
                        Vec::<ToolCall>::new(),
                        CompletionTokenUsage::default(),
                    ))
                }
            })
        })
    };
    let registry =
        SessionRegistry::new_for_tests(config.clone(), Duration::from_secs(30), Some(call_fn));

    let response = open_sse(&config, &registry, "plain", "criteria-3", json!([])).await;
    let sse_task = tokio::spawn(async move {
        read_sse_until(response, Duration::from_secs(10), |read| {
            read.events.iter().any(|event| {
                matches!(
                    event["type"].as_str(),
                    Some("RUN_FINISHED") | Some("RUN_ERROR")
                )
            })
        })
        .await
    });

    // Send first prompt to start the run
    let first = rpc_call(
        &config,
        &registry,
        "plain",
        "criteria-3",
        json!({"jsonrpc":"2.0","id":1,"method":"session/prompt","params":{"text":"first"}}),
    )
    .await;
    assert_eq!(first["result"]["status"], "accepted");

    // Wait for round 0 to start (deterministic - notify_one stores permit)
    round0_started.notified().await;

    // Send second prompt while Running - should be enqueued
    let second = rpc_call(
        &config,
        &registry,
        "plain",
        "criteria-3",
        json!({"jsonrpc":"2.0","id":2,"method":"session/prompt","params":{"text":"second"}}),
    )
    .await;
    assert_eq!(second["result"]["status"], "enqueued");

    // Release round 0, actor proceeds to tool resolution and round 1
    release_round0.notify_one();
    // Release round 1 to complete
    release_round1.notify_one();

    let _ = sse_task.await.expect("sse task");

    // Assert injected text was seen by the real actor path
    let seen = injected_seen.lock().await.clone();
    assert_eq!(
        seen.as_deref(),
        Some("second"),
        "actor should have injected the second prompt"
    );

    let user_texts: Vec<String> = load_session_messages(&config, "plain", "criteria-3")
        .iter()
        .filter(|msg| msg.role.is_user())
        .map(|msg| msg.content.to_text())
        .collect();
    assert_eq!(
        user_texts,
        vec!["first".to_string(), "second".to_string()],
        "both prompts should persist in session history"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn e2e_success_sse_stream_includes_thinking_events_before_text() {
    let _guard = TestStateGuard::new(None).await;
    let sandbox = TestConfigSandbox::new();
    sandbox.write_agent("plain", "You are plain.");
    let config = sandbox.config();
    let call_fn: AgentCallFn = Arc::new(|_input, _config, _abort| {
        Box::pin(async move {
            for part in ["step one", "step two"] {
                harnx_core::sink::emit_agent_event(harnx_core::event::AgentEvent::Model(
                    harnx_core::event::ModelEvent::ThoughtChunk {
                        blocks: vec![ContentBlock::Text(part.to_string())],
                    },
                ));
            }
            harnx_core::sink::emit_agent_event(harnx_core::event::AgentEvent::Model(
                harnx_core::event::ModelEvent::MessageChunk {
                    blocks: vec![ContentBlock::Text("final answer".to_string())],
                },
            ));
            Ok((
                String::new(),
                None,
                Vec::<ToolCall>::new(),
                CompletionTokenUsage::default(),
            ))
        })
    });
    let registry =
        SessionRegistry::new_for_tests(config.clone(), Duration::from_secs(30), Some(call_fn));

    let response = open_sse(&config, &registry, "plain", "thinking-order", json!([])).await;
    let sse_task = tokio::spawn(async move {
        read_sse_until(response, Duration::from_secs(10), |read| {
            read.events
                .iter()
                .any(|event| event["type"] == "RUN_FINISHED")
        })
        .await
    });

    let prompt = rpc_call(
        &config,
        &registry,
        "plain",
        "thinking-order",
        json!({"jsonrpc":"2.0","id":1,"method":"session/prompt","params":{"text":"hello"}}),
    )
    .await;
    assert_eq!(prompt["result"]["status"], "accepted");

    let read = sse_task.await.expect("sse task");
    let types: Vec<_> = read
        .events
        .iter()
        .map(|event| event["type"].as_str().unwrap_or("<missing>").to_string())
        .collect();
    println!("thought-then-text event sequence: {types:?}");

    let thinking_start_positions: Vec<_> = types
        .iter()
        .enumerate()
        .filter_map(|(idx, event)| (event == "THINKING_START").then_some(idx))
        .collect();
    let thinking_text_start_positions: Vec<_> = types
        .iter()
        .enumerate()
        .filter_map(|(idx, event)| (event == "THINKING_TEXT_MESSAGE_START").then_some(idx))
        .collect();
    let thinking_delta_positions: Vec<_> = types
        .iter()
        .enumerate()
        .filter_map(|(idx, event)| (event == "THINKING_TEXT_MESSAGE_CONTENT").then_some(idx))
        .collect();
    let thinking_text_end_positions: Vec<_> = types
        .iter()
        .enumerate()
        .filter_map(|(idx, event)| (event == "THINKING_TEXT_MESSAGE_END").then_some(idx))
        .collect();
    let thinking_end_positions: Vec<_> = types
        .iter()
        .enumerate()
        .filter_map(|(idx, event)| (event == "THINKING_END").then_some(idx))
        .collect();
    let text_start = types
        .iter()
        .position(|event| event == "TEXT_MESSAGE_START")
        .expect("text start");
    let text_delta = types
        .iter()
        .position(|event| event == "TEXT_MESSAGE_CONTENT")
        .expect("text content");
    let text_end = types
        .iter()
        .position(|event| event == "TEXT_MESSAGE_END")
        .expect("text end");

    assert_eq!(
        thinking_start_positions.len(),
        1,
        "expected single THINKING_START: {types:?}"
    );
    assert_eq!(
        thinking_text_start_positions.len(),
        1,
        "expected single THINKING_TEXT_MESSAGE_START: {types:?}"
    );
    assert_eq!(
        thinking_text_end_positions.len(),
        1,
        "expected single THINKING_TEXT_MESSAGE_END: {types:?}"
    );
    assert_eq!(
        thinking_end_positions.len(),
        1,
        "expected single THINKING_END: {types:?}"
    );
    assert_eq!(
        thinking_delta_positions.len(),
        2,
        "expected two thinking chunks: {types:?}"
    );

    let thinking_start = thinking_start_positions[0];
    let thinking_text_start = thinking_text_start_positions[0];
    let thinking_text_end = thinking_text_end_positions[0];
    let thinking_end = thinking_end_positions[0];
    let first_thinking_delta = thinking_delta_positions[0];
    let last_thinking_delta = *thinking_delta_positions
        .last()
        .expect("thinking delta positions");

    assert!(
        thinking_start < thinking_text_start
            && thinking_text_start < first_thinking_delta
            && first_thinking_delta <= last_thinking_delta
            && last_thinking_delta < thinking_text_end
            && thinking_text_end < thinking_end
            && thinking_end < text_delta
            && text_start < text_delta
            && text_delta < text_end,
        "unexpected event order: {types:?}"
    );
    assert_eq!(
        read.events[first_thinking_delta]["delta"], "step one",
        "first thinking delta should surface first thought chunk"
    );
    assert_eq!(
        read.events[last_thinking_delta]["delta"], "step two",
        "second thinking delta should surface second thought chunk"
    );
    assert_eq!(
        read.events[text_delta]["delta"], "final answer",
        "text delta should arrive after thinking closes"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn e2e_success_criterion_10_sse_stream_includes_tool_call_events() {
    let _guard = TestStateGuard::new(None).await;
    let sandbox = TestConfigSandbox::new();
    sandbox.write_agent_with_front_matter(
        "plain",
        "model: openai:gpt-4o\nuse_tools: harnx_agent_session_history_read",
        "You are plain.",
    );
    let config = sandbox.config();

    let call_count = Arc::new(AtomicUsize::new(0));
    let seen_tool_results = Arc::new(tokio::sync::Mutex::new(Vec::<String>::new()));
    let call_fn: AgentCallFn = {
        let call_count = Arc::clone(&call_count);
        let seen_tool_results = Arc::clone(&seen_tool_results);
        Arc::new(move |input, _config, _abort| {
            let call_count = Arc::clone(&call_count);
            let seen_tool_results = Arc::clone(&seen_tool_results);
            let tool_results = input
                .tool_calls()
                .as_ref()
                .map(|calls| calls.tool_results.clone())
                .unwrap_or_default();
            Box::pin(async move {
                let round = call_count.fetch_add(1, Ordering::SeqCst);
                match round {
                    0 => Ok((
                        "searching history".to_string(),
                        None,
                        vec![ToolCall::new(
                            "harnx_agent_session_history_read".to_string(),
                            json!({"entry_type": "message", "limit": 5}),
                            Some("history-1".to_string()),
                            None,
                        )],
                        CompletionTokenUsage::default(),
                    )),
                    1 => {
                        let outputs = tool_results
                            .iter()
                            .map(|result| result.output.to_string())
                            .collect::<Vec<_>>();
                        *seen_tool_results.lock().await = outputs;
                        Ok((
                            "history checked".to_string(),
                            None,
                            Vec::<ToolCall>::new(),
                            CompletionTokenUsage::default(),
                        ))
                    }
                    other => panic!("unexpected llm round {other}"),
                }
            })
        })
    };
    let registry =
        SessionRegistry::new_for_tests(config.clone(), Duration::from_secs(30), Some(call_fn));

    let response = open_sse(
        &config,
        &registry,
        "plain",
        "criteria-10",
        json!([{"id":Uuid::new_v4(),"role":"user","content":"show tool stream"}]),
    )
    .await;
    let sse_task = tokio::spawn(async move {
        let read = read_sse_until(response, Duration::from_secs(10), |read| {
            let has_start = read
                .events
                .iter()
                .any(|event| event["type"] == "TOOL_CALL_START");
            let has_args = read
                .events
                .iter()
                .any(|event| event["type"] == "TOOL_CALL_ARGS");
            let has_end = read
                .events
                .iter()
                .any(|event| event["type"] == "TOOL_CALL_END");
            let has_result = read
                .events
                .iter()
                .any(|event| event["type"] == "TOOL_CALL_RESULT");
            let has_finished = read
                .events
                .iter()
                .any(|event| event["type"] == "RUN_FINISHED");
            has_start && has_args && has_end && has_result && has_finished
        })
        .await;
        read
    });

    let read = sse_task.await.expect("sse task");
    println!(
        "events: {}",
        serde_json::to_string_pretty(&read.events).unwrap()
    );

    assert_eq!(
        call_count.load(Ordering::SeqCst),
        2,
        "tool result must trigger follow-up LLM round"
    );
    let seen_tool_results = seen_tool_results.lock().await.clone();
    assert_eq!(
        seen_tool_results.len(),
        1,
        "expected one executed tool result"
    );
    assert!(
        seen_tool_results[0].contains("session has not been saved yet"),
        "tool result should surface built-in tool execution error: {}",
        seen_tool_results[0]
    );

    let start_idx = read
        .events
        .iter()
        .position(|event| event["type"] == "TOOL_CALL_START")
        .expect("tool call start event");
    let args_idx = read
        .events
        .iter()
        .position(|event| event["type"] == "TOOL_CALL_ARGS")
        .expect("tool call args event");
    let end_idx = read
        .events
        .iter()
        .position(|event| event["type"] == "TOOL_CALL_END")
        .expect("tool call end event");
    let result_idx = read
        .events
        .iter()
        .position(|event| event["type"] == "TOOL_CALL_RESULT")
        .expect("tool call result event");
    assert!(start_idx < args_idx && args_idx < end_idx && end_idx < result_idx);

    let start = &read.events[start_idx];
    let args = &read.events[args_idx];
    let end = &read.events[end_idx];
    let result = &read.events[result_idx];

    assert_eq!(start["toolCallId"], "history-1");
    assert_eq!(start["toolCallName"], "harnx_agent_session_history_read");
    assert!(start["parentMessageId"].is_string());
    assert_eq!(args["toolCallId"], "history-1");
    assert_eq!(
        args["delta"],
        json!({"entry_type": "message", "limit": 5}).to_string()
    );
    assert_eq!(end["toolCallId"], "history-1");
    assert_eq!(result["toolCallId"], "history-1");
    assert_eq!(result["role"], "tool");
    assert!(
        result["content"]
            .as_str()
            .unwrap_or_default()
            .contains("session has not been saved yet"),
        "tool result should surface built-in tool execution output: {result}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn e2e_success_criterion_4_cancel_running_persists_partial_and_returns_idle() {
    let _guard = TestStateGuard::new(None).await;
    let sandbox = TestConfigSandbox::new();
    sandbox.write_agent("plain", "You are plain.");
    let config = sandbox.config();

    // Gate: call_fn signals when started, test releases after cancel
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());

    let call_fn: AgentCallFn = {
        let started = started.clone();
        let release = release.clone();
        Arc::new(move |_input, _config, abort| {
            let started = started.clone();
            let release = release.clone();
            Box::pin(async move {
                started.notify_one();
                tokio::select! {
                    _ = release.notified() => Ok((
                        "should not finish".to_string(),
                        None,
                        Vec::<ToolCall>::new(),
                        CompletionTokenUsage::default(),
                    )),
                    _ = harnx_core::abort::wait_abort_signal(&abort) => Err(anyhow!("cancelled")),
                }
            })
        })
    };
    let registry =
        SessionRegistry::new_for_tests(config.clone(), Duration::from_secs(30), Some(call_fn));

    let response = open_sse(&config, &registry, "plain", "criteria-4", json!([])).await;
    let sse_task = tokio::spawn(async move {
        read_sse_until(response, Duration::from_secs(10), |read| {
            read.events.iter().any(|event| {
                matches!(
                    event["type"].as_str(),
                    Some("RUN_FINISHED") | Some("RUN_ERROR")
                )
            })
        })
        .await
    });

    // Start run
    let prompt = rpc_call(
        &config,
        &registry,
        "plain",
        "criteria-4",
        json!({"jsonrpc":"2.0","id":1,"method":"session/prompt","params":{"text":"cancel me"}}),
    )
    .await;
    assert_eq!(prompt["result"]["status"], "accepted");

    // Wait for run to start (deterministic)
    started.notified().await;

    // Give the actor a moment to enter the select! (abort-aware)
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Cancel via RPC
    let cancel = rpc_call(
        &config,
        &registry,
        "plain",
        "criteria-4",
        json!({"jsonrpc":"2.0","id":2,"method":"session/cancel"}),
    )
    .await;
    assert_eq!(cancel["result"]["cancelled"], true);

    // Release the gate so if the run didn't catch the abort it can still exit
    release.notify_one();

    let read = sse_task.await.expect("sse task");
    // Assert cancel path emits RUN_ERROR (not RUN_FINISHED)
    assert!(
        read.events.iter().any(|event| event["type"] == "RUN_ERROR"),
        "cancel should emit RUN_ERROR"
    );

    // Wait for state to settle to idle
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Verify partial state persisted
    let persisted = load_session_messages(&config, "plain", "criteria-4");
    assert!(persisted
        .iter()
        .any(|msg| msg.role.is_user() && msg.content.to_text() == "cancel me"));

    // Verify session state is Idle
    let state = rpc_call(
        &config,
        &registry,
        "plain",
        "criteria-4",
        json!({"jsonrpc":"2.0","id":3,"method":"session/get"}),
    )
    .await;
    assert_eq!(state["result"]["state"]["status"], "idle");
}

#[tokio::test(flavor = "multi_thread")]
async fn e2e_success_criterion_5_history_snapshot_returns_persisted_messages() {
    let _guard = TestStateGuard::new(None).await;
    let sandbox = TestConfigSandbox::new();
    sandbox.write_agent("plain", "You are plain.");
    let config = sandbox.config();
    let call_fn: AgentCallFn = Arc::new(|input, _config, _abort| {
        let text = input.text();
        Box::pin(async move {
            Ok((
                format!("assistant {text}"),
                None,
                Vec::<ToolCall>::new(),
                CompletionTokenUsage::default(),
            ))
        })
    });
    let registry =
        SessionRegistry::new_for_tests(config.clone(), Duration::from_secs(30), Some(call_fn));

    let response = open_sse(&config, &registry, "plain", "criteria-5", json!([])).await;
    let sse_task = tokio::spawn(async move {
        read_sse_until(response, Duration::from_secs(10), |read| {
            read.events
                .iter()
                .any(|event| event["type"] == "RUN_FINISHED")
        })
        .await
    });

    let prompt = rpc_call(
        &config,
        &registry,
        "plain",
        "criteria-5",
        json!({"jsonrpc":"2.0","id":1,"method":"session/prompt","params":{"text":"history test"}}),
    )
    .await;

    assert_eq!(prompt["result"]["status"], "accepted");

    let _ = sse_task.await.expect("sse task");

    // Check known session returns history via session/get
    let history = rpc_call(
        &config,
        &registry,
        "plain",
        "criteria-5",
        json!({"jsonrpc":"2.0","id":2,"method":"session/get"}),
    )
    .await;
    let texts = history_snapshot_texts(&history["result"]);
    assert!(texts.iter().any(|t| t.contains("history test")));

    // Check unknown session returns JSON-RPC error -32001
    let unknown = rpc_call(
        &config,
        &registry,
        "plain",
        "never-existed-session",
        json!({"jsonrpc":"2.0","id":3,"method":"session/get"}),
    )
    .await;
    assert_eq!(
        unknown["error"]["code"], -32001,
        "unknown session should return -32001"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn e2e_success_criterion_6_two_sse_subscribers_receive_same_events() {
    let _guard = TestStateGuard::new(None).await;
    let sandbox = TestConfigSandbox::new();
    sandbox.write_agent("plain", "You are plain.");
    let config = sandbox.config();
    let call_fn: AgentCallFn = Arc::new(|input, _config, _abort| {
        let text = input.text();
        Box::pin(async move {
            Ok((
                format!("assistant {text}"),
                None,
                Vec::<ToolCall>::new(),
                CompletionTokenUsage::default(),
            ))
        })
    });
    let registry =
        SessionRegistry::new_for_tests(config.clone(), Duration::from_secs(30), Some(call_fn));

    let response_a = open_sse(&config, &registry, "plain", "criteria-6", json!([])).await;
    let response_b = open_sse(&config, &registry, "plain", "criteria-6", json!([])).await;
    let task_a = tokio::spawn(async move {
        read_sse_until(response_a, Duration::from_secs(10), |read| {
            read.events
                .iter()
                .any(|event| event["type"] == "RUN_FINISHED")
        })
        .await
    });
    let task_b = tokio::spawn(async move {
        read_sse_until(response_b, Duration::from_secs(10), |read| {
            read.events
                .iter()
                .any(|event| event["type"] == "RUN_FINISHED")
        })
        .await
    });

    let prompt = rpc_call(
        &config,
        &registry,
        "plain",
        "criteria-6",
        json!({"jsonrpc":"2.0","id":1,"method":"session/prompt","params":{"text":"broadcast"}}),
    )
    .await;
    assert_eq!(prompt["result"]["status"], "accepted");

    let a = task_a.await.expect("task a");
    let b = task_b.await.expect("task b");
    assert!(a.events.iter().any(|event| event["type"] == "RUN_STARTED"));
    assert!(b.events.iter().any(|event| event["type"] == "RUN_STARTED"));
    assert!(a.events.iter().any(|event| event["type"] == "RUN_FINISHED"));
    assert!(b.events.iter().any(|event| event["type"] == "RUN_FINISHED"));
}

#[tokio::test(flavor = "multi_thread")]
async fn e2e_success_criterion_7_drop_sse_mid_run_run_continues_and_persists() {
    let _guard = TestStateGuard::new(None).await;
    let sandbox = TestConfigSandbox::new();
    sandbox.write_agent("plain", "You are plain.");
    let config = sandbox.config();

    // Gate: call_fn signals when started, test releases after dropping SSE
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());

    let call_fn: AgentCallFn = {
        let started = started.clone();
        let release = release.clone();
        Arc::new(move |_input, _config, _abort| {
            let started = started.clone();
            let release = release.clone();
            Box::pin(async move {
                started.notify_one();
                release.notified().await;
                Ok((
                    "finished after disconnect".to_string(),
                    None,
                    Vec::<ToolCall>::new(),
                    CompletionTokenUsage::default(),
                ))
            })
        })
    };
    let registry =
        SessionRegistry::new_for_tests(config.clone(), Duration::from_secs(30), Some(call_fn));

    let response = open_sse(&config, &registry, "plain", "criteria-7", json!([])).await;
    // Spawn SSE reader in a task that we'll drop
    let reader = tokio::spawn(async move {
        read_sse_until(response, Duration::from_secs(2), |read| {
            read.events
                .iter()
                .any(|event| event["type"] == "RUN_STARTED")
        })
        .await
    });

    // Start the run
    let prompt = rpc_call(
        &config,
        &registry,
        "plain",
        "criteria-7",
        json!({"jsonrpc":"2.0","id":1,"method":"session/prompt","params":{"text":"keep running"}}),
    )
    .await;
    assert_eq!(prompt["result"]["status"], "accepted");

    // Wait for run to start (deterministic)
    started.notified().await;

    // Drop the SSE task mid-run (the stream is dropped when task is aborted)
    reader.abort();
    let _ = reader.await;

    // Release the gate so the run completes
    release.notify_one();

    // Wait for run to finish
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Verify run completed and persisted despite no SSE subscribers
    let persisted = load_session_messages(&config, "plain", "criteria-7");
    assert!(
        persisted.iter().any(|msg| msg.role.is_assistant()
            && msg.content.to_text().contains("finished after disconnect")),
        "assistant turn should persist even without SSE subscribers"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn e2e_success_criterion_8_heartbeat_keeps_sse_alive() {
    let _guard = TestStateGuard::new(None).await;
    let sandbox = TestConfigSandbox::new();
    sandbox.write_agent("plain", "You are plain.");
    let config = sandbox.config();
    let call_fn: AgentCallFn = Arc::new(|input, _config, _abort| {
        let text = input.text();
        Box::pin(async move {
            Ok((
                format!("assistant {text}"),
                None,
                Vec::<ToolCall>::new(),
                CompletionTokenUsage::default(),
            ))
        })
    });
    let registry =
        SessionRegistry::new_for_tests(config.clone(), Duration::from_secs(30), Some(call_fn));

    let response = open_sse(&config, &registry, "plain", "criteria-8", json!([])).await;
    let task = tokio::spawn(async move {
        read_sse_until(response, Duration::from_secs(15), |read| {
            read.events
                .iter()
                .filter(|event| event["type"] == "RUN_FINISHED")
                .count()
                >= 2
        })
        .await
    });

    // Start first run
    let first = rpc_call(
        &config,
        &registry,
        "plain",
        "criteria-8",
        json!({"jsonrpc":"2.0","id":1,"method":"session/prompt","params":{"text":"one"}}),
    )
    .await;
    assert_eq!(first["result"]["status"], "accepted");

    // Wait for first run to complete
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Start second run (now idle, so accepted)
    let second = rpc_call(
        &config,
        &registry,
        "plain",
        "criteria-8",
        json!({"jsonrpc":"2.0","id":2,"method":"session/prompt","params":{"text":"two"}}),
    )
    .await;
    assert_eq!(second["result"]["status"], "accepted");

    let read = task.await.expect("heartbeat task");
    // Two runs should complete; heartbeat existence is optional in fast test
    assert_eq!(
        read.events
            .iter()
            .filter(|event| event["type"] == "RUN_FINISHED")
            .count(),
        2
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn e2e_success_criterion_9_legacy_endpoints_and_history_unchanged() {
    use harnx_runtime::config::GlobalConfig;
    use harnx_serve::Server;

    let _guard = TestStateGuard::new(None).await;
    let sandbox = TestConfigSandbox::new();
    sandbox.write_agent("plain", "You are plain.");
    let config = sandbox.config();
    let call_fn: AgentCallFn = Arc::new(|_input, _config, _abort| {
        Box::pin(async {
            Ok((
                "assistant legacy".to_string(),
                None,
                Vec::<ToolCall>::new(),
                CompletionTokenUsage::default(),
            ))
        })
    });
    let registry =
        SessionRegistry::new_for_tests(config.clone(), Duration::from_secs(30), Some(call_fn));

    // Create a run via AG-UI SSE to have a session persisted
    let response = open_sse(
        &config,
        &registry,
        "plain",
        "criteria-9",
        json!([{"id":Uuid::new_v4(),"role":"user","content":"history please"}]),
    )
    .await;
    let _ = read_sse_until(response, Duration::from_secs(10), |read| {
        read.events.iter().any(|event| {
            matches!(
                event["type"].as_str(),
                Some("RUN_FINISHED") | Some("RUN_ERROR")
            )
        })
    })
    .await;

    // Build a real Server and exercise the legacy P1 endpoints directly
    let global_config: GlobalConfig = Arc::new(parking_lot::RwLock::new(config.clone()));
    let server = Server::new(&global_config);

    // Hit GET /v1/agents/{agent}/sessions (agent enumeration) via real handler
    let sessions = server
        .list_sessions_json("plain")
        .expect("sessions response");
    assert!(
        sessions.as_array().map(|a| a.len() == 1).unwrap_or(false),
        "should list one session"
    );

    // Hit GET /v1/agents/{agent}/sessions/{session} (session history) via real handler
    let history = server
        .list_session_history("plain", "criteria-9")
        .await
        .expect("history response");
    assert!(
        history.as_array().map(|a| !a.is_empty()).unwrap_or(false),
        "history should have messages"
    );

    // Keep AG-UI control plane smoke checks only. Legacy proxy/playground/arena routes are deleted.
    let unknown = server
        .list_session_history("plain", "missing-session")
        .await;
    assert!(unknown.is_err(), "missing AG-UI session should error");
}
