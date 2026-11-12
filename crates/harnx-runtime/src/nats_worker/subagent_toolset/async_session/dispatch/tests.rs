use super::*;
use crate::nats_session_log::NatsSessionLog;
use crate::nats_session_metadata::SessionMetadataStore;
use crate::nats_test_common::{spawn_nats_server, NatsServerHandle};
use crate::nats_worker::subagent_toolset::{SubagentNats, SubagentSessionRoute};
use anyhow::{Context, Result};
use harnx_toolset::{CheckpointStore, PartialResultStore, ToolRequest};
use harnx_toolset_server::invocation_journal::InvocationJournal;
use std::sync::Arc;

struct Fixture {
    _server: NatsServerHandle,
    js: async_nats::jetstream::Context,
    toolset: SubagentToolset,
    journal: InvocationJournal,
    request: ToolRequest,
}

impl Fixture {
    async fn new(args: Value) -> Result<Self> {
        let server = spawn_nats_server()
            .await?
            .context("nats-server required for dispatch tests")?;
        let js = async_nats::jetstream::new(async_nats::connect(server.url()).await?);
        let metadata = SessionMetadataStore::ensure(&js, 1).await?;
        let toolset = make_toolset(&js, metadata);
        let request = ToolRequest {
            run_context: Some(crate::nats_session::test_support::inherited_tool_context()),
            replay: None,
            operation_id: "operation".into(),
            call_id: "wire-id".into(),
            tool: "session_send".into(),
            args,
            parent_session_id: Some("trusted-parent".into()),
            parent_agent: None,
            parent_local_session_id: None,
            tool_call_id: Some("transcript-call".into()),
            tool_round: Some(1),
            capabilities: Default::default(),
        };
        let journal = InvocationJournal::ensure(&js, 1).await?;
        journal
            .record(&request, ("session_send", "helper", "server"))
            .await?;
        Ok(Self {
            _server: server,
            js,
            toolset,
            journal,
            request,
        })
    }

    async fn context(&self) -> Result<ToolInvocationContext> {
        let saved = self
            .journal
            .get(&self.request)
            .await?
            .context("journal row missing")?;
        Ok(ToolInvocationContext {
            call_id: self.request.call_id.clone(),
            invoking_session_id: self.request.parent_session_id.clone(),
            run_context: self.request.run_context.clone(),
            checkpoint: saved.checkpoint,
            checkpoint_store: Some(Arc::new(self.journal.checkpoint_store(&self.request))),
            partial_result_store: Some(Arc::new(self.journal.partial_result_store(&self.request))),
            ..Default::default()
        })
    }

    async fn send(&self) -> Result<(DispatchedPrompt, Option<ResolvedWait>)> {
        dispatch_send(
            &self.toolset,
            self.request.args.clone(),
            CancellationToken::new(),
            self.context().await?,
        )
        .await
        .map_err(anyhow::Error::new)
    }

    async fn freeze_limits(
        &self,
        handle: &PromptHandle,
    ) -> Result<(
        crate::nats_session_metadata::InvocationAdmission,
        RunLimitsRecord,
    )> {
        let storage = harnx_core::session_identity::session_key(Some("helper"), &handle.session_id);
        let admission = self
            .toolset
            .session_metadata
            .prompt_admission(&storage, &handle.prompt_id)
            .await?
            .unwrap();
        let frozen = RunLimitsRecord::admit_child(
            admission.parent.as_ref().unwrap(),
            admission.invocation_id.clone(),
            InvocationEdgeKind::Delegation,
            admission.admitted_at,
            Default::default(),
            None,
            crate::nats_session_metadata::CallTimeoutOverride::Omitted,
        )?;
        self.toolset
            .session_metadata
            .put_invocation_limits(&storage, &frozen)
            .await?;
        Ok((admission, frozen))
    }

