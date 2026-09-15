use super::*;
use crate::{execution_fence::GenerationFence, nats_test_common as common};
use anyhow::Context;
use futures_util::StreamExt;
use harnx_core::event::{ModelEvent, NoticeEvent, ToolEvent, TurnEvent};
use harnx_execution_control::{CancelReceipt, ExecutionStore, Owner};

struct Fixture {
    _server: common::NatsServerHandle,
    js: jetstream::Context,
    fence: GenerationFence,
}

impl Fixture {
    async fn new() -> Result<Self> {
        let server = common::spawn_nats_server()
            .await?
            .context("nats-server required")?;
        let js = jetstream::new(async_nats::connect(server.url()).await?);
        let store = ExecutionStore::ensure(&js, 1).await?;
        let fence = Self::generation(&store, "g1").await?;
        Ok(Self {
            _server: server,
            js,
            fence,
        })
    }

    async fn generation(store: &ExecutionStore, id: &str) -> Result<GenerationFence> {
        let op = store.session("session", None, Some(id)).await?;
        store
            .claim(&op.reference, Owner::invocation("worker"))
            .await?;
        let context = store.activate_gate(&op.reference).await?;
        Ok(GenerationFence::new(store.clone(), context))
    }

    async fn replace(&self) -> Result<GenerationFence> {
        use harnx_execution_control::{CommitAction, GateAction, OperationRef};
        let ctx = &self.fence.context;
        let next = OperationRef::new("session", "g2");
        // Exercise the Stage 2 gate directly. Normal prompt overlap stays disabled.
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
        let context = self
            .fence
            .store
            .gate_context(ctx.gate_root(), &next)
            .await?;
        Ok(GenerationFence::new(self.fence.store.clone(), context))
    }

    async fn append_committed_history(&self) -> Result<()> {
        let log = crate::nats_session_log::NatsSessionLog::new(self.js.clone(), "session");
        log.append_output(
            &self.fence,
            &harnx_core::session::SessionLogEntry::Message {
                id: Some("committed".into()),
                role: harnx_core::message::MessageRole::Assistant,
                content: harnx_core::message::MessageContent::Text("committed history".into()),
                timestamp: None,
                fence_token: None,
            },
            None,
        )
        .await?;
        Ok(())
    }

    async fn stop(&self) -> Result<CancelReceipt> {
        let operation = self
            .fence
            .store
            .cancel_operation(self.fence.context.generation(), None, false)
            .await?;
        Ok(CancelReceipt::from_operation(&operation, false))
    }
}

fn old_events() -> Vec<AgentEvent> {
    vec![
        AgentEvent::Model(ModelEvent::Final {
            output: "late final".into(),
            usage: Default::default(),
        }),
        AgentEvent::Tool(ToolEvent::Completed {
            id: "tool".into(),
            output: "late result".into(),
            markdown: None,
        }),
        AgentEvent::Tool(ToolEvent::Progress {
            id: "tool".into(),
            text: "late progress".into(),
        }),
        AgentEvent::Turn(TurnEvent::SubAgentProgress(
            harnx_core::event::SubAgentProgress {
                invocation_id: "old-child-invocation".into(),
                agent: "child".into(),
                session_id: "child-session".into(),
                status: harnx_core::event::SubAgentProgressStatus::Running,
                elapsed_ms: 10,
                usage: Default::default(),
                tool_call_count: 5,
                title: Some("Old child title".into()),
            },
        )),
        AgentEvent::Model(ModelEvent::Error("late error".into())),
        AgentEvent::Turn(TurnEvent::Ended {
            outcome: Default::default(),
        }),
    ]
}

#[test]
fn envelope_is_additive_and_high_sequence_never_overrides_generation() {
    let legacy = AdvisoryEnvelope::new(10, old_events().remove(0));
    let bytes = legacy.to_bytes().unwrap();
    assert!(!String::from_utf8_lossy(&bytes).contains("execution_id"));
    assert!(AdvisoryEnvelope::from_bytes(&bytes)
        .unwrap()
        .execution_id
        .is_none());
    let state = LiveEventState::default();
    state.select(Some("g2".into()));
    assert!(
        !state.should_render(&legacy, 0),
        "unknown live origin fails closed"
    );
    for event in old_events() {
        let envelope = AdvisoryEnvelope::new(u64::MAX, event).with_execution_id("g1");
        let back = AdvisoryEnvelope::from_bytes(&envelope.to_bytes().unwrap()).unwrap();
        assert_eq!(back.execution_id.as_deref(), Some("g1"));
        assert!(!state.should_render(&back, 10));
        let current = back.with_execution_id("g2");
        assert!(state.should_render(&current, 10));
    }
    let same_seq = AdvisoryEnvelope::new(10, old_events().remove(0)).with_execution_id("g2");
    assert!(state.should_render(&same_seq, 10));
    assert!(!state.should_render(&same_seq, 11));
    state.stop("g2");
    assert!(!state.should_render(&same_seq, 0));
}

