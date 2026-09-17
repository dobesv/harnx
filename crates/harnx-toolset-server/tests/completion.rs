mod common;

use anyhow::{Context, Result};
use common::{request_headers, wait_for_registration, TestHarness};
use harnx_toolset::{ToolErrorPayload, ToolReply, ToolRequest};
use harnx_toolset_server::invocation_journal::InvocationJournal;
use serde_json::json;
use std::time::Duration;

/// A handler that parks mid-call still owns its own outcome: whatever it
/// returns once released is what the caller and the journal both see.
async fn parked_tool_reply(original_error: Option<&str>) -> Result<()> {
    let harness = TestHarness::start()
        .await?
        .context("nats-server required")?;
    wait_for_registration(&harness.client, &harness.instance_id).await?;
    let call_id = "parked-call";
    let request = ToolRequest {
        replay: None,
        call_id: call_id.into(),
        operation_id: call_id.into(),
        tool: "echo".into(),
        args: json!({"park_result": true, "error": original_error}),
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
        result = &mut pending => anyhow::bail!("tool returned before it parked: {result:?}"),
        _ = tokio::time::sleep(Duration::from_secs(5)) => anyhow::bail!("tool did not start"),
    }
    harness.toolset.allow_cleanup.notify_one();
    let message = tokio::time::timeout(Duration::from_secs(5), pending).await??;
    let reply: ToolReply = serde_json::from_slice(&message.payload)?;
    let expected = match original_error {
        Some(original) => Err(ToolErrorPayload::Recoverable(original.into())),
        None => Ok(request.args.clone()),
    };
    assert_eq!(reply.result, expected);
    let journal =
        InvocationJournal::ensure(&async_nats::jetstream::new(harness.client.clone()), 1).await?;
    assert_eq!(
        journal.get(&request).await?.context("journal row")?.reply,
        Some(reply)
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn parked_tool_replies_with_its_own_result_once_released() -> Result<()> {
    parked_tool_reply(None).await
}

#[tokio::test(flavor = "multi_thread")]
async fn parked_tool_reports_its_original_error() -> Result<()> {
    parked_tool_reply(Some("child worker stopped without answering")).await
}
