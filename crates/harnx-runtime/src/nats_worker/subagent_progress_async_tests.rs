//! Live `SubAgentProgress` for dispatched prompts, mirroring
//! `subagent_progress_tests.rs`. The watcher must announce a dispatched
//! child as `Running`, keep the parent's advisory stream updated, and report
//! a terminal state only from the child prompt's durable settlement — never
//! from the dispatch receipt or a soft-wait expiry.
use super::tests::{
    env_lock, fixed_prompt_call_fn, seed_remote_config, slow_prompt_call_fn,
    spawn_metis_worker_with_call_fn, spawn_test_nats, subagent_test_env,
};
use crate::nats_event_sink::AdvisoryEnvelope;
use futures_util::StreamExt;
use harnx_core::event::{AgentEvent, SubAgentProgress, SubAgentProgressStatus, TurnEvent};
use harnx_toolset::{ToolInvocation, Toolset};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

const HEARTBEAT: Duration = Duration::from_millis(50);
const POLL: Duration = Duration::from_millis(100);
/// Generous bounds for a degraded CI runner; every test also has its own
/// shorter assertions where timing matters.
const OBSERVE_TIMEOUT: Duration = Duration::from_secs(30);

async fn async_progress_toolset(url: &str) -> Arc<super::subagent_toolset::SubagentToolset> {
    let client = async_nats::connect(url)
        .await
        .expect("connect sub-agent toolset to test nats");
    let jetstream = async_nats::jetstream::new(client.clone());
    let session_metadata =
        crate::nats_session_metadata::SessionMetadataStore::ensure(&jetstream, 1)
            .await
            .expect("open session metadata");
    Arc::new(
        super::subagent_toolset::SubagentToolset::new(
            "metis",
            super::subagent_toolset::SubagentSessionRoute::new(
                "local",
                super::SessionActivationRoute::ClusterShared,
            ),
            super::subagent_toolset::SubagentNats::new(
                client.clone(),
                jetstream,
                session_metadata,
                1,
            ),
        )
        .with_progress_heartbeat(HEARTBEAT)
        .with_progress_poll(POLL),
    )
}

/// A `session_send` caller with durable checkpoint/partial-result stores, the
/// transport-attested parent scope, and the parent transcript's tool call id.
async fn journaled_send_context(
    js: &async_nats::jetstream::Context,
    parent: &str,
    call_id: &str,
    args: &Value,
) -> harnx_toolset::ToolInvocationContext {
    let limits = crate::nats_session_metadata::RunLimitsRecord::admit_root(
        Default::default(),
        Default::default(),
        chrono::Utc::now(),
        Default::default(),
        None,
        crate::nats_session_metadata::CallTimeoutOverride::from_optional(None),
    )
    .expect("admit parent run limits");
    let admitted_at = chrono::Utc::now();
    let context = harnx_toolset::ToolInvocationContext {
        call_id: call_id.into(),
        invoking_session_id: Some(parent.into()),
        run_context: Some(harnx_toolset::AutonomousRunContext {
            snapshot: serde_json::to_value(&limits).expect("run limits serialize"),
            started_at_ms: admitted_at
                .timestamp_millis()
                .try_into()
                .expect("positive admission time"),
        }),
        ..Default::default()
    };
    let request = harnx_toolset::ToolRequest {
        operation_id: format!("{call_id}-operation"),
        call_id: call_id.into(),
        tool: "session_send".into(),
        args: args.clone(),
        run_context: context.run_context.clone(),
        parent_session_id: context.invoking_session_id.clone(),
        replay: None,
        parent_agent: None,
        parent_local_session_id: None,
        tool_call_id: None,
        tool_round: None,
        capabilities: Default::default(),
    };
    let journal = harnx_toolset_server::invocation_journal::InvocationJournal::ensure(js, 1)
        .await
        .expect("open invocation journal");
    journal
        .record(&request, ("session_send", "metis", "server"))
        .await
        .expect("journal the send request");
    harnx_toolset::ToolInvocationContext {
        checkpoint_store: Some(Arc::new(journal.checkpoint_store(&request))),
        partial_result_store: Some(Arc::new(journal.partial_result_store(&request))),
        ..context
    }
}

