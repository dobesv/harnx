//! Interruption races use barriers, never elapsed-time assumptions.
//!
//! A turn that began before a `Cancel` landed must not write behind it. Each
//! test interrupts the way a frontend does — one fenced `Cancel` append to the
//! session log, plus the abort signal the worker's own stream watcher fires on
//! seeing it — and then asserts against the log.
//!
//! The worker's appends expect the tail it last observed, so a `Cancel` that
//! arrived since turns each one into a `TurnInterrupted` rejection. That
//! observed tail is the `after_seq` the live-event sink seeds at activation,
//! which is why every sink here shares one.
mod common;

use anyhow::{Context, Result};
use harnx_core::{
    abort::AbortSignal,
    event::{AgentEvent, AgentEventSink, ModelEvent, TurnEvent},
    message::{MessageContent, MessageRole},
    session::SessionLogEntry,
    tool::ToolCall,
};
use harnx_runtime::{
    config::{session::SessionAppendSink, Config},
    nats_lease::{NatsLeaseAcquireParams, NatsLeaseConfig, NatsSessionLease},
    nats_session_log::NatsSessionLog,
    nats_worker::{FencedSessionLogSink, NatsSessionLogBackend},
    AgentLoopContext,
};
use serde_json::json;
use std::sync::{atomic::AtomicU64, Arc, Mutex};
use tokio::sync::Barrier;

struct Harness {
    _server: common::NatsServerHandle,
    js: async_nats::jetstream::Context,
    lease: Arc<NatsSessionLease>,
    /// Fired by the worker's stream watcher the moment it sees the `Cancel`.
    abort: AbortSignal,
}

impl Harness {
    async fn new() -> Result<Self> {
        let server = common::spawn_nats_server()
            .await?
            .context("nats-server required")?;
        let js = async_nats::jetstream::new(async_nats::connect(server.url()).await?);
        let lease = Arc::new(
            NatsSessionLease::acquire(NatsLeaseAcquireParams {
                jetstream: js.clone(),
                session_id: "race",
                worker_id: "worker".into(),
                generation: 1,
                config: NatsLeaseConfig::default(),
                session_metadata: None,
            })
            .await?
            .context("lease")?,
        );
        Ok(Self {
            _server: server,
            js,
            lease,
            abort: harnx_core::abort::create_abort_signal(),
        })
    }

    /// A worker sink whose observed tail is the log tail right now, exactly as
    /// one built at activation: the live-event sink seeds `after_seq` from the
    /// stream once, and every append through this sink advances it from there.
    /// Each turn gets its own, so a later turn cannot hand an earlier one a
    /// view of the log that skips the interruption in between.
    async fn sink(&self) -> Result<FencedSessionLogSink> {
        let tail = self.entries().await?.last().map_or(0, |(seq, _)| *seq);
        Ok(FencedSessionLogSink::new(
            NatsSessionLogBackend::new(self.js.clone(), "race", 1)
                .with_after_seq_observer(Arc::new(AtomicU64::new(tail))),
            self.lease.clone(),
        ))
    }

    /// Interrupt the way a frontend does: append one `Cancel` to the log, then
    /// fire the abort signal the worker's watcher fires on seeing it.
    async fn interrupt(&self) -> Result<u64> {
        let seq = NatsSessionLog::new_with_replicas(self.js.clone(), "race", 1)
            .append_event_async(&SessionLogEntry::cancel_request(
                "cancel-1".into(),
                "client:test".into(),
            ))
            .await?;
        self.abort.set_ctrlc();
        Ok(seq)
    }

    async fn entries(&self) -> Result<Vec<(u64, SessionLogEntry)>> {
        NatsSessionLog::new_with_replicas(self.js.clone(), "race", 1)
            .load_events_latest_async()
            .await
    }

    async fn admit_prompt(&self) -> Result<harnx_core::session::Session> {
        NatsSessionLog::new_with_replicas(self.js.clone(), "race", 1)
            .append_event_async(&SessionLogEntry::Message {
                id: Some("opening-prompt".into()),
                role: MessageRole::User,
                content: MessageContent::Text("work".into()),
                timestamp: None,
                fence_token: None,
            })
            .await?;
        let entries = self
            .entries()
            .await?
            .into_iter()
            .map(|(seq, entry)| (seq as usize, entry))
            .collect::<Vec<_>>();
        harnx_runtime::config::session::replay_log_entries_for_external(&entries, "race")
    }
}

