use super::*;
use futures_util::StreamExt;
use harnx_toolset_server::invocation_journal::InvocationJournal;
use serde_json::json;
use std::sync::Arc;

#[test]
fn replay_route_preserves_logical_identity_across_process_scopes() -> anyhow::Result<()> {
    let record = serde_json::from_value(json!({
        "request": {"call_id": "original", "operation_id": "original", "tool": "echo", "args": {}},
        "tool_name": "echo", "server": "configured-server", "server_scope": "departed-process",
        "tool_round": 1, "started_at_ms": 1, "reply": null
    }))?;
    let route = RegisteredTool {
        server: "configured-server".into(),
        selector_server: "configured-server".into(),
        raw_name: "echo".into(),
        request_timeout: None,
    };
    validate_replay_route(&route, &record)?;
    let mut wrong_server = route.clone();
    wrong_server.server = "new-alias-winner".into();
    assert!(validate_replay_route(&wrong_server, &record).is_err());
    let mut wrong_tool = route;
    wrong_tool.raw_name = "different-tool".into();
    assert!(validate_replay_route(&wrong_tool, &record).is_err());
    Ok(())
}

/// Rolling a turn forward with no `Cancel` behind it uses the journal both
/// ways. A call it already holds a reply for is returned from that record,
/// with no live registration needed. A call it has no reply for is sent to its
/// tool again as a replay attempt, and the tool's answer becomes the result.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resume_without_cancel_rolls_forward_saved_reply_then_replays_retryable_call(
) -> anyhow::Result<()> {
    let (url, mut nats, _store) = crate::nats_worker::tests::spawn_test_nats()
        .await
        .context("nats-server required")?;
    let client = async_nats::connect(&url).await?;
    let js = async_nats::jetstream::new(client.clone());
    save_reply(&js).await?;
    record_retryable_call(&js).await?;
    let provider = saved_reply_provider(client.clone()).await?;

    let saved = harnx_core::tool::ToolCall::new(
        "retired_echo".into(),
        json!({}),
        Some("model-call".into()),
        None,
    );
    assert!(!provider.has_tool(&saved.name));
    let result = provider
        .replay_recorded_call(replay_of(&saved), &harnx_core::abort::create_abort_signal())
        .await?
        .context("recovered reply")?;
    assert_eq!(result.value, json!({"answer": "saved"}));
    let provenance = result.execution_context.unwrap().provenance.unwrap();
    assert_eq!(provenance.server_scope, "original-scope");
    assert_eq!(provenance.server_identity, "retired");

    // The second call reached its tool server but never replied, so rolling
    // forward re-dispatches it rather than answering from the journal.
    let replayed = replay_responder(&client, provider.instance_id.clone()).await?;
    let retryable = harnx_core::tool::ToolCall::new(
        "retryable_echo".into(),
        json!({}),
        Some("retryable-call".into()),
        None,
    );
    let result = provider
        .replay_recorded_call(
            replay_of(&retryable),
            &harnx_core::abort::create_abort_signal(),
        )
        .await?
        .context("replayed reply")?;
    assert_eq!(result.value, json!({"answer": "replayed"}));
    let attempt = replayed
        .lock()
        .await
        .clone()
        .context("the tool server saw the replayed request")?;
    assert_eq!(attempt.call_id, "retryable-original");
    assert!(
        attempt.replay.is_some(),
        "a roll-forward dispatch is tagged as a replay attempt, not a fresh call"
    );

    let _ = nats.kill();
    let _ = nats.wait();
    Ok(())
}

fn replay_of(call: &harnx_core::tool::ToolCall) -> harnx_core::tool::ToolReplay<'_> {
    harnx_core::tool::ToolReplay {
        session_id: "parent",
        tool_round: 5,
        call,
        worker_id: Some("worker"),
        fence_token: Some(1),
        authorization: None,
    }
}

/// A journal row for a call that was dispatched and never answered — the one
/// shape roll-forward has to send to the tool again.
async fn record_retryable_call(js: &async_nats::jetstream::Context) -> anyhow::Result<()> {
    InvocationJournal::ensure(js, 1)
        .await?
        .record(
            &ToolRequest {
                replay: None,
                call_id: "retryable-original".into(),
                operation_id: "retryable-original".into(),
                tool: "echo".into(),
                args: json!({}),
                parent_session_id: Some("parent".into()),
                tool_call_id: Some("retryable-call".into()),
                capabilities: Default::default(),
            },
            ("retryable_echo", "original-scope", "retryable"),
            5,
        )
        .await
}

