//! Broker-backed checkpoint CAS and projection invariants, without a worker.

mod mismatches;
use super::*;
use a2a_lf::{StreamResponse, TaskState, TaskStatusUpdateEvent};
use harnx_a2a_server::store::{DedupeEntry, MessageIdentity};
use harnx_runtime::{ApprovalRef, HitlDecisionOutcome};

fn request(seq: u64) -> ApprovalRef {
    ApprovalRef {
        invocation_id: "stable-invocation".into(),
        tool_round_seq: 12,
        request_seq: seq,
        tool_call_id: format!("call-{seq}"),
    }
}
fn receipt(seq: u64) -> ReplyReceipt {
    ReplyReceipt::new(ReplyReceiptParams {
        message_id: format!("reply-{seq}"),
        fingerprint: "reply-fingerprint".into(),
        request: request(seq),
        approved: true,
        note: None,
    })
}
async fn current(f: &Fixture) -> Result<ContextSnapshot> {
    f.b.read_context(STORAGE).await?.context("context")
}
fn active(c: &ContextSnapshot) -> &ActiveTask {
    c.document.state.active.as_ref().unwrap()
}
async fn allocate(f: &Fixture) -> Result<(NatsSessionLease, ContextSnapshot)> {
    let lease = f.lease("hitl-owner").await?;
    let claim = f.claim(&f.a, &lease, "claim").await?;
    let allocated = f.active(&f.a, &claim).await?;
    let write =
        f.a.prepare_context_update(STORAGE, &allocated.version()?, "admitted", |state| {
            let a = state.active.as_mut().unwrap();
            a.admission.phase = AdmissionPhase::Admitted;
            a.admission.prompt_sequence = Some(11);
            a.snapshot.user_msg_id = a.admission.prompt_id.clone();
            a.snapshot.user_msg_seq = 11;
            a.snapshot.execution_id = a.admission.invocation_id.clone();
        })
        .await?;
    Ok((lease, f.a.commit_context(&write).await?))
}
async fn waiting(f: &Fixture, c: &ContextSnapshot, seq: u64) -> Result<ContextSnapshot> {
    f.a.set_hitl_checkpoint_waiting(
        STORAGE,
        &c.version()?,
        HitlWaitingParams {
            request: request(seq),
            observed_log_seq: seq,
            challenge_message: "Approve tool?",
        },
    )
    .await?;
    current(f).await
}
async fn intent(f: &Fixture, c: &ContextSnapshot, seq: u64) -> Result<ContextSnapshot> {
    assert!(
        f.a.record_hitl_reply_intent(
            STORAGE,
            &c.version()?,
            HitlReplyIntent {
                expected_approval: &request(seq),
                receipt: receipt(seq),
            },
        )
        .await?
    );
    current(f).await
}
async fn mapping(f: &Fixture, seq: u64) -> Result<()> {
    let r = receipt(seq);
    f.a.put_dedupe_mapping(
        harnx_a2a_server::store::DedupeMappingKind::Reply,
        STORAGE,
        harnx_a2a_server::store::DedupeMappingTarget::new(r.message_identity(), TASK),
    )
    .await
}
async fn following(f: &Fixture, c: &ContextSnapshot, seq: u64) -> Result<ContextSnapshot> {
    f.a.advance_hitl_to_following(
        STORAGE,
        &c.version()?,
        HitlAdvanceParams {
            expected_approval: &request(seq),
            outcome: &HitlDecisionOutcome::Applied,
        },
    )
    .await?;
    current(f).await
}

// Exercise the public wire contract: conditionally publish the committed envelope,
// read it back from JetStream, then CAS its subject checkpoint. Production uses
// OwnedTask::flush_pending; no status transition is allowed in this checkpoint CAS.
async fn publish_and_verify_drain_envelope(
    f: &Fixture,
    pending: &harnx_a2a_server::store::context::PendingEvent,
    subject: &str,
) -> Result<u64> {
    use async_nats::jetstream::message::PublishMessage;
    let stream = harnx_runtime::a2a_events::ensure(&f.js, 1).await?;
    let seq =
        f.js.send_publish(
            subject.to_owned(),
            PublishMessage::build()
                .message_id(pending.commit_id.clone())
                .expected_last_subject_sequence(pending.expected_subject_sequence)
                .payload(serde_json::to_vec(pending)?.into()),
        )
        .await?
        .await?
        .sequence;
    let saved = stream.get_last_raw_message_by_subject(subject).await?;
    assert_eq!(saved.sequence, seq);
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&saved.payload)?,
        serde_json::to_value(pending)?
    );
    Ok(seq)
}

