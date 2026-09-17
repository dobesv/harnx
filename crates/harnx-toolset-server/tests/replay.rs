mod common;
use anyhow::{Context, Result};
use common::{request_headers, wait_for_registration, TestHarness};
use harnx_toolset::{ReplayAttempt, ToolReply, ToolRequest};
use harnx_toolset_server::invocation_journal::InvocationJournal;
use serde_json::json;
use std::sync::atomic::Ordering;

/// A call the original worker journaled but never observed the result of, now
/// being retried by its replacement.
async fn interrupted_call(
    harness: &TestHarness,
    tool: &str,
) -> Result<(InvocationJournal, ToolRequest)> {
    wait_for_registration(&harness.client, &harness.instance_id).await?;
    let js = async_nats::jetstream::new(harness.client.clone());
    let mut request = common::request("replay-parent", "original-call");
    request.tool = tool.into();
    let journal = InvocationJournal::ensure(&js, 1).await?;
    journal
        .record(
            &request,
            (&format!("test_{tool}"), "test-scope", "____test"),
            7,
        )
        .await?;
    request.replay = Some(ReplayAttempt {
        attempt: 1,
        requested_by: "replacement-worker".into(),
    });
    Ok((journal, request))
}

async fn replay(harness: &TestHarness, request: &ToolRequest) -> Result<ToolReply> {
    let message = harness
        .client
        .request_with_headers(
            harness.instance_id.tool_subject("____test", &request.tool),
            request_headers(&request.call_id, &request.call_id),
            serde_json::to_vec(request)?.into(),
        )
        .await?;
    Ok(serde_json::from_slice(&message.payload)?)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tool_server_replays_idempotent_operation_and_persists_reply() -> Result<()> {
    let mut toolset = common::TestToolset::default();
    toolset.idempotent = true;
    let mut harness = TestHarness::with_toolset(toolset)
        .await?
        .context("nats-server required")?;
    let (journal, request) = interrupted_call(&harness, "echo").await?;
    assert_eq!(
        replay(&harness, &request).await?.result.unwrap(),
        json!({"value": 42})
    );
    assert_eq!(harness.toolset.echo_invocations.load(Ordering::SeqCst), 1);
    assert_eq!(
        journal
            .get(&request)
            .await?
            .unwrap()
            .reply
            .unwrap()
            .result
            .unwrap(),
        json!({"value": 42})
    );
    harness.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn durable_reply_is_returned_without_reinvoking_after_cache_loss() -> Result<()> {
    let mut harness = TestHarness::start()
        .await?
        .context("nats-server required")?;
    let (journal, request) = interrupted_call(&harness, "echo").await?;
    let saved = ToolReply {
        call_id: request.call_id.clone(),
        result: Ok(json!({"saved": true})),
    };
    journal.complete(&request, saved.clone()).await?;
    assert_eq!(
        tokio::time::timeout(
            std::time::Duration::from_secs(15),
            replay(&harness, &request)
        )
        .await
        .context("replay timed out")??,
        saved
    );
    assert_eq!(harness.toolset.echo_invocations.load(Ordering::SeqCst), 0);
    let js = async_nats::jetstream::new(harness.client.clone());
    tokio::time::timeout(
        std::time::Duration::from_secs(15),
        journal.purge_session("replay-parent"),
    )
    .await
    .context("purge timed out")??;
    assert!(InvocationJournal::ensure(&js, 1)
        .await?
        .get(&request)
        .await?
        .is_none());
    harness.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tool_server_rejects_non_retryable_replay_without_invoking() -> Result<()> {
    let mut harness = TestHarness::start()
        .await?
        .context("nats-server required")?;
    let (_, request) = interrupted_call(&harness, "echo").await?;
    let error = replay(&harness, &request).await?.result.unwrap_err();
    assert!(
        matches!(error, harnx_toolset::ToolErrorPayload::Recoverable(message) if message.contains("cannot replay"))
    );
    assert_eq!(harness.toolset.echo_invocations.load(Ordering::SeqCst), 0);
    harness.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn replay_cannot_change_original_arguments() -> Result<()> {
    let mut toolset = common::TestToolset::default();
    toolset.idempotent = true;
    let mut harness = TestHarness::with_toolset(toolset)
        .await?
        .context("nats-server required")?;
    let (_, mut request) = interrupted_call(&harness, "echo").await?;
    request.args = json!({"value": "different operation"});
    assert!(replay(&harness, &request).await?.result.is_err());
    assert_eq!(harness.toolset.echo_invocations.load(Ordering::SeqCst), 0);
    harness.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ordinary_duplicate_must_match_the_journal_even_with_cached_reply() -> Result<()> {
    let mut harness = TestHarness::start()
        .await?
        .context("nats-server required")?;
    let (journal, mut request) = interrupted_call(&harness, "echo").await?;
    journal
        .complete(
            &request,
            ToolReply {
                call_id: request.call_id.clone(),
                result: Ok(json!("saved")),
            },
        )
        .await?;
    assert_eq!(
        replay(&harness, &request).await?.result.unwrap(),
        json!("saved")
    );
    request.replay = None;
    request.args = json!({"different": true});
    assert!(replay(&harness, &request).await?.result.is_err());
    request.args = json!({"value": 42});
    journal.purge_session("replay-parent").await?;
    assert!(replay(&harness, &request).await?.result.is_err());
    assert_eq!(harness.toolset.echo_invocations.load(Ordering::SeqCst), 0);
    harness.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn journal_separates_standalone_calls_and_requires_persisted_records() -> Result<()> {
    let (_server, journal) = common::journal().await;
    let mut request = common::request("standalone", "standalone-call");
    request.parent_session_id = None;
    journal
        .record(&request, ("test_echo", "test-scope", "____test"), 1)
        .await?;
    let standalone = ToolReply {
        call_id: request.call_id.clone(),
        result: Ok(json!("standalone")),
    };
    journal.complete(&request, standalone.clone()).await?;
    request.parent_session_id = Some("standalone".into());
    journal
        .record(&request, ("test_echo", "test-scope", "____test"), 1)
        .await?;
    assert!(journal.get(&request).await?.unwrap().reply.is_none());
    journal.purge_session("standalone").await?;
    assert!(journal
        .record(&request, ("test_echo", "test-scope", "____test"), 1)
        .await
        .is_err());
    assert!(journal
        .complete(&request, standalone.clone())
        .await
        .is_err());
    assert!(journal
        .checkpoint("missing-session", "missing-call", json!({}))
        .await
        .is_err());
    request.parent_session_id = None;
    assert_eq!(
        journal.get(&request).await?.unwrap().reply,
        Some(standalone)
    );
    Ok(())
}
