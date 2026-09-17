//! The interruption behaviours the design spec names but no other suite
//! already proves.
//!
//! Every test here drives the real thing: a `nats-server`, a worker daemon
//! with a stub model, and where a tool is needed an in-process tool server.
//! Interruption is always one `Cancel` append to the session log; what a test
//! asserts is what the log, the tool control subject or the stream's own
//! message count says afterwards.

use crate::worker::{
    acquire_worker_lease, await_worker_ready, counting_stub_call_fn, local_nats_runtime_config,
    poll_until, require_nats_server, short_lease_config, start_tool_server, EnvGuard,
    CI_SAFE_TIMEOUT,
};
use anyhow::{Context, Result};
use harnx_core::abort::create_abort_signal;
use harnx_core::event::NullSink;
use harnx_core::instance::ServerScope;
use harnx_core::message::{MessageContent, MessageRole};
use harnx_core::session::{SessionLogEntry as Entry, ToolOutput};
use harnx_core::tool::ToolCall;
use harnx_runtime::config::session::{SessionAppendSink, INTERRUPTED_TOOL_RESPONSE_ERROR};
use harnx_runtime::nats_lease::NatsSessionLease;
use harnx_runtime::nats_session::InterruptOutcome;
use harnx_runtime::nats_session_log::{stream_name_for_session, NatsSessionLog};
use harnx_runtime::nats_session_metadata::{SessionInitializer, SessionOverrides};
use harnx_runtime::nats_worker::{
    publish_session_activate, run_worker_daemon, targeted_worker_ready_subject,
    worker_ready_subject, FencedSessionLogSink, LocalWorkerTarget, NatsSessionLogBackend,
    SessionActivate, SessionActivationRoute, WorkerDaemonConfig,
};
use harnx_runtime::{NatsSession, NatsSessionConfig};
use harnx_toolset::{
    CancellationGuarantee, ControlKind, ControlMessage, ToolInvokeError, ToolReply, ToolRequest,
    ToolSpec, Toolset,
};
use harnx_toolset_server::invocation_journal::InvocationJournal;
use serde_json::json;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio_util::task::AbortOnDropHandle;

fn log(js: &async_nats::jetstream::Context, key: &str) -> NatsSessionLog {
    NatsSessionLog::new(js.clone(), key)
}

fn user(text: &str) -> Entry {
    Entry::Message {
        id: Some(format!("{text}-id")),
        role: MessageRole::User,
        content: MessageContent::Text(text.into()),
        timestamp: None,
        fence_token: None,
    }
}

fn assistant(text: &str) -> Entry {
    Entry::Message {
        id: None,
        role: MessageRole::Assistant,
        content: MessageContent::Text(text.into()),
        timestamp: None,
        fence_token: None,
    }
}

fn tool_calls(ids: &[&str]) -> Entry {
    Entry::ToolCalls {
        text: String::new(),
        thought: None,
        calls: ids
            .iter()
            .map(|id| ToolCall::new("slow_tool".into(), json!({}), Some((*id).into()), None))
            .collect(),
        timestamp: None,
        fence_token: Some(1),
    }
}

fn tool_results(id: &str, output: serde_json::Value) -> Entry {
    Entry::ToolResults {
        results: vec![ToolOutput {
            id: Some(id.into()),
            name: "slow_tool".into(),
            output,
            markdown: None,
            content: Vec::new(),
            switch_agent: None,
        }],
        timestamp: None,
    }
}

async fn open_session(url: &str, id: &str) -> Result<NatsSession> {
    session_with(url, id, SessionActivationRoute::ClusterShared, None).await
}