async fn drain(f: &Fixture, c: &ContextSnapshot) -> Result<ContextSnapshot> {
    let pending = active(c).publication.pending.as_ref().context("pending")?;
    let subject = format!("a2a.tasks.{STORAGE}.01234567-89ab-cdef-0123-456789abcdef");
    let seq = publish_and_verify_drain_envelope(f, pending, &subject).await?;
    let write =
        f.a.prepare_context_update(
            STORAGE,
            &c.version()?,
            &uuid::Uuid::new_v4().to_string(),
            |state| {
                let a = state.active.as_mut().unwrap();
                a.publication.pending = None;
                a.publication.subject_sequence = seq;
                a.publication.confirmed_history.push(seq);
                if a.snapshot.task.status.state.is_terminal() {
                    a.projections.final_event = true;
                }
            },
        )
        .await?;
    f.a.commit_context(&write).await
}
fn assert_status(c: &ContextSnapshot, state: TaskState, seq: u64) {
    let a = active(c);
    assert_eq!(a.snapshot.task.status.state, state);
    assert_eq!(a.snapshot.stream_seq, seq);
    assert_eq!(a.publication.stream_seq, seq);
    let p = a.publication.pending.as_ref().unwrap();
    assert_eq!(p.task_sequence, seq);
    assert!(p.committed_at.is_some());
    let StreamResponse::StatusUpdate(event) = &p.response else {
        panic!("status event");
    };
    assert_eq!(event.task_id, TASK);
    assert_eq!(event.context_id, LOCAL);
    assert_eq!(
        serde_json::to_value(&event.status).unwrap(),
        serde_json::to_value(&a.snapshot.task.status).unwrap()
    );
}
async fn unchanged(f: &Fixture, before: &ContextSnapshot) -> Result<()> {
    let after = current(f).await?;
    assert_eq!(after.revision, before.revision);
    assert_eq!(
        serde_json::to_value(after.document)?,
        serde_json::to_value(&before.document)?
    );
    Ok(())
}
async fn terminal(f: &Fixture, c: &ContextSnapshot) -> Result<ContextSnapshot> {
    let w =
        f.a.prepare_context_update_checked(
            harnx_a2a_server::store::context::ContextMutation {
                storage_key: STORAGE,
                expected: &c.version()?,
                operation_id: "terminal",
            },
            |state| {
                let a = state.active.as_mut().unwrap();
                a.snapshot.task.status.state = TaskState::Completed;
                a.stop_confirmed = true;
                a.stage_event(
                    StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
                        task_id: TASK.into(),
                        context_id: LOCAL.into(),
                        status: a.snapshot.task.status.clone(),
                        metadata: None,
                    }),
                    "terminal",
                )
            },
        )
        .await?;
    let c = f.a.commit_context(&w).await?;
    drain(f, &c).await
}
async fn projected(f: &Fixture, c: &ContextSnapshot) -> Result<ContextSnapshot> {
    f.a.archive_context_terminal(STORAGE, TASK).await?;
    let w =
        f.a.prepare_context_update(STORAGE, &c.version()?, "projections", |state| {
            let a = state.active.as_mut().unwrap();
            a.projections.archive = true;
            a.projections.message_mapping = true;
        })
        .await?;
    f.a.commit_context(&w).await
}