/// Answer the replayed request the way the tool server would, recording what
/// arrived so the test can tell a replay attempt from a fresh dispatch.
async fn replay_responder(
    client: &async_nats::Client,
    scope: ServerScope,
) -> anyhow::Result<Arc<Mutex<Option<ToolRequest>>>> {
    let seen = Arc::new(Mutex::new(None));
    let mut requests = client
        .subscribe(scope.tool_subject("retryable", "echo"))
        .await?;
    client.flush().await?;
    let recorded = seen.clone();
    let client = client.clone();
    tokio::spawn(async move {
        let Some(message) = requests.next().await else {
            return;
        };
        let request: ToolRequest = serde_json::from_slice(&message.payload).expect("tool request");
        let reply = ToolReply {
            call_id: request.call_id.clone(),
            result: Ok(json!({"answer": "replayed"})),
            final_progress: None,
        };
        *recorded.lock().await = Some(request);
        if let Some(subject) = message.reply {
            let _ = client
                .publish(subject, serde_json::to_vec(&reply).expect("reply").into())
                .await;
        }
    });
    Ok(seen)
}

async fn save_reply(js: &async_nats::jetstream::Context) -> anyhow::Result<()> {
    let request = ToolRequest {
        replay: None,
        call_id: "original".into(),
        operation_id: "original".into(),
        tool: "echo".into(),
        args: json!({}),
        parent_session_id: Some("parent".into()),
        tool_call_id: Some("model-call".into()),
        capabilities: Default::default(),
    };
    let journal = InvocationJournal::ensure(js, 1).await?;
    journal
        .record(&request, ("retired_echo", "original-scope", "retired"), 5)
        .await?;
    journal
        .complete(
            &request,
            ToolReply {
                call_id: "original".into(),
                result: Ok(json!({"answer": "saved", "_meta": {
                    EXECUTION_CONTEXT_NAMESPACE: harnx_core::execution_context::ExecutionContextObservation::observe(
                        std::path::Path::new("/original/workspace"), std::path::Path::new("/original/workspace"))
                }})),
                final_progress: None,
            },
        )
        .await?;
    Ok(())
}

async fn saved_reply_provider(client: async_nats::Client) -> anyhow::Result<NatsToolProvider> {
    let instance_id = ServerScope::new();
    let subscription = client.subscribe(instance_id.control_subject()).await?;
    Ok(NatsToolProvider {
        client,
        instance_id,
        parent_session_id: Some("parent".into()),
        tools: HashMap::from([(
            "retryable_echo".to_string(),
            RegisteredTool {
                server: "retryable".into(),
                selector_server: "retryable".into(),
                raw_name: "echo".into(),
                request_timeout: None,
            },
        )]),
        registrations: Vec::new(),
        active_package: None,
        declarations: Vec::new(),
        registry: None,
        journal_replicas: 1,
        _control_subscription: Mutex::new(subscription),
        in_flight: NatsInFlightCalls::default(),
    })
}

/// Dispatch creates the invocation journal's bucket as often as any other
/// writer does, and whichever writer gets there first fixes the bucket's
/// replica count for all of them. It must therefore use the cluster's
/// configured durability rather than assume one replica.
///
/// A single-node server cannot host a three-replica bucket, and that refusal
/// is how the count that actually reached `InvocationJournal::ensure` is
/// visible here without a real cluster: with the configured 3 the bucket is
/// never created, where a hardcoded 1 would have created it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_journal_bucket_takes_the_configured_replica_count() -> anyhow::Result<()> {
    harnx_core::require_nextest();
    let options = crate::nats_test_common::SpawnNatsServerOptions {
        auth_token: Some("replica-token".into()),
    };
    let Some(server) = crate::nats_test_common::spawn_nats_server_with_options(options).await?
    else {
        return Ok(());
    };
    let _lock = crate::test_environment::env_lock_async().await;
    let _url =
        crate::test_environment::EnvGuard::new(crate::config::HARNX_NATS_URL_ENV, server.url());
    let _token = crate::test_environment::EnvGuard::new(
        crate::config::HARNX_NATS_TOKEN_ENV,
        "replica-token",
    );
    let _replicas = crate::test_environment::EnvGuard::new(
        harnx_nats_common::connect::HARNX_NATS_REPLICAS_ENV,
        "3",
    );

    let provider = NatsToolProvider::discover(
        &crate::config::Config::default(),
        ServerScope::new(),
        NatsInFlightCalls::default(),
        None,
    )
    .await?;
    assert_eq!(provider.journal_replicas, 3);

    let request = ToolRequest {
        replay: None,
        call_id: "wire-1".into(),
        operation_id: "wire-1".into(),
        tool: "echo".into(),
        args: json!({}),
        parent_session_id: Some("replica-session".into()),
        tool_call_id: None,
        capabilities: Default::default(),
    };
    provider
        .record_invocation(&request, "echo", "srv")
        .await
        .expect_err("a single-node broker cannot host the three-replica bucket that was asked for");
    let js = async_nats::jetstream::new(provider.client.clone());
    assert!(
        js.get_key_value(harnx_toolset_server::invocation_journal::BUCKET)
            .await
            .is_err(),
        "the bucket must not exist at the wrong durability"
    );
    Ok(())
}