    async fn events(
        &self,
        session_id: &str,
    ) -> Result<Vec<(u64, harnx_core::session::SessionLogEntry)>> {
        NatsSessionLog::new(
            self.js.clone(),
            harnx_core::session_identity::session_key(Some("helper"), session_id),
        )
        .load_events_latest_async()
        .await
    }
}

fn make_toolset(
    js: &async_nats::jetstream::Context,
    metadata: SessionMetadataStore,
) -> SubagentToolset {
    SubagentToolset::new(
        "helper",
        SubagentSessionRoute::new("dispatch", crate::SessionActivationRoute::ClusterShared),
        SubagentNats::new(js.client().clone(), js.clone(), metadata, 1),
    )
}

#[tokio::test]
async fn lost_reply_restarts_from_journal_with_same_handle_and_frozen_deadline() -> Result<()> {
    let f = Fixture::new(json!({"message": "original", "session_id": "   "})).await?;
    let (receipt, wait) = f.send().await?;
    assert!(wait.is_none(), "omitted send.wait is dispatch-only");
    assert_eq!(receipt.dispatch, Some(DispatchConfirmation::Confirmed));
    let handle = receipt.handle;
    let seq = receipt.prompt.user_msg_seq();
    let storage = harnx_core::session_identity::session_key(Some("helper"), &handle.session_id);
    let (admission, frozen) = f.freeze_limits(&handle).await?;
    let saved = f.journal.get(&f.request).await?.unwrap();
    assert!(
        saved.reply.is_none(),
        "simulate lost tool reply, not completed journal replay"
    );
    assert_eq!(
        saved.checkpoint.as_ref().unwrap()["prompt_id"],
        handle.prompt_id
    );
    assert_eq!(
        saved.partial_result.as_ref().unwrap()["prompt_id"],
        handle.prompt_id
    );
    assert_eq!(
        saved.partial_result.as_ref().unwrap()["session_id"],
        handle.session_id
    );
    let reopened = InvocationJournal::ensure(&f.js, 1).await?;
    let restarted = make_toolset(&f.js, SessionMetadataStore::ensure(&f.js, 1).await?);
    let context = ToolInvocationContext {
        checkpoint_store: Some(Arc::new(reopened.checkpoint_store(&f.request))),
        partial_result_store: Some(Arc::new(reopened.partial_result_store(&f.request))),
        ..f.context().await?
    };
    let (replay, _) = dispatch_send(
        &restarted,
        json!({"message": "must not replace original", "timeout_secs": 99999}),
        CancellationToken::new(),
        context,
    )
    .await?;
    assert_eq!(replay.handle, handle);
    assert_eq!(replay.prompt.user_msg_seq(), seq);
    assert_eq!(
        replay.prompt.execution_id(),
        Some(handle.prompt_id.as_str())
    );
    assert_eq!(f.events(&handle.session_id).await?.len(), 1);
    assert_eq!(
        f.toolset
            .session_metadata
            .prompt_admission(&storage, &handle.prompt_id)
            .await?,
        Some(admission)
    );
    assert_eq!(
        f.toolset
            .session_metadata
            .get_invocation_limits(&storage, &handle.prompt_id)
            .await?,
        Some(frozen)
    );
    Ok(())
}