async fn session_with(
    url: &str,
    id: &str,
    activation_route: SessionActivationRoute,
    tools: Option<Vec<String>>,
) -> Result<NatsSession> {
    let client = async_nats::connect(url).await?;
    NatsSession::new(
        NatsSessionConfig {
            cluster: "local".into(),
            initializer: SessionInitializer::inline(
                "",
                Default::default(),
                SessionOverrides {
                    use_tools: tools,
                    ..Default::default()
                },
            ),
            session_id: Some(id.into()),
            activation_route,
        },
        client.clone(),
        async_nats::jetstream::new(client),
        create_abort_signal(),
    )
    .await
}

/// The sequence an interrupt accepted, or a failure naming what it saw
/// instead. None of these tests sets up a turn that was already over.
fn accepted_seq(outcome: InterruptOutcome) -> Result<u64> {
    match outcome {
        InterruptOutcome::Accepted { cancel_seq } => Ok(cancel_seq),
        other => anyhow::bail!("the seeded turn must be interruptible, got {other:?}"),
    }
}

/// A turn's own writer, fenced at the log tail it can see right now — the
/// `after_seq` a worker seeds at activation and advances with each append.
async fn turn_writer(
    js: &async_nats::jetstream::Context,
    session_key: &str,
    lease: Arc<NatsSessionLease>,
) -> Result<FencedSessionLogSink> {
    let tail = log(js, session_key)
        .load_events_async()
        .await?
        .last()
        .map_or(0, |(seq, _)| *seq);
    Ok(FencedSessionLogSink::new(
        NatsSessionLogBackend::new(js.clone(), session_key)
            .with_after_seq_observer(Arc::new(AtomicU64::new(tail))),
        lease,
    ))
}

/// Start a cluster-shared worker on a short lease and return once it has
/// announced itself, so "nothing has happened yet" is a statement about the
/// worker rather than about how far it got starting up.
async fn spawn_daemon(
    url: &str,
    worker_id: &str,
    call_fn: harnx_runtime::agent_loop::AgentCallFn,
) -> Result<AbortOnDropHandle<Result<()>>> {
    let client = async_nats::connect(url).await?;
    let mut ready = client.subscribe(worker_ready_subject("local")).await?;
    client.flush().await?;
    let mut config = WorkerDaemonConfig::managing("local", worker_id);
    config.lease = short_lease_config();
    let mut daemon = AbortOnDropHandle::new(tokio::spawn(run_worker_daemon(
        local_nats_runtime_config(url),
        config,
        Some(call_fn),
        None,
    )));
    await_worker_ready(&mut daemon, &mut ready).await?;
    Ok(daemon)
}

/// Every `ToolResults` entry in the whole log that answers `call_id`.
fn results_for<'a>(
    entries: &'a [(u64, Entry)],
    call_id: &'a str,
) -> Vec<(u64, &'a Vec<ToolOutput>)> {
    entries
        .iter()
        .filter_map(|(seq, entry)| match entry {
            Entry::ToolResults { results, .. }
                if results.iter().any(|r| r.id.as_deref() == Some(call_id)) =>
            {
                Some((*seq, results))
            }
            _ => None,
        })
        .collect()
}

/// One call's answer inside a `ToolResults` entry.
fn result_for<'a>(results: &'a [ToolOutput], call_id: &str) -> Result<&'a ToolOutput> {
    results
        .iter()
        .find(|result| result.id.as_deref() == Some(call_id))
        .with_context(|| format!("the wind-up answers {call_id}"))
}

async fn await_wind_up(log: &NatsSessionLog, call_id: &str) -> Result<Vec<(u64, Entry)>> {
    poll_until(async || Ok(!results_for(&log.load_events_async().await?, call_id).is_empty()))
        .await?;
    log.load_events_async().await
}

/// A journal row the way dispatch writes one: keyed by the wire id the
/// provider minted for this attempt, with the id the transcript's `ToolCalls`
/// gave the call carried inside. Only the round and that inner id connect a
/// transcript call to its row.
fn journal_request(session_key: &str, call_id: &str) -> ToolRequest {
    ToolRequest {
        replay: None,
        operation_id: format!("op-{}", wire_id(call_id)),
        call_id: wire_id(call_id),
        tool: "slow_tool".into(),
        args: json!({}),
        parent_session_id: Some(session_key.into()),
        tool_call_id: Some(call_id.into()),
        capabilities: Default::default(),
    }
}

