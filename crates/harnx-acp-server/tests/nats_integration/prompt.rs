//! Prompt streaming and model-error behavior.

use std::sync::Arc;

use agent_client_protocol::schema::v1::{
    AudioContent, ContentBlock as AcpContentBlock, EmbeddedResource, EmbeddedResourceResource,
    ImageContent, PromptRequest, ResourceLink, StopReason, TextResourceContents,
};
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

/// Test that an image content block in the prompt is rejected with an ACP error
/// and does not start a turn or modify session state.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn image_prompt_is_rejected_without_turn() -> Result<()> {
    harnx_core::require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        return Ok(());
    };
    let config = test_config(&server.url);
    // Worker will panic if called; test verifies worker is NOT invoked
    let call_fn: AgentCallFn = Arc::new(|_input, _config, _abort| {
        Box::pin(async move {
            panic!("worker should not be called for rejected image prompt");
        })
    });
    let worker = spawn_worker(Arc::clone(&config), call_fn).await?;
    let agent = new_agent(&config);
    let session_id = initialize_and_create_session(&agent).await?;

    let request = PromptRequest::new(
        session_id.clone(),
        vec![AcpContentBlock::Image(ImageContent::new(
            "base64".to_string(),
            "image/png".to_string(),
        ))],
    );

    let error = tokio::time::timeout(TEST_TIMEOUT, agent.prompt(request))
        .await
        .context("image prompt did not return")?
        .expect_err("image prompt must be rejected");

    assert!(
        error
            .to_string()
            .contains("image content blocks are not supported"),
        "unexpected error: {error}"
    );

    worker.abort();
    let _ = worker.await;
    Ok(())
}

/// Test that an audio content block in the prompt is rejected with an ACP error.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn audio_prompt_is_rejected_without_turn() -> Result<()> {
    harnx_core::require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        return Ok(());
    };
    let config = test_config(&server.url);
    let call_fn: AgentCallFn = Arc::new(|_input, _config, _abort| {
        Box::pin(async move {
            panic!("worker should not be called for rejected audio prompt");
        })
    });
    let worker = spawn_worker(Arc::clone(&config), call_fn).await?;
    let agent = new_agent(&config);
    let session_id = initialize_and_create_session(&agent).await?;

    let request = PromptRequest::new(
        session_id.clone(),
        vec![AcpContentBlock::Audio(AudioContent::new(
            "base64".to_string(),
            "audio/mp3".to_string(),
        ))],
    );

    let error = tokio::time::timeout(TEST_TIMEOUT, agent.prompt(request))
        .await
        .context("audio prompt did not return")?
        .expect_err("audio prompt must be rejected");

    assert!(
        error
            .to_string()
            .contains("audio content blocks are not supported"),
        "unexpected error: {error}"
    );

    worker.abort();
    let _ = worker.await;
    Ok(())
}

/// Test that a text resource (EmbeddedResource with TextResourceContents) is accepted
/// and delivered to the worker.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn embedded_text_resource_is_accepted() -> Result<()> {
    harnx_core::require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        return Ok(());
    };
    let config = test_config(&server.url);
    let call_fn: AgentCallFn = Arc::new(|input, _config, _abort| {
        Box::pin(async move {
            // Verify the input contains the embedded resource format
            assert!(
                input.text().contains("--- Embedded Resource:"),
                "expected embedded resource format in prompt: {}",
                input.text()
            );
            Ok((
                "response".to_string(),
                None,
                vec![],
                harnx_runtime::client::CompletionTokenUsage::default(),
            ))
        })
    });
    let worker = spawn_worker(Arc::clone(&config), call_fn).await?;
    let agent = new_agent(&config);
    let session_id = initialize_and_create_session(&agent).await?;

    let text_resource = TextResourceContents::new("embedded content", "file:///test.txt");
    let request = PromptRequest::new(
        session_id.clone(),
        vec![AcpContentBlock::Resource(EmbeddedResource::new(
            EmbeddedResourceResource::TextResourceContents(text_resource),
        ))],
    );

    let response = tokio::time::timeout(TEST_TIMEOUT, agent.prompt(request))
        .await
        .context("embedded resource prompt did not finish")??;

    assert_eq!(response.stop_reason, StopReason::EndTurn);

    worker.abort();
    let _ = worker.await;
    Ok(())
}

/// Test that a ResourceLink is accepted and delivered to the worker.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn resource_link_is_accepted() -> Result<()> {
    harnx_core::require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        return Ok(());
    };
    let config = test_config(&server.url);
    let call_fn: AgentCallFn = Arc::new(|input, _config, _abort| {
        Box::pin(async move {
            assert!(
                input.text().contains("[Resource Link:"),
                "expected resource link format in prompt: {}",
                input.text()
            );
            Ok((
                "response".to_string(),
                None,
                vec![],
                harnx_runtime::client::CompletionTokenUsage::default(),
            ))
        })
    });
    let worker = spawn_worker(Arc::clone(&config), call_fn).await?;
    let agent = new_agent(&config);
    let session_id = initialize_and_create_session(&agent).await?;

    let request = PromptRequest::new(
        session_id.clone(),
        vec![AcpContentBlock::ResourceLink(ResourceLink::new(
            "doc".to_string(),
            "file:///doc.md".to_string(),
        ))],
    );

    let response = tokio::time::timeout(TEST_TIMEOUT, agent.prompt(request))
        .await
        .context("resource link prompt did not finish")??;

    assert_eq!(response.stop_reason, StopReason::EndTurn);

    worker.abort();
    let _ = worker.await;
    Ok(())
}
