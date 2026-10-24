//! Exercise durable acknowledgement recovery without relying on a slow CI runner.
use super::*;
use futures_util::StreamExt;
use harnx_runtime::nats_worker::{
    control_subject, ControlCommand, FencedSessionLogSink, NatsSessionLogBackend,
};
use harnx_runtime::{ApprovalRef, HitlDecisionOutcome};
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
        source
            .clone()
            .with_external_admission()
            .enqueue_text("await tool approval")
            .await?;
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
        let backend = generation::fenced_backend(&js, source.storage_key()).await?;
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

    async fn reference(&self) -> Result<ApprovalRef> {
        let entries = self.backend.load_events_latest_async().await?;
        let invocation_id = entries
            .iter()
            .find_map(|(_, entry)| match entry {
                SessionLogEntry::Message {
                    id: Some(id), role, ..
                } if role.is_user() => Some(id.clone()),
                _ => None,
            })
            .context("root invocation")?;
        let tool_round_seq = entries
            .iter()
            .rev()
            .find_map(|(seq, entry)| {
                matches!(entry, SessionLogEntry::ToolCalls { .. }).then_some(*seq)
            })
            .context("round")?;
        let request_seq = entries
            .iter()
            .rev()
            .find_map(|(seq, entry)| {
                matches!(entry, SessionLogEntry::HitlApprovalRequested { .. }).then_some(*seq)
            })
            .context("request")?;
        Ok(ApprovalRef {
            invocation_id,
            tool_round_seq,
            request_seq,
            tool_call_id: CALL.into(),
        })
    }

    async fn commit_decision(&self, approved: bool) -> Result<()> {
        self.commit_decision_with_note(approved, None).await
    }

    async fn commit_decision_with_note(&self, approved: bool, note: Option<String>) -> Result<()> {
        let entries = self.backend.load_events_latest_async().await?;
        let tail = entries.last().context("pending log")?.0;
        assert!(self
            .sink
            .append_hitl_event_cas(
                &SessionLogEntry::HitlApprovalDecision {
                    tool_call_id: CALL.into(),
                    approved,
                    note,
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

async fn recover_lost_ack(approved: bool, reuse_id: bool, exact: bool) -> Result<()> {
    let fixture = Fixture::new().await?;
    let reference = fixture.reference().await?;
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
                request_seq: Some(reference.request_seq),
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
    let decide = async {
        if exact {
            assert_eq!(
                fixture
                    .source
                    .decide_hitl_approval_for(&reference, approved, None)
                    .await?,
                HitlDecisionOutcome::Applied
            );
            Ok(true)
        } else {
            fixture
                .source
                .decide_hitl_approval(CALL, approved, None)
                .await
        }
    };
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
    recover_lost_ack(true, false, false).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nats_tool_confirmation_recovers_denial_after_lost_ack() -> Result<()> {
    recover_lost_ack(false, false, false).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nats_tool_confirmation_lost_ack_retains_original_request_when_id_is_reused() -> Result<()>
{
    recover_lost_ack(true, true, false).await
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nats_tool_confirmation_exact_recovers_lost_ack_before_next_request() -> Result<()> {
    recover_lost_ack(true, true, true).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nats_tool_confirmation_exact_duplicate_activates_original_invocation() -> Result<()> {
    use harnx_runtime::nats_worker::{notify_subject, SessionActivate};
    for approved in [true, false] {
        let fixture = Fixture::new().await?;
        let reference = fixture.reference().await?;
        fixture
            .commit_decision_with_note(approved, Some("original note".into()))
            .await?;
        fixture.complete_round().await?;
        let mut activations = fixture.client.subscribe(notify_subject("local")).await?;
        fixture.client.flush().await?;
        let before = fixture.backend.load_events_latest_async().await?;
        assert_eq!(
            fixture
                .source
                .decide_hitl_approval_for(&reference, approved, Some("retry note".into()))
                .await?,
            HitlDecisionOutcome::AlreadyApplied {
                approved,
                note: Some("original note".into())
            },
        );
        let activation = tokio::time::timeout(Duration::from_secs(2), activations.next())
            .await?
            .context("reactivation")?;
        let activation: SessionActivate = serde_json::from_slice(&activation.payload)?;
        assert_eq!(activation.session_id, fixture.source.storage_key());
        assert_eq!(activation.requested_seq, Some(before[0].0));
        assert_eq!(
            fixture.backend.load_events_latest_async().await?,
            before,
            "reactivation doesn't append a prompt or decision"
        );

        assert_eq!(
            fixture
                .source
                .decide_hitl_approval_for(&reference, !approved, None)
                .await?,
            HitlDecisionOutcome::Conflicting
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(50), activations.next())
                .await
                .is_err(),
            "conflict observed before delivery must not reactivate"
        );
        fixture.assert_single_decision().await?;
        assert!(
            fixture
                .source
                .decide_hitl_approval(CALL, approved, None)
                .await?
        );
        let activation = tokio::time::timeout(Duration::from_secs(2), activations.next())
            .await?
            .context("legacy retry reactivation")?;
        let activation: SessionActivate = serde_json::from_slice(&activation.payload)?;
        assert_eq!(activation.requested_seq, Some(before[0].0));
        fixture.lease.release().await?;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nats_tool_confirmation_exact_stale_refs_never_send_a_decision() -> Result<()> {
    let fixture = Fixture::new().await?;
    let reference = fixture.reference().await?;
    let mut commands = fixture
        .client
        .subscribe(control_subject(fixture.source.storage_key()))
        .await?;
    fixture.client.flush().await?;
    for reference in [
        ApprovalRef {
            invocation_id: "unrelated-invocation".into(),
            ..reference.clone()
        },
        ApprovalRef {
            request_seq: reference.request_seq + 1,
            ..reference.clone()
        },
        ApprovalRef {
            tool_round_seq: reference.tool_round_seq + 1,
            ..reference.clone()
        },
        ApprovalRef {
            tool_call_id: "different-call".into(),
            ..reference.clone()
        },
    ] {
        assert_eq!(
            fixture
                .source
                .decide_hitl_approval_for(&reference, true, None)
                .await?,
            HitlDecisionOutcome::Stale
        );
    }
    fixture.commit_decision(true).await?;
    fixture.complete_round().await?;
    fixture.append_pending_round().await?;
    let before = fixture.backend.load_events_latest_async().await?;
    assert_eq!(
        fixture
            .source
            .decide_hitl_approval_for(&reference, true, None)
            .await?,
        HitlDecisionOutcome::Stale
    );
    assert_eq!(
        fixture
            .source
            .decide_hitl_approval_for(&reference, false, None)
            .await?,
        HitlDecisionOutcome::Stale
    );
    assert_eq!(fixture.backend.load_events_latest_async().await?, before);
    assert!(
        tokio::time::timeout(Duration::from_millis(50), commands.next())
            .await
            .is_err()
    );
    fixture.lease.release().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nats_tool_confirmation_exact_terminal_is_stale_without_activation() -> Result<()> {
    use harnx_runtime::nats_worker::notify_subject;
    let fixture = Fixture::new().await?;
    let reference = fixture.reference().await?;
    fixture.commit_decision(true).await?;
    let root = fixture.backend.load_events_latest_async().await?[0].0;
    fixture
        .backend
        .append_event(&SessionLogEntry::TurnEnd {
            through_seq: root,
            fence_token: 1,
            timestamp: None,
            usage: None,
        })
        .await?;
    let mut activations = fixture.client.subscribe(notify_subject("local")).await?;
    fixture.client.flush().await?;
    for approved in [true, false] {
        assert_eq!(
            fixture
                .source
                .decide_hitl_approval_for(&reference, approved, None)
                .await?,
            HitlDecisionOutcome::Stale
        );
    }
    assert!(
        fixture
            .source
            .decide_hitl_approval(CALL, true, None)
            .await?,
        "legacy completed-decision retry remains true"
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(50), activations.next())
            .await
            .is_err()
    );
    fixture.lease.release().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nats_tool_confirmation_exact_retry_does_not_retarget_after_request_replacement(
) -> Result<()> {
    let fixture = Fixture::new().await?;
    let reference = fixture.reference().await?;
    let mut commands = fixture
        .client
        .subscribe(control_subject(fixture.source.storage_key()))
        .await?;
    fixture.client.flush().await?;
    let replace = async {
        let message = commands
            .next()
            .await
            .context("original exact decision command")?;
        assert!(
            matches!(ControlCommand::from_bytes(&message.payload)?, ControlCommand::HitlApprovalDecision { request_seq: Some(seq), .. } if seq == reference.request_seq)
        );
        fixture
            .backend
            .append_event(&SessionLogEntry::Rewind { after_seq: 1 })
            .await?;
        fixture.append_pending_round().await?;
        // Simulate the worker rejecting a request whose generation was replaced
        // between the frontend snapshot and the fenced worker append. No ACK.
        Ok::<_, anyhow::Error>(())
    };
    let decide = fixture
        .source
        .decide_hitl_approval_for(&reference, true, None);
    let (_, outcome) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::try_join!(replace, decide)
    })
    .await??;
    assert_eq!(outcome, HitlDecisionOutcome::Stale);
    assert!(fixture
        .backend
        .load_events_latest_async()
        .await?
        .iter()
        .all(|(_, entry)| !matches!(entry, SessionLogEntry::HitlApprovalDecision { .. })));
    assert!(
        tokio::time::timeout(Duration::from_millis(50), commands.next())
            .await
            .is_err(),
        "frontend must not send a second command against the replacement"
    );
    fixture.lease.release().await?;
    Ok(())
}