async fn drive_lifecycle_first_round(
    f: &Fixture,
    c: &ContextSnapshot,
    _original: &harnx_a2a_server::store::context::AdmissionState,
) -> Result<(ContextSnapshot, u64)> {
    let c = waiting(f, c, 20).await?;
    assert_status(&c, TaskState::InputRequired, 1);
    assert_eq!(
        active(&c).publication.pending.as_ref().unwrap().commit_id,
        format!("{TASK}:hitl:20:input-required")
    );
    let c = drain(f, &c).await?;
    let marker = active(&c).snapshot.stream_seq;
    let c = intent(f, &c, 20).await?;
    assert_eq!(active(&c).snapshot.stream_seq, marker);
    assert_eq!(
        active(&c).hitl_checkpoint.as_ref().unwrap().phase,
        HitlPhase::DecisionIntent
    );
    mapping(f, 20).await?;
    let c = following(f, &c, 20).await?;
    assert_status(&c, TaskState::Working, 2);
    let cp = active(&c).hitl_checkpoint.as_ref().unwrap();
    assert_eq!(cp.phase, HitlPhase::Following);
    assert_eq!(
        cp.reply.as_ref().unwrap().outcome.as_deref(),
        Some("applied")
    );
    Ok((c, marker))
}

async fn drive_lifecycle_second_round_and_clear(
    f: &Fixture,
    c: &ContextSnapshot,
    marker: u64,
    original: &harnx_a2a_server::store::context::AdmissionState,
) -> Result<()> {
    let c = drain(f, c).await?;
    let c = waiting(f, &c, 30).await?;
    assert_status(&c, TaskState::InputRequired, 3);
    assert!(active(&c).snapshot.stream_seq > marker);
    assert_eq!(&active(&c).admission, original);
    assert!(!active(&c).stop_confirmed);
    let c = drain(f, &c).await?;
    let c = terminal(f, &c).await?;
    let record = f.a.clear_hitl_checkpoint(STORAGE, &c.version()?).await?;
    assert_eq!(record.task.status.state, TaskState::Completed);
    let c = current(f).await?;
    assert!(active(&c).hitl_checkpoint.is_none());
    Ok(())
}

#[tokio::test]
async fn hitl_checkpoint_lifecycle_advances_boundary_cursor_and_retains_ticket() -> Result<()> {
    let f = Fixture::start().await?;
    let (lease, c) = allocate(&f).await?;
    let original = active(&c).admission.clone();
    let (c, marker) = drive_lifecycle_first_round(&f, &c, &original).await?;
    drive_lifecycle_second_round_and_clear(&f, &c, marker, &original).await?;
    lease.release().await?;
    Ok(())
}

fn assert_legacy_checkpoint_deserialization(c: &ContextSnapshot) -> Result<()> {
    let mut old = active(c).clone();
    old.hitl_checkpoint = None;
    let mut value = serde_json::to_value(&old)?;
    value.as_object_mut().unwrap().remove("hitl_checkpoint");
    let legacy: ActiveTask = serde_json::from_value(value)?;
    assert!(legacy.hitl_checkpoint.is_none());
    Ok(())
}

async fn assert_checkpoint_mismatch_phases(f: &Fixture, c: &ContextSnapshot) -> Result<()> {
    mismatches::assert_wrong_approvals(f, c).await?;
    mismatches::assert_wrong_receipt_and_clear(f, c).await?;
    let c = intent(f, c, 20).await?;
    mismatches::assert_intent_phase_mismatches(f, &c).await
}

#[tokio::test]
async fn hitl_checkpoint_mismatches_and_missing_checkpoint_do_not_commit_or_panic() -> Result<()> {
    let f = Fixture::start().await?;
    let (lease, c) = allocate(&f).await?;
    let c = mismatches::assert_missing_checkpoint(&f, &c).await?;
    let c = waiting(&f, &c, 20).await?;
    assert_legacy_checkpoint_deserialization(&c)?;
    assert_checkpoint_mismatch_phases(&f, &c).await?;
    lease.release().await?;
    Ok(())
}

async fn assert_output_outbox_blocks_parking(
    f: &Fixture,
    c: &ContextSnapshot,
) -> Result<ContextSnapshot> {
    let w =
        f.a.prepare_context_update_checked(
            harnx_a2a_server::store::context::ContextMutation {
                storage_key: STORAGE,
                expected: &c.version()?,
                operation_id: "output",
            },
            |s| {
                let a = s.active.as_mut().unwrap();
                a.stage_event(StreamResponse::Task(a.snapshot.task.clone()), "output")
            },
        )
        .await?;
    let c = f.a.commit_context(&w).await?;
    assert!(waiting(f, &c, 20).await.is_err());
    unchanged(f, &c).await?;
    let c = drain(f, &c).await?;
    let c = waiting(f, &c, 20).await?;
    assert_status(&c, TaskState::InputRequired, 2);
    Ok(c)
}

