use super::*;
use harnx_a2a_server::fault_injection::Pause;
use harnx_a2a_server::store::context::{ContextSnapshot, HitlPhase};
use harnx_runtime::ApprovalRef;
use harnx_runtime::{nats_session::fixed_admission::FixedAdmissionTicket, PromptProgress};

mod regressions;
mod support;

use crate::multi_replica::hitl_state::is_parked_waiting;

async fn parked(t: &TwoBackends, storage: &str, request_after: u64) -> Result<ContextSnapshot> {
    tokio::time::timeout(DEADLINE, async {
        loop {
            let context = t.h.store.read_context(storage).await?.context("context")?;
            if is_parked_waiting(&context, request_after) {
                return Ok(context);
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .context("park deadline")?
}

async fn start_parked(t: &TwoBackends) -> Result<(NatsSession, ContextSnapshot)> {
    let response = t.a.rpc("alice", "SendMessage", first_message()).await?;
    let task = result_task(&response)?;
    let session = session_for(
        &t.b,
        &t.h,
        task["contextId"].as_str().context("context id")?,
    )
    .await?;
    let context = parked(t, session.storage_key(), 0).await?;
    Ok((session, context))
}

fn ticket(context: &ContextSnapshot, storage: &str) -> Result<FixedAdmissionTicket> {
    let admission = &context
        .document
        .state
        .active
        .as_ref()
        .context("active")?
        .admission;
    FixedAdmissionTicket::from_parts(
        storage.into(),
        admission.invocation_id.clone(),
        admission.prompt_id.clone(),
        admission.closure_id.clone().context("closure")?,
        admission.fixed_predecessor,
    )
}

fn reply(context: &ContextSnapshot, text: &str) -> Value {
    let task = &context
        .document
        .state
        .active
        .as_ref()
        .unwrap()
        .snapshot
        .task;
    json!({"message":{"messageId":"recovery-reply","role":"ROLE_USER","contextId":task.context_id,"taskId":task.id,"parts":[{"text":text}]}})
}

async fn assert_original(
    t: &TwoBackends,
    session: &NatsSession,
    before: &ContextSnapshot,
) -> Result<()> {
    let after =
        t.h.store
            .read_context(session.storage_key())
            .await?
            .context("context")?;
    let before = before.document.state.active.as_ref().unwrap();
    let after = after.document.state.active.as_ref().unwrap();
    assert_eq!(before.snapshot.task.id, after.snapshot.task.id);
    assert_eq!(
        before.snapshot.task.context_id,
        after.snapshot.task.context_id
    );
    assert_eq!(before.message, after.message);
    if after.snapshot.task.status.state == TaskState::Completed {
        let text: String = after.snapshot.task.artifacts.as_ref().unwrap()[0]
            .parts
            .iter()
            .filter_map(|part| match &part.content {
                a2a_lf::PartContent::Text(text) => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert!(
            text.starts_with("Before approval. "),
            "lost prior segment: {text}"
        );
    }
    assert_eq!(
        before.admission.invocation_id,
        after.admission.invocation_id
    );
    assert_eq!(before.admission.prompt_id, after.admission.prompt_id);
    assert_eq!(before.admission.closure_id, after.admission.closure_id);
    assert_eq!(
        before.admission.fixed_predecessor,
        after.admission.fixed_predecessor
    );
    assert_eq!(
        before.admission.prompt_sequence,
        after.admission.prompt_sequence
    );
    assert_eq!(
        prompts(&NatsSessionLog::new(
            t.h.jetstream.clone(),
            session.storage_key()
        ))
        .await?
        .len(),
        1
    );
    Ok(())
}

async fn await_owner_unbusy(t: &TwoBackends, session: &NatsSession) -> Result<()> {
    tokio::time::timeout(DEADLINE, async {
        while t
            .a
            .backend
            .runner
            .is_busy(&ContextKey::new(&t.h.export, session.session_id()))
            .await
        {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .context("owner unbusy deadline")
}

async fn assert_readonly_inspection(
    t: &TwoBackends,
    session: &NatsSession,
    before_rev: u64,
) -> Result<()> {
    let mut inspected =
        t.b.backend
            .runner
            .fault_hooks()
            .pause(Boundary::HitlParkedInspected, 2);
    supervise(t, &t.b);
    tokio::time::timeout(DEADLINE, inspected.reached()).await?;
    let unchanged =
        t.h.store
            .read_context(session.storage_key())
            .await?
            .context("context")?;
    assert_eq!(
        unchanged.revision, before_rev,
        "read-only sweeps must not claim parked work"
    );
    assert_eq!(
        t.b.backend.runner.fault_hooks().count(Boundary::ContextCas),
        0
    );
    Ok(())
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recovery_hitl_restart_parked_reinspects_without_claim_and_reply_on_new_replica(
) -> Result<()> {
    let t = TwoBackends::start_script(Script::Tool).await?;
    let (session, before) = start_parked(&t).await?;
    t.a.backend.runner.shutdown().await;
    assert_readonly_inspection(&t, &session, before.revision).await?;
    let response = t.b.rpc("alice", "SendMessage", reply(&before, "y")).await?;
    assert_eq!(
        result_task(&response)?["status"]["state"],
        "TASK_STATE_COMPLETED"
    );
    assert_original(&t, &session, &before).await?;
    t.b.backend.abort.set_ctrlc();
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recovery_hitl_request_durable_before_working_status_cas_reconstructs_challenge(
) -> Result<()> {
    recover_request_before_status(false).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recovery_hitl_foreground_get_repairs_request_before_status() -> Result<()> {
    recover_request_before_status(true).await
}

struct UncheckpointedSetup {
    task: Value,
    session: NatsSession,
    before: ContextSnapshot,
    paused: Pause,
}

async fn setup_uncheckpointed_request(t: &TwoBackends) -> Result<UncheckpointedSetup> {
    let mut paused =
        t.a.backend
            .runner
            .fault_hooks()
            .pause(Boundary::HitlRequestObserved, 1);
    let first = t.a.rpc("alice", "SendMessage", first_message()).await?;
    tokio::time::timeout(DEADLINE, paused.reached()).await?;
    let task = result_task(&first)?;
    let session = session_for(&t.b, &t.h, task["contextId"].as_str().unwrap()).await?;
    let before =
        t.h.store
            .read_context(session.storage_key())
            .await?
            .context("context")?;
    support::assert_uncheckpointed_request(&before);
    remove_owner_lease(t, session.storage_key()).await?;
    Ok(UncheckpointedSetup {
        task: task.clone(),
        session,
        before,
        paused,
    })
}

async fn trigger_and_verify_recovered_request(
    t: &TwoBackends,
    setup: &UncheckpointedSetup,
    foreground: bool,
) -> Result<ContextSnapshot> {
    if foreground {
        let response =
            t.b.rpc("alice", "GetTask", json!({"id": setup.task["id"]}))
                .await?;
        assert_eq!(
            response["result"]["status"]["state"], "TASK_STATE_INPUT_REQUIRED",
            "{response}"
        );
    } else {
        supervise(t, &t.b);
    }
    let recovered = parked(t, setup.session.storage_key(), 0).await?;
    let active = recovered.document.state.active.as_ref().unwrap();
    assert!(!active.stop_confirmed);
    assert!(setup
        .session
        .fixed_prompt_completion(&ticket(&setup.before, setup.session.storage_key())?)
        .await?
        .is_none());
    assert_original(t, &setup.session, &setup.before).await?;
    Ok(recovered)
}

async fn complete_recovered_reply(
    t: &TwoBackends,
    session: &NatsSession,
    recovered: &ContextSnapshot,
) -> Result<()> {
    await_owner_unbusy(t, session).await?;
    t.a.backend.runner.shutdown().await;
    let response =
        t.b.rpc("alice", "SendMessage", reply(recovered, "y"))
            .await?;
    assert_eq!(
        result_task(&response)?["status"]["state"],
        "TASK_STATE_COMPLETED"
    );
    t.b.backend.abort.set_ctrlc();
    Ok(())
}

async fn recover_request_before_status(foreground: bool) -> Result<()> {
    let t = TwoBackends::start_script(Script::Tool).await?;
    let setup = setup_uncheckpointed_request(&t).await?;
    let recovered = trigger_and_verify_recovered_request(&t, &setup, foreground).await?;
    drop(setup.paused);
    complete_recovered_reply(&t, &setup.session, &recovered).await
}

struct PausedIntentSetup {
    session: NatsSession,
    before: ContextSnapshot,
    note: String,
    paused: Pause,
    send: tokio::task::JoinHandle<Result<Value>>,
}

async fn start_paused_reply_intent(t: &TwoBackends) -> Result<PausedIntentSetup> {
    let (session, before) = start_parked(t).await?;
    let note = format!("{} FULL-REASON-END", "é".repeat(3000));
    let mut paused =
        t.a.backend
            .runner
            .fault_hooks()
            .pause(Boundary::HitlReplyIntentCommitted, 1);
    let send = tokio::spawn({
        let app = t.a.app.clone();
        let message = reply(&before, &format!("n {note}"));
        async move { rpc(app, "alice", "SendMessage", message).await }
    });
    tokio::time::timeout(DEADLINE, paused.reached()).await?;
    assert!(t
        .h
        .store
        .get_reply_dedupe(session.storage_key(), "recovery-reply")
        .await?
        .is_none());
    Ok(PausedIntentSetup {
        session,
        before,
        note,
        paused,
        send,
    })
}

async fn assert_recovered_intent_decision(
    t: &TwoBackends,
    session: &NatsSession,
    note: &str,
) -> Result<()> {
    assert!(t
        .h
        .store
        .get_reply_dedupe(session.storage_key(), "recovery-reply")
        .await?
        .is_some());
    let entries = NatsSessionLog::new(t.h.jetstream.clone(), session.storage_key())
        .load_events_latest_required_async()
        .await?;
    let decisions: Vec<_> = entries
        .iter()
        .filter_map(|(_, e)| match e {
            SessionLogEntry::HitlApprovalDecision { approved, note, .. } => {
                Some((*approved, note.clone()))
            }
            _ => None,
        })
        .collect();
    assert_eq!(decisions, vec![(false, Some(note.to_owned()))]);
    assert!(
        serde_json::to_string(t.h.llm.requests.lock().last().unwrap())?.contains("FULL-REASON-END")
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recovery_hitl_accepted_intent_repairs_dedupe_and_delivers_full_saved_rejection(
) -> Result<()> {
    let t = TwoBackends::start_script(Script::Tool).await?;
    let setup = start_paused_reply_intent(&t).await?;
    remove_owner_lease(&t, setup.session.storage_key()).await?;
    supervise(&t, &t.b);
    let id = &setup
        .before
        .document
        .state
        .active
        .as_ref()
        .unwrap()
        .snapshot
        .task
        .id;
    assert_eq!(
        terminal(&t, id).await?.task.status.state,
        TaskState::Completed
    );
    assert_recovered_intent_decision(&t, &setup.session, &setup.note).await?;
    assert_original(&t, &setup.session, &setup.before).await?;
    drop(setup.paused);
    let _ = setup.send.await?;
    t.a.backend.runner.shutdown().await;
    t.b.backend.abort.set_ctrlc();
    Ok(())
}

async fn append_and_verify_running_decision(
    t: &TwoBackends,
    session: &NatsSession,
    before: &ContextSnapshot,
    request: &ApprovalRef,
) -> Result<()> {
    NatsSessionLog::new(t.h.jetstream.clone(), session.storage_key())
        .append_event_async(&SessionLogEntry::HitlApprovalDecision {
            tool_call_id: request.tool_call_id.clone(),
            approved: true,
            note: Some("saved audit".into()),
            fence_token: 1,
        })
        .await?;
    let progress = session
        .fixed_prompt_progress(&ticket(before, session.storage_key())?)
        .await?;
    assert!(matches!(
        progress,
        PromptProgress::Running {
            last_decision: Some(_)
        }
    ));
    Ok(())
}

async fn assert_committed_decision_supervision(
    t: &TwoBackends,
    session: &NatsSession,
    before: &ContextSnapshot,
    frozen: &harnx_runtime::nats_session_metadata::RunLimitsRecord,
) -> Result<()> {
    let active = before.document.state.active.as_ref().unwrap();
    let request = &active.hitl_checkpoint.as_ref().unwrap().request;
    let id = &active.snapshot.task.id;
    assert_eq!(
        terminal(t, id).await?.task.status.state,
        TaskState::Completed
    );
    assert_original(t, session, before).await?;
    assert_eq!(t.h.llm.requests.lock().len(), 1);
    assert_eq!(
        t.h.metadata
            .get_invocation_limits(session.storage_key(), &request.invocation_id)
            .await?,
        Some(frozen.clone())
    );
    t.b.backend.abort.set_ctrlc();
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recovery_hitl_committed_decision_without_activation_resumes_original_ticket() -> Result<()>
{
    let t = TwoBackends::start_script(Script::Tool).await?;
    let (session, before) = start_parked(&t).await?;
    let active = before.document.state.active.as_ref().unwrap();
    let request = &active.hitl_checkpoint.as_ref().unwrap().request;
    let frozen =
        t.h.metadata
            .get_invocation_limits(session.storage_key(), &request.invocation_id)
            .await?
            .context("frozen limits")?;
    append_and_verify_running_decision(&t, &session, &before, request).await?;
    t.a.backend.runner.shutdown().await;
    supervise(&t, &t.b);
    assert_committed_decision_supervision(&t, &session, &before, &frozen).await?;
    Ok(())
}

async fn apply_external_decision_and_wait(
    session: &NatsSession,
    before: &ContextSnapshot,
    tool_call_id: &str,
) -> Result<()> {
    assert!(
        session
            .decide_hitl_approval(tool_call_id, false, Some("external rejection".into()))
            .await?
    );
    tokio::time::timeout(DEADLINE, async {
        while session
            .fixed_prompt_completion(&ticket(before, session.storage_key())?)
            .await?
            .is_none()
        {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        Ok::<_, anyhow::Error>(())
    })
    .await?
}

async fn wait_inspected_pause(t: &TwoBackends) -> Result<Pause> {
    let mut inspected =
        t.b.backend
            .runner
            .fault_hooks()
            .pause(Boundary::HitlParkedInspected, 1);
    supervise(t, &t.b);
    tokio::time::timeout(DEADLINE, inspected.reached()).await?;
    Ok(inspected)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recovery_hitl_external_runtime_decision_is_picked_up_without_http() -> Result<()> {
    let t = TwoBackends::start_script(Script::Tool).await?;
    let (session, before) = start_parked(&t).await?;
    let active = before.document.state.active.as_ref().unwrap();
    let request = &active.hitl_checkpoint.as_ref().unwrap().request;
    let inspected = wait_inspected_pause(&t).await?;
    apply_external_decision_and_wait(&session, &before, &request.tool_call_id).await?;
    drop(inspected);
    let id = &before
        .document
        .state
        .active
        .as_ref()
        .unwrap()
        .snapshot
        .task
        .id;
    assert_eq!(
        terminal(&t, id).await?.task.status.state,
        TaskState::Completed
    );
    assert_original(&t, &session, &before).await?;
    t.b.backend.abort.set_ctrlc();
    Ok(())
}

async fn wait_for_next_progress_request(
    session: &NatsSession,
    before: &ContextSnapshot,
    q1_seq: u64,
) -> Result<()> {
    tokio::time::timeout(DEADLINE, async {
        loop {
            if matches!(
                session.fixed_prompt_progress(&ticket(before, session.storage_key())?).await?,
                PromptProgress::InputRequired(p) if p.request_seq > q1_seq
            ) {
                return Ok::<_, anyhow::Error>(());
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await?
}

struct PausedDecisionSend {
    paused: Pause,
    send: tokio::task::JoinHandle<Result<Value>>,
}

async fn pause_and_send_rejection(
    t: &TwoBackends,
    before: &ContextSnapshot,
) -> Result<PausedDecisionSend> {
    let mut paused =
        t.a.backend
            .runner
            .fault_hooks()
            .pause(Boundary::HitlDecisionCommitted, 1);
    let send = tokio::spawn({
        let app = t.a.app.clone();
        let message = reply(before, "n first tool rejected");
        async move { rpc(app, "alice", "SendMessage", message).await }
    });
    tokio::time::timeout(DEADLINE, paused.reached()).await?;
    Ok(PausedDecisionSend { paused, send })
}

async fn await_next_parked_request(
    t: &TwoBackends,
    session: &NatsSession,
    q1_seq: u64,
) -> Result<ContextSnapshot> {
    remove_owner_lease(t, session.storage_key()).await?;
    supervise(t, &t.b);
    let next = parked(t, session.storage_key(), q1_seq).await?;
    support::assert_next_request_parked(t, session, &next).await?;
    Ok(next)
}
async fn complete_next_parked_reply(
    t: &TwoBackends,
    session: &NatsSession,
    before: &ContextSnapshot,
    next: &ContextSnapshot,
) -> Result<()> {
    let mut next_reply = reply(next, "y");
    next_reply["message"]["messageId"] = json!("second-reply");
    let result = t.b.rpc("alice", "SendMessage", next_reply).await?;
    assert_eq!(
        result_task(&result)?["status"]["state"],
        "TASK_STATE_COMPLETED"
    );
    assert_original(t, session, before).await?;
    t.b.backend.abort.set_ctrlc();
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recovery_hitl_decision_before_attach_parks_next_request_never_replays_old_reply(
) -> Result<()> {
    let t = TwoBackends::start_script(Script::SerialTool).await?;
    let (session, before) = start_parked(&t).await?;
    let active = before.document.state.active.as_ref().unwrap();
    let q1 = active.hitl_checkpoint.as_ref().unwrap().request.clone();
    let dec = pause_and_send_rejection(&t, &before).await?;
    wait_for_next_progress_request(&session, &before, q1.request_seq).await?;
    let next = await_next_parked_request(&t, &session, q1.request_seq).await?;
    drop(dec.paused);
    let _ = dec.send.await?;
    t.a.backend.runner.shutdown().await;
    complete_next_parked_reply(&t, &session, &before, &next).await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recovery_hitl_background_cancel_uses_existing_scoped_stop() -> Result<()> {
    let t = TwoBackends::start_script(Script::Tool).await?;
    let (session, before) = start_parked(&t).await?;
    let id = &before
        .document
        .state
        .active
        .as_ref()
        .unwrap()
        .snapshot
        .task
        .id;
    t.h.store
        .request_task_cancel(session.storage_key(), id)
        .await?;
    supervise(&t, &t.b);
    assert_eq!(
        terminal(&t, id).await?.task.status.state,
        TaskState::Canceled
    );
    assert!(session
        .fixed_prompt_completion(&ticket(&before, session.storage_key())?)
        .await?
        .is_some());
    assert_original(&t, &session, &before).await?;
    t.b.backend.abort.set_ctrlc();
    Ok(())
}

struct DeferredRecoverySetup {
    session: NatsSession,
    original: Pause,
    envelope: harnx_a2a_server::store::context::PendingEvent,
}

async fn trigger_observed_request(
    t: &TwoBackends,
    original: &mut Pause,
) -> Result<(Value, NatsSession)> {
    let response = t.a.rpc("alice", "SendMessage", first_message()).await?;
    tokio::time::timeout(DEADLINE, original.reached()).await?;
    let task = result_task(&response)?;
    let session = session_for(&t.b, &t.h, task["contextId"].as_str().unwrap()).await?;
    Ok((task.clone(), session))
}
async fn trigger_and_defer_challenge_publish(t: &TwoBackends) -> Result<DeferredRecoverySetup> {
    let mut original =
        t.a.backend
            .runner
            .fault_hooks()
            .pause(Boundary::HitlRequestObserved, 1);
    let (_task, session) = trigger_observed_request(t, &mut original).await?;
    remove_owner_lease(t, session.storage_key()).await?;
    t.b.backend
        .store
        .context_fault_hooks()
        .fail_next_event_publish();
    let mut deferred =
        t.b.backend
            .runner
            .fault_hooks()
            .pause(Boundary::HitlRecoveryDeferred, 1);
    supervise(t, &t.b);
    tokio::time::timeout(DEADLINE, deferred.reached()).await?;
    let envelope = support::assert_retained_challenge_envelope(t, &session).await?;
    remove_owner_lease(t, session.storage_key()).await?;
    drop(deferred);
    Ok(DeferredRecoverySetup {
        session,
        original,
        envelope,
    })
}

async fn assert_recovered_challenge_state(
    t: &TwoBackends,
    session: &NatsSession,
    envelope: &harnx_a2a_server::store::context::PendingEvent,
) -> Result<ContextSnapshot> {
    let recovered = parked(t, session.storage_key(), 0).await?;
    let active = recovered.document.state.active.as_ref().unwrap();
    assert_eq!(active.snapshot.stream_seq, envelope.task_sequence);
    assert!(active.publication.subject_sequence > 0);
    assert!(session
        .fixed_prompt_completion(&ticket(&recovered, session.storage_key())?)
        .await?
        .is_none());
    Ok(recovered)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recovery_hitl_failed_challenge_publish_retains_stable_outbox_for_takeover() -> Result<()> {
    let t = TwoBackends::start_script(Script::Tool).await?;
    let setup = trigger_and_defer_challenge_publish(&t).await?;
    let recovered = assert_recovered_challenge_state(&t, &setup.session, &setup.envelope).await?;
    drop(setup.original);
    await_owner_unbusy(&t, &setup.session).await?;
    t.a.backend.runner.shutdown().await;
    let result =
        t.b.rpc("alice", "SendMessage", reply(&recovered, "y"))
            .await?;
    assert_eq!(
        result_task(&result)?["status"]["state"],
        "TASK_STATE_COMPLETED"
    );
    t.b.backend.abort.set_ctrlc();
    Ok(())
}

struct InconsistentHistorySetup {
    session: NatsSession,
    before: ContextSnapshot,
    saved: a2a_lf::Message,
    paused: Pause,
    send: tokio::task::JoinHandle<Result<Value>>,
}

async fn setup_inconsistent_saved_history(t: &TwoBackends) -> Result<InconsistentHistorySetup> {
    let (session, before) = start_parked(t).await?;
    let mut paused =
        t.a.backend
            .runner
            .fault_hooks()
            .pause(Boundary::HitlReplyIntentCommitted, 1);
    let send = tokio::spawn({
        let app = t.a.app.clone();
        let message = reply(&before, "n original reason");
        async move { rpc(app, "alice", "SendMessage", message).await }
    });
    tokio::time::timeout(DEADLINE, paused.reached()).await?;
    let saved = support::remove_saved_history(t, &session).await?;
    remove_owner_lease(t, session.storage_key()).await?;
    Ok(InconsistentHistorySetup {
        session,
        before,
        saved,
        paused,
        send,
    })
}

async fn defer_recovery_and_restore_history(
    t: &TwoBackends,
    session: &NatsSession,
    saved: a2a_lf::Message,
) -> Result<()> {
    let mut deferred =
        t.b.backend
            .runner
            .fault_hooks()
            .pause(Boundary::HitlRecoveryDeferred, 1);
    supervise(t, &t.b);
    tokio::time::timeout(DEADLINE, deferred.reached()).await?;
    support::assert_deferred_intent(t, session).await?;
    support::restore_saved_history(t, session, saved).await?;
    drop(deferred);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recovery_hitl_inconsistent_saved_history_leaves_exact_intent_for_retry() -> Result<()> {
    let t = TwoBackends::start_script(Script::Tool).await?;
    let setup = setup_inconsistent_saved_history(&t).await?;
    defer_recovery_and_restore_history(&t, &setup.session, setup.saved).await?;
    let id = &setup
        .before
        .document
        .state
        .active
        .as_ref()
        .unwrap()
        .snapshot
        .task
        .id;
    assert_eq!(
        terminal(&t, id).await?.task.status.state,
        TaskState::Completed
    );
    assert_original(&t, &setup.session, &setup.before).await?;
    drop(setup.paused);
    let _ = setup.send.await?;
    t.a.backend.runner.shutdown().await;
    t.b.backend.abort.set_ctrlc();
    Ok(())
}
