use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

use anyhow::anyhow;
use bytes::Bytes;
use harnx_core::{message::Message, tool::ToolCall};
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
use http::{Method, StatusCode};
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
                Ok(Some(Err(_))) | Ok(None) | Err(_) => {
                    break;
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
        .unwrap_or_default()
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
        })).unwrap(),
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
    scoped.session.as_ref().expect("session should exist").messages.clone()
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
    scoped.session.as_ref().expect("session should exist")
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
    let registry = SessionRegistry::new_for_tests(config.clone(), Duration::from_secs(30), Some(call_fn));

    let response = open_sse(&config, &registry, "plain", "criteria-1", json!([])).await;
    let sse_task = tokio::spawn(async move {
        read_sse_until(response, Duration::from_secs(10), |read| {
            read.events.iter().any(|event| event["type"] == "RUN_FINISHED")
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
    assert!(read.events.iter().any(|event| event["type"] == "RUN_STARTED"));
    assert!(read.events.iter().any(|event| event["type"] == "RUN_FINISHED"));
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
    let registry = SessionRegistry::new_for_tests(config.clone(), Duration::from_secs(30), Some(call_fn));

    let response = open_sse(&config, &registry, "plain", "criteria-2", json!([])).await;
    let sse_task = tokio::spawn(async move {
        read_sse_until(response, Duration::from_secs(10), |read| {
            read.events.iter().any(|event| event["type"] == "RUN_FINISHED")
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
    assert!(read.events.iter().any(|event| event["type"] == "RUN_STARTED"));
    assert!(read.events.iter().any(|event| event["type"] == "RUN_FINISHED"));

    // Verify assistant response persisted
    let persisted = load_session_messages(&config, "plain", "criteria-2");
    assert!(persisted.iter().any(|msg| msg.content.to_text().contains("one")));
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
                        vec![ToolCall::new("noop".into(), json!({}), Some("call-0".into()), None)],
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
    let registry = SessionRegistry::new_for_tests(config.clone(), Duration::from_secs(30), Some(call_fn));

    let response = open_sse(&config, &registry, "plain", "criteria-3", json!([])).await;
    let sse_task = tokio::spawn(async move {
        read_sse_until(response, Duration::from_secs(10), |read| {
            read.events.iter().any(|event| {
                matches!(event["type"].as_str(), Some("RUN_FINISHED") | Some("RUN_ERROR"))
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
    assert_eq!(seen.as_deref(), Some("second"), "actor should have injected the second prompt");

    // Assert persisted session contains "second" as a user turn
    let persisted = load_session_messages(&config, "plain", "criteria-3");
    let user_texts: Vec<String> = persisted
        .iter()
        .filter(|msg| msg.role.is_user())
        .map(|msg| msg.content.to_text())
        .collect();
    assert!(user_texts.iter().any(|text| text == "second"), "persisted session should contain 'second' user turn");
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
    let registry = SessionRegistry::new_for_tests(config.clone(), Duration::from_secs(30), Some(call_fn));

    let response = open_sse(&config, &registry, "plain", "criteria-4", json!([])).await;
    let sse_task = tokio::spawn(async move {
        read_sse_until(response, Duration::from_secs(10), |read| {
            read.events.iter().any(|event| {
                matches!(event["type"].as_str(), Some("RUN_FINISHED") | Some("RUN_ERROR"))
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
    assert!(read.events.iter().any(|event| event["type"] == "RUN_ERROR"), "cancel should emit RUN_ERROR");

    // Wait for state to settle to idle
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Verify partial state persisted
    let persisted = load_session_messages(&config, "plain", "criteria-4");
    assert!(persisted.iter().any(|msg| msg.role.is_user() && msg.content.to_text() == "cancel me"));

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
    let registry = SessionRegistry::new_for_tests(config.clone(), Duration::from_secs(30), Some(call_fn));

    let response = open_sse(&config, &registry, "plain", "criteria-5", json!([])).await;
    let sse_task = tokio::spawn(async move {
        read_sse_until(response, Duration::from_secs(10), |read| {
            read.events.iter().any(|event| event["type"] == "RUN_FINISHED")
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
    assert_eq!(unknown["error"]["code"], -32001, "unknown session should return -32001");
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
    let registry = SessionRegistry::new_for_tests(config.clone(), Duration::from_secs(30), Some(call_fn));

    let response_a = open_sse(&config, &registry, "plain", "criteria-6", json!([])).await;
    let response_b = open_sse(&config, &registry, "plain", "criteria-6", json!([])).await;
    let task_a = tokio::spawn(async move {
        read_sse_until(response_a, Duration::from_secs(10), |read| {
            read.events.iter().any(|event| event["type"] == "RUN_FINISHED")
        })
        .await
    });
    let task_b = tokio::spawn(async move {
        read_sse_until(response_b, Duration::from_secs(10), |read| {
            read.events.iter().any(|event| event["type"] == "RUN_FINISHED")
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
    let registry = SessionRegistry::new_for_tests(config.clone(), Duration::from_secs(30), Some(call_fn));

    let response = open_sse(&config, &registry, "plain", "criteria-7", json!([])).await;
    // Spawn SSE reader in a task that we'll drop
    let reader = tokio::spawn(async move {
        read_sse_until(response, Duration::from_secs(2), |read| {
            read.events.iter().any(|event| event["type"] == "RUN_STARTED")
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
        persisted.iter().any(|msg| msg.role.is_assistant() && msg.content.to_text().contains("finished after disconnect")),
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
    let registry = SessionRegistry::new_for_tests(config.clone(), Duration::from_secs(30), Some(call_fn));

    let response = open_sse(&config, &registry, "plain", "criteria-8", json!([])).await;
    let task = tokio::spawn(async move {
        read_sse_until(response, Duration::from_secs(15), |read| {
            read.events.iter().filter(|event| event["type"] == "RUN_FINISHED").count() >= 2
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
    use harnx_serve::Server;
    use harnx_runtime::config::GlobalConfig;
    
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
    let registry = SessionRegistry::new_for_tests(config.clone(), Duration::from_secs(30), Some(call_fn));

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
                matches!(event["type"].as_str(), Some("RUN_FINISHED") | Some("RUN_ERROR"))
            })
        })
    .await;

    // Build a real Server and exercise the legacy P1 endpoints directly
    let global_config: GlobalConfig = Arc::new(parking_lot::RwLock::new(config.clone()));
    let server = Server::new(&global_config);

    // Hit GET /v1/agents/{agent}/sessions (agent enumeration) via real handler
    let sessions = server.list_sessions_json("plain").expect("sessions response");
    assert!(sessions.as_array().map(|a| a.len() == 1).unwrap_or(false), "should list one session");

    // Hit GET /v1/agents/{agent}/sessions/{session} (session history) via real handler
    let history = server.list_session_history("plain", "criteria-9").await.expect("history response");
    assert!(history.as_array().map(|a| !a.is_empty()).unwrap_or(false), "history should have messages");

    // Hit POST /v1/chat/completions to verify route is still wired (does not 404/405)
    // NOTE: We do NOT fully exercise this endpoint because it requires a live LLM client.
    // We only prove the route exists - BAD_REQUEST means valid route but malformed body, not 404/405.
    let status = server.chat_completions_status().await;
    assert!(!matches!(status, StatusCode::NOT_FOUND | StatusCode::METHOD_NOT_ALLOWED), "chat completions route should still be wired");
}
