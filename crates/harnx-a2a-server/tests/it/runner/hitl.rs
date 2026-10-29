#[cfg(feature = "fault-injection")]
use harnx_a2a_server::fault_injection::{FaultHooks, Pause};
use harnx_a2a_server::runner::StartTurnResult;
use harnx_a2a_server::store::context::ContextVersion;
use harnx_runtime::nats_session::fixed_admission::FixedAdmissionTicket;
use harnx_runtime::nats_session_metadata::RunLimitsRecord;
use harnx_runtime::PendingHitlApproval;
use serde_json::Value;
#[cfg(feature = "fault-injection")]
use tokio_util::task::AbortOnDropHandle;
mod assertions;
#[cfg(feature = "fault-injection")]
mod external;
mod fixtures;
mod reply_note;

use super::*;
use assertions::*;
use fixtures::*;
use harnx_a2a_server::store::context::{ContextIdentity, ContextSnapshot, HitlPhase};
use harnx_runtime::{
    nats_lease::{NatsLeaseAcquireParams, NatsLeaseConfig, NatsSessionLease},
    ApprovalRef, HitlDecisionOutcome, NatsSession, PromptProgress,
};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runner_hitl_tool_suspends_and_releases_owner_without_stopping_invocation() -> Result<()> {
    let h = Harness::start(Script::Tool).await?;
    let session = h.session(None, &alice()).await?;
    let (_, context) = challenge(&h, &session).await?;
    assert_suspended(&h, &session, &context).await
}

struct InvocationSnapshot {
    started: StartTurnResult,
    context: ContextSnapshot,
    ticket: FixedAdmissionTicket,
    frozen: RunLimitsRecord,
}

async fn setup_suspended_turn(h: &Harness, session: &NatsSession) -> Result<InvocationSnapshot> {
    let (started, context) = challenge(h, session).await?;
    let ticket = ticket(&context, session.storage_key())?;
    let frozen = h
        .metadata
        .get_invocation_limits(session.storage_key(), ticket.invocation_id())
        .await?
        .context("frozen limits")?;
    Ok(InvocationSnapshot {
        started,
        context,
        ticket,
        frozen,
    })
}

async fn apply_denial_and_advance(
    h: &Harness,
    session: &NatsSession,
    context: &ContextSnapshot,
    ticket: &FixedAdmissionTicket,
) -> Result<()> {
    let active = context.document.state.active.as_ref().unwrap();
    let request = &active.hitl_checkpoint.as_ref().unwrap().request;
    assert_eq!(
        session
            .decide_hitl_approval_for(request, false, Some("not this handoff".into()))
            .await?,
        HitlDecisionOutcome::Applied
    );
    wait_terminal_progress(session, ticket).await?;
    following(h, session, context).await
}