/// Subscribe to a parent's advisory stream before any progress can flow.
async fn subscribe_parent_events(url: &str, parent: &str) -> async_nats::Subscriber {
    let client = async_nats::connect(url)
        .await
        .expect("connect progress observer");
    crate::nats_session_log::NatsSessionLog::new_with_replicas(
        async_nats::jetstream::new(client.clone()),
        parent,
        1,
    )
    .load_events_async()
    .await
    .expect("create parent transcript");
    let events = client
        .subscribe(crate::nats_event_sink::events_subject(parent))
        .await
        .expect("subscribe parent progress");
    client.flush().await.expect("flush progress subscription");
    events
}

async fn next_progress(events: &mut async_nats::Subscriber) -> SubAgentProgress {
    tokio::time::timeout(OBSERVE_TIMEOUT, async {
        loop {
            let message = events.next().await.expect("parent events remain open");
            let envelope =
                AdvisoryEnvelope::from_bytes(&message.payload).expect("decode progress advisory");
            let AgentEvent::SubAgent { event, .. } = envelope.event else {
                continue;
            };
            let AgentEvent::Turn(TurnEvent::SubAgentProgress(progress)) = *event else {
                continue;
            };
            return progress;
        }
    })
    .await
    .expect("receive sub-agent progress")
}

/// Every progress advisory from now through the terminal one, inclusive. A
/// fast child can settle before the watcher's initial durable scan binds;
/// then the terminal is the FIRST advisory, so tests must not assume a
/// `Running` announcement arrived first.
async fn progress_until_terminal(events: &mut async_nats::Subscriber) -> Vec<SubAgentProgress> {
    tokio::time::timeout(OBSERVE_TIMEOUT, async {
        let mut seen = Vec::new();
        loop {
            let progress = next_progress(events).await;
            let terminal = progress.status != SubAgentProgressStatus::Running;
            seen.push(progress);
            if terminal {
                return seen;
            }
        }
    })
    .await
    .expect("watcher publishes terminal progress")
}

/// Every Running snapshot the watcher publishes, then the terminal one:
/// (running history, terminal). The running history proves the announcement
/// and heartbeat cadence; the terminal proves settlement from durable
/// evidence.
async fn running_then_terminal(
    events: &mut async_nats::Subscriber,
) -> (Vec<SubAgentProgress>, SubAgentProgress) {
    tokio::time::timeout(OBSERVE_TIMEOUT, async {
        let mut running = Vec::new();
        loop {
            let progress = next_progress(events).await;
            if progress.status == SubAgentProgressStatus::Running {
                running.push(progress);
            } else {
                return (running, progress);
            }
        }
    })
    .await
    .expect("watcher reports terminal progress")
}

/// Drain pending progress events without blocking, returning what arrived.
/// One deadline bounds the whole drain: a heartbeat cadence at the old
/// per-message timeout would otherwise keep the drain (and `env_lock`)
/// spinning indefinitely.
async fn drain_progress(events: &mut async_nats::Subscriber) -> Vec<SubAgentProgress> {
    let mut seen = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_millis(500);
    while let Ok(message) = tokio::time::timeout_at(deadline, events.next()).await {
        let message = message.expect("parent events remain open");
        let Ok(envelope) = AdvisoryEnvelope::from_bytes(&message.payload) else {
            continue;
        };
        let AgentEvent::SubAgent { event, .. } = envelope.event else {
            continue;
        };
        let AgentEvent::Turn(TurnEvent::SubAgentProgress(progress)) = *event else {
            continue;
        };
        seen.push(progress);
    }
    seen
}