async fn assert_pending_outbox_blocks_following(
    f: &Fixture,
    c: &ContextSnapshot,
) -> Result<ContextSnapshot> {
    let c = intent(f, c, 20).await?;
    let pending = serde_json::to_value(&active(&c).publication.pending)?;
    assert!(following(f, &c, 20).await.is_err());
    unchanged(f, &c).await?;
    assert_eq!(
        serde_json::to_value(&active(&current(f).await?).publication.pending)?,
        pending
    );
    let c = drain(f, &c).await?;
    let c = following(f, &c, 20).await?;
    assert_status(&c, TaskState::Working, 3);
    Ok(c)
}

#[tokio::test]
async fn hitl_checkpoint_unresolved_outbox_never_overwritten_and_rejects_snapshot_changes(
) -> Result<()> {
    let f = Fixture::start().await?;
    let (lease, c) = allocate(&f).await?;
    let c = assert_output_outbox_blocks_parking(&f, &c).await?;
    assert_pending_outbox_blocks_following(&f, &c).await?;
    lease.release().await?;
    Ok(())
}

async fn drive_to_projected_terminal(f: &Fixture, c: &ContextSnapshot) -> Result<ContextSnapshot> {
    let c = waiting(f, c, 20).await?;
    let c = drain(f, &c).await?;
    let c = intent(f, &c, 20).await?;
    let c = following(f, &c, 20).await?;
    let c = drain(f, &c).await?;
    mismatches::assert_following_receipt_immutable(f, &c).await?;
    let c = terminal(f, &c).await?;
    projected(f, &c).await
}

async fn assert_premature_clear_and_retire_rejected(
    f: &Fixture,
    c: &ContextSnapshot,
) -> Result<()> {
    assert!(f
        .a
        .clear_hitl_checkpoint(STORAGE, &c.version()?)
        .await
        .is_err());
    assert!(f
        .a
        .prepare_context_update(STORAGE, &c.version()?, "retire-before-mapping", |s| s
            .active =
            None)
        .await
        .is_err());
    unchanged(f, c).await
}

async fn retire_context(f: &Fixture, c: &ContextSnapshot) -> Result<()> {
    mapping(f, 20).await?;
    f.a.clear_hitl_checkpoint(STORAGE, &c.version()?).await?;
    let c = current(f).await?;
    let w =
        f.a.prepare_context_update(STORAGE, &c.version()?, "retire", |s| s.active = None)
            .await?;
    f.a.commit_context(&w).await?;
    Ok(())
}

async fn assert_retired_context_archive(f: &Fixture) -> Result<()> {
    let fresh = harnx_a2a_server::store::A2aStore::new(f.metadata.clone());
    assert_eq!(
        fresh.get_reply_dedupe(STORAGE, "reply-20").await?,
        Some(DedupeEntry::new(TASK, "reply-fingerprint"))
    );
    assert_eq!(
        fresh
            .get_task(STORAGE, TASK)
            .await?
            .context("archive")?
            .task
            .status
            .state,
        TaskState::Completed
    );
    Ok(())
}

async fn map_and_retire_context(f: &Fixture, c: &ContextSnapshot) -> Result<()> {
    retire_context(f, c).await?;
    assert_retired_context_archive(f).await
}

#[tokio::test]
async fn hitl_checkpoint_receipt_cannot_be_replaced_cleared_or_retired_before_mapping() -> Result<()>
{
    let f = Fixture::start().await?;
    let (lease, c) = allocate(&f).await?;
    let c = drive_to_projected_terminal(&f, &c).await?;
    assert_premature_clear_and_retire_rejected(&f, &c).await?;
    map_and_retire_context(&f, &c).await?;
    lease.release().await?;
    Ok(())
}