async fn resume_and_drain_output(
    h: &Harness,
    session: &NatsSession,
    task_id: &str,
    expected_start: &StartTurnResult,
) -> Result<String> {
    let mut resumed = h
        .runner
        .resume_hitl_turn(&h.export, &alice().into(), session.clone())
        .await?;
    assert_eq!(resumed.snapshot.task.id, expected_start.snapshot.task.id);
    assert_eq!(
        resumed.snapshot.user_msg_seq,
        expected_start.snapshot.user_msg_seq
    );
    assert_eq!(
        resumed.snapshot.execution_id,
        expected_start.snapshot.execution_id
    );
    assert_eq!(
        text(&resumed.snapshot.task.artifacts.as_ref().unwrap()[0].parts),
        "Before approval. "
    );
    let assembled = drain_resumed_artifact(&mut resumed.events, "Before approval. ").await?;
    assert_eq!(assembled, "Before approval. Hello world");
    assert_completed_output(h, session, task_id, &assembled).await?;
    Ok(assembled)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runner_hitl_resume_recovers_output_written_before_attach_and_keeps_ticket() -> Result<()> {
    let h = Harness::start(Script::Tool).await?;
    let session = h.session(None, &alice()).await?;
    let snap = setup_suspended_turn(&h, &session).await?;
    apply_denial_and_advance(&h, &session, &snap.context, &snap.ticket).await?;
    resume_and_drain_output(&h, &session, &snap.started.snapshot.task.id, &snap.started).await?;
    assert_eq!(
        h.metadata
            .get_invocation_limits(session.storage_key(), snap.ticket.invocation_id())
            .await?,
        Some(snap.frozen)
    );
    Ok(())
}

#[cfg(feature = "fault-injection")]
async fn verify_outbox_retry_on_publish_failure(
    h: &Harness,
    session: &NatsSession,
    mut committed: Pause,
    hooks: &FaultHooks,
) -> Result<()> {
    tokio::time::timeout(DEADLINE, committed.reached()).await?;
    let before = h.store.read_context(session.storage_key()).await?.unwrap();
    let active = before.document.state.active.as_ref().unwrap();
    let pending = active.publication.pending.clone().unwrap();
    assert!(pending.commit_id.ends_with(":input-required"));
    hooks.fail_next_event_publish();
    let mut retried = hooks.pause(
        harnx_a2a_server::fault_injection::Boundary::HitlOutboxCommitted,
        2,
    );
    drop(committed);
    tokio::time::timeout(DEADLINE, retried.reached()).await?;
    let during = h.store.read_context(session.storage_key()).await?.unwrap();
    let during_active = during.document.state.active.as_ref().unwrap();
    assert_eq!(
        serde_json::to_value(during_active.publication.pending.as_ref())?,
        serde_json::to_value(Some(&pending))?
    );
    assert!(!during_active.stop_confirmed);
    Ok(())
}

#[cfg(feature = "fault-injection")]
async fn drain_events_and_assert_suspended(
    h: &Harness,
    session: &NatsSession,
    events: &mut tokio::sync::broadcast::Receiver<A2aEvent>,
) -> Result<()> {
    while !event(events).await?.ends_response() {}
    let context = parked(h, session).await?;
    assert_suspended(h, session, &context).await?;
    assert!(
        h.logs.text().contains("injected event publish failure"),
        "{}",
        h.logs.text()
    );
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
    let committed = hooks.pause(Boundary::HitlOutboxCommitted, 1);
    let mut started = h.send(&session, prompt()).await?;
    verify_outbox_retry_on_publish_failure(&h, &session, committed, &hooks).await?;
    drain_events_and_assert_suspended(&h, &session, &mut started.events).await?;
    Ok(())
}

async fn apply_first_and_wait_second(
    h: &Harness,
    session: &NatsSession,
    context: &ContextSnapshot,
    ticket: &FixedAdmissionTicket,
) -> Result<PendingHitlApproval> {
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
    let second = wait_second_approval(session, ticket, first.request_seq, &h.logs.text()).await?;
    following(h, session, context).await?;
    Ok(second)
}

async fn resume_and_assert_second_parked(
    h: &Harness,
    session: &NatsSession,
    data: (&StartTurnResult, &ContextSnapshot, &FixedAdmissionTicket),
    second: PendingHitlApproval,
) -> Result<()> {
    let (started, context, ticket) = data;
    let mut resumed = h
        .runner
        .resume_hitl_turn(&h.export, &alice().into(), session.clone())
        .await?;
    assert_nonterminal_boundary(&mut resumed.events).await?;
    let parked = parked(h, session).await?;
    assert_next_approval_parked(
        started,
        context,
        &parked,
        second.approval_ref(ticket.invocation_id()),
    )?;
    assert_eq!(h.llm.requests.lock().len(), 2);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runner_hitl_resume_publishes_next_approval_already_pending_before_attach() -> Result<()> {
    let h = Harness::start(Script::SerialTool).await?;
    let session = h.session(None, &alice()).await?;
    let (started, context) = challenge(&h, &session).await?;
    let ticket = ticket(&context, session.storage_key())?;
    let second = apply_first_and_wait_second(&h, &session, &context, &ticket).await?;
    resume_and_assert_second_parked(&h, &session, (&started, &context, &ticket), second).await?;
    Ok(())
}

#[cfg(feature = "fault-injection")]
struct BoundReplySetup {
    http: crate::unary::Http,
    started: StartTurnResult,
    context: ContextSnapshot,
    task: Value,
}

#[cfg(feature = "fault-injection")]
async fn setup_bound_reply_fixture(session: &NatsSession, h: Harness) -> Result<BoundReplySetup> {
    use crate::unary::Http;
    let (started, context) = challenge(&h, session).await?;
    let task = serde_json::to_value(&started.snapshot.task)?;
    let http = Http::from_harness(h).await?;
    Ok(BoundReplySetup {
        http,
        started,
        context,
        task,
    })
}

#[cfg(feature = "fault-injection")]
async fn wait_for_bound_reply(
    send: &mut (impl std::future::Future<Output = Result<Value>> + Unpin),
    bound: &mut Pause,
) -> Result<()> {
    tokio::select! {
        response = send => anyhow::bail!("reply returned before binding: {response:?}"),
        reached = tokio::time::timeout(DEADLINE, bound.reached()) => { reached?; }
    }
    Ok(())
}

#[cfg(feature = "fault-injection")]
async fn advance_to_newer_checkpoint(
    http: &crate::unary::Http,
    session: &NatsSession,
    context: &ContextSnapshot,
) -> Result<ContextSnapshot> {
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
    following(&http.h, session, context).await?;
    let mut resumed = http
        .h
        .runner
        .resume_hitl_turn(&http.h.export, &alice().into(), session.clone())
        .await?;
    while !event(&mut resumed.events).await?.ends_response() {}
    let newer = parked(&http.h, session).await?;
    let new_req = &newer
        .document
        .state
        .active
        .as_ref()
        .unwrap()
        .hitl_checkpoint
        .as_ref()
        .unwrap()
        .request;
    assert!(new_req.request_seq > request.request_seq);
    Ok(newer)
}

#[cfg(feature = "fault-injection")]
async fn assert_bound_reply_rejected(
    http: &crate::unary::Http,
    session: &NatsSession,
    send: impl std::future::Future<Output = Result<Value>>,
    task_id: &str,
) -> Result<()> {
    let response = send.await?;
    crate::unary::code(&response, -32602);
    assert_single_decision_and_task_history(&http.h, session, task_id, "late-y").await
}

#[cfg(feature = "fault-injection")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runner_hitl_bound_reply_never_retargets_a_newer_checkpoint() -> Result<()> {
    use crate::unary::hitl::reply;
    use harnx_a2a_server::fault_injection::Boundary;
    use serde_json::json;
    let h = Harness::start(Script::SerialTool).await?;
    let session = h.session(None, &alice()).await?;
    let setup = setup_bound_reply_fixture(&session, h).await?;
    let mut bound = setup
        .http
        .h
        .runner
        .fault_hooks()
        .pause(Boundary::HitlReplyBound, 1);
    let send = setup.http.rpc(
        "SendMessage",
        json!({"message": reply("late-y", &setup.task, "y", true)}),
    );
    tokio::pin!(send);
    wait_for_bound_reply(&mut send, &mut bound).await?;
    advance_to_newer_checkpoint(&setup.http, &session, &setup.context).await?;
    drop(bound);
    assert_bound_reply_rejected(&setup.http, &session, send, &setup.started.snapshot.task.id)
        .await?;
    Ok(())
}

async fn seed_saved_intent_message(
    h: &Harness,
    session: &NatsSession,
    context: &ContextSnapshot,
    answer: &Value,
) -> Result<()> {
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
    seed_saved_intent(h, session, &request, &message).await?;
    assert!(h
        .store
        .get_reply_dedupe(session.storage_key(), "saved-no")
        .await?
        .is_none());
    Ok(())
}

async fn assert_saved_intent_reply_completed(
    http: &crate::unary::Http,
    session: &NatsSession,
    answer: Value,
    task_id: &str,
) -> Result<()> {
    let response = http
        .rpc("SendMessage", serde_json::json!({"message": answer}))
        .await?;
    assert_eq!(
        response["result"]["task"]["status"]["state"], "TASK_STATE_COMPLETED",
        "{response}"
    );
    assert_eq!(response["result"]["task"]["id"], task_id);
    assert!(http
        .h
        .store
        .get_reply_dedupe(session.storage_key(), "saved-no")
        .await?
        .is_some());
    assert_saved_intent_history_and_events(&http.h, session, task_id, "saved-no").await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runner_hitl_duplicate_finishes_saved_intent_without_new_admission() -> Result<()> {
    use crate::unary::{hitl::reply, Http};
    let h = Harness::start(Script::Tool).await?;
    let session = h.session(None, &alice()).await?;
    let (started, context) = challenge(&h, &session).await?;
    let task = serde_json::to_value(&started.snapshot.task)?;
    let answer = reply("saved-no", &task, "n saved full reason", false);
    seed_saved_intent_message(&h, &session, &context, &answer).await?;
    let http = Http::from_harness(h).await?;
    assert_saved_intent_reply_completed(&http, &session, answer, &started.snapshot.task.id).await?;
    Ok(())
}

#[cfg(feature = "fault-injection")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runner_hitl_reply_requeries_already_terminal_progress() -> Result<()> {
    external::reply_after_external_decision(Script::Tool, true).await
}

#[cfg(feature = "fault-injection")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runner_hitl_reply_requeries_already_pending_next_challenge() -> Result<()> {
    external::reply_after_external_decision(Script::SerialTool, false).await
}