#[tokio::test]
async fn composed_plan_text_and_image_attachments_match_prompt_composer_and_replay_without_blob(
) -> Result<()> {
    use harnx_blob_store::media::{ensure_attachments_bucket, put_media};
    use harnx_core::cid_url::CidUrl;
    let image =
        "cid:media:parent/source/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let text =
        "cid:media:parent/source/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    let plan = "cid:plan:parent/source/work";
    let f = Fixture::new(json!({"message": "inspect", "session_id": "attachments", "attachments": [image, text, plan], "wait": {}})).await?;
    let bucket = ensure_attachments_bucket(&f.js, 1).await?;
    put_media(&bucket, &CidUrl::parse(image)?, b"image", "image/png").await?;
    put_media(&bucket, &CidUrl::parse(text)?, b"text", "text/plain").await?;
    let expected = f
        .toolset
        .compose_prompt_content(
            "inspect".into(),
            vec![image.into(), text.into(), plan.into()],
        )
        .await?;
    let (receipt, wait) = f.send().await?;
    assert_eq!(
        wait.unwrap().limit,
        crate::nats_worker::subagent_toolset::async_session::WaitLimit::Seconds(240)
    );
    let entries = f.events(&receipt.handle.session_id).await?;
    assert!(
        matches!(&entries[0].1, harnx_core::session::SessionLogEntry::Message { content, .. } if content == &expected)
    );
    bucket.delete(CidUrl::parse(image)?.kv_key()).await?;
    let (replay, _) = f.send().await?;
    assert_eq!(receipt.handle, replay.handle);
    assert_eq!(f.events(&receipt.handle.session_id).await?.len(), 1);
    Ok(())
}

#[tokio::test]
async fn broker_activation_rejection_returns_recoverable_handle_then_exact_replay_repairs(
) -> Result<()> {
    use async_nats::jetstream::stream::{Config, DiscardPolicy};
    let f = Fixture::new(json!({"message": "work", "session_id": "activation-rejected"})).await?;
    let subject = crate::nats_worker::notify_subject("dispatch");
    let stream =
        f.js.create_stream(Config {
            name: "WORK_NOTIFY_dispatch".into(),
            subjects: vec![subject.clone()],
            max_messages: 1,
            discard: DiscardPolicy::New,
            ..Default::default()
        })
        .await?;
    f.js.publish(subject, "block activation".into())
        .await?
        .await?;
    let (receipt, _) = f.send().await?;
    assert_eq!(receipt.dispatch, Some(DispatchConfirmation::Unconfirmed));
    let saved = f.journal.get(&f.request).await?.unwrap();
    assert_eq!(
        saved.partial_result.unwrap()["prompt_id"],
        receipt.handle.prompt_id
    );
    stream.purge().await?;
    let (replay, _) = f.send().await?;
    assert_eq!(replay.dispatch, Some(DispatchConfirmation::Confirmed));
    assert_eq!(receipt.handle, replay.handle);
    assert_eq!(receipt.prompt.user_msg_seq(), replay.prompt.user_msg_seq());
    assert_eq!(f.events(&receipt.handle.session_id).await?.len(), 1);
    Ok(())
}

#[tokio::test]
async fn cancelling_or_dropping_sender_after_dispatch_does_not_interrupt_child() -> Result<()> {
    let f =
        Fixture::new(json!({"message": "keep working", "session_id": "independent-child"})).await?;
    let cancel = CancellationToken::new();
    let (receipt, _) = dispatch_send(
        &f.toolset,
        f.request.args.clone(),
        cancel.clone(),
        f.context().await?,
    )
    .await?;
    let handle = receipt.handle;
    cancel.cancel();
    drop(receipt.prompt);
    // No worker was required to accept delivery. Its durable activation is
    // still present after the sender and its observation token are gone.
    let mut stream = f.js.get_stream("WORK_NOTIFY_dispatch").await?;
    assert_eq!(stream.info().await?.state.messages, 1);
    let entries = f.events(&handle.session_id).await?;
    assert_eq!(entries.len(), 1);
    assert!(!entries
        .iter()
        .any(|(_, entry)| matches!(entry, harnx_core::session::SessionLogEntry::Cancel { .. })));
    Ok(())
}

struct FailPartial;
#[async_trait::async_trait]
impl PartialResultStore for FailPartial {
    async fn record_partial_result(&self, _: Value) -> Result<()> {
        anyhow::bail!("injected partial journal failure")
    }
}

struct LoseCheckpointAck(Arc<dyn CheckpointStore>);
#[async_trait::async_trait]
impl CheckpointStore for LoseCheckpointAck {
    async fn checkpoint(&self, value: Value) -> Result<Value> {
        self.0.checkpoint(value).await?;
        anyhow::bail!("injected checkpoint acknowledgement loss")
    }
}