async fn assert_oversized_note_bounded(
    f: &Fixture,
    c: &ContextSnapshot,
) -> Result<ContextSnapshot> {
    let mut reply = receipt(20);
    // Direct struct construction must not bypass the helper's UTF-8-safe bound.
    reply.note = Some("€".repeat(5000));
    f.a.record_hitl_reply_intent(
        STORAGE,
        &c.version()?,
        HitlReplyIntent {
            expected_approval: &request(20),
            receipt: reply,
        },
    )
    .await?;
    let c = current(f).await?;
    let note = active(&c)
        .hitl_checkpoint
        .as_ref()
        .unwrap()
        .reply
        .as_ref()
        .unwrap()
        .note
        .as_ref()
        .unwrap();
    assert!(note.len() <= NOTE_BYTE_LIMIT);
    assert_eq!(note.len(), NOTE_BYTE_LIMIT - 1);
    Ok(c)
}

async fn assert_reply_dedupe_mappings(f: &Fixture) -> Result<()> {
    let (a, b) = tokio::join!(mapping(f, 20), mapping(f, 20));
    a?;
    b?;
    assert!(f
        .b
        .put_dedupe_mapping(
            harnx_a2a_server::store::DedupeMappingKind::Reply,
            STORAGE,
            harnx_a2a_server::store::DedupeMappingTarget::new(
                MessageIdentity::new("reply-20", "opposite"),
                TASK,
            ),
        )
        .await
        .is_err());
    let other = "authority-local.11234567-89ab-cdef-0123-456789abcdef";
    assert!(f
        .b
        .put_dedupe_mapping(
            harnx_a2a_server::store::DedupeMappingKind::Reply,
            STORAGE,
            harnx_a2a_server::store::DedupeMappingTarget::new(
                MessageIdentity::new("reply-20", "reply-fingerprint"),
                other
            )
        )
        .await
        .is_err());
    let fresh = harnx_a2a_server::store::A2aStore::new(f.metadata.clone());
    assert_eq!(
        fresh.get_reply_dedupe(STORAGE, "reply-20").await?,
        Some(DedupeEntry::new(TASK, "reply-fingerprint"))
    );
    assert!(fresh.get_reply_dedupe(STORAGE, "missing").await?.is_none());
    Ok(())
}

#[tokio::test]
async fn hitl_checkpoint_reply_dedupe_conflicts_persist_and_note_is_bounded() -> Result<()> {
    let f = Fixture::start().await?;
    let (lease, c) = allocate(&f).await?;
    let c = waiting(&f, &c, 20).await?;
    let _c = assert_oversized_note_bounded(&f, &c).await?;
    assert_reply_dedupe_mappings(&f).await?;
    lease.release().await?;
    Ok(())
}

async fn release_to_parked(
    f: &Fixture,
    c: &ContextSnapshot,
    lease: harnx_runtime::nats_lease::NatsSessionLease,
) -> Result<ContextSnapshot> {
    assert!(f
        .a
        .prepare_context_update(STORAGE, &c.version()?, "retire-parked", |s| s.active = None)
        .await
        .is_err());
    let w =
        f.a.prepare_context_release(STORAGE, &c.version()?, "park")
            .await?;
    f.a.commit_context(&w).await?;
    lease.release().await?;
    current(f).await
}

fn assert_parked_checkpoint_state(parked: &ContextSnapshot) {
    assert!(parked.document.owner.is_none());
    assert!(!active(parked).stop_confirmed);
    assert_eq!(
        active(parked).hitl_checkpoint.as_ref().unwrap().request,
        request(20)
    );
}

async fn park_and_verify_ownerless(
    f: &Fixture,
    c: &ContextSnapshot,
    lease: harnx_runtime::nats_lease::NatsSessionLease,
) -> Result<()> {
    let c = waiting(f, c, 20).await?;
    let c = drain(f, &c).await?;
    let parked = release_to_parked(f, &c, lease).await?;
    assert_parked_checkpoint_state(&parked);
    assert!(f
        .a
        .record_hitl_reply_intent(
            STORAGE,
            &c.version()?,
            HitlReplyIntent {
                expected_approval: &request(20),
                receipt: receipt(20),
            },
        )
        .await
        .is_err());
    Ok(())
}

