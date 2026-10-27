use super::*;

pub(super) async fn assert_parked_deadline_cancellation(
    t: &TwoBackends,
    session: &NatsSession,
    before: &ContextSnapshot,
) -> Result<()> {
    let active = before.document.state.active.as_ref().unwrap();
    let task_id = &active.snapshot.task.id;
    let request = &active.hitl_checkpoint.as_ref().unwrap().request;
    assert_eq!(
        terminal(t, task_id).await?.task.status.state,
        TaskState::Canceled
    );
    let log = NatsSessionLog::new(t.h.jetstream.clone(), session.storage_key());
    let entries = log.load_events_latest_required_async().await?;
    assert!(!entries
        .iter()
        .any(|(_, e)| matches!(e, SessionLogEntry::HitlApprovalDecision { .. })));
    assert_eq!(
        session
            .decide_hitl_approval_for(request, true, None)
            .await?,
        HitlDecisionOutcome::Stale
    );
    let late = t.b.rpc("alice", "SendMessage", reply(before, "y")).await?;
    assert_eq!(late["error"]["code"], -32602, "{late}");
    assert_eq!(
        late["error"]["message"],
        "terminal tasks cannot be continued; send a new message with contextId"
    );
    assert_eq!(
        log.load_events_latest_required_async().await?.len(),
        entries.len()
    );
    assert_eq!(
        t.h.task(task_id).await?.task.status.state,
        TaskState::Canceled
    );
    assert_eq!(t.h.llm.requests.lock().len(), 1);
    assert_original(t, session, before).await?;
    t.b.backend.abort.set_ctrlc();
    Ok(())
}
