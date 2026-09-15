use super::*;
use crate::nats_session_metadata::{
    MetadataOutput, SessionInitializer, SessionMetadata, SessionMetadataStore,
    SessionOverrideUpdate,
};
use crate::nats_test_common as common;
use harnx_execution_control::{
    ExecutionContext, ExecutionStore, InterruptScope, OperationRef, Owner,
};
use std::sync::Arc;
use tokio::sync::Barrier;

struct Fixture {
    _server: common::NatsServerHandle,
    fence: GenerationFence,
    log: NatsSessionLog,
    js: jetstream::Context,
}

impl Fixture {
    async fn new() -> Result<Self> {
        let server = common::spawn_nats_server()
            .await?
            .context("nats-server required")?;
        let js = jetstream::new(async_nats::connect(server.url()).await?);
        let store = ExecutionStore::ensure(&js, 1).await?;
        let key = harnx_core::session_identity::session_key(None, "session");
        let ctx = store
            .open_gate(OperationRef::new(&key, "g1"), Owner::invocation("worker"))
            .await?;
        Ok(Self {
            _server: server,
            fence: GenerationFence::new(store, ctx),
            log: NatsSessionLog::new(js.clone(), key),
            js,
        })
    }

    async fn stop(&self) -> Result<()> {
        let ctx = &self.fence.context;
        self.fence
            .store
            .interrupt(
                &InterruptScope {
                    gate_root: ctx.gate_root().clone(),
                    operation: ctx.operation().clone(),
                    reason: "stop".into(),
                },
                "stop",
            )
            .await?;
        Ok(())
    }

    async fn replace(&self) -> Result<GenerationFence> {
        let ctx = &self.fence.context;
        let next = OperationRef::new(&ctx.generation().session_id, "g2");
        self.fence
            .store
            .commit_if_admissible(
                ctx,
                CommitAction {
                    id: "replace".into(),
                    kind: GateAction::ReplaceGeneration {
                        generation: next.clone(),
                        owner: ctx.owner().clone(),
                    },
                },
            )
            .await?;
        Ok(GenerationFence::new(
            self.fence.store.clone(),
            ExecutionContext::new(
                next.clone(),
                ctx.gate_root().clone(),
                next,
                (ctx.owner().clone(), ctx.owner().clone()),
            ),
        ))
    }

    async fn transcript(&self, entry: SessionLogEntry) -> Result<CommitReceipt> {
        self.fence
            .output(
                OutputKind::Transcript,
                serde_json::to_value(TranscriptOutput {
                    entry,
                    expected_tail: None,
                })?,
            )
            .await
    }
}

fn assistant(text: impl Into<String>) -> SessionLogEntry {
    SessionLogEntry::Message {
        id: None,
        role: harnx_core::message::MessageRole::Assistant,
        content: harnx_core::message::MessageContent::Text(text.into()),
        timestamp: None,
        fence_token: Some(1),
    }
}

