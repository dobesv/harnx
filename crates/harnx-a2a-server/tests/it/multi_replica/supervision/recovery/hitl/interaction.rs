//! Middleware boundaries keep the existing fixed admission across replica recovery.
use super::*;
use harnx_core::interaction::InteractionResponse;
use harnx_runtime::interactions::derive_pending_interactions;

fn send_yes(t: &TwoBackends, context: &ContextSnapshot) -> tokio::task::JoinHandle<Result<Value>> {
    let app = t.a.app.clone();
    let message = reply(context, "yes");
    tokio::spawn(async move { rpc(app, "alice", "SendMessage", message).await })
}

async fn start_ready(script: Script) -> Result<(TwoBackends, NatsSession, ContextSnapshot)> {
    let backends = TwoBackends::start_script(script).await?;
    let (session, context) = start_parked(&backends).await?;
    Ok((backends, session, context))
}

async fn begin(t: &TwoBackends) -> Result<(NatsSession, Value)> {
    let first = t.a.rpc("alice", "SendMessage", first_message()).await?;
    let task = result_task(&first)?.clone();
    let session = session_for(&t.b, &t.h, task["contextId"].as_str().unwrap()).await?;
    Ok((session, task))
}

async fn pending(
    t: &TwoBackends,
    session: &NatsSession,
) -> Result<harnx_runtime::interactions::PendingInteraction> {
    let entries = NatsSessionLog::new(t.h.jetstream.clone(), session.storage_key())
        .load_events_latest_required_async()
        .await?;
    derive_pending_interactions(&entries)?
        .into_iter()
        .next()
        .context("pending interaction")
}