async fn takeover_and_assert_intent(f: &Fixture) -> Result<()> {
    let next = f.lease("new-owner").await?;
    let c = f.claim(&f.b, &next, "takeover").await?;
    let c = intent(f, &c, 20).await?;
    assert_eq!(active(&c).snapshot.task.id, TASK);
    assert_eq!(active(&c).snapshot.stream_seq, 1);
    assert!(f
        .a
        .prepare_context_update(STORAGE, &c.version()?, "retire-intent", |s| s.active = None)
        .await
        .is_err());
    next.release().await
}

#[tokio::test]
async fn hitl_checkpoint_parked_owner_release_and_takeover_preserve_nonretirable_work() -> Result<()>
{
    let f = Fixture::start().await?;
    let (lease, c) = allocate(&f).await?;
    park_and_verify_ownerless(&f, &c, lease).await?;
    takeover_and_assert_intent(&f).await?;
    Ok(())
}

async fn drive_concurrent_reply_intents(
    f: &Fixture,
    version: &ContextVersion,
) -> Result<ReplyReceipt> {
    let request = request(20);
    let approve = receipt(20);
    let mut reject = receipt(20);
    reject.message_id = "reject-reply".into();
    reject.approved = false;
    let (a, b) = tokio::join!(
        f.a.record_hitl_reply_intent(
            STORAGE,
            version,
            HitlReplyIntent {
                expected_approval: &request,
                receipt: approve.clone(),
            },
        ),
        f.b.record_hitl_reply_intent(
            STORAGE,
            version,
            HitlReplyIntent {
                expected_approval: &request,
                receipt: reject.clone(),
            },
        ),
    );
    assert_ne!(a.is_ok(), b.is_ok());
    Ok(if a.is_ok() { approve } else { reject })
}

async fn assert_concurrent_reply_winner(
    f: &Fixture,
    expected: ReplyReceipt,
) -> Result<ContextSnapshot> {
    let request = request(20);
    let c = current(f).await?;
    assert_eq!(
        active(&c).hitl_checkpoint.as_ref().unwrap().reply.as_ref(),
        Some(&expected)
    );
    assert_eq!(active(&c).snapshot.stream_seq, 1);
    assert!(f
        .b
        .record_hitl_reply_intent(
            STORAGE,
            &c.version()?,
            HitlReplyIntent {
                expected_approval: &request,
                receipt: expected,
            },
        )
        .await
        .is_err());
    unchanged(f, &c).await?;
    Ok(c)
}

#[tokio::test]
async fn hitl_checkpoint_concurrent_replies_keep_first_exact_intent() -> Result<()> {
    let f = Fixture::start().await?;
    let (lease, c) = allocate_and_drain_waiting(&f).await?;
    let expected = drive_concurrent_reply_intents(&f, &c.version()?).await?;
    assert_concurrent_reply_winner(&f, expected).await?;
    lease.release().await?;
    Ok(())
}

async fn allocate_and_drain_waiting(f: &Fixture) -> Result<(NatsSessionLease, ContextSnapshot)> {
    let (lease, c) = allocate(f).await?;
    let c = waiting(f, &c, 20).await?;
    let c = drain(f, &c).await?;
    Ok((lease, c))
}

async fn advance_external_decision_following(
    f: &Fixture,
    c: &ContextSnapshot,
) -> Result<ContextSnapshot> {
    f.a.advance_hitl_to_following(
        STORAGE,
        &c.version()?,
        HitlAdvanceParams {
            expected_approval: &request(20),
            outcome: &HitlDecisionOutcome::AlreadyApplied {
                approved: true,
                note: None,
            },
        },
    )
    .await?;
    let c = current(f).await?;
    assert_status(&c, TaskState::Working, 2);
    assert!(active(&c).hitl_checkpoint.as_ref().unwrap().reply.is_none());
    Ok(c)
}

#[tokio::test]
async fn hitl_checkpoint_external_decision_can_follow_without_inline_reply() -> Result<()> {
    let f = Fixture::start().await?;
    let (lease, c) = allocate_and_drain_waiting(&f).await?;
    let c = advance_external_decision_following(&f, &c).await?;
    let c = drain(&f, &c).await?;
    let c = waiting(&f, &c, 30).await?;
    assert_status(&c, TaskState::InputRequired, 3);
    lease.release().await?;
    Ok(())
}
