mod common;

use anyhow::{Context, Result};
use common::{request_headers, wait_for_registration, TestHarness};
use harnx_execution_control::{ExecutionStore, OperationRef, Owner};
use harnx_toolset::{ToolErrorPayload, ToolReply, ToolRequest};
use serde_json::json;
use std::time::Duration;

async fn orphaned_child_reply(original_error: Option<&str>) -> Result<()> {
    let harness = TestHarness::start()
        .await?
        .context("nats-server required")?;
    wait_for_registration(&harness.client, &harness.instance_id).await?;
    let store =
        ExecutionStore::ensure(&async_nats::jetstream::new(harness.client.clone()), 1).await?;
    let call_id = "orphaned-child-call";
    let request = ToolRequest {
        call_id: call_id.into(),
        operation_id: call_id.into(),
        tool: "echo".into(),
        args: json!({"gate_result": true, "error": original_error}),
        parent_session_id: None,
        tool_call_id: None,
        capabilities: Default::default(),
    };
    let pending = harness.client.send_request(
        harness.echo_subject(),
        async_nats::Request::new()
            .headers(request_headers(call_id, call_id))
            .payload(serde_json::to_vec(&request)?.into())
            .timeout(None),
    );
    tokio::pin!(pending);
    tokio::select! {
        _ = harness.toolset.slow_started.notified() => {},
        result = &mut pending => anyhow::bail!("tool returned before the gate: {result:?}"),
        _ = tokio::time::sleep(Duration::from_secs(5)) => anyhow::bail!("tool did not start"),
    }
    let reference = OperationRef::new(call_id, call_id);
    let child = store
        .session("orphaned-session", Some(reference.clone()), None)
        .await?;
    store
        .claim(&child.reference, Owner::invocation("vanished-worker"))
        .await?;
    // The child's worker disappears without recording owner_stopped. The
    // tool future still completes, just as a sub-agent's lease watchdog does.
    harness.toolset.allow_cleanup.notify_one();
    let message = tokio::time::timeout(Duration::from_secs(10), pending).await??;
    let reply: ToolReply = serde_json::from_slice(&message.payload)?;
    let Err(ToolErrorPayload::Fatal(error)) = reply.result else {
        anyhow::bail!("must report unconfirmed shutdown, got {:?}", reply.result);
    };
    assert!(error.contains("tool shutdown unconfirmed"), "{error}");
    if let Some(original_error) = original_error {
        assert!(
            error.contains(original_error),
            "original failure was lost: {error}"
        );
    }
    let operation = store
        .get(&reference)
        .await?
        .context("invocation retained")?;
    assert!(operation.owner_stopped);
    assert!(
        !operation.state.is_terminal(),
        "must not attest descendant shutdown"
    );
    assert!(operation.children.contains(&child.reference));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn completed_tool_with_orphaned_child_replies_instead_of_waiting_forever() -> Result<()> {
    orphaned_child_reply(None).await
}

#[tokio::test(flavor = "multi_thread")]
async fn failed_tool_with_orphaned_child_reports_original_error() -> Result<()> {
    orphaned_child_reply(Some(
        "child worker stopped without answering (session_id: orphaned-session)",
    ))
    .await
}
