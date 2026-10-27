//! Exercise durable acknowledgement recovery without relying on a slow CI runner.
use super::*;
use futures_util::StreamExt;
use harnx_runtime::nats_worker::{
    control_subject, notify_subject, ControlCommand, FencedSessionLogSink, NatsSessionLogBackend,
    SessionActivate,
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

async fn connect_fixture_services() -> Result<(
    common::NatsServerHandle,
    async_nats::Client,
    async_nats::jetstream::Context,
    NatsSession,
)> {
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
    Ok((server, client, js, source))
}

async fn acquire_fixture_lease(
    js: &async_nats::jetstream::Context,
    storage_key: &str,
) -> Result<(
    Arc<NatsSessionLease>,
    NatsSessionLogBackend,
    FencedSessionLogSink,
)> {
    let lease = Arc::new(
        NatsSessionLease::acquire(NatsLeaseAcquireParams {
            jetstream: js.clone(),
            session_id: storage_key,
            worker_id: "ack-recovery-worker".into(),
            generation: 1,
            config: Default::default(),
            session_metadata: None,
        })
        .await?
        .context("worker lease")?,
    );
    let backend = generation::fenced_backend(js, storage_key).await?;
    let sink = FencedSessionLogSink::new(backend.clone(), lease.clone());
    Ok((lease, backend, sink))
}

impl Fixture {
    async fn new() -> Result<Self> {
        let (_server, client, js, source) = connect_fixture_services().await?;
        let (lease, backend, sink) = acquire_fixture_lease(&js, source.storage_key()).await?;
        let fixture = Self {
            _server,
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

async fn run_lost_ack_worker(
    fixture: &Fixture,
    commands: &mut async_nats::Subscriber,
    reference: &ApprovalRef,
    plan: (bool, bool),
) -> Result<()> {
    let (approved, reuse_id) = plan;
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
    Ok(())
}

async fn run_lost_ack_decision(
    source: &NatsSession,
    reference: &ApprovalRef,
    approved: bool,
    exact: bool,
) -> Result<bool> {
    if exact {
        assert_eq!(
            source
                .decide_hitl_approval_for(reference, approved, None)
                .await?,
            HitlDecisionOutcome::Applied
        );
        Ok(true)
    } else {
        source.decide_hitl_approval(CALL, approved, None).await
    }
}

async fn execute_lost_ack_exchange(
    fixture: &Fixture,
    commands: &mut async_nats::Subscriber,
    reference: &ApprovalRef,
    plan: (bool, bool, bool),
) -> Result<bool> {
    let (approved, reuse_id, exact) = plan;
    let apply = run_lost_ack_worker(fixture, commands, reference, (approved, reuse_id));
    let decide = run_lost_ack_decision(&fixture.source, reference, approved, exact);
    let (_, applied) = tokio::time::timeout(Duration::from_secs(10), async {
        tokio::try_join!(apply, decide)
    })
    .await??;
    Ok(applied)
}

async fn recover_lost_ack(approved: bool, reuse_id: bool, exact: bool) -> Result<()> {
    let fixture = Fixture::new().await?;
    let reference = fixture.reference().await?;
    let mut commands = fixture
        .client
        .subscribe(control_subject(fixture.source.storage_key()))
        .await?;
    fixture.client.flush().await?;
    let applied = execute_lost_ack_exchange(
        &fixture,
        &mut commands,
        &reference,
        (approved, reuse_id, exact),
    )
    .await?;
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

async fn assert_idempotent_decision_after_completion(approved: bool) -> Result<()> {
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
    fixture.assert_single_decision().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nats_tool_confirmation_repeated_decision_is_idempotent_after_completion() -> Result<()> {
    for approved in [false, true] {
        assert_idempotent_decision_after_completion(approved).await?;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nats_tool_confirmation_exact_recovers_lost_ack_before_next_request() -> Result<()> {
    recover_lost_ack(true, true, true).await
}

async fn setup_duplicate_fixture(
    approved: bool,
) -> Result<(
    Fixture,
    ApprovalRef,
    async_nats::Subscriber,
    Vec<(u64, SessionLogEntry)>,
)> {
    let fixture = Fixture::new().await?;
    let reference = fixture.reference().await?;
    fixture
        .commit_decision_with_note(approved, Some("original note".into()))
        .await?;
    fixture.complete_round().await?;
    let activations = fixture.client.subscribe(notify_subject("local")).await?;
    fixture.client.flush().await?;
    let before = fixture.backend.load_events_latest_async().await?;
    Ok((fixture, reference, activations, before))
}

async fn assert_already_applied_activation(
    fixture: &Fixture,
    reference: &ApprovalRef,
    activations: &mut async_nats::Subscriber,
    plan: (bool, u64),
) -> Result<()> {
    let (approved, root_seq) = plan;
    assert_eq!(
        fixture
            .source
            .decide_hitl_approval_for(reference, approved, Some("retry note".into()))
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
    assert_eq!(activation.requested_seq, Some(root_seq));
    Ok(())
}

async fn assert_conflicting_and_legacy_activation(
    fixture: &Fixture,
    reference: &ApprovalRef,
    activations: &mut async_nats::Subscriber,
    plan: (bool, u64),
) -> Result<()> {
    let (approved, root_seq) = plan;
    assert_eq!(
        fixture
            .source
            .decide_hitl_approval_for(reference, !approved, None)
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
    assert_eq!(activation.requested_seq, Some(root_seq));
    Ok(())
}

async fn check_exact_duplicate_activation(approved: bool) -> Result<()> {
    let (fixture, reference, mut activations, before) = setup_duplicate_fixture(approved).await?;
    let root_seq = before[0].0;
    assert_already_applied_activation(&fixture, &reference, &mut activations, (approved, root_seq))
        .await?;
    assert_eq!(
        fixture.backend.load_events_latest_async().await?,
        before,
        "reactivation doesn't append a prompt or decision"
    );
    assert_conflicting_and_legacy_activation(
        &fixture,
        &reference,
        &mut activations,
        (approved, root_seq),
    )
    .await?;
    fixture.lease.release().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nats_tool_confirmation_exact_duplicate_activates_original_invocation() -> Result<()> {
    for approved in [true, false] {
        check_exact_duplicate_activation(approved).await?;
    }
    Ok(())
}
async fn assert_mutated_refs_stale(source: &NatsSession, reference: &ApprovalRef) -> Result<()> {
    for stale_ref in [
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
            source
                .decide_hitl_approval_for(&stale_ref, true, None)
                .await?,
            HitlDecisionOutcome::Stale
        );
    }
    Ok(())
}

async fn assert_post_advance_ref_stale(
    fixture: &Fixture,
    reference: &ApprovalRef,
    commands: &mut async_nats::Subscriber,
) -> Result<()> {
    fixture.commit_decision(true).await?;
    fixture.complete_round().await?;
    fixture.append_pending_round().await?;
    let before = fixture.backend.load_events_latest_async().await?;
    assert_eq!(
        fixture
            .source
            .decide_hitl_approval_for(reference, true, None)
            .await?,
        HitlDecisionOutcome::Stale
    );
    assert_eq!(
        fixture
            .source
            .decide_hitl_approval_for(reference, false, None)
            .await?,
        HitlDecisionOutcome::Stale
    );
    assert_eq!(fixture.backend.load_events_latest_async().await?, before);
    assert!(
        tokio::time::timeout(Duration::from_millis(50), commands.next())
            .await
            .is_err()
    );
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
    assert_mutated_refs_stale(&fixture.source, &reference).await?;
    assert_post_advance_ref_stale(&fixture, &reference, &mut commands).await?;
    fixture.lease.release().await?;
    Ok(())
}
async fn setup_terminal_fixture() -> Result<(Fixture, ApprovalRef, async_nats::Subscriber)> {
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
    let activations = fixture.client.subscribe(notify_subject("local")).await?;
    fixture.client.flush().await?;
    Ok((fixture, reference, activations))
}

async fn assert_terminal_fixture_decisions(
    source: &NatsSession,
    reference: &ApprovalRef,
    activations: &mut async_nats::Subscriber,
) -> Result<()> {
    for approved in [true, false] {
        assert_eq!(
            source
                .decide_hitl_approval_for(reference, approved, None)
                .await?,
            HitlDecisionOutcome::Stale
        );
    }
    assert!(
        source.decide_hitl_approval(CALL, true, None).await?,
        "legacy completed-decision retry remains true"
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(50), activations.next())
            .await
            .is_err()
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nats_tool_confirmation_exact_terminal_is_stale_without_activation() -> Result<()> {
    let (fixture, reference, mut activations) = setup_terminal_fixture().await?;
    assert_terminal_fixture_decisions(&fixture.source, &reference, &mut activations).await?;
    fixture.lease.release().await?;
    Ok(())
}
async fn simulate_replaced_request_worker(
    fixture: &Fixture,
    commands: &mut async_nats::Subscriber,
    expected_request_seq: u64,
) -> Result<()> {
    let message = commands
        .next()
        .await
        .context("original exact decision command")?;
    assert!(
        matches!(ControlCommand::from_bytes(&message.payload)?, ControlCommand::HitlApprovalDecision { request_seq: Some(seq), .. } if seq == expected_request_seq)
    );
    fixture
        .backend
        .append_event(&SessionLogEntry::Rewind { after_seq: 1 })
        .await?;
    fixture.append_pending_round().await?;
    Ok(())
}

async fn assert_no_decision_and_no_second_command(
    backend: &NatsSessionLogBackend,
    commands: &mut async_nats::Subscriber,
) -> Result<()> {
    assert!(backend
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
    Ok(())
}

async fn setup_fixture_commands() -> Result<(Fixture, ApprovalRef, async_nats::Subscriber)> {
    let fixture = Fixture::new().await?;
    let reference = fixture.reference().await?;
    let commands = fixture
        .client
        .subscribe(control_subject(fixture.source.storage_key()))
        .await?;
    fixture.client.flush().await?;
    Ok((fixture, reference, commands))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nats_tool_confirmation_exact_retry_does_not_retarget_after_request_replacement(
) -> Result<()> {
    let (fixture, reference, mut commands) = setup_fixture_commands().await?;
    let replace = simulate_replaced_request_worker(&fixture, &mut commands, reference.request_seq);
    let decide = fixture
        .source
        .decide_hitl_approval_for(&reference, true, None);
    let (_, outcome) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::try_join!(replace, decide)
    })
    .await??;
    assert_eq!(outcome, HitlDecisionOutcome::Stale);
    assert_no_decision_and_no_second_command(&fixture.backend, &mut commands).await?;
    fixture.lease.release().await?;
    Ok(())
}