#[tokio::test]
async fn checkpoint_ack_loss_and_partial_journal_failure_happen_before_executable_append(
) -> Result<()> {
    let f = Fixture::new(json!({"message": "work"})).await?;
    let mut context = f.context().await?;
    context.checkpoint_store = Some(Arc::new(LoseCheckpointAck(
        context.checkpoint_store.take().unwrap(),
    )));
    assert!(dispatch_send(
        &f.toolset,
        f.request.args.clone(),
        CancellationToken::new(),
        context
    )
    .await
    .is_err());
    let saved = f
        .journal
        .get(&f.request)
        .await?
        .unwrap()
        .checkpoint
        .unwrap();
    let session_id = saved["session_id"].as_str().unwrap();
    assert!(f.events(session_id).await?.is_empty());
    let mut context = f.context().await?;
    context.partial_result_store = Some(Arc::new(FailPartial));
    assert!(dispatch_send(
        &f.toolset,
        f.request.args.clone(),
        CancellationToken::new(),
        context
    )
    .await
    .is_err());
    assert!(f.events(session_id).await?.is_empty());
    let (receipt, _) = f.send().await?;
    assert_eq!(receipt.handle.session_id, session_id);
    assert_eq!(f.events(session_id).await?.len(), 1);
    Ok(())
}

#[tokio::test]
async fn invalid_arguments_missing_or_forged_scope_and_pre_cancel_have_no_checkpoint() -> Result<()>
{
    let f = Fixture::new(json!({"message": "work"})).await?;
    for args in [
        json!({"message": " "}),
        json!({"message": "work", "wait": {"seconds": 241}}),
        json!({"message": "work", "attachments": ["not-cid"]}),
        json!({"message": "work", "__harnx_parent_session_id": "forged-parent"}),
        json!({"message": "work", "timeout_secs": u64::MAX}),
    ] {
        assert!(dispatch_send(
            &f.toolset,
            args,
            CancellationToken::new(),
            f.context().await?
        )
        .await
        .is_err());
    }
    let mut missing = f.context().await?;
    missing.invoking_session_id = None;
    assert!(dispatch_send(
        &f.toolset,
        f.request.args.clone(),
        CancellationToken::new(),
        missing
    )
    .await
    .is_err());
    let mut mismatch = f.context().await?;
    mismatch.invoking_session = Some(harnx_toolset::SessionRef {
        agent: Some("other".into()),
        session_id: "parent".into(),
    });
    assert!(dispatch_send(
        &f.toolset,
        f.request.args.clone(),
        CancellationToken::new(),
        mismatch
    )
    .await
    .is_err());
    let cancel = CancellationToken::new();
    cancel.cancel();
    assert!(dispatch_send(
        &f.toolset,
        f.request.args.clone(),
        cancel,
        f.context().await?
    )
    .await
    .is_err());
    assert!(f
        .journal
        .get(&f.request)
        .await?
        .unwrap()
        .checkpoint
        .is_none());
    Ok(())
}

#[tokio::test]
async fn saved_checkpoint_cannot_cross_agent_parent_cluster_or_prompt_scope() -> Result<()> {
    let f = Fixture::new(json!({"message": "work", "session_id": "checkpoint-scope"})).await?;
    let (receipt, _) = f.send().await?;
    for (field, value) in [
        ("storage_key", "another-agent-key"),
        ("parent_storage_key", "another-parent"),
        ("cluster", "another-cluster"),
        ("prompt_id", "another-prompt"),
    ] {
        let mut context = f.context().await?;
        context.checkpoint.as_mut().unwrap()[field] = json!(value);
        assert!(dispatch_send(
            &f.toolset,
            f.request.args.clone(),
            CancellationToken::new(),
            context
        )
        .await
        .is_err());
    }
    assert_eq!(f.events(&receipt.handle.session_id).await?.len(), 1);
    Ok(())
}

