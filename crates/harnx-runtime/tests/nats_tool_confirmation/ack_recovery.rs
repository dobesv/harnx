//! Exercise durable acknowledgement recovery without relying on a slow CI runner.
use super::*;
use futures_util::StreamExt;
use harnx_runtime::nats_worker::{
    control_subject, ControlCommand, FencedSessionLogSink, NatsSessionLogBackend,
};
use std::time::Duration;

const CALL: &str = "lost-ack-call";

struct Fixture {
    _server: common::NatsServerHandle,
    source: NatsSession,
    backend: NatsSessionLogBackend,
    sink: FencedSessionLogSink,
    lease: Arc<NatsSessionLease>,
    client: async_nats::Client,
}

impl Fixture {
    async fn new() -> Result<Self> {
        let server = common::spawn_nats_server()
            .await?
            .context("nats-server required")?;
        let client = async_nats::connect(server.url()).await?;
        let js = async_nats::jetstream::new(client.clone());
        let source = NatsSession::new(
            NatsSessionConfig {
                cluster: "local".into(),
                initializer: SessionInitializer::inline("", Default::default(), Default::default()),
                session_id: Some("hitl-ack-recovery".into()),
                activation_route: harnx_runtime::SessionActivationRoute::ClusterShared,
            },
            client.clone(),
            js.clone(),
            create_abort_signal(),
        )
        .await?;
        source.enqueue_text("await tool approval").await?;
        let lease = Arc::new(
            NatsSessionLease::acquire(NatsLeaseAcquireParams {
                jetstream: js.clone(),
                session_id: source.storage_key(),
                worker_id: "ack-recovery-worker".into(),
                generation: 1,
                config: Default::default(),
                session_metadata: None,
            })
            .await?
            .context("worker lease")?,
        );
        let fence = generation::generation_fence(
            &js,
            source.storage_key(),
            harnx_execution_control::Owner {
                instance_id: lease.worker_id().into(),
                fence: lease.fence_token(),
            },
        )
        .await?;
        let backend =
            NatsSessionLogBackend::new(js, source.storage_key()).with_execution(Some(fence));
        let sink = FencedSessionLogSink::new(backend.clone(), lease.clone());
        let fixture = Self {
            _server: server,
            source,
            backend,
            sink,
            lease,
            client,
        };
        fixture.append_pending_round().await?;
        Ok(fixture)
    }

    async fn append_pending_round(&self) -> Result<()> {
        self.backend
            .append_event(&SessionLogEntry::ToolCalls {
                text: "approval needed".into(),
                thought: None,
                calls: vec![ToolCall::new(
                    "read".into(),
                    json!({}),
                    Some(CALL.into()),
                    None,
                )],
                timestamp: None,
                fence_token: Some(self.lease.fence_token()),
            })
            .await?;
        let tail = self
            .backend
            .append_event(&SessionLogEntry::HitlApprovalRequested {
                tool_call_id: CALL.into(),
                summary: "approve read".into(),
                fence_token: self.lease.fence_token(),
            })
            .await?;
        assert!(tail > 0);
        Ok(())
    }

    async fn commit_decision(&self, approved: bool) -> Result<()> {
        let entries = self.backend.load_events_latest_async().await?;
        let tail = entries.last().context("pending log")?.0;
        assert!(self
            .sink
            .append_hitl_event_cas(
                &SessionLogEntry::HitlApprovalDecision {
                    tool_call_id: CALL.into(),
                    approved,
                    note: None,
                    fence_token: self.lease.fence_token(),
                },
                tail
            )
            .await?
            .is_some());
        Ok(())
    }

    async fn complete_round(&self) -> Result<()> {
        self.backend
            .append_event(&SessionLogEntry::ToolResults {
                results: vec![],
                timestamp: None,
            })
            .await?;
        Ok(())
    }

    async fn assert_single_decision(&self) -> Result<()> {
        let entries = self.backend.load_events_latest_async().await?;
        assert_eq!(
            entries
                .iter()
                .filter(|(_, entry)| matches!(entry, SessionLogEntry::HitlApprovalDecision { .. }))
                .count(),
            1
        );
        Ok(())
    }
}

async fn recover_lost_ack(approved: bool, reuse_id: bool) -> Result<()> {
    let fixture = Fixture::new().await?;
    let mut commands = fixture
        .client
        .subscribe(control_subject(fixture.source.storage_key()))
        .await?;
    fixture.client.flush().await?;
    let apply = async {
        let message = commands.next().await.context("worker control request")?;
        assert_eq!(
            ControlCommand::from_bytes(&message.payload)?,
            ControlCommand::HitlApprovalDecision {
                tool_call_id: CALL.into(),
                approved,
                note: None,
            }
        );
        assert!(
            message.reply.is_some(),
            "request/reply attempt reached worker"
        );
        fixture.commit_decision(approved).await?;
        fixture.complete_round().await?;
        if reuse_id {
            fixture.append_pending_round().await?;
        }
        // Deliberately drop the ACK. The frontend must prove success from the log.
        Ok::<_, anyhow::Error>(())
    };
    let decide = fixture.source.decide_hitl_approval(CALL, approved, None);
    let (_, applied) = tokio::time::timeout(Duration::from_secs(10), async {
        tokio::try_join!(apply, decide)
    })
    .await??;
    assert!(
        applied,
        "committed decision must survive acknowledgement loss"
    );
    fixture.assert_single_decision().await?;
    fixture.lease.release().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nats_tool_confirmation_recovers_approval_after_lost_ack() -> Result<()> {
    recover_lost_ack(true, false).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nats_tool_confirmation_recovers_denial_after_lost_ack() -> Result<()> {
    recover_lost_ack(false, false).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nats_tool_confirmation_lost_ack_retains_original_request_when_id_is_reused() -> Result<()>
{
    recover_lost_ack(true, true).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nats_tool_confirmation_repeated_decision_is_idempotent_after_completion() -> Result<()> {
    for approved in [false, true] {
        let fixture = Fixture::new().await?;
        fixture.commit_decision(approved).await?;
        fixture.complete_round().await?;
        fixture.lease.release().await?;
        assert!(
            fixture
                .source
                .decide_hitl_approval(CALL, approved, Some("retry note".into()))
                .await?
        );
        assert!(
            !fixture
                .source
                .decide_hitl_approval(CALL, !approved, None)
                .await?
        );
        fixture.assert_single_decision().await?;
    }
    Ok(())
}