/// The wire id dispatch would have minted for a transcript call. A uuid in
/// production; anything unrelated to the transcript id will do here.
fn wire_id(call_id: &str) -> String {
    format!("wire-{call_id}")
}

/// The reply a tool wrote to the journal before anyone read it back. It is
/// addressed to the wire id, which is the only id the tool ever saw.
async fn complete_in_journal(
    journal: &InvocationJournal,
    session_key: &str,
    call_id: &str,
    result: serde_json::Value,
) -> Result<()> {
    journal
        .complete(
            &journal_request(session_key, call_id),
            ToolReply {
                call_id: wire_id(call_id),
                result: Ok(result),
            },
        )
        .await?;
    Ok(())
}

const LOST: &str = "tool-lost-the-race";
const WON: &str = "tool-won-the-race";
const UNANSWERED: &str = "no-reply-call";
const ANSWERED: &str = "replied-call";

/// Seed the two-call round `placeholder_and_real_result_race_...` races: both
/// calls recorded under the same scope and server, `WON`'s reply already
/// waiting in the journal before the interrupt lands.
async fn seed_racing_round(
    js: &async_nats::jetstream::Context,
    journal: &InvocationJournal,
    key: &str,
) -> Result<()> {
    let round = log(js, key)
        .append_event_async(&tool_calls(&[LOST, WON]))
        .await?;
    for call in [LOST, WON] {
        journal
            .record(
                &journal_request(key, call),
                ("slow_tool", "raced-scope", "srv-raced"),
                round,
            )
            .await?;
    }
    complete_in_journal(journal, key, WON, json!({"answer": "in time"})).await?;
    Ok(())
}

/// Seed the two-call round `resume_after_partial_wind_up_...` resumes: each
/// call recorded under its own server name in the departed worker's scope,
/// with only `ANSWERED`'s reply already in the journal.
async fn seed_partial_round(
    js: &async_nats::jetstream::Context,
    journal: &InvocationJournal,
    key: &str,
    orphan_scope: &ServerScope,
) -> Result<()> {
    let round = log(js, key)
        .append_event_async(&tool_calls(&[UNANSWERED, ANSWERED]))
        .await?;
    for (call, server_name) in [(UNANSWERED, "srv-unanswered"), (ANSWERED, "srv-answered")] {
        journal
            .record(
                &journal_request(key, call),
                ("slow_tool", orphan_scope.as_str(), server_name),
                round,
            )
            .await?;
    }
    complete_in_journal(journal, key, ANSWERED, json!({"answer": "done"})).await?;
    Ok(())
}

/// Wait for wind-up's resent `Cancel` on `control` and check it names
/// `UNANSWERED` by wire id and the server the journal recorded for it — the
/// one detail a resent cancel cannot get from the transcript, only from the
/// journal.
async fn expect_resent_cancel_for_unanswered(
    control: &mut async_nats::Subscriber,
    journal: &InvocationJournal,
    key: &str,
) -> Result<()> {
    let message = tokio::time::timeout(CI_SAFE_TIMEOUT, futures_util::StreamExt::next(control))
        .await
        .context("timed out waiting for the resent orphan cancel")?
        .context("control subscription closed early")?;
    let resent: ControlMessage = serde_json::from_slice(&message.payload)?;
    assert_eq!(resent.kind, ControlKind::Cancel);
    assert_eq!(
        resent.call_id,
        wire_id(UNANSWERED),
        "the cancel names the call by the wire id, the only one the tool server answers to"
    );
    assert!(
        journal.recorded(key, &resent.call_id).await?.is_some(),
        "the id on the wire is the key the tool server's own orphan-cancel lookup uses"
    );
    assert_eq!(
        resent.server, "srv-unanswered",
        "the cancel goes to the server the journal says took the call"
    );
    assert!(
        !resent.cancellation_id.is_empty(),
        "an orphan cancel names the cancellation that stopped the turn"
    );
    Ok(())
}