fn deferred_projection(
    log: NatsSessionLog,
    fence: GenerationFence,
    receipt: CommitReceipt,
    release: Arc<Barrier>,
) -> tokio::task::JoinHandle<Result<Option<u64>>> {
    tokio::spawn(async move {
        release.wait().await;
        log.project_through(&fence, &receipt).await
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn committed_append_crash_retry_is_ordered_before_cancel_and_g2() -> Result<()> {
    let f = Fixture::new().await?;
    let entry = assistant("x".repeat(192 * 1024));
    let receipt = f.transcript(entry.clone()).await?;
    // Crash after external append, before cursor ack. Retry must read durable ID.
    f.log
        .project_entry(&projection_id(&receipt), &entry, None)
        .await?;
    let release = Arc::new(Barrier::new(2));
    let retry = deferred_projection(f.log.clone(), f.fence.clone(), receipt, release.clone());
    f.stop().await?;
    f.log.append_cancellation(&f.fence, (1, 1)).await?;
    let newer = f.replace().await?;
    f.log.append_output(&newer, &assistant("g2"), None).await?;
    f.fence
        .store
        .checkpoint_gate(f.fence.context.gate_root())
        .await?;
    release.wait().await;
    assert_eq!(retry.await??, Some(1));
    let entries = f.log.load_events_latest_async().await?;
    assert_eq!(entries.len(), 3);
    assert!(matches!(entries[1].1, SessionLogEntry::Cancel { .. }));
    assert!(
        matches!(&entries[2].1, SessionLogEntry::Message { content, .. } if content.to_text() == "g2")
    );
    assert!(f
        .log
        .append_output(&f.fence, &entry, None)
        .await
        .unwrap_err()
        .is::<harnx_execution_control::Interrupted>());
    Ok(())
}

fn tool_round() -> [SessionLogEntry; 3] {
    let call = harnx_core::tool::ToolCall::new(
        "helper_session_prompt".into(),
        serde_json::json!({"message": "work"}),
        Some("call".into()),
        None,
    );
    [
        SessionLogEntry::ToolCalls {
            text: "delegate".into(),
            thought: None,
            calls: vec![call],
            timestamp: None,
            fence_token: Some(1),
        },
        SessionLogEntry::SubAgentStarted {
            agent: "helper".into(),
            session_id: "child".into(),
            invocation_id: Some("invocation".into()),
            tool_call_id: Some("call".into()),
            started_at: None,
        },
        SessionLogEntry::ToolResults {
            results: vec![harnx_core::session::ToolOutput {
                id: Some("call".into()),
                name: "helper_session_prompt".into(),
                output: serde_json::json!("done"),
                markdown: None,
                content: vec![],
                switch_agent: None,
            }],
            timestamp: None,
        },
    ]
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_projectors_preserve_tool_and_subagent_order() -> Result<()> {
    let f = Fixture::new().await?;
    let mut receipts = Vec::new();
    for entry in tool_round() {
        receipts.push(f.transcript(entry).await?);
    }
    let ready = Arc::new(Barrier::new(3));
    let tasks: Vec<_> = receipts
        .into_iter()
        .skip(1)
        .rev()
        .map(|receipt| deferred_projection(f.log.clone(), f.fence.clone(), receipt, ready.clone()))
        .collect();
    ready.wait().await;
    for task in tasks {
        task.await??;
    }
    let entries = f.log.load_events_latest_async().await?;
    assert_eq!(entries.len(), 3);
    assert!(matches!(entries[0].1, SessionLogEntry::ToolCalls { .. }));
    assert!(matches!(
        entries[1].1,
        SessionLogEntry::SubAgentStarted { .. }
    ));
    assert!(matches!(entries[2].1, SessionLogEntry::ToolResults { .. }));
    // Reconstruction queues the sub-agent message until tool results are paired.
    crate::nats_session_log::load_session_from_entries(&entries, "session")?;
    Ok(())
}

async fn title(fence: &GenerationFence, text: &str) -> Result<CommitReceipt> {
    fence
        .output(
            OutputKind::SessionMetadata,
            serde_json::to_value(MetadataOutput::Title {
                title: text.into(),
                manual: false,
                tokens: 1,
            })?,
        )
        .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delayed_metadata_projection_cannot_overwrite_g2_or_skip_prior_updates() -> Result<()> {
    let f = Fixture::new().await?;
    let metadata = SessionMetadataStore::ensure(&f.js, 1).await?;
    metadata
        .create(&SessionMetadata::new(
            "session",
            SessionInitializer::inline("", Default::default(), Default::default()),
        ))
        .await?;
    let old = f
        .fence
        .output(
            OutputKind::SessionMetadata,
            serde_json::to_value(MetadataOutput::Override(
                SessionOverrideUpdate::Temperature(Some(0.2)),
            ))?,
        )
        .await?;
    f.stop().await?;
    let newer = f.replace().await?;
    let release = Arc::new(Barrier::new(2));
    let late = deferred_projection(f.log.clone(), f.fence.clone(), old, release.clone());
    let receipt = title(&newer, "g2 title").await?;
    f.log.project_through(&newer, &receipt).await?;
    release.wait().await;
    late.await??;
    let current = metadata.get(&f.log.session_id).await?.unwrap().metadata;
    assert_eq!(current.overrides.temperature, Some(0.2));
    assert_eq!(current.title.value.as_deref(), Some("g2 title"));
    assert!(title(&f.fence, "late g1 title")
        .await
        .unwrap_err()
        .is::<harnx_execution_control::Interrupted>());
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn committed_map_projection_compares_content_not_yaml_key_order() -> Result<()> {
    let f = Fixture::new().await?;
    let entry = SessionLogEntry::DataUrls {
        urls: std::collections::HashMap::from([
            ("first".into(), "one".into()),
            ("second".into(), "two".into()),
        ]),
    };
    let receipt = f.transcript(entry.clone()).await?;
    let id = projection_id(&receipt);
    // A previous projector can serialize a HashMap in another key order.
    f.log.ensure_stream().await?;
    f.js.send_publish(
        f.log.subject.clone(),
        PublishMessage::build()
            .message_id(id.clone())
            .payload(bytes::Bytes::from_static(
                b"type: data_urls\nurls:\n  second: two\n  first: one\n",
            )),
    )
    .await?
    .await?;
    assert_eq!(f.log.project_entry(&id, &entry, None).await?, Some(1));
    let different = SessionLogEntry::DataUrls {
        urls: std::collections::HashMap::new(),
    };
    assert!(f.log.project_entry(&id, &different, None).await.is_err());
    Ok(())
}

#[tokio::test]
async fn paused_cancel_projection_cannot_cover_a_later_generation_prompt() -> Result<()> {
    let f = Fixture::new().await?;
    f.stop().await?;
    let receipt = f
        .fence
        .store
        .commit_if_admissible(
            &f.fence.context,
            CommitAction {
                id: "cancel-at-empty-tail".into(),
                kind: GateAction::RecordCancellation {
                    through_seq: 0,
                    fence_token: 1,
                },
            },
        )
        .await?;
    let release = Arc::new(Barrier::new(2));
    let projector = deferred_projection(f.log.clone(), f.fence.clone(), receipt, release.clone());
    let g2 = f.replace().await?;
    // User entries are client-authored, not worker CommitOutput. The stream CAS
    // must protect this gap before G2's first worker output reaches the gate.
    let prompt_seq = f
        .log
        .append_event_async(&SessionLogEntry::Message {
            id: Some("new-prompt".into()),
            role: harnx_core::message::MessageRole::User,
            content: harnx_core::message::MessageContent::Text("new turn".into()),
            timestamp: None,
            fence_token: None,
        })
        .await?;
    release.wait().await;
    projector.await??;
    let entries = f.log.load_events_latest_async().await?;
    assert_eq!(entries.len(), 1);
    assert_eq!(
        crate::nats_session::requested_seq_status(&entries, prompt_seq)?,
        crate::nats_session::RequestedSeqStatus::Pending
    );
    g2.check("g2-still-active").await?;
    Ok(())
}

#[tokio::test]
async fn concurrent_cancel_projection_loser_reports_tail_conflict_not_missing_evidence(
) -> Result<()> {
    let f = Fixture::new().await?;
    f.log
        .append_output(&f.fence, &assistant("before stop"), None)
        .await?;
    f.stop().await?;
    // Worker and recovery captured tail 1 with different lease audit revisions.
    assert_eq!(f.log.append_cancellation(&f.fence, (1, 2)).await?, Some(2));
    assert_eq!(f.log.append_cancellation(&f.fence, (1, 1)).await?, None);
    let entries = f.log.load_events_latest_async().await?;
    assert_eq!(entries.len(), 2);
    assert!(matches!(
        entries.last(),
        Some((_, SessionLogEntry::Cancel { .. }))
    ));
    Ok(())
}