async fn send(
    toolset: &Arc<super::subagent_toolset::SubagentToolset>,
    args: Value,
    context: harnx_toolset::ToolInvocationContext,
) -> Value {
    toolset
        .invoke_with_context(ToolInvocation {
            tool: "session_send".into(),
            args,
            cancel: CancellationToken::new(),
            context,
        })
        .await
        .expect("session_send succeeds")
}

async fn wait(
    toolset: Arc<super::subagent_toolset::SubagentToolset>,
    args: Value,
    parent: String,
) -> Value {
    toolset
        .invoke_with_context(ToolInvocation {
            tool: "session_wait".into(),
            args,
            cancel: CancellationToken::new(),
            context: harnx_toolset::ToolInvocationContext {
                call_id: "wait-call".into(),
                invoking_session_id: Some(parent),
                ..Default::default()
            },
        })
        .await
        .expect("session_wait succeeds")
}

fn blocking_model(
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
) -> crate::agent_loop::AgentCallFn {
    Arc::new(move |_, _, _| {
        let entered = Arc::clone(&entered);
        let release = Arc::clone(&release);
        Box::pin(async move {
            entered.notify_one();
            release.notified().await;
            Ok((
                "child released".to_string(),
                None,
                vec![],
                Default::default(),
            ))
        })
    })
}

fn failing_call_fn() -> crate::agent_loop::AgentCallFn {
    Arc::new(move |_, _, _| Box::pin(async move { anyhow::bail!("child model exploded") }))
}

/// Shared setup for every test in this module: broker, metis worker running
/// `call_fn`, the parent's advisory subscription, the toolset, and every
/// guard (env lock, env vars, seeded config, store dir) that must outlive
/// the test.
struct ProgressBed {
    daemon: tokio::task::JoinHandle<anyhow::Result<()>>,
    nats: super::tests::TestNatsServer,
    parent: String,
    js: async_nats::jetstream::Context,
    events: async_nats::Subscriber,
    toolset: Arc<super::subagent_toolset::SubagentToolset>,
    _store_dir: tempfile::TempDir,
    _seeded: super::tests::SeededRemoteParentConfig,
    _env: (
        super::tests::TestEnvGuard,
        super::tests::TestEnvGuard,
        super::tests::TestEnvGuard,
    ),
    _env_lock: tokio::sync::MutexGuard<'static, ()>,
}

impl ProgressBed {
    /// Returns `None` when no NATS server binary is available, mirroring
    /// the early return the tests previously made inline.
    async fn spawn(parent: &str, call_fn: crate::agent_loop::AgentCallFn) -> Option<Self> {
        let env_lock = env_lock().await;
        let (url, nats, store_dir) = spawn_test_nats().await?;
        let seeded = seed_remote_config(&url);
        let env = subagent_test_env(&url, &seeded);
        let daemon = spawn_metis_worker_with_call_fn(&url, call_fn);
        let events = subscribe_parent_events(&url, parent).await;
        let toolset = async_progress_toolset(&url).await;
        let js = async_nats::jetstream::new(async_nats::connect(&url).await.unwrap());
        Some(Self {
            daemon,
            nats,
            parent: parent.to_owned(),
            js,
            events,
            toolset,
            _store_dir: store_dir,
            _seeded: seeded,
            _env: env,
            _env_lock: env_lock,
        })
    }

    /// Run `session_send` with a journaled parent context, returning the
    /// tool's result.
    async fn send(&self, args: Value, call_id: &str) -> Value {
        let context = journaled_send_context(&self.js, &self.parent, call_id, &args).await;
        send(&self.toolset, args, context).await
    }

    /// Send with a wait long enough to settle even a slow child, returning
    /// the settled result.
    async fn settled_send(&self, session_id: &str, tool_call_id: &str) -> Value {
        let args = json!({
            "message": "run fast",
            "session_id": session_id,
            "wait": { "seconds": 30 },
            "__harnx_tool_call_id": tool_call_id,
        });
        self.send(args, &format!("send-{tool_call_id}")).await
    }

