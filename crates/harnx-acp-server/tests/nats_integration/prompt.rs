//! Prompt streaming and model-error behavior.

use std::sync::Arc;

use agent_client_protocol::schema::v1::StopReason;
use anyhow::{Context, Result};
use harnx_core::event::{AgentEvent, ContentBlock, ModelEvent};
use harnx_runtime::AgentCallFn;

use super::support::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn prompt_turn_streams_in_order() -> Result<()> {
    harnx_core::require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        return Ok(());
    };
    let config = test_config(&server.url);
    let call_fn: AgentCallFn = Arc::new(|_input, _config, _abort| {
        Box::pin(async move {
            for text in ["hello ", "from ", "assistant"] {
                harnx_core::sink::emit_agent_event(AgentEvent::Model(ModelEvent::MessageChunk {
                    blocks: vec![ContentBlock::Text(text.to_string())],
                }));
                tokio::task::yield_now().await;
            }
            Ok((
                "hello from assistant".to_string(),
                None,
                vec![],
                harnx_runtime::client::CompletionTokenUsage::default(),
            ))
        })
    });
    let worker = spawn_worker(Arc::clone(&config), call_fn).await?;
    let agent = new_agent(&config);
    let mut client = attach_test_client(Arc::clone(&agent), PermissionReply::Reject).await?;
    let session_id = initialize_and_create_session(&agent).await?;

    let response = tokio::time::timeout(
        TEST_TIMEOUT,
        agent.prompt(text_prompt(session_id.clone(), "say hello")),
    )
    .await
    .context("prompt did not finish")??;
    assert_eq!(response.stop_reason, StopReason::EndTurn);

    let mut chunks = Vec::new();
    while chunks.len() < 3 {
        let notification = tokio::time::timeout(TEST_TIMEOUT, client.notifications.recv())
            .await
            .context("timed out waiting for session/update")?
            .context("ACP notification stream closed")?;
        assert_eq!(notification.session_id, session_id);
        if let Some(text) = notification_text(notification) {
            chunks.push(text);
        }
    }
    assert_eq!(chunks, ["hello ", "from ", "assistant"]);

    worker.abort();
    let _ = worker.await;
    Ok(())
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn worker_model_error_is_returned_to_acp_caller() -> Result<()> {
    harnx_core::require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        return Ok(());
    };
    let config = test_config(&server.url);
    let call_fn: AgentCallFn = Arc::new(|_input, _config, _abort| {
        Box::pin(async move { anyhow::bail!("simulated model failure") })
    });
    let worker = spawn_worker(Arc::clone(&config), call_fn).await?;
    let agent = new_agent(&config);
    let session_id = initialize_and_create_session(&agent).await?;

    let error = tokio::time::timeout(
        TEST_TIMEOUT,
        agent.prompt(text_prompt(session_id, "fail this turn")),
    )
    .await
    .context("failed model turn did not return")?
    .expect_err("worker model failure must be an ACP error");
    assert!(
        error.to_string().contains("simulated model failure"),
        "unexpected ACP error: {error}"
    );

    worker.abort();
    let _ = worker.await;
    Ok(())
}
