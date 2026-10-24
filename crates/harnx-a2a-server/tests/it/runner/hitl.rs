use super::*;
use harnx_a2a_server::store::context::{
    AdmissionPhase, ContextIdentity, ContextSnapshot, HitlPhase,
};
use harnx_runtime::{
    nats_lease::{NatsLeaseAcquireParams, NatsLeaseConfig, NatsSessionLease},
    nats_session::fixed_admission::FixedAdmissionTicket,
    HitlDecisionOutcome, NatsSession, PromptProgress,
};

async fn parked(h: &Harness, session: &NatsSession) -> Result<ContextSnapshot> {
    tokio::time::timeout(DEADLINE, async {
        loop {
            let context = h
                .store
                .read_context(session.storage_key())
                .await?
                .context("context")?;
            if context.document.owner.is_none()
                && !h
                    .runner
                    .is_busy(&ContextKey::new(&h.export, session.session_id()))
                    .await
            {
                return Ok(context);
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .context("park deadline")?
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

async fn challenge(
    h: &Harness,
    session: &NatsSession,
) -> Result<(harnx_a2a_server::runner::StartTurnResult, ContextSnapshot)> {
    let mut started = h.send(session, prompt()).await?;
    loop {
        let next = event(&mut started.events).await?;
        if next.ends_response() {
            assert!(!next.is_terminal());
            let StreamResponse::StatusUpdate(update) = next.response else {
                unreachable!()
            };
            assert_eq!(update.status.state, TaskState::InputRequired);
            let message = update.status.message.context("challenge message")?;
            assert_eq!(message.role, Role::Agent);
            assert!(message
                .parts
                .iter()
                .all(|part| matches!(part.content, PartContent::Text(_))));
            let challenge = text(&message.parts);
            for expected in [
                "target_session_handoff",
                "denied-target",
                "Reply `y`",
                "audit log",
                "reject",
            ] {
                assert!(challenge.contains(expected), "{challenge}");
            }
            break;
        }
    }
    let context = parked(h, session).await?;
    Ok((started, context))
}

async fn assert_suspended(
    h: &Harness,
    session: &NatsSession,
    context: &ContextSnapshot,
) -> Result<()> {
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
    let admission = h
        .metadata
        .active_admission(session.storage_key(), &entries)
        .await?
        .context("runtime admission must remain busy")?;
    assert_eq!(admission.invocation_id.as_str(), ticket.invocation_id());
    let mut other = admission;
    other.invocation_id = harnx_runtime::nats_session_metadata::InvocationIdentity::new();
    other.fixed_ticket = None;
    other.prompt_content = None;
    let error = h
        .metadata
        .reserve_admission(session.storage_key(), &other, &entries)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("session busy"), "{error:#}");
    assert_eq!(h.llm.requests.lock().len(), 1);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runner_hitl_tool_suspends_and_releases_owner_without_stopping_invocation() -> Result<()> {
    let h = Harness::start(Script::Tool).await?;
    let session = h.session(None, &alice()).await?;
    let (_, context) = challenge(&h, &session).await?;
    assert_suspended(&h, &session, &context).await
}

async fn following(h: &Harness, session: &NatsSession, context: &ContextSnapshot) -> Result<()> {
    let lease = NatsSessionLease::acquire_scoped(
        NatsLeaseAcquireParams {
            jetstream: h.jetstream.clone(),
            session_id: session.storage_key(),
            worker_id: "test-following".into(),
            generation: 1,
            config: NatsLeaseConfig {
                replicas: 1,
                ..Default::default()
            },
            session_metadata: None,
        },
        "a2a",
    )
    .await?
    .context("scoped lease")?;
    let write = h
        .store
        .prepare_context_claim(
            ContextIdentity {
                storage_key: session.storage_key(),
                local_id: session.session_id(),
            },
            &lease,
            "resume-claim",
        )
        .await?;
    let owned = h.store.commit_context(&write).await?;
    let request = &context
        .document
        .state
        .active
        .as_ref()
        .unwrap()
        .hitl_checkpoint
        .as_ref()
        .unwrap()
        .request;
    h.store
        .advance_hitl_to_following(
            session.storage_key(),
            &owned.version()?,
            request,
            &HitlDecisionOutcome::Applied,
        )
        .await?;
    let current = h.store.read_context(session.storage_key()).await?.unwrap();
    let write = h
        .store
        .prepare_context_release(session.storage_key(), &current.version()?, "resume-release")
        .await?;
    h.store.commit_context(&write).await?;
    lease.release().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runner_hitl_resume_recovers_output_written_before_attach_and_keeps_ticket() -> Result<()> {
    let h = Harness::start(Script::Tool).await?;
    let session = h.session(None, &alice()).await?;
    let (started, context) = challenge(&h, &session).await?;
    let ticket = ticket(&context, session.storage_key())?;
    let request = &context
        .document
        .state
        .active
        .as_ref()
        .unwrap()
        .hitl_checkpoint
        .as_ref()
        .unwrap()
        .request;
    let frozen = h
        .metadata
        .get_invocation_limits(session.storage_key(), ticket.invocation_id())
        .await?
        .context("frozen limits")?;
    assert_eq!(
        session
            .decide_hitl_approval_for(request, false, Some("not this handoff".into()))
            .await?,
        HitlDecisionOutcome::Applied
    );
    // Force terminal output to be durable before any resumed follower exists.
    tokio::time::timeout(DEADLINE, async {
        loop {
            if matches!(
                session.fixed_prompt_progress(&ticket).await?,
                PromptProgress::Terminal(_)
            ) {
                return Ok::<_, anyhow::Error>(());
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .context("runtime completion deadline")??;
    following(&h, &session, &context).await?;
    let mut resumed = h
        .runner
        .resume_hitl_turn(&h.export, &alice().into(), session.clone())
        .await?;
    assert_eq!(resumed.snapshot.task.id, started.snapshot.task.id);
    assert_eq!(resumed.snapshot.user_msg_seq, started.snapshot.user_msg_seq);
    assert_eq!(resumed.snapshot.execution_id, started.snapshot.execution_id);
    assert_eq!(
        text(&resumed.snapshot.task.artifacts.as_ref().unwrap()[0].parts),
        "Before approval. "
    );
    let mut assembled = "Before approval. ".to_owned();
    loop {
        let next = event(&mut resumed.events).await?;
        if let StreamResponse::ArtifactUpdate(update) = &next.response {
            assert_eq!(update.artifact.artifact_id, "answer");
            append_artifact(&mut assembled, update);
        }
        if next.is_terminal() {
            break;
        }
    }
    assert_eq!(assembled, "Before approval. Hello world");
    let final_task = h.task(&started.snapshot.task.id).await?;
    assert_eq!(final_task.task.status.state, TaskState::Completed);
    assert_eq!(
        text(&final_task.task.artifacts.as_ref().unwrap()[0].parts),
        assembled
    );
    assert_eq!(
        h.metadata
            .get_invocation_limits(session.storage_key(), ticket.invocation_id())
            .await?,
        Some(frozen)
    );
    let entries = NatsSessionLog::new_with_replicas(h.jetstream.clone(), session.storage_key(), 1)
        .load_events_latest_required_async()
        .await?;
    assert_eq!(entries.iter().filter(|(_, entry)| matches!(entry, SessionLogEntry::Message { role, content, .. } if role.is_user() && !harnx_core::timing_identity::is_timing_notice(&content.to_text()))).count(), 1);
    assert_eq!(h.llm.requests.lock().len(), 2);
    Ok(())
}

#[cfg(feature = "fault-injection")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runner_hitl_transient_outbox_failure_retries_same_challenge_without_failure() -> Result<()>
{
    use harnx_a2a_server::fault_injection::Boundary;
    let h = Harness::start(Script::Tool).await?;
    let session = h.session(None, &alice()).await?;
    let hooks = h.store.context_fault_hooks();
    let mut committed = hooks.pause(Boundary::HitlOutboxCommitted, 1);
    let mut started = h.send(&session, prompt()).await?;
    tokio::time::timeout(DEADLINE, committed.reached()).await?;
    let before = h.store.read_context(session.storage_key()).await?.unwrap();
    let pending = before
        .document
        .state
        .active
        .as_ref()
        .unwrap()
        .publication
        .pending
        .clone()
        .unwrap();
    assert!(pending.commit_id.ends_with(":input-required"));
    hooks.fail_next_event_publish();
    let mut retried = hooks.pause(Boundary::HitlOutboxCommitted, 2);
    drop(committed);
    tokio::time::timeout(DEADLINE, retried.reached()).await?;
    let during = h.store.read_context(session.storage_key()).await?.unwrap();
    assert_eq!(
        serde_json::to_value(
            during
                .document
                .state
                .active
                .as_ref()
                .unwrap()
                .publication
                .pending
                .as_ref()
        )?,
        serde_json::to_value(Some(&pending))?
    );
    assert!(
        !during
            .document
            .state
            .active
            .as_ref()
            .unwrap()
            .stop_confirmed
    );
    drop(retried);
    loop {
        if event(&mut started.events).await?.ends_response() {
            break;
        }
    }
    let context = parked(&h, &session).await?;
    assert_suspended(&h, &session, &context).await?;
    assert!(
        h.logs.text().contains("injected event publish failure"),
        "{}",
        h.logs.text()
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runner_hitl_resume_publishes_next_approval_already_pending_before_attach() -> Result<()> {
    let h = Harness::start(Script::SerialTool).await?;
    let session = h.session(None, &alice()).await?;
    let (started, context) = challenge(&h, &session).await?;
    let ticket = ticket(&context, session.storage_key())?;
    let first = context
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
    assert_eq!(
        session
            .decide_hitl_approval_for(&first, false, None)
            .await?,
        HitlDecisionOutcome::Applied
    );
    let second = tokio::time::timeout(DEADLINE, async {
        loop {
            match session.fixed_prompt_progress(&ticket).await? {
                PromptProgress::InputRequired(pending)
                    if pending.request_seq > first.request_seq =>
                {
                    return Ok::<_, anyhow::Error>(pending)
                }
                PromptProgress::Terminal(result) => anyhow::bail!(
                    "expected second approval, got terminal {result:?}; {}",
                    h.logs.text()
                ),
                _ => {}
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .context("second approval deadline")??;
    following(&h, &session, &context).await?;
    let mut resumed = h
        .runner
        .resume_hitl_turn(&h.export, &alice().into(), session.clone())
        .await?;
    loop {
        let next = event(&mut resumed.events).await?;
        if next.ends_response() {
            assert!(!next.is_terminal());
            break;
        }
    }
    let parked = parked(&h, &session).await?;
    let active = parked.document.state.active.as_ref().unwrap();
    assert_eq!(active.snapshot.task.id, started.snapshot.task.id);
    assert_eq!(
        active.admission,
        context.document.state.active.as_ref().unwrap().admission
    );
    let checkpoint = active.hitl_checkpoint.as_ref().unwrap();
    assert_eq!(
        checkpoint.request,
        second.approval_ref(ticket.invocation_id())
    );
    assert_eq!(checkpoint.phase, HitlPhase::Waiting);
    assert!(active.snapshot.stream_seq > started.snapshot.stream_seq);
    assert!(!active.stop_confirmed);
    assert!(active.publication.pending.is_none());
    assert_eq!(
        text(&active.snapshot.task.artifacts.as_ref().unwrap()[0].parts),
        "Before approval. Between approvals. "
    );
    assert_eq!(h.llm.requests.lock().len(), 2);
    Ok(())
}

#[cfg(feature = "fault-injection")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runner_hitl_bound_reply_never_retargets_a_newer_checkpoint() -> Result<()> {
    use crate::unary::{hitl::reply, Http};
    use harnx_a2a_server::fault_injection::Boundary;
    use serde_json::json;
    let h = Harness::start(Script::SerialTool).await?;
    let session = h.session(None, &alice()).await?;
    let (started, context) = challenge(&h, &session).await?;
    let http = Http::from_harness(h).await?;
    let task = serde_json::to_value(&started.snapshot.task)?;
    let hooks = http.h.runner.fault_hooks();
    let mut bound = hooks.pause(Boundary::HitlReplyBound, 1);
    let send = http.rpc(
        "SendMessage",
        json!({"message":reply("late-y", &task, "y", true)}),
    );
    tokio::pin!(send);
    tokio::select! {
        response = &mut send => anyhow::bail!("reply returned before binding: {response:?}"),
        reached = tokio::time::timeout(DEADLINE, bound.reached()) => { reached?; }
    }
    let request = &context
        .document
        .state
        .active
        .as_ref()
        .unwrap()
        .hitl_checkpoint
        .as_ref()
        .unwrap()
        .request;
    session
        .decide_hitl_approval_for(request, false, None)
        .await?;
    following(&http.h, &session, &context).await?;
    let mut resumed = http
        .h
        .runner
        .resume_hitl_turn(&http.h.export, &alice().into(), session.clone())
        .await?;
    while !event(&mut resumed.events).await?.ends_response() {}
    let newer = parked(&http.h, &session).await?;
    assert!(
        newer
            .document
            .state
            .active
            .as_ref()
            .unwrap()
            .hitl_checkpoint
            .as_ref()
            .unwrap()
            .request
            .request_seq
            > request.request_seq
    );
    drop(bound);
    let response = send.await?;
    crate::unary::code(&response, -32602);
    let entries =
        NatsSessionLog::new_with_replicas(http.h.jetstream.clone(), session.storage_key(), 1)
            .load_events_latest_required_async()
            .await?;
    assert_eq!(
        entries
            .iter()
            .filter(|(_, e)| matches!(e, SessionLogEntry::HitlApprovalDecision { .. }))
            .count(),
        1
    );
    let latest = http.h.task(&started.snapshot.task.id).await?;
    assert_eq!(latest.task.status.state, TaskState::InputRequired);
    assert!(!latest
        .task
        .history
        .as_ref()
        .unwrap()
        .iter()
        .any(|m| m.message_id == "late-y"));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runner_hitl_duplicate_finishes_saved_intent_without_new_admission() -> Result<()> {
    use crate::unary::{hitl::reply, Http};
    use harnx_a2a_server::store::{context::ReplyReceipt, message_fingerprint};
    use serde_json::json;
    let h = Harness::start(Script::Tool).await?;
    let session = h.session(None, &alice()).await?;
    let (started, context) = challenge(&h, &session).await?;
    let task = serde_json::to_value(&started.snapshot.task)?;
    let answer = reply("saved-no", &task, "n saved full reason", false);
    let message: a2a_lf::Message = serde_json::from_value(answer.clone())?;
    let request = context
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
    let lease = NatsSessionLease::acquire_scoped(
        NatsLeaseAcquireParams {
            jetstream: h.jetstream.clone(),
            session_id: session.storage_key(),
            worker_id: "seed-intent".into(),
            generation: 1,
            config: NatsLeaseConfig {
                replicas: 1,
                ..Default::default()
            },
            session_metadata: None,
        },
        "a2a",
    )
    .await?
    .context("lease")?;
    let write = h
        .store
        .prepare_context_claim(
            ContextIdentity {
                storage_key: session.storage_key(),
                local_id: session.session_id(),
            },
            &lease,
            "seed-claim",
        )
        .await?;
    let owned = h.store.commit_context(&write).await?;
    let write = h
        .store
        .prepare_context_update_checked(
            session.storage_key(),
            &owned.version()?,
            "seed-reply",
            |state| {
                let active = state.active.as_mut().unwrap();
                let checkpoint = active.hitl_checkpoint.as_mut().unwrap();
                checkpoint.phase = HitlPhase::DecisionIntent;
                checkpoint.reply = Some(ReplyReceipt::new(
                    message.message_id.clone(),
                    message_fingerprint(&message.parts),
                    request.clone(),
                    false,
                    Some("saved full reason".into()),
                ));
                active.snapshot.task.history.as_mut().unwrap().push(message);
                Ok(())
            },
        )
        .await?;
    let owned = h.store.commit_context(&write).await?;
    let write = h
        .store
        .prepare_context_release(session.storage_key(), &owned.version()?, "seed-release")
        .await?;
    h.store.commit_context(&write).await?;
    lease.release().await?;
    assert!(h
        .store
        .get_reply_dedupe(session.storage_key(), "saved-no")
        .await?
        .is_none());
    let http = Http::from_harness(h).await?;
    let response = http.rpc("SendMessage", json!({"message":answer})).await?;
    assert_eq!(
        response["result"]["task"]["status"]["state"], "TASK_STATE_COMPLETED",
        "{response}"
    );
    assert_eq!(response["result"]["task"]["id"], task["id"]);
    assert!(http
        .h
        .store
        .get_reply_dedupe(session.storage_key(), "saved-no")
        .await?
        .is_some());
    assert_eq!(
        http.h
            .task(&started.snapshot.task.id)
            .await?
            .task
            .history
            .as_ref()
            .unwrap()
            .iter()
            .filter(|m| m.message_id == "saved-no")
            .count(),
        1
    );
    let entries =
        NatsSessionLog::new_with_replicas(http.h.jetstream.clone(), session.storage_key(), 1)
            .load_events_latest_required_async()
            .await?;
    assert_eq!(
        entries
            .iter()
            .filter(|(_, e)| matches!(e, SessionLogEntry::HitlApprovalDecision { .. }))
            .count(),
        1
    );
    assert_eq!(entries.iter().filter(|(_, entry)| matches!(entry, SessionLogEntry::Message { role, content, .. } if role.is_user() && !harnx_core::timing_identity::is_timing_notice(&content.to_text()))).count(), 1);
    Ok(())
}

#[cfg(feature = "fault-injection")]
async fn reply_after_external_decision(script: Script, terminal: bool) -> Result<()> {
    use crate::unary::{hitl::reply, Http};
    use harnx_a2a_server::fault_injection::Boundary;
    use serde_json::json;
    let h = Harness::start(script).await?;
    let session = h.session(None, &alice()).await?;
    let (started, context) = challenge(&h, &session).await?;
    let fixed = ticket(&context, session.storage_key())?;
    let request = context
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
    let http = Http::from_harness(h).await?;
    let task = serde_json::to_value(&started.snapshot.task)?;
    let mut committed = http
        .h
        .runner
        .fault_hooks()
        .pause(Boundary::HitlReplyIntentCommitted, 1);
    let send = http.rpc(
        "SendMessage",
        json!({"message":reply("external-race",&task,"y",false)}),
    );
    tokio::pin!(send);
    tokio::select! {
        response = &mut send => anyhow::bail!("reply returned before intent: {response:?}"),
        reached = tokio::time::timeout(DEADLINE, committed.reached()) => { reached?; }
    }
    assert!(http
        .h
        .store
        .get_reply_dedupe(session.storage_key(), "external-race")
        .await?
        .is_none());
    session
        .decide_hitl_approval_for(&request, false, Some("external decision".into()))
        .await?;
    tokio::time::timeout(DEADLINE, async {
        loop {
            match session.fixed_prompt_progress(&fixed).await? {
                PromptProgress::Terminal(_) if terminal => return Ok::<_, anyhow::Error>(()),
                PromptProgress::InputRequired(p)
                    if !terminal && p.request_seq > request.request_seq =>
                {
                    return Ok(())
                }
                _ => tokio::time::sleep(std::time::Duration::from_millis(10)).await,
            }
        }
    })
    .await??;
    drop(committed);
    let response = send.await?;
    assert_eq!(response["result"]["task"]["id"], task["id"], "{response}");
    assert_eq!(
        response["result"]["task"]["status"]["state"],
        if terminal {
            "TASK_STATE_COMPLETED"
        } else {
            "TASK_STATE_INPUT_REQUIRED"
        },
        "{response}"
    );
    let entries =
        NatsSessionLog::new_with_replicas(http.h.jetstream.clone(), session.storage_key(), 1)
            .load_events_latest_required_async()
            .await?;
    assert_eq!(
        entries
            .iter()
            .filter(|(_, e)| matches!(e, SessionLogEntry::HitlApprovalDecision { .. }))
            .count(),
        1
    );
    assert!(http
        .h
        .store
        .get_reply_dedupe(session.storage_key(), "external-race")
        .await?
        .is_some());
    Ok(())
}

#[cfg(feature = "fault-injection")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runner_hitl_reply_requeries_already_terminal_progress() -> Result<()> {
    reply_after_external_decision(Script::Tool, true).await
}

#[cfg(feature = "fault-injection")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runner_hitl_reply_requeries_already_pending_next_challenge() -> Result<()> {
    reply_after_external_decision(Script::SerialTool, false).await
}

#[cfg(feature = "fault-injection")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runner_hitl_duplicate_inflight_on_other_backend_returns_next_boundary_once() -> Result<()>
{
    use crate::unary::{hitl::reply, spawn_strict_limits_server, Http, HttpRpcCall};
    use harnx_a2a_server::fault_injection::Boundary;
    use serde_json::json;
    let h = Harness::start(Script::SerialTool).await?;
    let session = h.session(None, &alice()).await?;
    let (started, _) = challenge(&h, &session).await?;
    let http = Http::from_harness(h).await?;
    // Separate Backend means no shared request gate. Its stricter data-part
    // limit cannot alter an already accepted plain-text control message.
    let (url, _server) = spawn_strict_limits_server(&http).await?;
    let task = serde_json::to_value(&started.snapshot.task)?;
    let answer = reply("same-inflight", &task, "n", false);
    let mut bound = http
        .h
        .runner
        .fault_hooks()
        .pause(Boundary::HitlReplyBound, 1);
    let send = http.rpc("SendMessage", json!({"message":answer}));
    tokio::pin!(send);
    tokio::select! {
        response = &mut send => anyhow::bail!("reply returned before binding: {response:?}"),
        reached = tokio::time::timeout(DEADLINE, bound.reached()) => { reached?; }
    }
    let winner = http
        .rpc_call(HttpRpcCall::new(
            &url,
            "alice",
            "SendMessage",
            json!({"message":answer}),
        ))
        .await?;
    assert_eq!(
        winner["result"]["task"]["status"]["state"], "TASK_STATE_INPUT_REQUIRED",
        "{winner}"
    );
    drop(bound);
    let duplicate = send.await?;
    assert_eq!(duplicate["result"]["task"]["id"], task["id"], "{duplicate}");
    assert_eq!(
        duplicate["result"]["task"]["status"]["state"],
        "TASK_STATE_INPUT_REQUIRED"
    );
    let entries =
        NatsSessionLog::new_with_replicas(http.h.jetstream.clone(), session.storage_key(), 1)
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
        http.h
            .task(&started.snapshot.task.id)
            .await?
            .task
            .history
            .as_ref()
            .unwrap()
            .iter()
            .filter(|m| m.message_id == "same-inflight")
            .count(),
        1
    );
    assert_eq!(http.h.llm.requests.lock().len(), 2);
    Ok(())
}
