use super::*;
use harnx_a2a_server::store::context::AdmissionPhase;

fn assert_suspended_context_state(context: &ContextSnapshot) -> Result<()> {
    let active = context.document.state.active.as_ref().context("active")?;
    assert_eq!(active.admission.phase, AdmissionPhase::Admitted);
    assert!(!active.stop_confirmed);
    assert!(active.publication.pending.is_none());
    let checkpoint = active.hitl_checkpoint.as_ref().context("checkpoint")?;
    assert_eq!(checkpoint.phase, HitlPhase::Waiting);
    assert!(checkpoint.reply.is_none());
    assert_eq!(
        text(&active.snapshot.task.artifacts.as_ref().unwrap()[0].parts),
        "Before approval. "
    );
    Ok(())
}

async fn assert_suspended_admission_busy(
    h: &Harness,
    session: &NatsSession,
    ticket: &harnx_runtime::nats_session::fixed_admission::FixedAdmissionTicket,
    entries: &[(u64, SessionLogEntry)],
) -> Result<()> {
    let admission = h
        .metadata
        .active_admission(session.storage_key(), entries)
        .await?
        .context("runtime admission must remain busy")?;
    assert_eq!(admission.invocation_id.as_str(), ticket.invocation_id());
    let mut other = admission;
    other.invocation_id = harnx_runtime::nats_session_metadata::InvocationIdentity::new();
    other.fixed_ticket = None;
    other.prompt_content = None;
    let error = h
        .metadata
        .reserve_admission(session.storage_key(), &other, entries)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("session busy"), "{error:#}");
    assert_eq!(h.llm.requests.lock().len(), 1);
    Ok(())
}

pub(super) async fn assert_suspended(
    h: &Harness,
    session: &NatsSession,
    context: &ContextSnapshot,
) -> Result<()> {
    assert_suspended_context_state(context)?;
    let checkpoint = context
        .document
        .state
        .active
        .as_ref()
        .unwrap()
        .hitl_checkpoint
        .as_ref()
        .unwrap();
    let ticket = ticket(context, session.storage_key())?;
    assert!(
        matches!(session.fixed_prompt_progress(&ticket).await?, PromptProgress::InputRequired(p) if p.request_seq == checkpoint.request.request_seq)
    );
    assert!(session.fixed_prompt_completion(&ticket).await?.is_none());
    let entries = NatsSessionLog::new_with_replicas(h.jetstream.clone(), session.storage_key(), 1)
        .load_events_latest_required_async()
        .await?;
    assert!(!entries.iter().any(|(_, entry)| matches!(
        entry,
        SessionLogEntry::TurnEnd { .. }
            | SessionLogEntry::Cancel { .. }
            | SessionLogEntry::Error { .. }
            | SessionLogEntry::HandoffCommitted { .. }
            | SessionLogEntry::HitlApprovalDecision { .. }
    )));
    assert_suspended_admission_busy(h, session, &ticket, &entries).await?;
    Ok(())
}

pub(super) async fn drain_resumed_artifact(
    rx: &mut tokio::sync::broadcast::Receiver<A2aEvent>,
    initial: &str,
) -> Result<String> {
    let mut assembled = initial.to_owned();
    loop {
        let next = event(rx).await?;
        if let StreamResponse::ArtifactUpdate(update) = &next.response {
            assert_eq!(update.artifact.artifact_id, "answer");
            super::super::append_artifact(&mut assembled, update);
        }
        if next.is_terminal() {
            break;
        }
    }
    Ok(assembled)
}

pub(super) async fn assert_completed_output(
    h: &Harness,
    session: &NatsSession,
    task_id: &str,
    assembled: &str,
) -> Result<()> {
    let final_task = h.task(task_id).await?;
    assert_eq!(final_task.task.status.state, TaskState::Completed);
    assert_eq!(
        text(&final_task.task.artifacts.as_ref().unwrap()[0].parts),
        assembled
    );
    let entries = NatsSessionLog::new_with_replicas(h.jetstream.clone(), session.storage_key(), 1)
        .load_events_latest_required_async()
        .await?;
    assert_eq!(
        entries
            .iter()
            .filter(|(_, entry)| matches!(
                entry,
                SessionLogEntry::Message { role, content, .. }
                    if role.is_user() && !harnx_core::timing_identity::is_timing_notice(&content.to_text())
            ))
            .count(),
        1
    );
    assert_eq!(h.llm.requests.lock().len(), 2);
    Ok(())
}