/// A `Cancel` ends the turn, so the turn's own writer never gets its results
/// into the log. The wind-up that follows owes the interrupted call an answer
/// and writes the placeholder instead — the tool's own late output is not
/// what lands.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancel_after_tool_calls_rejects_late_tool_results() -> Result<()> {
    const CALL: &str = "cut-off-call";
    let Some(server) = require_nats_server().await? else {
        return Ok(());
    };
    let session = open_session(server.url(), "cancel-late-results").await?;
    let js = async_nats::jetstream::new(async_nats::connect(server.url()).await?);
    let key = session.storage_key().to_string();
    let log = log(&js, &key);

    log.append_event_async(&user("run the tool")).await?;
    let lease = acquire_worker_lease(&js, &key, "interrupted-worker").await?;
    let writer = turn_writer(&js, &key, lease.clone()).await?;
    writer.append(&tool_calls(&[CALL]))?;

    let cancel_seq = accepted_seq(session.interrupt("client cancel").await?)?;

    let error = writer
        .append(&tool_results(CALL, json!("late tool success")))
        .expect_err("a result appended behind a Cancel must be rejected");
    assert!(
        format!("{error:#}").contains("turn interrupted by a Cancel"),
        "expected a turn-interruption rejection, got {error:#}"
    );

    // The interrupted worker dies without releasing its lease; the next one
    // closes the round out.
    lease.stop_renewal_for_test().await;
    let calls = Arc::new(AtomicUsize::new(0));
    let _daemon = spawn_daemon(
        server.url(),
        "wind-up-worker",
        counting_stub_call_fn(calls.clone()),
    )
    .await?;

    let entries = await_wind_up(&log, CALL).await?;
    let wound = results_for(&entries, CALL);
    assert_eq!(wound.len(), 1, "one wind-up answers the round: {entries:?}");
    let (seq, results) = wound[0];
    assert!(seq > cancel_seq, "the results follow the Cancel");
    assert_eq!(
        results[0].output["error"], INTERRUPTED_TOOL_RESPONSE_ERROR,
        "the rejected result is replaced by a placeholder, not recovered"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    Ok(())
}

/// A message typed while the worker was mid-append moves the tail out from
/// under it. That is not a rival writer: the worker folds the message into
/// its view and appends in front of it, so neither write is lost.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn queued_user_message_absorbed_by_worker_append_retry() -> Result<()> {
    let Some(server) = require_nats_server().await? else {
        return Ok(());
    };
    let session = open_session(server.url(), "queued-append-retry").await?;
    let js = async_nats::jetstream::new(async_nats::connect(server.url()).await?);
    let key = session.storage_key().to_string();
    let log = log(&js, &key);

    log.append_event_async(&user("do the thing")).await?;
    let lease = acquire_worker_lease(&js, &key, "appending-worker").await?;
    // The writer's observed tail is the opening prompt...
    let writer = turn_writer(&js, &key, lease.clone()).await?;
    // ...and the user types again before it gets its own entry in.
    let queued_seq = log.append_event_async(&user("actually, this too")).await?;

    let appended_seq = writer.append(&assistant("worker output"))?;
    assert!(
        appended_seq > queued_seq,
        "the retry appends in front of the queued message, not at the stale tail"
    );

    let entries = log.load_events_async().await?;
    let texts = entries
        .iter()
        .filter_map(|(_, entry)| match entry {
            Entry::Message { content, role, .. } => Some((*role, content.to_text())),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        texts,
        vec![
            (MessageRole::User, "do the thing".to_string()),
            (MessageRole::User, "actually, this too".to_string()),
            (MessageRole::Assistant, "worker output".to_string()),
        ],
        "the queued message survives the worker's retry: {entries:?}"
    );
    lease.release().await?;
    Ok(())
}

