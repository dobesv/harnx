mod assertions;
use assertions::assert_parked_deadline_cancellation;

use super::*;
use harnx_a2a_server::fault_injection::Pause;
use harnx_a2a_server::store::context::ContextIdentity;
use harnx_a2a_server::store::context::ContextVersion;
use harnx_runtime::{
    nats_lease::{NatsLeaseAcquireParams, NatsLeaseConfig, NatsSessionLease},
    ApprovalRef, HitlDecisionOutcome,
};

fn short_deadline(t: &TwoBackends) -> Result<()> {
    let path = t.h.config_dir().join("agents/runner.md");
    let agent = std::fs::read_to_string(&path)?;
    std::fs::write(
        path,
        agent.replacen(
            "model: mock:test\n",
            "model: mock:test\nrun_limits:\n  timeout_secs: 3\n",
            1,
        ),
    )?;
    Ok(())
}

async fn poll_cancellation_result(
    session: &NatsSession,
    ticket: &FixedAdmissionTicket,
) -> Result<harnx_runtime::NatsTurnResult> {
    let result = tokio::time::timeout(DEADLINE, async {
        loop {
            if let Some(result) = session.fixed_prompt_completion(ticket).await? {
                return Ok::<_, anyhow::Error>(result);
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await??;
    assert!(result.was_cancelled);
    Ok(result)
}

async fn deadline_result(session: &NatsSession, ticket: &FixedAdmissionTicket) -> Result<()> {
    let result = poll_cancellation_result(session, ticket).await?;
    let marker = result
        .error
        .as_deref()
        .and_then(harnx_runtime::parse_timeout_terminal)
        .context("runtime deadline marker")?;
    assert_eq!(marker.invocation_id, ticket.invocation_id());
    let frozen = session
        .metadata_store()
        .get_invocation_limits(session.storage_key(), ticket.invocation_id())
        .await?
        .context("frozen limits")?;
    assert_eq!(marker.deadline, frozen.deadline.context("deadline")?);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recovery_hitl_deadline_owner_gone_settles_canceled_and_rejects_late_reply() -> Result<()> {
    parked_deadline(false).await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recovery_hitl_deadline_reconciler_settles_canceled_and_rejects_late_reply() -> Result<()> {
    parked_deadline(true).await?;
    Ok(())
}

async fn start_deadline_fixture(t: &TwoBackends) -> Result<(NatsSession, ContextSnapshot)> {
    short_deadline(t)?;
    start_parked(t).await
}

async fn reconcile_foreground_orphan(
    t: &TwoBackends,
    session: &NatsSession,
    task_id: &str,
) -> Result<()> {
    let still_parked =
        t.h.store
            .read_context(session.storage_key())
            .await?
            .unwrap();
    assert_eq!(
        still_parked
            .document
            .state
            .active
            .as_ref()
            .unwrap()
            .snapshot
            .task
            .status
            .state,
        TaskState::InputRequired
    );
    t.b.backend
        .runner
        .reconcile_orphan(
            TaskAccess {
                export: &t.h.export,
                owner: &alice().into(),
                task_id,
            },
            session,
        )
        .await?;
    Ok(())
}

async fn parked_deadline(background: bool) -> Result<()> {
    let t = TwoBackends::start_script(Script::Tool).await?;
    let (session, before) = start_deadline_fixture(&t).await?;
    let active = before.document.state.active.as_ref().unwrap();
    let task_id = &active.snapshot.task.id;
    let ticket = ticket(&before, session.storage_key())?;
    // No publisher remains to project the detached worker deadline.
    t.a.backend.runner.shutdown().await;
    if background {
        supervise(&t, &t.b);
    }
    deadline_result(&session, &ticket).await?;
    if !background {
        reconcile_foreground_orphan(&t, &session, task_id).await?;
    }
    assert_parked_deadline_cancellation(&t, &session, &before).await
}

async fn assert_ownerless_following_state(t: &TwoBackends, session: &NatsSession) -> Result<()> {
    let ownerless =
        t.h.store
            .read_context(session.storage_key())
            .await?
            .unwrap();
    assert!(ownerless.document.owner.is_none());
    let following = ownerless.document.state.active.as_ref().unwrap();
    assert_eq!(following.snapshot.task.status.state, TaskState::Working);
    assert_eq!(
        following.hitl_checkpoint.as_ref().unwrap().phase,
        HitlPhase::Following
    );
    Ok(())
}

async fn append_crash_decision(
    t: &TwoBackends,
    session: &NatsSession,
    request: &ApprovalRef,
    before: &ContextSnapshot,
) -> Result<()> {
    NatsSessionLog::new(t.h.jetstream.clone(), session.storage_key())
        .append_event_async(&SessionLogEntry::HitlApprovalDecision {
            tool_call_id: request.tool_call_id.clone(),
            approved: true,
            note: None,
            fence_token: 1,
        })
        .await?;
    seed_following(t, session, before).await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recovery_hitl_foreground_get_repairs_ownerless_working_following() -> Result<()> {
    let t = TwoBackends::start_script(Script::Tool).await?;
    let (session, before) = start_parked(&t).await?;
    t.a.backend.runner.shutdown().await;
    let active = before.document.state.active.as_ref().unwrap();
    let request = &active.hitl_checkpoint.as_ref().unwrap().request;
    // Crash after durable decision/Following, before activation or follower attach.
    append_crash_decision(&t, &session, request, &before).await?;
    assert_ownerless_following_state(&t, &session).await?;
    let response =
        t.b.rpc("alice", "GetTask", json!({"id": active.snapshot.task.id}))
            .await?;
    assert!(response.get("error").is_none(), "{response}");
    // No background supervision or second request can rescue a skipped GetTask.
    assert_eq!(
        terminal(&t, &active.snapshot.task.id)
            .await?
            .task
            .status
            .state,
        TaskState::Completed
    );
    assert_original(&t, &session, &before).await?;
    assert_eq!(t.h.llm.requests.lock().len(), 1);
    t.b.backend.runner.shutdown().await;
    Ok(())
}

async fn acquire_following_crash_lease(
    t: &TwoBackends,
    storage_key: &str,
) -> Result<NatsSessionLease> {
    NatsSessionLease::acquire_scoped(
        NatsLeaseAcquireParams {
            jetstream: t.h.jetstream.clone(),
            session_id: storage_key,
            worker_id: "following-crash".into(),
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
    .context("lease")
}

async fn claim_and_advance_following(
    t: &TwoBackends,
    session: &NatsSession,
    lease: &NatsSessionLease,
    before: &ContextSnapshot,
) -> Result<ContextSnapshot> {
    let identity = ContextIdentity {
        storage_key: session.storage_key(),
        local_id: session.session_id(),
    };
    let write =
        t.h.store
            .prepare_context_claim(identity, lease, "following-claim")
            .await?;
    let owned = t.h.store.commit_context(&write).await?;
    let active = before.document.state.active.as_ref().unwrap();
    let request = &active.hitl_checkpoint.as_ref().unwrap().request;
    t.h.store
        .advance_hitl_to_following(
            session.storage_key(),
            &owned.version()?,
            harnx_a2a_server::store::context::HitlAdvanceParams {
                expected_approval: request,
                outcome: &HitlDecisionOutcome::Applied,
            },
        )
        .await?;
    let current =
        t.h.store
            .read_context(session.storage_key())
            .await?
            .unwrap();
    Ok(current)
}

async fn release_following_crash(
    t: &TwoBackends,
    session: &NatsSession,
    version: &ContextVersion,
    lease: NatsSessionLease,
) -> Result<()> {
    let write =
        t.h.store
            .prepare_context_release(session.storage_key(), version, "following-release")
            .await?;
    t.h.store.commit_context(&write).await?;
    lease.release().await?;
    Ok(())
}

async fn seed_following(
    t: &TwoBackends,
    session: &NatsSession,
    before: &ContextSnapshot,
) -> Result<()> {
    let lease = acquire_following_crash_lease(t, session.storage_key()).await?;
    let current = claim_and_advance_following(t, session, &lease, before).await?;
    release_following_crash(t, session, &current.version()?, lease).await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_deadline_without_hitl_settles_canceled() -> Result<()> {
    live_deadline(false).await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_deadline_publish_failure_still_settles_canceled() -> Result<()> {
    live_deadline(true).await?;
    Ok(())
}

struct LiveDeadlinePause {
    task: Value,
    session: NatsSession,
    paused: Pause,
}

async fn start_live_deadline_pause(t: &TwoBackends) -> Result<LiveDeadlinePause> {
    short_deadline(t)?;
    let hooks = t.a.backend.runner.fault_hooks();
    let mut paused = hooks.pause(Boundary::Publication, 2);
    let first = t.a.rpc("alice", "SendMessage", first_message()).await?;
    tokio::time::timeout(DEADLINE, paused.reached()).await?;
    let task = result_task(&first)?;
    let session = session_for(&t.b, &t.h, task["contextId"].as_str().unwrap()).await?;
    Ok(LiveDeadlinePause {
        task: task.clone(),
        session,
        paused,
    })
}

async fn assert_live_deadline_canceled(
    t: &TwoBackends,
    task_id: &str,
    fail_publish: bool,
) -> Result<()> {
    assert_eq!(
        terminal(t, task_id).await?.task.status.state,
        TaskState::Canceled
    );
    assert_eq!(t.h.llm.requests.lock().len(), 1);
    t.a.backend.runner.shutdown().await;
    if fail_publish {
        assert!(t.h.logs.text().contains("injected event publish failure"));
    }
    Ok(())
}

async fn live_deadline(fail_publish: bool) -> Result<()> {
    let t = TwoBackends::start().await?;
    let live = start_live_deadline_pause(&t).await?;
    let context =
        t.h.store
            .read_context(live.session.storage_key())
            .await?
            .unwrap();
    deadline_result(
        &live.session,
        &ticket(&context, live.session.storage_key())?,
    )
    .await?;
    if fail_publish {
        t.a.backend
            .store
            .context_fault_hooks()
            .fail_next_event_publish();
    }
    drop(live.paused);
    assert_live_deadline_canceled(&t, live.task["id"].as_str().unwrap(), fail_publish).await?;
    Ok(())
}
