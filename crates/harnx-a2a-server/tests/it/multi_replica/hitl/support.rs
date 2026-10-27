use super::*;

pub(super) async fn start_race(t: &TwoBackends) -> Result<(Value, NatsSession, ContextSnapshot)> {
    let mut params = first_message();
    params.as_object_mut().unwrap().remove("configuration");
    let initial = rpc(t.a.app.clone(), "alice", "SendMessage", params).await?;
    let task = initial["result"]["task"].clone();
    assert_eq!(
        task["status"]["state"], "TASK_STATE_INPUT_REQUIRED",
        "{initial}"
    );
    let session = t
        .resume(&t.b, task["contextId"].as_str().context("context")?)
        .await?;
    let first = parked(t, &session, 0).await?;
    Ok((task, session, first))
}

pub(super) fn assert_delayed_reply(delayed: &Value, task: &Value, duplicate: bool) {
    if duplicate {
        assert_eq!(delayed["result"]["task"]["id"], task["id"], "{delayed}");
        assert_eq!(
            delayed["result"]["task"]["status"]["state"],
            "TASK_STATE_INPUT_REQUIRED"
        );
    } else {
        assert_eq!(delayed["error"]["code"], -32602, "{delayed}");
    }
}

pub(super) async fn assert_undecided_gate(
    t: &TwoBackends,
    session: &NatsSession,
    q1: &ApprovalRef,
    q2: &ApprovalRef,
) -> Result<NatsSessionLog> {
    let log = NatsSessionLog::new(t.h.jetstream.clone(), session.storage_key());
    let entries = log.load_events_latest_required_async().await?;
    let decisions: Vec<_> = entries
        .iter()
        .filter_map(|(_, e)| match e {
            SessionLogEntry::HitlApprovalDecision {
                tool_call_id,
                approved,
                note,
                ..
            } => Some((tool_call_id.as_str(), *approved, note.as_deref())),
            _ => None,
        })
        .collect();
    assert_eq!(
        decisions,
        vec![(q1.tool_call_id.as_str(), true, Some("first audit"))]
    );
    let unchanged = parked(t, session, q1.request_seq).await?;
    assert_eq!(request(&unchanged), q2);
    let active = unchanged.document.state.active.as_ref().unwrap();
    assert!(active.hitl_checkpoint.as_ref().unwrap().reply.is_none());
    let history = active.snapshot.task.history.as_ref().unwrap();
    assert_eq!(
        history
            .iter()
            .filter(|m| m.message_id == "winning-reply")
            .count(),
        1
    );
    assert!(!history.iter().any(|m| m.message_id == "losing-reply"));
    assert_eq!(prompts(&log).await?.len(), 1);
    assert_eq!(
        t.h.llm.requests.lock().len(),
        1,
        "Q2 must not require another model round"
    );

    Ok(log)
}

pub(super) struct RaceCompletion<'a> {
    pub task: &'a Value,
    pub first: &'a ContextSnapshot,
    pub log: &'a NatsSessionLog,
}

pub(super) async fn complete_on_loser(
    t: &TwoBackends,
    loser: &Replica,
    expected: RaceCompletion<'_>,
) -> Result<()> {
    let RaceCompletion { task, first, log } = expected;
    let final_response = rpc(
        loser.app.clone(),
        "alice",
        "SendMessage",
        json!({"message":crate::unary::hitl::reply("second-reply", task, "y", false)}),
    )
    .await?;
    assert_eq!(final_response["result"]["task"]["id"], task["id"]);
    assert_eq!(
        final_response["result"]["task"]["status"]["state"], "TASK_STATE_COMPLETED",
        "{final_response}"
    );
    let final_record = t.h.task(task["id"].as_str().unwrap()).await?;
    let original = &first.document.state.active.as_ref().unwrap().snapshot;
    assert_eq!(final_record.execution_id, original.execution_id);
    assert_eq!(final_record.user_msg_seq, original.user_msg_seq);
    assert_eq!(prompts(log).await?.len(), 1);
    assert_eq!(
        log.load_events_latest_required_async()
            .await?
            .iter()
            .filter(|(_, e)| matches!(e, SessionLogEntry::HitlApprovalDecision { .. }))
            .count(),
        2
    );
    assert_eq!(t.h.llm.requests.lock().len(), 2);
    Ok(())
}

pub(super) async fn assert_terminal_retries(
    t: &TwoBackends,
    task: &Value,
    winner_message: &Value,
) -> Result<()> {
    // Durable mappings work on either replica even after the terminal boundary.
    for replica in [&t.a, &t.b] {
        let retry = rpc(
            replica.app.clone(),
            "alice",
            "SendMessage",
            json!({"message":winner_message}),
        )
        .await?;
        assert_eq!(retry["result"]["task"]["id"], task["id"], "{retry}");
        assert_eq!(
            retry["result"]["task"]["status"]["state"],
            "TASK_STATE_COMPLETED"
        );
    }
    Ok(())
}
