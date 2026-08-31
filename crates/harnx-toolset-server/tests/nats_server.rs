mod common;

use anyhow::{Context, Result};
use common::{request_headers, wait_for_registration, TestHarness, TestToolset, TOKEN};
use harnx_core::execution_context::{ExecutionContextObservation, EXECUTION_CONTEXT_NAMESPACE};
use harnx_nats_common::connect::NatsConnection;
use harnx_toolset::{ControlKind, ControlMessage, ToolReply, ToolRequest};
use harnx_toolset_server::{
    registration_key, serve_with_shutdown, ServeLifecycle, TOOL_REGISTRY_BUCKET,
};
use serde_json::json;
use std::collections::BTreeSet;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

async fn assert_registration(harness: &TestHarness) -> Result<()> {
    let registration = wait_for_registration(&harness.client, &harness.instance_id).await?;
    assert_eq!(registration.server, "test");
    assert_eq!(
        registration.schema_version,
        harnx_toolset_server::TOOL_SCHEMA_VERSION
    );
    Ok(())
}

async fn assert_idempotent_replay(harness: &TestHarness) -> Result<()> {
    let request = ToolRequest {
        call_id: "call-echo".to_string(),
        tool: "echo".to_string(),
        args: json!({ "value": 42 }),
        parent_session_id: None,
        capabilities: Default::default(),
    };
    for _ in 0..2 {
        let message = harness
            .client
            .request_with_headers(
                harness.echo_subject(),
                request_headers(&request.call_id, "logical-echo"),
                serde_json::to_vec(&request)?.into(),
            )
            .await?;
        let reply: ToolReply = serde_json::from_slice(&message.payload)?;
        assert_eq!(
            reply.result.expect("echo request should succeed"),
            json!({ "value": 42 })
        );
    }
    assert_eq!(harness.toolset.echo_invocations.load(Ordering::SeqCst), 1);
    Ok(())
}

async fn assert_execution_context_capability_is_per_request(harness: &TestHarness) -> Result<()> {
    let directory = tempfile::tempdir()?;
    let observation = ExecutionContextObservation::observe(directory.path(), directory.path());
    let args = json!({
        "content": [],
        "_meta": {
            EXECUTION_CONTEXT_NAMESPACE: observation,
        }
    });
    let opted_in = ToolRequest {
        call_id: "call-context-enabled".to_string(),
        tool: "echo".to_string(),
        args: args.clone(),
        parent_session_id: None,
        capabilities: BTreeSet::from([EXECUTION_CONTEXT_NAMESPACE.to_string()]),
    };
    let message = harness
        .client
        .request_with_headers(
            harness.echo_subject(),
            request_headers(&opted_in.call_id, "logical-context"),
            serde_json::to_vec(&opted_in)?.into(),
        )
        .await?;
    let reply: ToolReply = serde_json::from_slice(&message.payload)?;
    let enabled = reply.result.expect("context request succeeds");
    let provenance = &enabled["_meta"][EXECUTION_CONTEXT_NAMESPACE]["provenance"];
    assert_eq!(provenance["tool_name"], "echo");
    assert_eq!(provenance["call_id"], opted_in.call_id);

    let opted_out = ToolRequest {
        call_id: "call-context-disabled".to_string(),
        tool: "echo".to_string(),
        args,
        parent_session_id: None,
        capabilities: BTreeSet::new(),
    };
    let message = harness
        .client
        .request_with_headers(
            harness.echo_subject(),
            request_headers(&opted_out.call_id, "logical-context"),
            serde_json::to_vec(&opted_out)?.into(),
        )
        .await?;
    let reply: ToolReply = serde_json::from_slice(&message.payload)?;
    let disabled = reply.result.expect("legacy context request succeeds");
    assert!(disabled
        .get("_meta")
        .and_then(|meta| meta.get(EXECUTION_CONTEXT_NAMESPACE))
        .is_none());
    Ok(())
}