#[cfg(feature = "fault-injection")]
async fn wait_for_strict_bound_reply(
    send: &mut (impl std::future::Future<Output = Result<Value>> + Unpin),
    bound: &mut Pause,
) -> Result<()> {
    tokio::select! {
        response = send => anyhow::bail!("reply returned before binding: {response:?}"),
        reached = tokio::time::timeout(DEADLINE, bound.reached()) => { reached?; }
    }
    Ok(())
}

#[cfg(feature = "fault-injection")]
struct InflightReplyCall<'a> {
    url: &'a str,
    answer: &'a Value,
    task_id: &'a str,
}

#[cfg(feature = "fault-injection")]
async fn assert_inflight_winner_and_duplicate(
    http: &crate::unary::Http,
    call: InflightReplyCall<'_>,
    send: impl std::future::Future<Output = Result<Value>>,
) -> Result<()> {
    let winner = http
        .rpc_call(crate::unary::HttpRpcCall::new(
            call.url,
            "alice",
            "SendMessage",
            serde_json::json!({"message": call.answer}),
        ))
        .await?;
    assert_eq!(
        winner["result"]["task"]["status"]["state"], "TASK_STATE_INPUT_REQUIRED",
        "{winner}"
    );
    let duplicate = send.await?;
    assert_eq!(
        duplicate["result"]["task"]["id"], call.task_id,
        "{duplicate}"
    );
    assert_eq!(
        duplicate["result"]["task"]["status"]["state"],
        "TASK_STATE_INPUT_REQUIRED"
    );
    Ok(())
}