/// A tool that finishes while the interrupt is travelling writes its result
/// to the journal, not the log, so wind-up and the tool race for every call.
/// Whoever gets there first, the round is answered by exactly one
/// `ToolResults`, and each call in it reflects who won: the tool that beat
/// wind-up keeps its real output, the one that did not gets the placeholder.
/// A result that lands afterwards is too late to reopen the round.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn placeholder_and_real_result_race_yields_exactly_one_tool_results_entry() -> Result<()> {
    let Some(server) = require_nats_server().await? else {
        return Ok(());
    };
    let session = open_session(server.url(), "placeholder-race").await?;
    let client = async_nats::connect(server.url()).await?;
    let js = async_nats::jetstream::new(client.clone());
    let key = session.storage_key().to_string();
    let log = log(&js, &key);

    log.append_event_async(&user("run both")).await?;
    let journal = InvocationJournal::ensure(&js, 1).await?;
    // One tool got its reply into the journal before the interrupt; the other
    // is still running when wind-up closes the round out.
    seed_racing_round(&js, &journal, &key).await?;

    let cancel_seq = accepted_seq(session.interrupt("client cancel").await?)?;
    let calls = Arc::new(AtomicUsize::new(0));
    let _daemon = spawn_daemon(
        server.url(),
        "race-worker",
        counting_stub_call_fn(calls.clone()),
    )
    .await?;
    let entries = await_wind_up(&log, LOST).await?;
    let wound = results_for(&entries, LOST);
    assert_eq!(wound.len(), 1, "one entry answers the whole round");
    assert_eq!(
        result_for(wound[0].1, WON)?.output,
        json!({"answer": "in time"}),
        "the reply that beat wind-up is kept, not overwritten by a placeholder"
    );
    assert_eq!(
        result_for(wound[0].1, LOST)?.output["error"],
        INTERRUPTED_TOOL_RESPONSE_ERROR
    );

    // The slower tool's real result arrives after the placeholder was written.
    complete_in_journal(&journal, &key, LOST, json!({"answer": "too late"})).await?;
    publish_session_activate(
        &js,
        "local",
        &SessionActivate::new(&key).with_requested_seq(cancel_seq),
    )
    .await?;

    // A turn published behind that activation cannot start before the worker
    // has finished with it, so its completion is when the race is decided. It
    // needs its own session handle: interrupting latches the abort signal on
    // the one that asked for it.
    let next = open_session(server.url(), session.session_id()).await?;
    let result = tokio::time::timeout(
        CI_SAFE_TIMEOUT,
        next.run_turn("carry on", Arc::new(NullSink), None),
    )
    .await??;
    assert!(!result.was_cancelled, "the next turn runs on its own");

    let entries = log.load_events_async().await?;
    let wound = results_for(&entries, LOST);
    assert_eq!(
        wound.len(),
        1,
        "the round the Cancel interrupted is answered exactly once: {entries:?}"
    );
    assert_eq!(
        result_for(wound[0].1, LOST)?.output["error"],
        INTERRUPTED_TOOL_RESPONSE_ERROR,
        "a result that lands after the round was answered does not reopen it"
    );
    Ok(())
}