#[test]
fn trusted_identity_alone_derives_namespaced_parent_scope_and_full_receipt_budget_is_checked() {
    let context = ToolInvocationContext {
        invoking_session: Some(harnx_toolset::SessionRef {
            agent: Some("parent-agent".into()),
            session_id: "parent".into(),
        }),
        ..Default::default()
    };
    assert_eq!(
        trusted_parent_scope(&context).unwrap(),
        harnx_core::session_identity::session_key(Some("parent-agent"), "parent")
    );
    assert!(ensure_receipt_fits("session", &"p".repeat(500), 1024).is_err());
    assert!(ensure_receipt_fits("session", "prompt", 1024).is_ok());
}

#[tokio::test]
async fn concurrent_replays_share_one_prompt_and_one_exact_activation() -> Result<()> {
    let f = Fixture::new(json!({"message": "work"})).await?;
    let first = f.context().await?;
    let second = f.context().await?;
    let (first, second) = tokio::join!(
        dispatch_send(
            &f.toolset,
            f.request.args.clone(),
            CancellationToken::new(),
            first
        ),
        dispatch_send(
            &f.toolset,
            f.request.args.clone(),
            CancellationToken::new(),
            second
        ),
    );
    let (first, _) = first?;
    let (second, _) = second?;
    assert_eq!(first.handle, second.handle);
    assert_eq!(first.prompt.user_msg_seq(), second.prompt.user_msg_seq());
    assert_eq!(f.events(&first.handle.session_id).await?.len(), 1);
    let mut stream = f.js.get_stream("WORK_NOTIFY_dispatch").await?;
    assert_eq!(stream.info().await?.state.messages, 1);
    Ok(())
}

#[tokio::test]
async fn independent_send_to_busy_session_returns_error_not_acceptance() -> Result<()> {
    let f = Fixture::new(json!({"message": "work", "session_id": "busy"})).await?;
    let (accepted, _) = f.send().await?;
    let mut competing = f.request.clone();
    competing.call_id = "independent".into();
    competing.operation_id = "independent-operation".into();
    f.journal
        .record(&competing, ("session_send", "helper", "server"))
        .await?;
    let context = ToolInvocationContext {
        call_id: competing.call_id.clone(),
        checkpoint: None,
        checkpoint_store: Some(Arc::new(f.journal.checkpoint_store(&competing))),
        partial_result_store: Some(Arc::new(f.journal.partial_result_store(&competing))),
        ..f.context().await?
    };
    let result = dispatch_send(
        &f.toolset,
        competing.args,
        CancellationToken::new(),
        context,
    )
    .await;
    let Err(error) = result else {
        panic!("busy session claimed acceptance");
    };
    assert!(error.to_string().contains("session busy"));
    assert_eq!(f.events(&accepted.handle.session_id).await?.len(), 1);
    Ok(())
}

#[tokio::test]
async fn missing_journal_stores_fail_before_checkpoint_or_executable_append() -> Result<()> {
    let f = Fixture::new(json!({"message": "work", "session_id": "unjournaled"})).await?;
    for missing_checkpoint in [true, false] {
        let mut context = f.context().await?;
        let expected = if missing_checkpoint {
            context.checkpoint_store = None;
            "async dispatch requires durable checkpoint store"
        } else {
            context.partial_result_store = None;
            "async dispatch requires durable partial-result store"
        };
        let result = dispatch_send(
            &f.toolset,
            f.request.args.clone(),
            CancellationToken::new(),
            context,
        )
        .await;
        let Err(error) = result else {
            panic!("unjournaled dispatch was accepted");
        };
        assert_eq!(error.to_string(), expected);
        let saved = f.journal.get(&f.request).await?.unwrap();
        assert!(saved.checkpoint.is_none());
        assert!(saved.partial_result.is_none());
        assert!(f.events("unjournaled").await?.is_empty());
    }
    Ok(())
}