/// Every append a turn makes after its interruption is rejected as one.
fn assert_interrupted(result: Result<u64>) {
    let error = result.expect_err("append behind a Cancel must be rejected");
    assert!(
        format!("{error:#}").contains("turn interrupted by a Cancel"),
        "expected a turn-interruption rejection, got {error:#}"
    );
}

fn assistant(text: &str) -> SessionLogEntry {
    SessionLogEntry::Message {
        id: None,
        role: MessageRole::Assistant,
        content: MessageContent::Text(text.into()),
        timestamp: None,
        fence_token: None,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_vs_transcript_same_lease_new_generation_succeeds() -> Result<()> {
    let h = Harness::new().await?;
    let old = h.sink().await?;
    let entered = Arc::new(Barrier::new(2));
    let release = Arc::new(Barrier::new(2));
    let worker = {
        let (entered, release) = (entered.clone(), release.clone());
        tokio::spawn(async move {
            entered.wait().await;
            release.wait().await;
            [
                assistant("stale assistant"),
                SessionLogEntry::ToolResults {
                    results: vec![],
                    timestamp: None,
                },
                SessionLogEntry::SubAgentStarted {
                    agent: "helper".into(),
                    session_id: "stale-child".into(),
                    invocation_id: Some("call".into()),
                    tool_call_id: Some("tool".into()),
                    started_at: None,
                },
            ]
            .map(|entry| old.append(&entry))
        })
    };
    entered.wait().await;
    let cancel_seq = h.interrupt().await?;
    // The next turn starts from the interrupted log, so its own sink observes
    // the Cancel as the tail and appends in front of it.
    h.sink().await?.append(&assistant("new generation"))?;
    release.wait().await;
    for result in worker.await? {
        assert_interrupted(result);
    }
    assert!(h.lease.is_held(), "same lease remains valid");
    let entries = h.entries().await?;
    assert_eq!(entries.len(), 2);
    assert!(matches!(&entries[0], (seq, SessionLogEntry::Cancel { .. }) if *seq == cancel_seq));
    assert!(
        matches!(&entries[1].1, SessionLogEntry::Message { content, .. } if content.to_text() == "new generation")
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stale_lifecycle_reducer_cannot_end_new_turn() -> Result<()> {
    let h = Harness::new().await?;
    let sink = h.sink().await?;
    let release = Arc::new(Barrier::new(2));
    let late = {
        let release = release.clone();
        tokio::spawn(async move {
            release.wait().await;
            sink.append(&SessionLogEntry::TurnEnd {
                through_seq: 1,
                fence_token: 0,
                timestamp: None,
                usage: None,
            })
        })
    };
    h.interrupt().await?;
    h.sink().await?.append(&assistant("next turn"))?;
    release.wait().await;
    assert_interrupted(late.await?);
    assert!(!h
        .entries()
        .await?
        .iter()
        .any(|(_, entry)| matches!(entry, SessionLogEntry::TurnEnd { .. })));
    Ok(())
}

#[derive(Default)]
struct Events(Mutex<Vec<AgentEvent>>);
impl AgentEventSink for Events {
    fn emit(&self, event: AgentEvent) {
        self.0.lock().unwrap().push(event);
    }
}

async fn cancel_model(tool_calls: bool) -> Result<()> {
    let h = Harness::new().await?;
    let mut config = Config::default();
    let mut session = h.admit_prompt().await?;
    session.runtime = Some(Arc::new(
        Arc::new(h.sink().await?) as Arc<dyn SessionAppendSink>
    ));
    config.session = Some(session);
    let config = Arc::new(parking_lot::RwLock::new(config));
    let entered = Arc::new(Barrier::new(2));
    let release = Arc::new(Barrier::new(2));
    let call = paused_model(entered.clone(), release.clone(), tool_calls);
    let ctx = model_context(config.clone(), h.abort.clone(), call);
    let mut input = harnx_runtime::config::input::from_str(&config, "work", None);
    input.skip_user_log_append = true;
    let admitted = h.entries().await?;
    let events = Arc::new(Events::default());
    let worker = {
        let events = events.clone();
        tokio::spawn(async move {
            harnx_core::sink::with_agent_event_sink(
                events,
                harnx_runtime::run_agent_loop(&ctx, input),
            )
            .await
        })
    };
    entered.wait().await;
    let cancel_seq = h.interrupt().await?;
    release.wait().await;
    let error = worker.await?.err().context("must interrupt")?;
    assert!(
        format!("{error:#}").contains("interrupted"),
        "expected an interruption, got {error:#}"
    );
    let entries = h.entries().await?;
    let (interrupted, tail) = entries.split_at(entries.len() - 1);
    assert_eq!(
        serde_json::to_value(interrupted)?,
        serde_json::to_value(admitted)?,
        "admitted input survives; no assistant, tool calls, results, or TurnEnd"
    );
    assert!(matches!(&tail[0], (seq, SessionLogEntry::Cancel { .. }) if *seq == cancel_seq));
    assert_eq!(config.read().session.as_ref().unwrap().messages.len(), 1);
    let events = events.0.lock().unwrap();
    assert!(!events.iter().any(|event| matches!(
        event,
        AgentEvent::Model(ModelEvent::Final { .. })
            | AgentEvent::Turn(TurnEvent::Ended { .. })
            | AgentEvent::Tool(_)
    )));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_during_model_stream_persists_nothing_from_the_model() -> Result<()> {
    cancel_model(false).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_vs_model_persist_with_returned_tools() -> Result<()> {
    cancel_model(true).await
}

struct PausedTool {
    entered: Arc<Barrier>,
    release: Arc<Barrier>,
    failure: bool,
}
#[async_trait::async_trait]
impl harnx_core::tool::ToolProvider for PausedTool {
    fn name(&self) -> &str {
        "paused"
    }
    fn has_tool(&self, name: &str) -> bool {
        name == "paused"
    }
    async fn call_tool(
        &self,
        _name: &str,
        _arguments: serde_json::Value,
        _abort: &harnx_core::abort::AbortSignal,
    ) -> Result<harnx_core::tool::ToolProviderOutput, harnx_core::tool::ToolError> {
        self.entered.wait().await;
        self.release.wait().await;
        if self.failure {
            return Err(harnx_core::tool::ToolError::Recoverable(anyhow::anyhow!(
                "late failure"
            )));
        }
        Ok(harnx_core::tool::ToolProviderOutput::new(json!(
            "late success"
        )))
    }
}

async fn cancel_tool_output(failure: bool) -> Result<()> {
    let h = Harness::new().await?;
    let config = Arc::new(parking_lot::RwLock::new(Config::default()));
    let scope = harnx_core::instance::ServerScope::new();
    let mut eval = harnx_runtime::tool::build_tool_eval_context(
        harnx_runtime::tool::BuildToolEvalContextParams::new(&config, &scope),
    )
    .await;
    let entered = Arc::new(Barrier::new(2));
    let release = Arc::new(Barrier::new(2));
    eval.providers = vec![Arc::new(PausedTool {
        entered: entered.clone(),
        release: release.clone(),
        failure,
    })];
    eval.allowed_tool_names.insert("paused".into());
    let post_hooks = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let results = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    eval.dispatch_hook_fn = {
        let count = post_hooks.clone();
        Arc::new(move |event| {
            if !matches!(event, harnx_core::hooks::HookEvent::PreToolUse { .. }) {
                count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
            Box::pin(async {
                harnx_core::hooks::HookOutcome {
                    control: harnx_core::hooks::HookResultControl::Continue,
                    result: Default::default(),
                }
            })
        })
    };
    eval.emit_tool_call_fn = Arc::new(|_, _| {});
    eval.emit_tool_result_fn = {
        let count = results.clone();
        Arc::new(move |_, _| {
            count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        })
    };
    let abort = h.abort.clone();
    let worker = tokio::spawn(async move {
        harnx_engine::tool::eval_tool_calls(
            &eval,
            vec![ToolCall::new(
                "paused".into(),
                json!({"payload": "x".repeat(192 * 1024)}),
                Some("call".into()),
                None,
            )],
            &abort,
        )
        .await
    });
    entered.wait().await;
    let cancel_seq = h.interrupt().await?;
    release.wait().await;
    let error = worker.await?.err().context("tool must interrupt")?;
    assert!(
        format!("{error:#}").contains("interrupted"),
        "expected an interruption, got {error:#}"
    );
    assert_eq!(post_hooks.load(std::sync::atomic::Ordering::SeqCst), 0);
    assert_eq!(results.load(std::sync::atomic::Ordering::SeqCst), 0);
    assert!(
        matches!(h.entries().await?.last(), Some((seq, SessionLogEntry::Cancel { .. })) if *seq == cancel_seq)
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_after_handler_suppresses_post_hooks_and_success_events() -> Result<()> {
    cancel_tool_output(false).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_after_handler_suppresses_recoverable_failure() -> Result<()> {
    cancel_tool_output(true).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_during_pre_hook_prevents_queued_dispatch() -> Result<()> {
    let h = Harness::new().await?;
    let config = Arc::new(parking_lot::RwLock::new(Config::default()));
    let scope = harnx_core::instance::ServerScope::new();
    let mut eval = harnx_runtime::tool::build_tool_eval_context(
        harnx_runtime::tool::BuildToolEvalContextParams::new(&config, &scope),
    )
    .await;
    let entered = Arc::new(Barrier::new(2));
    let release = Arc::new(Barrier::new(2));
    eval.dispatch_hook_fn = {
        let (entered, release) = (entered.clone(), release.clone());
        Arc::new(move |_| {
            let (entered, release) = (entered.clone(), release.clone());
            Box::pin(async move {
                entered.wait().await;
                release.wait().await;
                harnx_core::hooks::HookOutcome {
                    control: harnx_core::hooks::HookResultControl::Continue,
                    result: Default::default(),
                }
            })
        })
    };
    eval.emit_tool_call_fn = Arc::new(|_, _| panic!("stopped dispatch emitted start"));
    eval.emit_tool_result_fn = Arc::new(|_, _| panic!("stopped dispatch emitted result"));
    let abort = h.abort.clone();
    let worker = tokio::spawn(async move {
        harnx_engine::tool::eval_tool_calls(
            &eval,
            vec![ToolCall::new(
                "never_dispatch".into(),
                json!({}),
                Some("call".into()),
                None,
            )],
            &abort,
        )
        .await
    });
    entered.wait().await;
    h.interrupt().await?;
    release.wait().await;
    let error = worker
        .await?
        .err()
        .context("queued dispatch must interrupt")?;
    assert!(
        format!("{error:#}").contains("interrupted"),
        "expected an interruption, got {error:#}"
    );
    Ok(())
}

fn paused_model(
    entered: Arc<Barrier>,
    release: Arc<Barrier>,
    tool_calls: bool,
) -> harnx_runtime::AgentCallFn {
    let (entered, release) = (entered.clone(), release.clone());
    Arc::new(move |_input, _config, _abort| {
        let (entered, release) = (entered.clone(), release.clone());
        Box::pin(async move {
            entered.wait().await;
            release.wait().await;
            let calls = if tool_calls {
                vec![ToolCall::new(
                    "never_dispatch".into(),
                    json!({}),
                    Some("tool".into()),
                    None,
                )]
            } else {
                vec![]
            };
            Ok(("late completion".into(), None, calls, Default::default()))
        })
    })
}

fn model_context(
    config: harnx_runtime::config::GlobalConfig,
    abort_signal: AbortSignal,
    call: harnx_runtime::AgentCallFn,
) -> AgentLoopContext {
    AgentLoopContext {
        config,
        instance_id: harnx_core::instance::ServerScope::new(),
        abort_signal,
        token_budget: None,
        usage_at_start: Default::default(),
        call_fn: Some(call),
        on_tool_round: None,
        on_hitl_approval_required: None,
        on_text_response: None,
        initial_with_embeddings: false,
        initial_resume_count: 0,
        max_resume: Some(0),
        nats_hook_provider: None,
        pending_async_context: None,
        working_dir: None,
    }
}