    /// Teardown: stop the worker, then the broker.
    async fn finish(mut self) {
        self.daemon.abort();
        let _ = self.daemon.await;
        let _ = self.nats.kill();
        let _ = self.nats.wait();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dispatch_announces_running_and_reports_terminal_from_durable_evidence() {
    let Some(mut bed) = ProgressBed::spawn(
        "async-progress-parent-1",
        slow_prompt_call_fn("async child response", Duration::from_millis(250)),
    )
    .await
    else {
        return;
    };
    let args = json!({
        "message": "report async progress",
        "session_id": "asyncchild1",
        "__harnx_tool_call_id": "parent-async-call-1",
    });
    let receipt = bed.send(args, "async-send-call-1").await;

    // The dispatch receipt is a tool result, not child progress: no terminal
    // summary rides on it, and it returns before the child settles.
    assert_eq!(receipt["status"], "pending");
    assert!(
        receipt.get("sub_agent_progress").is_none(),
        "a dispatch receipt never carries progress"
    );
    let prompt_id = receipt["prompt_id"].as_str().expect("prompt id").to_owned();
    assert_eq!(receipt["session_id"], "asyncchild1");

    let (running, terminal) = running_then_terminal(&mut bed.events).await;

    let announced = running.first().expect("the watcher announces the dispatch");
    assert_eq!(
        announced.elapsed_ms, 0,
        "announced as Running at dispatch, before any heartbeat"
    );
    assert!(
        running
            .iter()
            .skip(1)
            .any(|progress| progress.elapsed_ms > 0),
        "heartbeat updates keep the row live"
    );
    let mut last = 0;
    for progress in running.iter().chain([&terminal]) {
        assert!(
            progress.elapsed_ms >= last,
            "elapsed only moves forward across a call's snapshots"
        );
        last = progress.elapsed_ms;
    }
    for progress in running.iter().chain([&terminal]) {
        assert_eq!(
            progress.invocation_id, prompt_id,
            "one row per dispatched prompt"
        );
        assert_eq!(progress.agent, "metis");
        assert_eq!(progress.session_id, "asyncchild1");
        assert_eq!(
            progress.tool_call_id.as_deref(),
            Some("parent-async-call-1"),
            "every snapshot names the parent's tool call"
        );
    }
    assert_eq!(terminal.status, SubAgentProgressStatus::Done);

    bed.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn expired_wait_never_emits_terminal_progress() {
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let Some(mut bed) = ProgressBed::spawn(
        "async-progress-parent-2",
        blocking_model(Arc::clone(&entered), Arc::clone(&release)),
    )
    .await
    else {
        return;
    };
    let args = json!({
        "message": "hold until released",
        "session_id": "asyncchild2",
        "wait": { "seconds": 1 },
        "__harnx_tool_call_id": "parent-async-call-2",
    });
    let result = bed.send(args, "async-send-call-2").await;

    // The soft wait expired while the child was still running.
    assert_eq!(result["status"], "pending");
    assert_eq!(result["wait_expired"], true);
    assert_eq!(result["terminal"], false);
    assert!(
        result.get("sub_agent_progress").is_none(),
        "an expired wait never carries progress"
    );
    let prompt_id = result["prompt_id"].as_str().expect("prompt id").to_owned();

    // The watcher outlives the expired wait and keeps the child Running: the
    // next advisory it publishes proves it, without sleeping on a guess.
    tokio::time::timeout(OBSERVE_TIMEOUT, entered.notified())
        .await
        .expect("worker never entered the model");
    let live = next_progress(&mut bed.events).await;
    assert_eq!(live.status, SubAgentProgressStatus::Running);
    assert_eq!(live.invocation_id, prompt_id);
    for progress in drain_progress(&mut bed.events).await {
        assert_eq!(progress.status, SubAgentProgressStatus::Running);
        assert_eq!(progress.invocation_id, prompt_id);
    }

    // Terminal progress arrives only once the child durably settles. One
    // bounded wait through the terminal: the helper's outer timeout fails
    // the test instead of looping forever if the child never settles.
    release.notify_one();
    let terminal = progress_until_terminal(&mut bed.events)
        .await
        .pop()
        .expect("the terminal advisory closes the collected sequence");
    assert_eq!(terminal.status, SubAgentProgressStatus::Done);
    assert_eq!(terminal.invocation_id, prompt_id);

    bed.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn session_wait_reports_live_progress_and_attaches_terminal_summary() {
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let Some(mut bed) = ProgressBed::spawn(
        "async-progress-parent-3",
        blocking_model(Arc::clone(&entered), Arc::clone(&release)),
    )
    .await
    else {
        return;
    };
    let args = json!({
        "message": "dispatch then wait",
        "session_id": "asyncchild3",
        "__harnx_tool_call_id": "parent-async-call-3",
    });
    let receipt = bed.send(args, "async-send-call-3").await;
    let prompt_id = receipt["prompt_id"].as_str().expect("prompt id").to_owned();

    tokio::time::timeout(OBSERVE_TIMEOUT, entered.notified())
        .await
        .expect("worker never entered the model");
    release.notify_one();

    let waiter = tokio::spawn(wait(
        Arc::clone(&bed.toolset),
        json!({ "session_id": "asyncchild3", "prompt_id": prompt_id, "seconds": 30 }),
        bed.parent.clone(),
    ));
    let result = match tokio::time::timeout(OBSERVE_TIMEOUT, waiter).await {
        Ok(joined) => joined.expect("wait task joins"),
        Err(_) => panic!("session_wait never returned"),
    };

    // The settled observation carries a durable terminal summary for replay.
    assert_eq!(result["status"], "completed");
    assert_eq!(result["terminal"], true);
    let progress: SubAgentProgress = serde_json::from_value(result["sub_agent_progress"].clone())
        .expect("terminal observation carries a progress summary");
    assert_eq!(progress.status, SubAgentProgressStatus::Done);
    assert_eq!(progress.invocation_id, prompt_id);
    assert_eq!(progress.agent, "metis");
    assert_eq!(progress.session_id, "asyncchild3");

    // One announcement only: the wait reuses the dispatch's watcher instead
    // of starting a second one for the same child prompt.
    let mut announcements = 0;
    loop {
        let progress = next_progress(&mut bed.events).await;
        if progress.elapsed_ms == 0 && progress.status == SubAgentProgressStatus::Running {
            announcements += 1;
        }
        if progress.status != SubAgentProgressStatus::Running {
            assert_eq!(progress.status, SubAgentProgressStatus::Done);
            assert_eq!(progress.invocation_id, prompt_id);
            break;
        }
    }
    assert_eq!(
        announcements, 1,
        "session_wait must not announce a second watcher for the same prompt"
    );

    bed.finish().await;
}

/// A failing child settles through a durable `Error`; the watcher reports
/// `Failed`, and the settled wait's summary agrees end to end. A fast child
/// can settle before the watcher announces `Running`, so the terminal may be
/// the first advisory.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failing_child_reports_failed_terminal() {
    let Some(mut bed) = ProgressBed::spawn("async-progress-parent-5", failing_call_fn()).await
    else {
        return;
    };
    let result = bed.settled_send("asyncchild5", "parent-async-call-5").await;

    assert_eq!(result["status"], "failed", "{result}");
    assert_eq!(result["terminal"], true);
    let summary: SubAgentProgress = serde_json::from_value(result["sub_agent_progress"].clone())
        .expect("failed observation carries a progress summary");
    assert_eq!(summary.status, SubAgentProgressStatus::Failed);
    assert_eq!(summary.agent, "metis");
    assert_eq!(summary.session_id, "asyncchild5");

    // Live side: Failed from the durable Error row — never a Done for a
    // child that did not finish.
    let seen = progress_until_terminal(&mut bed.events).await;
    let terminal = seen.last().expect("terminal advisory");
    assert_eq!(terminal.status, SubAgentProgressStatus::Failed);
    assert_eq!(terminal.invocation_id, result["prompt_id"]);

    bed.finish().await;
}

/// The watcher's terminal usage is the durable `TurnEnd` total, and the
/// settled summary carries the same number.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn terminal_usage_matches_durable_turn_total() {
    let usage = std::sync::Arc::new(crate::client::CompletionTokenUsage::new(
        Some(150),
        Some(37),
        Some(12),
    ));
    let call_fn: crate::agent_loop::AgentCallFn = Arc::new(move |_input, _config, _abort| {
        let usage = std::sync::Arc::clone(&usage);
        Box::pin(async move {
            Ok((
                "usage-bearing child".to_string(),
                None,
                vec![],
                (*usage).clone(),
            ))
        })
    });
    let Some(mut bed) = ProgressBed::spawn("async-progress-parent-6", call_fn).await else {
        return;
    };
    let result = bed.settled_send("asyncchild6", "parent-async-call-6").await;

    assert_eq!(result["status"], "completed", "{result}");
    let session_id = result["session_id"].as_str().unwrap().to_owned();
    let prompt_id = result["prompt_id"].as_str().unwrap().to_owned();
    // The child's durable terminal usage is the source of truth for both the
    // watcher's terminal event and the replay summary.
    let log = crate::nats_session_log::NatsSessionLog::new(
        bed.js.clone(),
        harnx_core::session_identity::session_key(Some("metis"), &session_id),
    );
    let entries = log.load_events_async().await.unwrap();
    let durable = entries
        .iter()
        .rev()
        .find_map(|(_, entry)| match entry {
            harnx_core::session::SessionLogEntry::TurnEnd { usage, .. } => usage.clone(),
            _ => None,
        })
        .expect("child turn recorded its TurnEnd usage");
    assert!(
        durable.input_tokens >= 150,
        "child usage recorded: {durable:?}"
    );

    let seen = progress_until_terminal(&mut bed.events).await;
    let terminal = seen.last().expect("terminal advisory");
    assert_eq!(terminal.status, SubAgentProgressStatus::Done);
    assert_eq!(terminal.invocation_id, prompt_id);
    assert_eq!(
        terminal.usage, durable,
        "watcher terminal usage is the durable total"
    );

    let summary: SubAgentProgress = serde_json::from_value(result["sub_agent_progress"].clone())
        .expect("settled send.wait carries a progress summary");
    assert_eq!(
        summary.usage, durable,
        "replay summary carries the durable total"
    );

    bed.finish().await;
}

/// A wait that settles a fast child still reports through the shared watcher
/// and attaches the same terminal summary the sync path attaches. A fast
/// child can settle before the watcher announces `Running`, so the terminal
/// may be the first advisory.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn send_with_wait_reports_progress_through_settlement() {
    let Some(mut bed) = ProgressBed::spawn(
        "async-progress-parent-4",
        fixed_prompt_call_fn("fast child"),
    )
    .await
    else {
        return;
    };
    let result = bed.settled_send("asyncchild4", "parent-async-call-4").await;

    assert_eq!(result["status"], "completed", "{result}");
    assert_eq!(result["terminal"], true);
    let summary: SubAgentProgress = serde_json::from_value(result["sub_agent_progress"].clone())
        .expect("settled send.wait carries a progress summary");
    assert_eq!(summary.status, SubAgentProgressStatus::Done);
    assert_eq!(summary.agent, "metis");
    assert_eq!(summary.session_id, "asyncchild4");

    let seen = progress_until_terminal(&mut bed.events).await;
    for progress in &seen {
        assert_eq!(
            progress.tool_call_id.as_deref(),
            Some("parent-async-call-4"),
            "every advisory names the parent's tool call"
        );
    }
    let terminal = seen.last().expect("terminal advisory");
    assert_eq!(terminal.status, SubAgentProgressStatus::Done);
    assert_eq!(terminal.invocation_id, result["prompt_id"]);

    bed.finish().await;
}