#[tokio::test]
async fn queued_publisher_discards_g1_after_g2_and_keeps_creation_identity() -> Result<()> {
    let fixture = Fixture::new().await?;
    let client = fixture.js.client().clone();
    let mut subscriber = client.subscribe(events_subject("session")).await?;
    let old = NatsEventSink::new(client.clone(), fixture.js.clone(), "session")
        .await
        .with_execution(Some(fixture.fence.clone()));
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    old.publisher
        .send(PublisherCommand::Barrier(entered_tx, release_rx))
        .ok()
        .unwrap();
    entered_rx.await?;
    old.note_durable_seq(u64::MAX);
    for event in old_events() {
        old.emit(event);
    }
    fixture.stop().await?;
    let next = fixture.replace().await?;
    let current = old.clone().with_execution(Some(next));
    release_tx.send(()).unwrap();
    current.emit(AgentEvent::Notice(NoticeEvent::Info("g2 marker".into())));
    current.flush().await?;
    let message = subscriber.next().await.context("publisher stopped")?;
    let envelope = AdvisoryEnvelope::from_bytes(&message.payload)?;
    assert_eq!(envelope.execution_id.as_deref(), Some("g2"));
    assert!(
        matches!(envelope.event, AgentEvent::Notice(NoticeEvent::Info(text)) if text == "g2 marker")
    );
    assert!(
        fixture.fence.events_stopped(),
        "drain installs an enqueue fast reject"
    );
    for event in old_events() {
        old.emit(event);
    }
    current.emit(AgentEvent::Notice(NoticeEvent::Info(
        "second marker".into(),
    )));
    current.flush().await?;
    let envelope = AdvisoryEnvelope::from_bytes(&subscriber.next().await.unwrap().payload)?;
    assert_eq!(envelope.execution_id.as_deref(), Some("g2"));
    Ok(())
}

#[tokio::test]
async fn reconnect_loads_stop_before_draining_buffered_advisories_and_keeps_history() -> Result<()>
{
    let fixture = Fixture::new().await?;
    let client = fixture.js.client().clone();
    fixture.append_committed_history().await?;
    // This subscription is the reconnect barrier: publish while it buffers,
    // accept stop, then run the exact attach continuation used in production.
    let subscriber = client.subscribe(events_subject("session")).await?;
    for event in old_events() {
        publish_envelope(
            &client,
            &events_subject("session"),
            AdvisoryEnvelope::new(u64::MAX, event).with_execution_id("g1"),
        )
        .await?;
    }
    client.flush().await?;
    let receipt = fixture.stop().await?;
    assert!(receipt.cancelled);
    let state = LiveEventState::default(); // Simulate losing all frontend memory.
    let mut stream =
        SessionEventStream::finish_attach(fixture.js.clone(), "session", state, subscriber).await?;
    assert_eq!(stream.live_state().active().as_deref(), Some("g1"));
    assert!(
        !stream.live_state().allows(Some("g1")),
        "fence must already be loaded before next/drain"
    );
    assert_eq!(
        stream.history().len(),
        1,
        "stopping live events does not erase history"
    );
    for _ in old_events() {
        let envelope = stream.next().await.unwrap();
        assert!(!stream.should_render(&envelope));
    }
    let next = fixture.replace().await?;
    stream.refresh_generation().await?;
    assert!(stream
        .live_state()
        .allows(Some(&next.context.generation().execution_id)));
    for event in old_events() {
        assert!(
            !stream.should_render(&AdvisoryEnvelope::new(u64::MAX, event).with_execution_id("g1"))
        );
    }
    let new_event = AdvisoryEnvelope::new(u64::MAX, old_events().remove(0)).with_execution_id("g2");
    assert!(
        stream.should_render(&new_event),
        "shared observer can follow g2"
    );
    stream.follow_generation("g1".into());
    stream.refresh_generation().await?;
    assert_eq!(stream.live_state().active().as_deref(), Some("g1"));
    assert!(
        !stream.should_render(&new_event),
        "dedicated g1 follower cannot adopt g2"
    );
    Ok(())
}

#[tokio::test]
async fn durable_turn_end_keeps_its_admitted_generation_after_gate_replacement() -> Result<()> {
    use harnx_core::{
        message::{MessageContent, MessageRole},
        session::SessionLogEntry,
    };
    let fixture = Fixture::new().await?;
    let log = crate::nats_session_log::NatsSessionLog::new(fixture.js.clone(), "session");
    let store = &fixture.fence.store;
    let reference = fixture.fence.context.generation();
    store.reserve_prompt(reference, "original-user").await?;
    let seq = log
        .append_event_async(&SessionLogEntry::Message {
            id: Some("original-user".into()),
            role: MessageRole::User,
            content: MessageContent::Text("original prompt".into()),
            timestamp: None,
            fence_token: None,
        })
        .await?;
    store.commit_prompt(reference, "original-user", seq).await?;
    log.append_output(
        &fixture.fence,
        &SessionLogEntry::TurnEnd {
            through_seq: seq,
            fence_token: 1,
            timestamp: None,
            usage: None,
        },
        None,
    )
    .await?;
    let stream =
        SessionEventStream::attach(fixture.js.clone(), fixture.js.client().clone(), "session")
            .await?;
    fixture.stop().await?;
    fixture.replace().await?;
    stream.refresh_generation().await?;
    assert_eq!(stream.live_state().active().as_deref(), Some("g2"));
    assert_eq!(stream.history_generation().await?.as_deref(), Some("g1"));
    assert!(
        !stream
            .live_state()
            .matches(stream.history_generation().await?.as_deref()),
        "a durable g1 TurnEnd cannot be stamped as g2's idle transition"
    );
    Ok(())
}