async fn assert_concurrent_idempotency(harness: &TestHarness) -> Result<()> {
    let invocations_before = harness.toolset.echo_invocations.load(Ordering::SeqCst);
    let args = json!({ "value": 43, "delay_ms": 100 });
    let first = ToolRequest {
        call_id: "call-concurrent-a".to_string(),
        tool: "echo".to_string(),
        args: args.clone(),
        parent_session_id: None,
        capabilities: Default::default(),
    };
    let second = ToolRequest {
        call_id: "call-concurrent-b".to_string(),
        tool: "echo".to_string(),
        args: args.clone(),
        parent_session_id: None,
        capabilities: Default::default(),
    };
    let first_call = harness.client.request_with_headers(
        harness.echo_subject(),
        request_headers(&first.call_id, "logical-concurrent"),
        serde_json::to_vec(&first)?.into(),
    );
    let second_call = harness.client.request_with_headers(
        harness.echo_subject(),
        request_headers(&second.call_id, "logical-concurrent"),
        serde_json::to_vec(&second)?.into(),
    );
    let (first_reply, second_reply) = tokio::join!(first_call, second_call);
    for (message, expected_id) in [
        (first_reply?, first.call_id.as_str()),
        (second_reply?, second.call_id.as_str()),
    ] {
        let reply: ToolReply = serde_json::from_slice(&message.payload)?;
        assert_eq!(reply.call_id, expected_id);
        assert_eq!(reply.result.expect("concurrent echo should succeed"), args);
    }
    assert_eq!(
        harness.toolset.echo_invocations.load(Ordering::SeqCst),
        invocations_before + 1,
        "concurrent duplicate must execute once"
    );
    Ok(())
}

async fn request_early_failure(
    harness: &TestHarness,
    headers: async_nats::HeaderMap,
    payload: Vec<u8>,
    expected: (&str, &str),
) -> Result<()> {
    let request = async_nats::Request::new()
        .headers(headers)
        .payload(payload.into())
        .timeout(Some(Duration::from_secs(1)));
    let message = tokio::time::timeout(
        Duration::from_secs(1),
        harness.client.send_request(harness.echo_subject(), request),
    )
    .await??;
    let reply: ToolReply = serde_json::from_slice(&message.payload)?;
    assert_eq!(reply.call_id, expected.0);
    match reply.result {
        Err(harnx_toolset::ToolErrorPayload::Recoverable(message)) => {
            assert!(message.contains(expected.1), "{message}");
        }
        other => anyhow::bail!("expected recoverable early failure, got {other:?}"),
    }
    Ok(())
}

async fn assert_early_failure_replies(harness: &TestHarness) -> Result<()> {
    request_early_failure(
        harness,
        request_headers("call-malformed", "logical-malformed"),
        b"not-json".to_vec(),
        ("call-malformed", "decode tool request payload"),
    )
    .await?;
    let mismatched = ToolRequest {
        call_id: "payload-call".to_string(),
        tool: "echo".to_string(),
        args: json!({}),
        parent_session_id: None,
        capabilities: Default::default(),
    };
    request_early_failure(
        harness,
        request_headers("header-call", "logical-mismatch"),
        serde_json::to_vec(&mismatched)?,
        ("header-call", "call ID header does not match payload"),
    )
    .await?;
    let missing_key = ToolRequest {
        call_id: "call-missing-key".to_string(),
        tool: "echo".to_string(),
        args: json!({}),
        parent_session_id: None,
        capabilities: Default::default(),
    };
    let mut headers = async_nats::HeaderMap::new();
    headers.insert(harnx_toolset::HDR_CALL_ID, missing_key.call_id.as_str());
    request_early_failure(
        harness,
        headers,
        serde_json::to_vec(&missing_key)?,
        ("call-missing-key", "missing Idempotency-Key"),
    )
    .await
}

