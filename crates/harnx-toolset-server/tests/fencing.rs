mod common;
use anyhow::{Context, Result};
use common::{request_headers, TestHarness, TestToolset};
use harnx_execution_control::{ExecutionStore, Interrupted, OperationRef, Owner};
use harnx_toolset::{ToolErrorPayload, ToolReply, ToolRequest};
use harnx_toolset_server::{invocation_admission, invocation_journal::InvocationJournal};
use serde_json::json;
use std::{
    sync::{atomic::Ordering, Arc},
    time::Duration,
};
use tokio::sync::Barrier;

struct Fixture {
    harness: TestHarness,
    store: ExecutionStore,
    journal: InvocationJournal,
    request: ToolRequest,
    parent: OperationRef,
}
impl Fixture {
    async fn new(toolset: TestToolset) -> Result<Self> {
        let harness = TestHarness::with_toolset(toolset)
            .await?
            .context("nats-server required")?;
        common::wait_for_registration(&harness.client, &harness.instance_id).await?;
        let js = async_nats::jetstream::new(harness.client.clone());
        let store = ExecutionStore::ensure(&js, 1).await?;
        let parent = store
            .session("fenced-parent", None, Some("generation-one"))
            .await?
            .reference;
        store
            .claim(
                &parent,
                Owner {
                    instance_id: "worker".into(),
                    fence: 1,
                },
            )
            .await?;
        let child = OperationRef::new(&parent.session_id, "call-one");
        store.child(child.clone(), parent.clone()).await?;
        let request = ToolRequest {
            execution: Some(invocation_admission::capture(&store, &child).await?),
            replay_execution: None,
            replay: None,
            operation_id: child.execution_id.clone(),
            call_id: child.execution_id,
            tool: "echo".into(),
            args: json!({"success": true}),
            parent_session_id: Some(parent.session_id.clone()),
            tool_call_id: Some("model-call".into()),
            capabilities: Default::default(),
        };
        let journal = InvocationJournal::ensure(&js).await?;
        journal
            .record(&request, ("test_echo", "scope", "____test"), 1)
            .await?;
        Ok(Self {
            harness,
            store,
            journal,
            request,
            parent,
        })
    }
    fn send(&self) -> tokio::task::JoinHandle<Result<ToolReply>> {
        let client = self.harness.client.clone();
        let subject = self.harness.echo_subject();
        let request = self.request.clone();
        tokio::spawn(async move {
            let message = client
                .send_request(
                    subject,
                    async_nats::Request::new()
                        .headers(request_headers(&request.call_id, &request.call_id))
                        .payload(serde_json::to_vec(&request)?.into())
                        .timeout(Some(Duration::from_secs(15))),
                )
                .await?;
            Ok(serde_json::from_slice(&message.payload)?)
        })
    }
    async fn restart_reader(&self) -> Result<InvocationJournal> {
        let client = async_nats::ConnectOptions::new()
            .token(common::TOKEN.into())
            .connect(&self.harness._server.url)
            .await?;
        InvocationJournal::ensure(&async_nats::jetstream::new(client)).await
    }
    async fn stop(&self) -> Result<()> {
        self.store
            .cancel_operation(&self.parent, Some("stop-generation"), false)
            .await?;
        Ok(())
    }
    async fn assert_recovery_interrupted(&self) -> Result<()> {
        let reader = self.restart_reader().await?;
        assert!(reader
            .completed_reply(&self.request)
            .await
            .unwrap_err()
            .is::<Interrupted>());
        Ok(())
    }
}
fn assert_interrupted(reply: ToolReply) {
    assert!(
        matches!(reply.result, Err(ToolErrorPayload::Interrupted(_))),
        "{reply:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interrupt_before_journal_completion_discards_late_success_and_duplicate() -> Result<()> {
    let ready = Arc::new(Barrier::new(2));
    let release = Arc::new(Barrier::new(2));
    let mut tool = TestToolset::default();
    tool.reply_barriers = Some((ready.clone(), release.clone()));
    let mut fixture = Fixture::new(tool).await?;
    let call = fixture.send();
    ready.wait().await;
    fixture.stop().await?;
    release.wait().await;
    assert_interrupted(call.await??);
    assert!(fixture
        .journal
        .get(&fixture.request)
        .await?
        .unwrap()
        .reply
        .is_none());
    fixture.assert_recovery_interrupted().await?;
    assert_interrupted(fixture.send().await??);
    assert_eq!(
        fixture
            .harness
            .toolset
            .echo_invocations
            .load(Ordering::SeqCst),
        1
    );
    fixture.harness.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn committed_reply_remains_history_but_cache_and_restart_consumption_stop() -> Result<()> {
    let mut fixture = Fixture::new(TestToolset::default()).await?;
    assert!(fixture.send().await??.result.is_ok());
    let saved = fixture.journal.get(&fixture.request).await?.unwrap();
    assert!(saved.reply_commit.is_some());
    fixture.stop().await?;
    assert_eq!(
        fixture.journal.get(&fixture.request).await?.unwrap().reply,
        saved.reply
    );
    assert_interrupted(fixture.send().await??);
    fixture.assert_recovery_interrupted().await?;
    assert_eq!(
        fixture
            .harness
            .toolset
            .echo_invocations
            .load(Ordering::SeqCst),
        1
    );
    fixture.harness.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retired_operation_requires_retained_authority_and_proof() -> Result<()> {
    let mut fixture = Fixture::new(TestToolset::default()).await?;
    assert!(fixture.send().await??.result.is_ok());
    fixture.store.status(&fixture.parent).await?;
    let producer = &fixture.request.execution.as_ref().unwrap().producer;
    assert!(fixture.store.get(producer.operation()).await?.is_none());
    let reader = fixture.restart_reader().await?;
    assert!(reader
        .completed_reply(&fixture.request)
        .await?
        .unwrap()
        .result
        .is_ok());
    fixture.stop().await?;
    fixture.assert_recovery_interrupted().await?;
    fixture.harness.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn legacy_saved_success_without_generation_or_commit_is_not_recoverable() -> Result<()> {
    let mut fixture = Fixture::new(TestToolset::default()).await?;
    let mut legacy = fixture.request.clone();
    legacy.call_id = "legacy".into();
    legacy.operation_id = "legacy".into();
    legacy.execution = None;
    fixture
        .journal
        .record(&legacy, ("test_echo", "scope", "____test"), 2)
        .await?;
    let kv = async_nats::jetstream::new(fixture.harness.client.clone())
        .get_key_value(harnx_toolset_server::invocation_journal::BUCKET)
        .await?;
    let key = "sessions/fenced-parent/legacy";
    let mut record = fixture.journal.get(&legacy).await?.unwrap();
    record.reply = Some(ToolReply {
        call_id: legacy.call_id.clone(),
        result: Ok(json!("unproved")),
    });
    kv.put(key, serde_json::to_vec(&record)?.into()).await?;
    assert!(fixture
        .restart_reader()
        .await?
        .completed_reply(&legacy)
        .await
        .is_err());
    fixture.harness.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interrupted_replay_attempt_cannot_claim_or_redispatch() -> Result<()> {
    let mut tool = TestToolset::default();
    tool.idempotent = true;
    let mut fixture = Fixture::new(tool).await?;
    let consumer = fixture.request.execution.as_ref().unwrap().consumer.clone();
    fixture.request.replay = Some(consumer.owner().clone());
    invocation_admission::prepare_replay(&fixture.store, &mut fixture.request, consumer).await?;
    let ready = Arc::new(Barrier::new(2));
    let release = Arc::new(Barrier::new(2));
    let store = fixture.store.clone();
    let request = fixture.request.clone();
    let task = {
        let ready = ready.clone();
        let release = release.clone();
        tokio::spawn(async move {
            ready.wait().await;
            release.wait().await;
            harnx_toolset_server::reply_fence::admit(&store, &request).await
        })
    };
    ready.wait().await;
    fixture.stop().await?;
    release.wait().await;
    assert!(task.await?.unwrap_err().is::<Interrupted>());
    assert_interrupted(fixture.send().await??);
    assert_eq!(
        fixture
            .harness
            .toolset
            .echo_invocations
            .load(Ordering::SeqCst),
        0
    );
    fixture.harness.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restart_recovers_committed_blob_without_journal_projection_but_not_after_stop(
) -> Result<()> {
    let mut fixture = Fixture::new(TestToolset::default()).await?;
    // Larger than the gate's bounded action record. The commit binds the exact
    // immutable blob by digest rather than weakening that storage bound.
    let reply = ToolReply {
        call_id: fixture.request.call_id.clone(),
        result: Ok(json!({"text": "x".repeat(96 * 1024)})),
    };
    fixture
        .journal
        .complete(&fixture.request, reply.clone())
        .await?;
    let mut record = fixture.journal.get(&fixture.request).await?.unwrap();
    record.reply = None;
    record.reply_commit = None;
    let kv = async_nats::jetstream::new(fixture.harness.client.clone())
        .get_key_value(harnx_toolset_server::invocation_journal::BUCKET)
        .await?;
    kv.put(
        "sessions/fenced-parent/call-one",
        serde_json::to_vec(&record)?.into(),
    )
    .await?;
    assert_eq!(
        fixture
            .restart_reader()
            .await?
            .completed_reply(&fixture.request)
            .await?,
        Some(reply)
    );
    fixture.stop().await?;
    fixture.assert_recovery_interrupted().await?;
    fixture.harness.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn legacy_metadata_loss_on_pruned_interrupted_node_is_still_typed() -> Result<()> {
    let mut fixture = Fixture::new(TestToolset::default()).await?;
    assert!(fixture.send().await??.result.is_ok());
    fixture.store.status(&fixture.parent).await?;
    fixture.stop().await?;
    let kv = async_nats::jetstream::new(fixture.harness.client.clone())
        .get_key_value(harnx_toolset_server::invocation_journal::BUCKET)
        .await?;
    let mut record = fixture.journal.get(&fixture.request).await?.unwrap();
    record.request.execution = None;
    record.reply_commit = None;
    fixture.request = record.request.clone();
    kv.put(
        "sessions/fenced-parent/call-one",
        serde_json::to_vec(&record)?.into(),
    )
    .await?;
    fixture.assert_recovery_interrupted().await?;
    fixture.harness.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pruned_generation_reply_and_cached_duplicate_remain_interrupted_after_g2() -> Result<()> {
    let mut fixture = Fixture::new(TestToolset::default()).await?;
    assert!(fixture.send().await??.result.is_ok());
    fixture.store.status(&fixture.parent).await?;
    let producer = &fixture.request.execution.as_ref().unwrap().producer;
    assert!(fixture.store.get(producer.operation()).await?.is_none());
    fixture.stop().await?;
    fixture
        .store
        .mutate(&fixture.parent, |operation| {
            operation.transition(harnx_execution_control::OperationState::Unconfirmed)
        })
        .await?;
    fixture
        .store
        .abandon_unconfirmed(&fixture.parent.session_id, &fixture.parent.execution_id)
        .await?;
    let next = fixture
        .store
        .session(&fixture.parent.session_id, None, Some("generation-two"))
        .await?;
    fixture
        .store
        .claim(
            &next.reference,
            Owner {
                instance_id: "new-worker".into(),
                fence: 2,
            },
        )
        .await?;
    fixture.store.activate_gate(&next.reference).await?;
    assert!(fixture.store.get(&fixture.parent).await?.is_none());
    fixture.assert_recovery_interrupted().await?;
    assert_interrupted(fixture.send().await??);
    assert_eq!(
        fixture
            .harness
            .toolset
            .echo_invocations
            .load(Ordering::SeqCst),
        1
    );
    fixture.harness.shutdown().await;
    Ok(())
}
