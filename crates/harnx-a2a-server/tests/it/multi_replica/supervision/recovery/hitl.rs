use super::*;
use harnx_a2a_server::store::context::{ContextSnapshot, HitlPhase};
use harnx_runtime::{nats_session::fixed_admission::FixedAdmissionTicket, PromptProgress};

async fn parked(t: &TwoBackends, storage: &str, request_after: u64) -> Result<ContextSnapshot> {
    tokio::time::timeout(DEADLINE, async {
        loop {
            let context = t.h.store.read_context(storage).await?.context("context")?;
            let active = context.document.state.active.as_ref().context("active")?;
            if context.document.owner.is_none()
                && active.publication.pending.is_none()
                && active.snapshot.task.status.state == TaskState::InputRequired
                && active
                    .hitl_checkpoint
                    .as_ref()
                    .is_some_and(|c| c.request.request_seq > request_after)
            {
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recovery_hitl_restart_parked_reinspects_without_claim_and_reply_on_new_replica(
) -> Result<()> {
    let t = TwoBackends::start_script(Script::Tool).await?;
    let (session, before) = start_parked(&t).await?;
    t.a.backend.runner.shutdown().await;
    let mut inspected =
        t.b.backend
            .runner
            .fault_hooks()
            .pause(Boundary::HitlParkedInspected, 2);
    supervise(&t, &t.b);
    tokio::time::timeout(DEADLINE, inspected.reached()).await?;
    let unchanged =
        t.h.store
            .read_context(session.storage_key())
            .await?
            .context("context")?;
    assert_eq!(
        unchanged.revision, before.revision,
        "read-only sweeps must not claim parked work"
    );
    assert_eq!(
        t.b.backend.runner.fault_hooks().count(Boundary::ContextCas),
        0
    );
    drop(inspected);
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

async fn recover_request_before_status(foreground: bool) -> Result<()> {
    let t = TwoBackends::start_script(Script::Tool).await?;
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
    remove_owner_lease(&t, session.storage_key()).await?;
    if foreground {
        let response =
            t.b.rpc("alice", "GetTask", json!({"id":task["id"]}))
                .await?;
        assert_eq!(
            response["result"]["status"]["state"], "TASK_STATE_INPUT_REQUIRED",
            "{response}"
        );
    } else {
        supervise(&t, &t.b);
    }
    let recovered = parked(&t, session.storage_key(), 0).await?;
    let active = recovered.document.state.active.as_ref().unwrap();
    assert!(!active.stop_confirmed);
    assert!(session
        .fixed_prompt_completion(&ticket(&before, session.storage_key())?)
        .await?
        .is_none());
    assert_original(&t, &session, &before).await?;
    drop(paused);
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
    .await?;
    t.a.backend.runner.shutdown().await;
    let response =
        t.b.rpc("alice", "SendMessage", reply(&recovered, "y"))
            .await?;
    assert_eq!(
        result_task(&response)?["status"]["state"],
        "TASK_STATE_COMPLETED"
    );
    t.b.backend.abort.set_ctrlc();
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recovery_hitl_accepted_intent_repairs_dedupe_and_delivers_full_saved_rejection(
) -> Result<()> {
    let t = TwoBackends::start_script(Script::Tool).await?;
    let (session, before) = start_parked(&t).await?;
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
    remove_owner_lease(&t, session.storage_key()).await?;
    supervise(&t, &t.b);
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
    assert_eq!(decisions, vec![(false, Some(note.clone()))]);
    assert!(
        serde_json::to_string(t.h.llm.requests.lock().last().unwrap())?.contains("FULL-REASON-END")
    );
    assert_original(&t, &session, &before).await?;
    drop(paused);
    let _ = send.await?;
    t.a.backend.runner.shutdown().await;
    t.b.backend.abort.set_ctrlc();
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recovery_hitl_committed_decision_without_activation_resumes_original_ticket() -> Result<()>
{
    let t = TwoBackends::start_script(Script::Tool).await?;
    let (session, before) = start_parked(&t).await?;
    let request = &before
        .document
        .state
        .active
        .as_ref()
        .unwrap()
        .hitl_checkpoint
        .as_ref()
        .unwrap()
        .request;
    let frozen =
        t.h.metadata
            .get_invocation_limits(session.storage_key(), &request.invocation_id)
            .await?
            .context("frozen limits")?;
    // Model the durable worker commit before activation/follower attachment.
    NatsSessionLog::new(t.h.jetstream.clone(), session.storage_key())
        .append_event_async(&SessionLogEntry::HitlApprovalDecision {
            tool_call_id: request.tool_call_id.clone(),
            approved: true,
            note: Some("saved audit".into()),
            fence_token: 1,
        })
        .await?;
    assert!(matches!(
        session
            .fixed_prompt_progress(&ticket(&before, session.storage_key())?)
            .await?,
        PromptProgress::Running {
            last_decision: Some(_)
        }
    ));
    t.a.backend.runner.shutdown().await;
    supervise(&t, &t.b);
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
    assert_eq!(t.h.llm.requests.lock().len(), 1);
    assert_eq!(
        t.h.metadata
            .get_invocation_limits(session.storage_key(), &request.invocation_id)
            .await?,
        Some(frozen)
    );
    t.b.backend.abort.set_ctrlc();
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recovery_hitl_external_runtime_decision_is_picked_up_without_http() -> Result<()> {
    let t = TwoBackends::start_script(Script::Tool).await?;
    let (session, before) = start_parked(&t).await?;
    let request = &before
        .document
        .state
        .active
        .as_ref()
        .unwrap()
        .hitl_checkpoint
        .as_ref()
        .unwrap()
        .request;
    let mut inspected =
        t.b.backend
            .runner
            .fault_hooks()
            .pause(Boundary::HitlParkedInspected, 1);
    supervise(&t, &t.b);
    tokio::time::timeout(DEADLINE, inspected.reached()).await?;
    assert!(
        session
            .decide_hitl_approval(
                &request.tool_call_id,
                false,
                Some("external rejection".into())
            )
            .await?
    );
    tokio::time::timeout(DEADLINE, async {
        while session
            .fixed_prompt_completion(&ticket(&before, session.storage_key())?)
            .await?
            .is_none()
        {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        Ok::<_, anyhow::Error>(())
    })
    .await??;
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recovery_hitl_decision_before_attach_parks_next_request_never_replays_old_reply(
) -> Result<()> {
    let t = TwoBackends::start_script(Script::SerialTool).await?;
    let (session, before) = start_parked(&t).await?;
    let q1 = before
        .document
        .state
        .active
        .as_ref()
        .unwrap()
        .hitl_checkpoint
        .as_ref()
        .unwrap()
        .request
        .clone();
    let mut paused =
        t.a.backend
            .runner
            .fault_hooks()
            .pause(Boundary::HitlDecisionCommitted, 1);
    let send = tokio::spawn({
        let app = t.a.app.clone();
        let message = reply(&before, "n first tool rejected");
        async move { rpc(app, "alice", "SendMessage", message).await }
    });
    tokio::time::timeout(DEADLINE, paused.reached()).await?;
    tokio::time::timeout(DEADLINE, async {
        loop {
            if matches!(session.fixed_prompt_progress(&ticket(&before, session.storage_key())?).await?, PromptProgress::InputRequired(p) if p.request_seq > q1.request_seq) { return Ok::<_, anyhow::Error>(()); }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }).await??;
    remove_owner_lease(&t, session.storage_key()).await?;
    supervise(&t, &t.b);
    let next = parked(&t, session.storage_key(), q1.request_seq).await?;
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
    drop(paused);
    let _ = send.await?;
    t.a.backend.runner.shutdown().await;
    let mut next_reply = reply(&next, "y");
    next_reply["message"]["messageId"] = json!("second-reply");
    let result = t.b.rpc("alice", "SendMessage", next_reply).await?;
    assert_eq!(
        result_task(&result)?["status"]["state"],
        "TASK_STATE_COMPLETED"
    );
    assert_original(&t, &session, &before).await?;
    t.b.backend.abort.set_ctrlc();
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recovery_hitl_failed_challenge_publish_retains_stable_outbox_for_takeover() -> Result<()> {
    let t = TwoBackends::start_script(Script::Tool).await?;
    let mut original =
        t.a.backend
            .runner
            .fault_hooks()
            .pause(Boundary::HitlRequestObserved, 1);
    let response = t.a.rpc("alice", "SendMessage", first_message()).await?;
    tokio::time::timeout(DEADLINE, original.reached()).await?;
    let task = result_task(&response)?;
    let session = session_for(&t.b, &t.h, task["contextId"].as_str().unwrap()).await?;
    remove_owner_lease(&t, session.storage_key()).await?;
    t.b.backend
        .store
        .context_fault_hooks()
        .fail_next_event_publish();
    let mut deferred =
        t.b.backend
            .runner
            .fault_hooks()
            .pause(Boundary::HitlRecoveryDeferred, 1);
    supervise(&t, &t.b);
    tokio::time::timeout(DEADLINE, deferred.reached()).await?;
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
    remove_owner_lease(&t, session.storage_key()).await?;
    drop(deferred);
    let recovered = parked(&t, session.storage_key(), 0).await?;
    let active = recovered.document.state.active.as_ref().unwrap();
    assert_eq!(active.snapshot.stream_seq, envelope.task_sequence);
    assert!(active.publication.subject_sequence > 0);
    assert!(session
        .fixed_prompt_completion(&ticket(&recovered, session.storage_key())?)
        .await?
        .is_none());
    drop(original);
    // Let the stale owner unwind without issuing a graceful shutdown interrupt.
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
    .await?;
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recovery_hitl_inconsistent_saved_history_leaves_exact_intent_for_retry() -> Result<()> {
    let t = TwoBackends::start_script(Script::Tool).await?;
    let (session, before) = start_parked(&t).await?;
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
                session.storage_key(),
                &intent.version()?,
                "remove-saved-history",
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
    remove_owner_lease(&t, session.storage_key()).await?;
    let mut deferred =
        t.b.backend
            .runner
            .fault_hooks()
            .pause(Boundary::HitlRecoveryDeferred, 1);
    supervise(&t, &t.b);
    tokio::time::timeout(DEADLINE, deferred.reached()).await?;
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
    // Restore only the missing audit history. Original exact receipt remains.
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
    let update =
        t.h.store
            .prepare_context_update_checked(
                session.storage_key(),
                &owned.version()?,
                "restore-saved-history",
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
    let release =
        t.h.store
            .prepare_context_release(session.storage_key(), &owned.version()?, "restore-release")
            .await?;
    t.h.store.commit_context(&release).await?;
    lease.release().await?;
    drop(deferred);
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
    drop(paused);
    let _ = send.await?;
    t.a.backend.runner.shutdown().await;
    t.b.backend.abort.set_ctrlc();
    Ok(())
}