#[cfg(feature = "fault-injection")]
struct StrictInflightSetup {
    http: crate::unary::Http,
    session: NatsSession,
    task_id: String,
    url: String,
    answer: Value,
    _server: AbortOnDropHandle<()>,
}

#[cfg(feature = "fault-injection")]
async fn setup_strict_inflight_test() -> Result<StrictInflightSetup> {
    use crate::unary::{hitl::reply, spawn_strict_limits_server, Http};
    let h = Harness::start(Script::SerialTool).await?;
    let session = h.session(None, &alice()).await?;
    let (started, _) = challenge(&h, &session).await?;
    let http = Http::from_harness(h).await?;
    let (url, _server) = spawn_strict_limits_server(&http).await?;
    let task = serde_json::to_value(&started.snapshot.task)?;
    let answer = reply("same-inflight", &task, "n", false);
    Ok(StrictInflightSetup {
        http,
        session,
        task_id: started.snapshot.task.id,
        url,
        answer,
        _server,
    })
}

#[cfg(feature = "fault-injection")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runner_hitl_duplicate_inflight_on_other_backend_returns_next_boundary_once() -> Result<()>
{
    use harnx_a2a_server::fault_injection::Boundary;
    let s = setup_strict_inflight_test().await?;
    let mut bound = s
        .http
        .h
        .runner
        .fault_hooks()
        .pause(Boundary::HitlReplyBound, 1);
    let send = s
        .http
        .rpc("SendMessage", serde_json::json!({"message": &s.answer}));
    tokio::pin!(send);
    wait_for_strict_bound_reply(&mut send, &mut bound).await?;
    drop(bound);
    assert_inflight_winner_and_duplicate(
        &s.http,
        InflightReplyCall {
            url: &s.url,
            answer: &s.answer,
            task_id: &s.task_id,
        },
        send,
    )
    .await?;
    assert_inflight_duplicate_records(&s.http.h, &s.session, &s.task_id, "same-inflight").await?;
    Ok(())
}