async fn assert_completed(
    t: &TwoBackends,
    session: &NatsSession,
    before: &ContextSnapshot,
) -> Result<()> {
    let original = before.document.state.active.as_ref().unwrap();
    let done = terminal(t, &original.snapshot.task.id)
        .await
        .with_context(|| t.h.logs.text())?;
    assert_eq!(
        done.task.status.state,
        TaskState::Completed,
        "{}",
        t.h.logs.text()
    );
    assert_eq!(done.execution_id, original.snapshot.execution_id);
    assert_eq!(done.user_msg_id, original.snapshot.user_msg_id);
    let entries = NatsSessionLog::new(t.h.jetstream.clone(), session.storage_key())
        .load_events_latest_required_async()
        .await?;
    assert_eq!(
        entries
            .iter()
            .filter(|(_, e)| matches!(e, SessionLogEntry::InteractionResponse { .. }))
            .count(),
        1
    );
    assert!(!entries
        .iter()
        .any(|(_, e)| matches!(e, SessionLogEntry::Cancel { .. })));
    assert_eq!(
        std::fs::read_to_string(t.h.config_dir().join("executions"))?
            .lines()
            .count(),
        1
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recovery_interaction_restart_parked_reply_on_new_replica() -> Result<()> {
    let (t, session, before) = start_ready(Script::ConfirmTool).await?;
    t.a.backend.runner.shutdown().await;
    assert_readonly_inspection(&t, &session, before.revision)
        .await
        .with_context(|| t.h.logs.text())?;
    let response =
        t.b.rpc("alice", "SendMessage", reply(&before, "yes"))
            .await?;
    assert_eq!(
        result_task(&response)?["status"]["state"],
        "TASK_STATE_COMPLETED",
        "{response}"
    );
    assert_completed(&t, &session, &before).await?;
    t.b.backend.abort.set_ctrlc();
    Ok(())
}

async fn recover_reply(boundary: Boundary) -> Result<()> {
    let (t, session, before) = start_ready(Script::ConfirmTool).await?;
    let mut pause = t.a.backend.runner.fault_hooks().pause(boundary, 1);
    let send = tokio::spawn({
        let app = t.a.app.clone();
        let message = reply(&before, "yes recovery note");
        async move { rpc(app, "alice", "SendMessage", message).await }
    });
    tokio::time::timeout(DEADLINE, pause.reached()).await?;
    remove_owner_lease(&t, session.storage_key()).await?;
    supervise(&t, &t.b);
    assert_completed(&t, &session, &before).await?;
    drop(pause);
    let _ = send.await?;
    t.a.backend.runner.shutdown().await;
    t.b.backend.abort.set_ctrlc();
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recovery_interaction_saved_intent_delivered_by_new_replica() -> Result<()> {
    recover_reply(Boundary::HitlReplyIntentCommitted).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recovery_interaction_response_committed_before_follower_attach() -> Result<()> {
    recover_reply(Boundary::HitlDecisionCommitted).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recovery_interaction_external_response_is_picked_up_without_http() -> Result<()> {
    let (t, session, before) = start_ready(Script::ConfirmTool).await?;
    t.a.backend.runner.shutdown().await;
    let request = pending(&t, &session).await?;
    let outcome = session
        .decide_interaction(
            &request.interaction_id,
            request.request_seq,
            &InteractionResponse {
                action: "approve".into(),
                data: Default::default(),
                note: None,
            },
        )
        .await?;
    assert!(outcome.is_applied());
    supervise(&t, &t.b);
    assert_completed(&t, &session, &before).await?;
    t.b.backend.abort.set_ctrlc();
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recovery_interaction_only_visible_after_suspension_ready() -> Result<()> {
    use harnx_runtime::fault_injection::{pause_point, RecoveryFaultPoint};
    let t = TwoBackends::start_script(Script::ConfirmHeld).await?;
    let (session, task) = begin(&t).await?;
    let mut pause = pause_point(
        session.storage_key(),
        RecoveryFaultPoint::InteractionRequestedAfter,
    );
    t.h.llm.release.notify_one();
    pause.wait_reached().await?;
    let log = NatsSessionLog::new(t.h.jetstream.clone(), session.storage_key());
    let entries = log.load_events_latest_required_async().await?;
    assert!(entries
        .iter()
        .any(|(_, e)| matches!(e, SessionLogEntry::InteractionRequested { .. })));
    assert!(derive_pending_interactions(&entries)?.is_empty());
    let result =
        t.b.rpc("alice", "GetTask", json!({"id": task["id"]}))
            .await?;
    assert_eq!(
        result["result"]["status"]["state"], "TASK_STATE_WORKING",
        "{result}"
    );
    pause.release();
    let before = parked(&t, session.storage_key(), 0).await?;
    assert!(pending(&t, &session)
        .await?
        .summary()
        .contains("Run counted command?"));
    let response =
        t.b.rpc("alice", "SendMessage", reply(&before, "yes"))
            .await?;
    assert_eq!(
        result_task(&response)?["status"]["state"],
        "TASK_STATE_COMPLETED",
        "{response}"
    );
    assert_completed(&t, &session, &before).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recovery_interaction_unsupported_kind_is_deny_only() -> Result<()> {
    let t = TwoBackends::start_script(Script::ConfirmTool).await?;
    let mut pause =
        t.a.backend
            .runner
            .fault_hooks()
            .pause(Boundary::HitlRequestObserved, 1);
    let (session, _) = begin(&t).await?;
    tokio::time::timeout(DEADLINE, pause.reached()).await?;
    let log = NatsSessionLog::new(t.h.jetstream.clone(), session.storage_key());
    let entries = log.load_events_latest_required_async().await?;
    let (_, mut request) = entries
        .into_iter()
        .find(|(_, e)| matches!(e, SessionLogEntry::InteractionRequested { .. }))
        .context("request")?;
    let SessionLogEntry::InteractionRequested {
        kind,
        interaction_id,
        tool_round_seq,
        ..
    } = &mut request
    else {
        unreachable!()
    };
    let original_id = interaction_id.clone();
    *kind = "future-kind".into();
    *interaction_id = "future-interaction".into();
    let round = *tool_round_seq;
    // Seed a future producer before A2A projects its first challenge. No frontend
    // should need a new script host function to fail closed on an unknown kind.
    let seq = log.append_event_async(&request).await?;
    log.append_event_async(&SessionLogEntry::SuspensionReady {
        tool_round_seq: round,
        pending: vec!["future-interaction".into(), original_id],
        fence_token: 0,
    })
    .await?;
    remove_owner_lease(&t, session.storage_key()).await?;
    supervise(&t, &t.b);
    let before = parked(&t, session.storage_key(), 0).await?;
    drop(pause);
    let active = before.document.state.active.as_ref().unwrap();
    let challenge = active
        .snapshot
        .task
        .status
        .message
        .as_ref()
        .context("challenge")?;
    assert!(serde_json::to_string(&challenge.parts)?.contains("not supported by this client"));
    let response =
        t.b.rpc("alice", "SendMessage", reply(&before, "yes"))
            .await?;
    assert_eq!(
        result_task(&response)?["status"]["state"],
        "TASK_STATE_INPUT_REQUIRED",
        "{response}"
    );
    assert_eq!(pending(&t, &session).await?.request.kind, "future-kind");
    let after =
        t.h.store
            .read_context(session.storage_key())
            .await?
            .unwrap();
    assert_eq!(
        after.revision, before.revision,
        "unsupported approval must not persist intent"
    );
    let response =
        t.b.rpc(
            "alice",
            "SendMessage",
            reply(&before, "no unsupported kind"),
        )
        .await?;
    assert_eq!(
        result_task(&response)?["status"]["state"],
        "TASK_STATE_INPUT_REQUIRED",
        "{response}"
    );
    let entries = log.load_events_latest_required_async().await?;
    assert!(entries.iter().any(|(_, e)| matches!(e, SessionLogEntry::InteractionResponse { request_seq, response, .. } if *request_seq == seq && response.action == "deny")));
    assert!(!t.h.config_dir().join("executions").exists());
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recovery_interaction_conflicting_answer_keeps_first_durable_decision() -> Result<()> {
    use harnx_runtime::fault_injection::{pause_point, RecoveryFaultPoint};
    let (t, session, before) = start_ready(Script::ConfirmTool).await?;
    let mut intent =
        t.a.backend
            .runner
            .fault_hooks()
            .pause(Boundary::HitlReplyIntentCommitted, 1);
    let mut delivered =
        t.a.backend
            .runner
            .fault_hooks()
            .pause(Boundary::HitlDecisionCommitted, 1);
    let send = send_yes(&t, &before);
    tokio::time::timeout(DEADLINE, intent.reached()).await?;
    let mut accepted = pause_point(
        session.storage_key(),
        RecoveryFaultPoint::AcceptedResponseAfter,
    );
    let request = pending(&t, &session).await?;
    let external = tokio::spawn({
        let session = session.clone();
        async move {
            session
                .decide_interaction(
                    &request.interaction_id,
                    request.request_seq,
                    &InteractionResponse {
                        action: "deny".into(),
                        data: Default::default(),
                        note: None,
                    },
                )
                .await
        }
    });
    accepted.wait_reached().await?;
    drop(intent);
    tokio::time::timeout(DEADLINE, delivered.reached()).await?;
    accepted.release();
    assert!(external.await??.is_applied());
    drop(delivered);
    let response = send.await??;
    assert_eq!(
        result_task(&response)?["status"]["state"],
        "TASK_STATE_COMPLETED",
        "{response}"
    );
    let entries = NatsSessionLog::new(t.h.jetstream.clone(), session.storage_key())
        .load_events_latest_required_async()
        .await?;
    let decisions: Vec<_> = entries
        .iter()
        .filter_map(|(_, e)| match e {
            SessionLogEntry::InteractionResponse { response, .. } => Some(response.action.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(decisions, ["deny"]);
    assert!(!t.h.config_dir().join("executions").exists());
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recovery_interaction_stale_bound_reply_cannot_answer_next_request() -> Result<()> {
    let (t, session, before) = start_ready(Script::ConfirmSerial).await?;
    let request = pending(&t, &session).await?;
    let mut bound =
        t.a.backend
            .runner
            .fault_hooks()
            .pause(Boundary::HitlReplyBound, 1);
    let send = send_yes(&t, &before);
    tokio::time::timeout(DEADLINE, bound.reached()).await?;
    assert!(session
        .decide_interaction(
            &request.interaction_id,
            request.request_seq,
            &InteractionResponse {
                action: "approve".into(),
                data: Default::default(),
                note: None,
            }
        )
        .await?
        .is_applied());
    let next = tokio::time::timeout(DEADLINE, async {
        loop {
            if let Ok(next) = pending(&t, &session).await {
                if next.interaction_id != request.interaction_id {
                    return next;
                }
            }
            tokio::task::yield_now().await;
        }
    })
    .await?;
    drop(bound);
    let response = send.await??;
    assert_eq!(
        result_task(&response)?["status"]["state"],
        "TASK_STATE_INPUT_REQUIRED",
        "{response}"
    );
    assert_eq!(pending(&t, &session).await?, next);
    let entries = NatsSessionLog::new(t.h.jetstream.clone(), session.storage_key())
        .load_events_latest_required_async()
        .await?;
    assert_eq!(
        entries
            .iter()
            .filter(|(_, e)| matches!(e, SessionLogEntry::InteractionResponse { .. }))
            .count(),
        1
    );
    assert!(!t.h.config_dir().join("executions").exists());
    Ok(())
}