async fn assert_cancellation(harness: &TestHarness) -> Result<()> {
    let request = ToolRequest {
        call_id: "call-slow".to_string(),
        tool: "slow".to_string(),
        args: json!({}),
        parent_session_id: None,
        capabilities: Default::default(),
    };
    let slow_request = harness.client.request_with_headers(
        harness.instance_id.tool_subject("____test", "slow"),
        request_headers(&request.call_id, "logical-slow"),
        serde_json::to_vec(&request)?.into(),
    );
    tokio::pin!(slow_request);
    tokio::select! {
        _ = harness.toolset.slow_started.notified() => {}
        result = &mut slow_request => anyhow::bail!("slow request completed before cancellation: {result:?}"),
    }
    let control = ControlMessage {
        call_id: request.call_id.clone(),
        kind: ControlKind::Cancel,
    };
    harness
        .client
        .publish_with_headers(
            harness.instance_id.control_subject(),
            request_headers(&request.call_id, "cancel-slow"),
            serde_json::to_vec(&control)?.into(),
        )
        .await?;
    harness.client.flush().await?;
    let message = tokio::time::timeout(Duration::from_secs(2), &mut slow_request).await??;
    let reply: ToolReply = serde_json::from_slice(&message.payload)?;
    assert!(matches!(
        reply.result,
        Err(harnx_toolset::ToolErrorPayload::Fatal(_))
    ));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn registers_invokes_caches_and_cancels() -> Result<()> {
    let Some(harness) = TestHarness::start().await? else {
        return Ok(());
    };
    assert_registration(&harness).await?;
    assert_idempotent_replay(&harness).await?;
    assert_execution_context_capability_is_per_request(&harness).await?;
    assert_concurrent_idempotency(&harness).await?;
    assert_early_failure_replies(&harness).await?;
    assert_cancellation(&harness).await
}

async fn registration_revision(client: &async_nats::Client, key: &str) -> Result<Option<u64>> {
    let jetstream = async_nats::jetstream::new(client.clone());
    // The bucket may not exist yet if no tool server has published to this
    // scope. That is "no revision yet", not a failure worth propagating here.
    let Ok(store) = jetstream.get_key_value(TOOL_REGISTRY_BUCKET).await else {
        return Ok(None);
    };
    Ok(store.entry(key).await?.map(|entry| entry.revision))
}

async fn wait_for_revision_beyond(
    client: &async_nats::Client,
    key: &str,
    previous: u64,
) -> Result<u64> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(revision) = registration_revision(client, key).await? {
            if revision > previous {
                return Ok(revision);
            }
        }
        if Instant::now() >= deadline {
            anyhow::bail!("timed out waiting for registration revision to advance past {previous}");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// A rolling deploy publishes the replacement's registration under the same
/// `{scope}.{identity_token}` key before the old instance finishes shutting
/// down (new pod ready before old pod terminates is Kubernetes' normal
/// sequence). The old instance's shutdown must delete only its own
/// registration, never the replacement's.
#[tokio::test(flavor = "multi_thread")]
async fn old_instances_shutdown_does_not_delete_a_replacements_registration() -> Result<()> {
    harnx_core::require_nextest();
    let Some(mut harness) = TestHarness::start().await? else {
        return Ok(());
    };
    let key = registration_key(&harness.instance_id, "____test");
    let old_revision = wait_for_revision_beyond(&harness.client, &key, 0).await?;

    let new_shutdown = CancellationToken::new();
    let new_client = async_nats::ConnectOptions::new()
        .token(TOKEN.to_string())
        .connect(&harness._server.url)
        .await?;
    let new_task = {
        let instance_id = harness.instance_id.clone();
        let shutdown = new_shutdown.clone();
        tokio::spawn(async move {
            serve_with_shutdown(
                Arc::new(TestToolset::default()),
                instance_id,
                NatsConnection {
                    client: new_client,
                    replicas: 1,
                },
                ServeLifecycle::new(shutdown, None),
            )
            .await
        })
    };
    let new_revision = wait_for_revision_beyond(&harness.client, &key, old_revision).await?;
    assert!(new_revision > old_revision);

    // Shut down the OLD instance. Its unconditional delete used to remove
    // whatever is currently at `key` -- the replacement's entry.
    harness.shutdown().await;

    let jetstream = async_nats::jetstream::new(harness.client.clone());
    let store = jetstream.get_key_value(TOOL_REGISTRY_BUCKET).await?;
    assert!(
        store.get(&key).await?.is_some(),
        "old instance's shutdown deleted the replacement's registration"
    );
    let revision_after_old_shutdown = registration_revision(&harness.client, &key)
        .await?
        .expect("registration entry should still exist");
    assert_eq!(
        revision_after_old_shutdown, new_revision,
        "the surviving registration must be the replacement's, not a re-published old one"
    );

    // The replacement's own shutdown should still delete its own, current
    // registration -- the conditional delete must not become a no-op.
    new_shutdown.cancel();
    new_task
        .await
        .context("join replacement task")?
        .context("replacement exited with an error")?;
    assert!(
        store.get(&key).await?.is_none(),
        "the replacement's own shutdown must delete its own current registration"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn graceful_shutdown_removes_the_registration() -> Result<()> {
    harnx_core::require_nextest();
    let Some(mut harness) = TestHarness::start().await? else {
        return Ok(());
    };
    assert_registration(&harness).await?;
    assert!(
        harness.readiness.is_ready(),
        "server should be ready after subscriptions flush"
    );

    let jetstream = async_nats::jetstream::new(harness.client.clone());
    let registry = jetstream.get_key_value(TOOL_REGISTRY_BUCKET).await?;
    let key = registration_key(&harness.instance_id, "____test");
    assert!(registry.get(&key).await?.is_some());

    harness.shutdown().await;
    assert!(
        !harness.readiness.is_ready(),
        "server should become not ready before shutdown completes"
    );

    assert!(
        registry.get(&key).await?.is_none(),
        "registration should be gone immediately after a graceful shutdown, \
         not left to expire"
    );
    Ok(())
}

/// Shutdown must drain in-flight requests before deregistering, so a caller
/// already waiting on a reply gets one instead of blocking on its own 60s
/// timeout.
///
/// Uses `echo` with `delay_ms` rather than `slow` + a cancel control message:
/// once `serve_requests` returns (because shutdown was cancelled), the main
/// loop stops polling the `controls` subscription, so a control message sent
/// after that point would never be picked up. `delay_ms` needs nothing from
/// the main loop -- the delay runs inside the already-spawned request task.
#[tokio::test(flavor = "multi_thread")]
async fn shutdown_drains_in_flight_requests_before_deregistering() -> Result<()> {
    harnx_core::require_nextest();
    let Some(mut harness) = TestHarness::start().await? else {
        return Ok(());
    };
    assert_registration(&harness).await?;

    let request = ToolRequest {
        call_id: "call-drain".to_string(),
        tool: "echo".to_string(),
        args: json!({"delay_ms": 500}),
        parent_session_id: None,
        capabilities: Default::default(),
    };
    let delayed_request = harness.client.request_with_headers(
        harness.instance_id.tool_subject("____test", "echo"),
        request_headers(&request.call_id, "logical-drain"),
        serde_json::to_vec(&request)?.into(),
    );
    tokio::pin!(delayed_request);

    // Wait for the handler to have started (and thus be counted as
    // in-flight) without waiting for its delay to elapse.
    let started_deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if harness.toolset.echo_invocations.load(Ordering::SeqCst) >= 1 {
            break;
        }
        if Instant::now() >= started_deadline {
            anyhow::bail!("timed out waiting for the delayed echo request to start");
        }
        tokio::select! {
            result = &mut delayed_request => {
                anyhow::bail!("delayed request completed before it should have: {result:?}")
            }
            () = tokio::time::sleep(Duration::from_millis(10)) => {}
        }
    }

    // The request is now in flight, sleeping out its delay. Trigger shutdown
    // and confirm it does NOT deregister while the request is still running.
    harness.shutdown.cancel();
    let server_task = harness
        .server_task
        .take()
        .expect("server task should still be present");
    assert!(
        !server_task.is_finished(),
        "shutdown must wait for the in-flight request, not deregister immediately"
    );

    let jetstream = async_nats::jetstream::new(harness.client.clone());
    let registry = jetstream.get_key_value(TOOL_REGISTRY_BUCKET).await?;
    let key = registration_key(&harness.instance_id, "____test");
    assert!(
        registry.get(&key).await?.is_some(),
        "registration must still be present while the request is in flight"
    );

    // The caller waiting on the delayed request must still get a reply.
    let message = tokio::time::timeout(Duration::from_secs(2), &mut delayed_request).await??;
    let reply: ToolReply = serde_json::from_slice(&message.payload)?;
    assert!(reply.result.is_ok(), "got: {reply:?}");

    // Now that the in-flight request finished, shutdown should proceed
    // promptly rather than waiting out the full drain deadline.
    tokio::time::timeout(Duration::from_secs(2), server_task)
        .await
        .context("shutdown did not complete promptly after the in-flight request finished")?
        .context("join server task")??;
    assert!(
        registry.get(&key).await?.is_none(),
        "registration should be gone once the in-flight request finished and shutdown completed"
    );
    Ok(())
}