pub(super) fn assert_next_approval_parked(
    started: &harnx_a2a_server::runner::StartTurnResult,
    context: &ContextSnapshot,
    parked: &ContextSnapshot,
    expected_ref: ApprovalRef,
) -> Result<()> {
    let active = parked.document.state.active.as_ref().unwrap();
    assert_eq!(active.snapshot.task.id, started.snapshot.task.id);
    assert_eq!(
        active.admission,
        context.document.state.active.as_ref().unwrap().admission
    );
    let checkpoint = active.hitl_checkpoint.as_ref().unwrap();
    assert_eq!(checkpoint.request, expected_ref);
    assert_eq!(checkpoint.phase, HitlPhase::Waiting);
    assert!(active.snapshot.stream_seq > started.snapshot.stream_seq);
    assert!(!active.stop_confirmed);
    assert!(active.publication.pending.is_none());
    assert_eq!(
        text(&active.snapshot.task.artifacts.as_ref().unwrap()[0].parts),
        "Before approval. Between approvals. "
    );
    Ok(())
}

#[cfg(feature = "fault-injection")]
pub(super) async fn assert_single_decision_and_task_history(
    h: &Harness,
    session: &NatsSession,
    task_id: &str,
    exclude_msg_id: &str,
) -> Result<()> {
    let entries = NatsSessionLog::new_with_replicas(h.jetstream.clone(), session.storage_key(), 1)
        .load_events_latest_required_async()
        .await?;
    assert_eq!(
        entries
            .iter()
            .filter(|(_, e)| matches!(e, SessionLogEntry::HitlApprovalDecision { .. }))
            .count(),
        1
    );
    let latest = h.task(task_id).await?;
    assert_eq!(latest.task.status.state, TaskState::InputRequired);
    assert!(!latest
        .task
        .history
        .as_ref()
        .unwrap()
        .iter()
        .any(|m| m.message_id == exclude_msg_id));
    Ok(())
}

pub(super) async fn assert_saved_intent_history_and_events(
    h: &Harness,
    session: &NatsSession,
    task_id: &str,
    message_id: &str,
) -> Result<()> {
    assert_eq!(
        h.task(task_id)
            .await?
            .task
            .history
            .as_ref()
            .unwrap()
            .iter()
            .filter(|m| m.message_id == message_id)
            .count(),
        1
    );
    let entries = NatsSessionLog::new_with_replicas(h.jetstream.clone(), session.storage_key(), 1)
        .load_events_latest_required_async()
        .await?;
    assert_eq!(
        entries
            .iter()
            .filter(|(_, e)| matches!(e, SessionLogEntry::HitlApprovalDecision { .. }))
            .count(),
        1
    );
    assert_eq!(
        entries
            .iter()
            .filter(|(_, entry)| matches!(
                entry,
                SessionLogEntry::Message { role, content, .. }
                    if role.is_user() && !harnx_core::timing_identity::is_timing_notice(&content.to_text())
            ))
            .count(),
        1
    );
    Ok(())
}

#[cfg(feature = "fault-injection")]
pub(super) async fn assert_inflight_duplicate_records(
    h: &Harness,
    session: &NatsSession,
    task_id: &str,
    message_id: &str,
) -> Result<()> {
    let entries = NatsSessionLog::new_with_replicas(h.jetstream.clone(), session.storage_key(), 1)
        .load_events_latest_required_async()
        .await?;
    assert_eq!(
        entries
            .iter()
            .filter(|(_, e)| matches!(e, SessionLogEntry::HitlApprovalDecision { .. }))
            .count(),
        1
    );
    assert_eq!(
        h.task(task_id)
            .await?
            .task
            .history
            .as_ref()
            .unwrap()
            .iter()
            .filter(|m| m.message_id == message_id)
            .count(),
        1
    );
    assert_eq!(h.llm.requests.lock().len(), 2);
    Ok(())
}

pub(super) async fn assert_nonterminal_boundary(
    rx: &mut tokio::sync::broadcast::Receiver<A2aEvent>,
) -> Result<()> {
    loop {
        let next = event(rx).await?;
        if next.ends_response() {
            assert!(!next.is_terminal());
            return Ok(());
        }
    }
}
