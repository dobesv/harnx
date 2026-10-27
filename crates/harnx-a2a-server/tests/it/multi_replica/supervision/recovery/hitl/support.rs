use super::*;
use harnx_a2a_server::store::context::PendingEvent;

pub(super) fn assert_uncheckpointed_request(before: &ContextSnapshot) {
    assert_eq!(
        before
            .document
            .state
            .active
            .as_ref()
            .unwrap()
            .snapshot
            .task
            .status
            .state,
        TaskState::Working
    );
    assert!(before
        .document
        .state
        .active
        .as_ref()
        .unwrap()
        .hitl_checkpoint
        .is_none());
}

pub(super) async fn remove_saved_history(
    t: &TwoBackends,
    session: &NatsSession,
) -> Result<a2a_lf::Message> {
    let intent =
        t.h.store
            .read_context(session.storage_key())
            .await?
            .context("context")?;
    let saved = intent
        .document
        .state
        .active
        .as_ref()
        .unwrap()
        .snapshot
        .task
        .history
        .as_ref()
        .unwrap()
        .iter()
        .find(|m| m.message_id == "recovery-reply")
        .unwrap()
        .clone();
    // Model inconsistent persisted history, not permission to use the inline note.
    let update =
        t.h.store
            .prepare_context_update_checked(
                harnx_a2a_server::store::context::ContextMutation {
                    storage_key: session.storage_key(),
                    expected: &intent.version()?,
                    operation_id: "remove-saved-history",
                },
                |state| {
                    state
                        .active
                        .as_mut()
                        .unwrap()
                        .snapshot
                        .task
                        .history
                        .as_mut()
                        .unwrap()
                        .retain(|m| m.message_id != saved.message_id);
                    Ok(())
                },
            )
            .await?;
    t.h.store.commit_context(&update).await?;
    Ok(saved)
}

pub(super) async fn assert_deferred_intent(t: &TwoBackends, session: &NatsSession) -> Result<()> {
    let unchanged =
        t.h.store
            .read_context(session.storage_key())
            .await?
            .context("context")?;
    let active = unchanged.document.state.active.as_ref().unwrap();
    assert_eq!(active.snapshot.task.status.state, TaskState::InputRequired);
    assert_eq!(
        active.hitl_checkpoint.as_ref().unwrap().phase,
        HitlPhase::DecisionIntent
    );
    assert!(unchanged.document.owner.is_none());
    let entries = NatsSessionLog::new(t.h.jetstream.clone(), session.storage_key())
        .load_events_latest_required_async()
        .await?;
    assert!(!entries.iter().any(|(_, e)| matches!(
        e,
        SessionLogEntry::HitlApprovalDecision { .. } | SessionLogEntry::Cancel { .. }
    )));
    Ok(())
}

async fn claim_history_repair(
    t: &TwoBackends,
    session: &NatsSession,
) -> Result<(harnx_runtime::nats_lease::NatsSessionLease, ContextSnapshot)> {
    let lease = harnx_runtime::nats_lease::NatsSessionLease::acquire_scoped(
        harnx_runtime::nats_lease::NatsLeaseAcquireParams {
            jetstream: t.h.jetstream.clone(),
            session_id: session.storage_key(),
            worker_id: "history-repair".into(),
            generation: 1,
            config: harnx_runtime::nats_lease::NatsLeaseConfig {
                replicas: 1,
                ..Default::default()
            },
            session_metadata: None,
        },
        "a2a",
    )
    .await?
    .context("repair lease")?;
    let claim =
        t.h.store
            .prepare_context_claim(
                harnx_a2a_server::store::context::ContextIdentity {
                    storage_key: session.storage_key(),
                    local_id: session.session_id(),
                },
                &lease,
                "history-repair-claim",
            )
            .await?;
    let owned = t.h.store.commit_context(&claim).await?;
    Ok((lease, owned))
}

async fn push_saved_history(
    t: &TwoBackends,
    session: &NatsSession,
    owned: &ContextSnapshot,
    saved: a2a_lf::Message,
) -> Result<ContextSnapshot> {
    let update =
        t.h.store
            .prepare_context_update_checked(
                harnx_a2a_server::store::context::ContextMutation {
                    storage_key: session.storage_key(),
                    expected: &owned.version()?,
                    operation_id: "restore-saved-history",
                },
                |state| {
                    state
                        .active
                        .as_mut()
                        .unwrap()
                        .snapshot
                        .task
                        .history
                        .as_mut()
                        .unwrap()
                        .push(saved);
                    Ok(())
                },
            )
            .await?;
    let owned = t.h.store.commit_context(&update).await?;
    Ok(owned)
}

async fn release_history_repair(
    t: &TwoBackends,
    session: &NatsSession,
    owned: &ContextSnapshot,
    lease: harnx_runtime::nats_lease::NatsSessionLease,
) -> Result<()> {
    let release =
        t.h.store
            .prepare_context_release(session.storage_key(), &owned.version()?, "restore-release")
            .await?;
    t.h.store.commit_context(&release).await?;
    lease.release().await?;
    Ok(())
}

pub(super) async fn restore_saved_history(
    t: &TwoBackends,
    session: &NatsSession,
    saved: a2a_lf::Message,
) -> Result<()> {
    // Restore only the missing audit history. Original exact receipt remains.
    let (lease, owned) = claim_history_repair(t, session).await?;
    let owned = push_saved_history(t, session, &owned, saved).await?;
    release_history_repair(t, session, &owned, lease).await?;
    Ok(())
}

pub(super) async fn assert_next_request_parked(
    t: &TwoBackends,
    session: &NatsSession,
    next: &ContextSnapshot,
) -> Result<()> {
    let checkpoint = next
        .document
        .state
        .active
        .as_ref()
        .unwrap()
        .hitl_checkpoint
        .as_ref()
        .unwrap();
    assert_eq!(checkpoint.phase, HitlPhase::Waiting);
    assert!(checkpoint.reply.is_none());
    assert!(t
        .h
        .store
        .get_reply_dedupe(session.storage_key(), "recovery-reply")
        .await?
        .is_some());
    let entries = NatsSessionLog::new(t.h.jetstream.clone(), session.storage_key())
        .load_events_latest_required_async()
        .await?;
    assert_eq!(
        entries
            .iter()
            .filter(|(_, e)| matches!(e, SessionLogEntry::HitlApprovalDecision { .. }))
            .count(),
        1
    );
    Ok(())
}

pub(super) async fn assert_retained_challenge_envelope(
    t: &TwoBackends,
    session: &NatsSession,
) -> Result<PendingEvent> {
    let committed =
        t.h.store
            .read_context(session.storage_key())
            .await?
            .context("context")?;
    let active = committed.document.state.active.as_ref().unwrap();
    let envelope = active
        .publication
        .pending
        .as_ref()
        .context("retained challenge envelope")?
        .clone();
    let request = &active.hitl_checkpoint.as_ref().unwrap().request;
    assert_eq!(
        envelope.commit_id,
        format!(
            "{}:hitl:{}:input-required",
            active.snapshot.task.id, request.request_seq
        )
    );
    assert!(!active.stop_confirmed);
    Ok(envelope)
}
