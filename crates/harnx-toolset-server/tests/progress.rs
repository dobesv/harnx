mod common;

use anyhow::Result;
use async_trait::async_trait;
use common::{request_headers, serve, tool_spec};
use futures_util::StreamExt;
use harnx_toolset::{
    ProgressChunk, ProgressMessage, ToolInvocation, ToolInvokeError, ToolProgressLocation,
    ToolProgressPatch, ToolReply, ToolRequest, ToolSpec, Toolset, CAPABILITY_TOOL_PROGRESS,
    TOOL_PROGRESS_MAX_LOCATIONS, TOOL_PROGRESS_MAX_STRING_BYTES,
};
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

struct ProgressToolset;

#[async_trait]
impl Toolset for ProgressToolset {
    fn name(&self) -> &str {
        "progress"
    }

    fn tools(&self) -> Vec<ToolSpec> {
        vec![tool_spec("work")]
    }

    async fn invoke(
        &self,
        _tool: &str,
        _args: Value,
        _cancel: CancellationToken,
    ) -> Result<Value, ToolInvokeError> {
        panic!("server must call invoke_with_context")
    }

    async fn invoke_with_context(
        &self,
        invocation: ToolInvocation,
    ) -> Result<Value, ToolInvokeError> {
        invocation.context.progress.update(ToolProgressPatch {
            title: Some("é".repeat(TOOL_PROGRESS_MAX_STRING_BYTES)),
            locations: Some(
                (0..TOOL_PROGRESS_MAX_LOCATIONS + 10)
                    .map(|index| ToolProgressLocation {
                        path: format!("src/{index}.rs").into(),
                        line: Some(index as u32),
                    })
                    .collect(),
            ),
            markdown: Some("Working".into()),
            ..Default::default()
        });
        Ok(json!({"ok": true}))
    }
}

fn request(call_id: &str, supports_progress: bool) -> ToolRequest {
    ToolRequest {
        replay: None,
        operation_id: call_id.to_string(),
        call_id: call_id.to_string(),
        tool: "work".into(),
        args: json!({}),
        parent_session_id: None,
        tool_call_id: None,
        capabilities: if supports_progress {
            BTreeSet::from([CAPABILITY_TOOL_PROGRESS.to_string()])
        } else {
            BTreeSet::new()
        },
    }
}

async fn call(
    client: &async_nats::Client,
    subject: String,
    request: &ToolRequest,
) -> Result<ToolReply> {
    let message = client
        .request_with_headers(
            subject,
            request_headers(&request.call_id, &request.operation_id),
            serde_json::to_vec(request)?.into(),
        )
        .await?;
    Ok(serde_json::from_slice(&message.payload)?)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn negotiated_progress_is_bounded_published_and_attached_to_reply() -> Result<()> {
    let (server, client, _journal) = serve(ProgressToolset).await;
    let mut updates = client.subscribe(server.control_subject()).await?;
    client.flush().await?;

    let opted_in = request("progress-enabled", true);
    let reply = call(&client, server.tool_subject("work"), &opted_in).await?;
    assert_eq!(reply.result.expect("tool succeeds"), json!({"ok": true}));
    let final_progress = reply.final_progress.expect("final progress snapshot");
    assert!(final_progress.title.unwrap().len() <= TOOL_PROGRESS_MAX_STRING_BYTES);
    assert_eq!(
        final_progress.locations.unwrap().len(),
        TOOL_PROGRESS_MAX_LOCATIONS
    );

    let message = tokio::time::timeout(Duration::from_secs(2), updates.next())
        .await?
        .expect("progress subscription remains open");
    let progress: ProgressMessage = serde_json::from_slice(&message.payload)?;
    assert_eq!(progress.call_id, opted_in.call_id);
    let ProgressChunk::V1(patch) = progress.chunk;
    assert_eq!(patch.markdown.as_deref(), Some("Working"));
    assert_eq!(patch.locations.unwrap().len(), TOOL_PROGRESS_MAX_LOCATIONS);

    let opted_out = request("progress-disabled", false);
    let reply = call(&client, server.tool_subject("work"), &opted_out).await?;
    assert!(reply.final_progress.is_none());
    assert!(
        tokio::time::timeout(Duration::from_millis(400), updates.next())
            .await
            .is_err()
    );
    Ok(())
}