/// A worker that died between cancelling some of its calls and writing the
/// results left the round half closed. The next activation resends a cancel
/// for every call the journal has no reply for — addressed from the journal
/// row, because the process that dispatched it is gone — and leaves the
/// answered one alone.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn resume_after_partial_wind_up_resends_cancels_for_calls_without_journal_reply() -> Result<()>
{
    let Some(server) = require_nats_server().await? else {
        return Ok(());
    };
    let session = open_session(server.url(), "partial-wind-up").await?;
    let client = async_nats::connect(server.url()).await?;
    let js = async_nats::jetstream::new(client.clone());
    let key = session.storage_key().to_string();
    let log = log(&js, &key);

    log.append_event_async(&user("run both")).await?;
    let orphan_scope = ServerScope::from_string("departed-worker-scope");
    let journal = InvocationJournal::ensure(&js, 1).await?;
    seed_partial_round(&js, &journal, &key, &orphan_scope).await?;
    let mut control = client.subscribe(orphan_scope.control_subject()).await?;
    client.flush().await?;

    let cancel_seq = accepted_seq(session.interrupt("client cancel").await?)?;
    let calls = Arc::new(AtomicUsize::new(0));
    let _daemon = spawn_daemon(
        server.url(),
        "partial-wind-up-worker",
        counting_stub_call_fn(calls.clone()),
    )
    .await?;

    expect_resent_cancel_for_unanswered(&mut control, &journal, &key).await?;

    let entries = await_wind_up(&log, UNANSWERED).await?;
    let wound = results_for(&entries, UNANSWERED);
    assert_eq!(wound.len(), 1);
    assert!(wound[0].0 > cancel_seq);
    let results = wound[0].1;
    assert_eq!(
        results
            .iter()
            .find(|r| r.id.as_deref() == Some(UNANSWERED))
            .context("the unanswered call is answered")?
            .output["error"],
        INTERRUPTED_TOOL_RESPONSE_ERROR
    );
    assert_eq!(
        results
            .iter()
            .find(|r| r.id.as_deref() == Some(ANSWERED))
            .context("the answered call keeps its result")?
            .output,
        json!({"answer": "done"})
    );
    assert!(
        tokio::time::timeout(
            Duration::from_millis(500),
            futures_util::StreamExt::next(&mut control)
        )
        .await
        .is_err(),
        "a call the journal already answered is not cancelled again"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    Ok(())
}

/// A local worker's id changes when the frontend restarts, so the wind-up
/// activation the interrupt published is addressed to a worker that no longer
/// exists. The pre-restart worker is represented by an id nothing ever
/// consumed for — the same stranded state a dead worker leaves behind, since
/// the targeted stream keeps a message only while someone is listening for it.
/// Attaching is what repairs it: the frontend re-derives the session's state
/// and publishes a fresh activation to its own worker.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn frontend_attach_republishes_wind_up_activation_after_local_restart() -> Result<()> {
    const CALL: &str = "stranded-call";
    const SESSION: &str = "attach-republish";
    // Local workers are addressed under the reserved local session scope, not
    // the name of the cluster their connection happens to come from.
    const LOCAL_SCOPE: &str = "__local__";
    let Some(server) = require_nats_server().await? else {
        return Ok(());
    };
    let targeted = |worker_id: &str| SessionActivationRoute::WorkerTargeted {
        session_scope: LOCAL_SCOPE.into(),
        worker_id: worker_id.into(),
    };
    let before_restart = session_with(
        server.url(),
        SESSION,
        targeted("worker-before-restart"),
        None,
    )
    .await?;
    let client = async_nats::connect(server.url()).await?;
    let js = async_nats::jetstream::new(client.clone());
    let key = before_restart.storage_key().to_string();
    let log = log(&js, &key);
    log.append_event_async(&user("run the tool")).await?;
    log.append_event_async(&tool_calls(&[CALL])).await?;

    // The interrupt's wind-up activation is addressed to the worker that died
    // with the old frontend, so nothing consumes it.
    accepted_seq(before_restart.interrupt("client cancel").await?)?;

    let mut ready = client
        .subscribe(targeted_worker_ready_subject(LocalWorkerTarget::new(
            LOCAL_SCOPE,
            "worker-after-restart",
        )?))
        .await?;
    client.flush().await?;
    let calls = Arc::new(AtomicUsize::new(0));
    // A targeted local worker takes its connection from the frontend's
    // environment handoff rather than a configured cluster.
    let _environment = EnvGuard::tool_server_environment(&ServerScope::new(), server.url());
    let mut config = WorkerDaemonConfig::local("worker-after-restart")?;
    config.lease = short_lease_config();
    let mut daemon = AbortOnDropHandle::new(tokio::spawn(run_worker_daemon(
        local_nats_runtime_config(server.url()),
        config,
        Some(counting_stub_call_fn(calls.clone())),
        None,
    )));
    await_worker_ready(&mut daemon, &mut ready).await?;
    assert!(
        results_for(&log.load_events_async().await?, CALL).is_empty(),
        "a running worker with no activation for this session winds nothing up"
    );

    let after_restart = session_with(
        server.url(),
        SESSION,
        targeted("worker-after-restart"),
        None,
    )
    .await?;
    assert!(
        after_restart.republish_pending_activation().await?,
        "an interrupted session still owes a wind-up when a frontend attaches"
    );

    let entries = await_wind_up(&log, CALL).await?;
    assert_eq!(results_for(&entries, CALL).len(), 1);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    Ok(())
}

