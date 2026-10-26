#![cfg(unix)]
//! The shared server's KV ceiling is separate from the bridge presentation cap.

mod fixture;

use anyhow::Result;
use harnx_toolset::{ReplayAttempt, ToolErrorPayload, ToolReply};
use serde_json::json;

use fixture::{stop_bridge, Fixture};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bridge_result_below_presentation_cap_is_bounded_durably_by_independent_kv_limit(
) -> Result<()> {
    let original: rmcp::model::CallToolResult = serde_json::from_value(json!({
        "content": [{"type": "text", "text": "x".repeat(20_000)}, {"type": "text", "text": "small part"}],
        "structuredContent": {"schema": "unchanged"}, "_meta": {"kept": true}, "isError": true
    }))?;
    let original = serde_json::to_value(original)?;
    assert!(serde_json::to_vec(&original)?.len() < 32_768);
    let mut fixture = Fixture::new(None, "shared-size-call", &original).await?;
    let mut saved = None;
    for restart in [false, true] {
        let (mut child, scope) = fixture.start().await?;
        if restart {
            fixture.request.replay = Some(ReplayAttempt {
                attempt: 1,
                requested_by: "replacement-worker".into(),
            });
        }
        let message = fixture.send(&scope).await?;
        assert!(message.payload.len() < 8192);
        assert!(message.headers.is_none());
        let reply: ToolReply = serde_json::from_slice(&message.payload)?;
        assert!(matches!(
            reply.result,
            Err(ToolErrorPayload::Recoverable(_))
        ));
        fixture.assert_durable(&message).await?;
        if let Some(saved) = &saved {
            assert_eq!(&message.payload, saved);
        } else {
            saved = Some(message.payload);
        }
        fixture.assert_original_reply(&original).await?;
        stop_bridge(&mut child).await?;
    }
    fixture.assert_child_calls(1)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bridged_part_offload_and_shared_terminal_guard_work_below_default_chunk_size() -> Result<()>
{
    // Short lines leave a preview above the independent 8 KiB KV cap; a single
    // line would also be clipped by the bridge's existing line limit.
    let text = ("x".repeat(300) + "\n").repeat(1000);
    let original = json!({"content": [{"type": "text", "text": text}]});
    let fixture = Fixture::new(Some(16 * 1024), "small-wire-bridge", &original).await?;
    let (mut child, scope) = fixture.start().await?;
    let message = fixture.send(&scope).await?;
    let reply: ToolReply = serde_json::from_slice(&message.payload)?;
    let Err(ToolErrorPayload::Recoverable(error)) = &reply.result else {
        anyhow::bail!("expected recoverable terminal size result, got {reply:?}")
    };
    assert!(message.payload.len() <= 16 * 1024);
    fixture.assert_durable(&message).await?;
    let normalized = fixture.retained_bridge_result(error).await?;
    fixture.assert_text_original(&normalized, &text).await?;
    stop_bridge(&mut child).await
}