#[derive(Default)]
struct CountingTool {
    calls: AtomicUsize,
}

#[async_trait::async_trait]
impl Toolset for CountingTool {
    fn name(&self) -> &str {
        "counter"
    }

    fn tools(&self) -> Vec<ToolSpec> {
        vec![ToolSpec {
            name: "ping".into(),
            description: "Answer immediately".into(),
            input_schema: json!({"type": "object"}),
            idempotent_hint: true,
            read_only_hint: true,
            cancellation_guarantee: CancellationGuarantee::Cooperative,
            timeout_secs: None,
            meta: None,
        }]
    }

    async fn invoke(
        &self,
        _tool: &str,
        _args: serde_json::Value,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> std::result::Result<serde_json::Value, ToolInvokeError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(json!({"content": [{"type": "text", "text": "pong"}]}))
    }
}

/// A tool round costs the session log a bounded number of entries and the
/// broker no control-plane state at all. There is no gate bucket any more:
/// running tools must not bring one back.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_tool_round_adds_bounded_stream_entries_and_no_control_plane_keys() -> Result<()> {
    const ROUNDS: usize = 5;
    let Some(server) = require_nats_server().await? else {
        return Ok(());
    };
    let client = async_nats::connect(server.url()).await?;
    let js = async_nats::jetstream::new(client.clone());
    let scope = ServerScope::new();
    let _environment = EnvGuard::tool_server_environment(&scope, server.url());
    let tool = Arc::new(CountingTool::default());
    let _tool_server = start_tool_server(&js, client.clone(), scope, tool.clone()).await?;
    let model_calls = Arc::new(AtomicUsize::new(0));
    let _daemon = AbortOnDropHandle::new(tokio::spawn(run_worker_daemon(
        local_nats_runtime_config(server.url()),
        WorkerDaemonConfig::new("local", "bounded-growth-worker"),
        Some(tool_round_model(model_calls.clone(), ROUNDS)),
        None,
    )));
    let session = session_with(
        server.url(),
        "bounded-growth",
        SessionActivationRoute::ClusterShared,
        Some(vec!["counter_ping".into()]),
    )
    .await?;

    let result = tokio::time::timeout(
        CI_SAFE_TIMEOUT,
        session.run_turn("count for me", Arc::new(NullSink), None),
    )
    .await??;
    assert_eq!(result.response.as_deref(), Some("counted"));
    assert_eq!(tool.calls.load(Ordering::SeqCst), ROUNDS);

    let messages = js
        .get_stream(stream_name_for_session(session.storage_key()))
        .await?
        .info()
        .await?
        .state
        .messages;
    assert!(
        (2 * ROUNDS as u64..=(3 * ROUNDS + 4) as u64).contains(&messages),
        "{ROUNDS} tool rounds cost a call and a result each and no more than {} entries \
         in total, got {messages}",
        3 * ROUNDS + 4
    );
    // Not merely an error: the bucket has to be absent, so a broker this test
    // simply could not reach cannot stand in for one that was never created.
    let absent = js
        .get_stream("KV_harnx_execution_control")
        .await
        .err()
        .context("a tool round must not create a control-plane bucket")?;
    assert!(
        matches!(
            absent.kind(),
            async_nats::jetstream::context::GetStreamErrorKind::JetStream(error)
                if error.error_code()
                    == async_nats::jetstream::ErrorCode::STREAM_NOT_FOUND
        ),
        "expected the control-plane bucket to be absent, got {absent:#}"
    );
    Ok(())
}

/// One tool call per round for `rounds` rounds, then a final answer.
fn tool_round_model(
    calls: Arc<AtomicUsize>,
    rounds: usize,
) -> harnx_runtime::agent_loop::AgentCallFn {
    Arc::new(move |_input, _config, _abort| {
        let round = calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            if round >= rounds {
                return Ok(("counted".into(), None, vec![], Default::default()));
            }
            Ok((
                String::new(),
                None,
                vec![ToolCall::new(
                    "counter_ping".into(),
                    json!({}),
                    Some(format!("ping-{round}")),
                    None,
                )],
                Default::default(),
            ))
        })
    })
}

/// Spec §6 has an activation of an interrupted session resend a cancel for
/// every call the journal holds no reply for, "even if wind-up placeholders
/// already exist" — but only while the round is still open. Once the results
/// are durable the turn is over, and a later activation has nothing left to
/// cancel: a tool server that outlived the turn must not keep being told
/// about a call the log closed out long ago.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn no_cancel_is_resent_once_placeholders_are_durable() -> Result<()> {
    let Some(server) = require_nats_server().await? else {
        return Ok(());
    };
    let session = open_session(server.url(), "settled-wind-up").await?;
    let client = async_nats::connect(server.url()).await?;
    let js = async_nats::jetstream::new(client.clone());
    let key = session.storage_key().to_string();
    let log = log(&js, &key);

    log.append_event_async(&user("run both")).await?;
    let orphan_scope = ServerScope::from_string("settled-wind-up-scope");
    let journal = InvocationJournal::ensure(&js, 1).await?;
    seed_partial_round(&js, &journal, &key, &orphan_scope).await?;
    let mut control = client.subscribe(orphan_scope.control_subject()).await?;
    client.flush().await?;

    let cancel_seq = accepted_seq(session.interrupt("client cancel").await?)?;
    let calls = Arc::new(AtomicUsize::new(0));
    let _daemon = spawn_daemon(
        server.url(),
        "settled-wind-up-worker",
        counting_stub_call_fn(calls.clone()),
    )
    .await?;

    // The wind-up sends its one cancel and then answers the round.
    expect_resent_cancel_for_unanswered(&mut control, &journal, &key).await?;
    let entries = await_wind_up(&log, UNANSWERED).await?;
    assert_eq!(results_for(&entries, UNANSWERED).len(), 1);

    // The same wind-up activation, redelivered once the results are durable.
    publish_session_activate(
        &js,
        "local",
        &SessionActivate::new(&key).with_requested_seq(cancel_seq),
    )
    .await?;

    // A turn published behind that activation cannot start until the worker
    // has finished with it, so its completion is what proves the redelivery
    // was handled rather than merely still queued.
    let next = open_session(server.url(), session.session_id()).await?;
    let result = tokio::time::timeout(
        CI_SAFE_TIMEOUT,
        next.run_turn("carry on", Arc::new(NullSink), None),
    )
    .await??;
    assert!(!result.was_cancelled, "the next turn runs on its own");
    assert!(
        tokio::time::timeout(
            Duration::from_millis(250),
            futures_util::StreamExt::next(&mut control)
        )
        .await
        .is_err(),
        "an answered round has nothing left to cancel"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "only the turn typed after the interruption reached the model"
    );
    Ok(())
}
