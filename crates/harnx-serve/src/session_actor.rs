#![allow(dead_code)]

#[path = "session_actor_test_executor.rs"]
mod test_executor;
#[path = "session_actor_test_log.rs"]
mod test_log;

mod cancellation;
mod handoff;
mod registry;

pub use crate::session_actor_types::*;
pub use registry::SessionRegistry;
pub use test_log::load_test_session_messages;

use crate::ag_ui::{derive_thread_id, AgUiSink};
use ag_ui_core::{
    event::{BaseEvent, Event, RunErrorEvent, RunFinishedEvent, RunStartedEvent},
    types::{
        ids::{MessageId, RunId, ThreadId},
        message::Message as AgUiMessage,
    },
};
use anyhow::Context;
use chrono::Utc;
use dashmap::DashMap;
use harnx_core::{
    abort::{create_abort_signal, AbortSignal},
    tool::ToolResult,
};
use harnx_runtime::{
    config::{self, Config, GlobalConfig, SessionAttachmentPath, LOCAL_CLUSTER_KEY},
    local_orchestrator::{activation_route_for_cluster, LocalWorkerSupervisor},
    AgentCallFn, AgentLoopContext, NatsSession, NatsSessionConfig, OnToolRoundFn,
};
use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::{
    sync::{broadcast, mpsc, Mutex},
    time::{sleep_until, Instant, Sleep},
};
use tokio_util::task::AbortOnDropHandle;

const COMMAND_BUFFER: usize = 32;
const BROADCAST_BUFFER: usize = 64;
const FAR_FUTURE_SECS: u64 = 365 * 24 * 60 * 60;

type SessionMap = Arc<DashMap<SessionKey, SessionHandle>>;

static NEXT_ACTOR_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Clone)]
struct SessionActorConfig {
    base_config: Config,
    call_fn: Option<AgentCallFn>,
    /// One supervisor shared by every actor in this long-lived server.
    local_worker: Arc<Mutex<Option<LocalWorkerSupervisor>>>,
}

struct RunFinished {
    run_id: RunId,
    result: anyhow::Result<harnx_runtime::LoopResult>,
    sink: Arc<BroadcastEventSender>,
    thread_id: ThreadId,
    attachment_refs: Vec<String>,
}

struct HitlApprovalFinished {
    result: Result<bool, String>,
    reply: tokio::sync::oneshot::Sender<Result<bool, String>>,
}

struct ActorTurnParams {
    admitted: Option<harnx_runtime::nats_session::AppendedPrompt>,
    prompt_config: GlobalConfig,
    call_fn: Option<AgentCallFn>,
    abort_signal: AbortSignal,
    inject_rx: Option<mpsc::Receiver<String>>,
    working_dir: Option<std::path::PathBuf>,
    event_tx: broadcast::Sender<Event>,
    text: String,
    attachment_refs: Vec<String>,
    sink: Arc<BroadcastEventSender>,
    local_worker: Arc<Mutex<Option<LocalWorkerSupervisor>>>,
    agent: String,
    session_id: String,
}

struct SessionActor {
    key: SessionKey,
    actor_id: u64,
    registry: SessionMap,
    rx: mpsc::Receiver<SessionCommand>,
    broadcast_tx: broadcast::Sender<Event>,
    subscribers: usize,
    state: SessionState,
    execution_id: Option<String>,
    execution_state: Option<harnx_execution_control::OperationState>,
    pending: VecDeque<PendingPrompt>,
    active_run: Option<ActiveRun>,
    run_done_tx: mpsc::Sender<RunFinished>,
    run_done_rx: mpsc::Receiver<RunFinished>,
    hitl_approval_done_tx: mpsc::Sender<HitlApprovalFinished>,
    hitl_approval_done_rx: mpsc::Receiver<HitlApprovalFinished>,
    /// In-flight turn task, aborted on drop so a panicking or stopping actor doesn't leak it.
    /// Dropping the actor requests cancellation via `JoinHandle::abort`: the task is dropped at
    /// its next await, so a pending write may be dropped rather than completed, and a replacement
    /// actor can overlap until this task actually terminates. That bounds the double-writer window
    /// (issue #1468) but isn't a strict single-writer guarantee. That would need a registry-side
    /// join or actor-mediated writes.
    run_done_task: Option<AbortOnDropHandle<()>>,
    reap_ttl: Duration,
    reap_deadline: Option<Instant>,
    history_snapshot: Vec<AgUiMessage>,
    history_warnings: Vec<String>,
    /// Cached durable log entries for control-state hydration on promptless subscribe.
    /// Set by `refresh_history_snapshot` when loading from NATS.
    log_entries: Option<Vec<(u64, harnx_core::session::SessionLogEntry)>>,
    /// Cached session tokens usage for augmenting hydrated usage events with context fields.
    /// Captured during `refresh_history_snapshot` from the reconstructed session.
    tokens_usage: Option<crate::ag_ui::UsageContextSnapshot>,
    /// Canonical metadata state for replaying attached history without another metadata lookup.
    session_base: Option<harnx_core::session::Session>,
    actor_config: SessionActorConfig,
}

fn spawn_session_actor(
    key: SessionKey,
    registry: SessionMap,
    reap_ttl: Duration,
    actor_config: SessionActorConfig,
) -> SessionHandle {
    let (actor, handle) = make_session_actor(key, registry, reap_ttl, actor_config);
    tokio::spawn(actor.start());
    handle
}

fn make_session_actor(
    key: SessionKey,
    registry: SessionMap,
    reap_ttl: Duration,
    actor_config: SessionActorConfig,
) -> (SessionActor, SessionHandle) {
    let (tx, rx) = mpsc::channel(COMMAND_BUFFER);
    let (broadcast_tx, _) = broadcast::channel(BROADCAST_BUFFER);
    let (run_done_tx, run_done_rx) = mpsc::channel(COMMAND_BUFFER);
    let (hitl_approval_done_tx, hitl_approval_done_rx) = mpsc::channel(COMMAND_BUFFER);
    let actor_id = NEXT_ACTOR_ID.fetch_add(1, Ordering::Relaxed);
    let handle = SessionHandle {
        tx: tx.clone(),
        actor_id,
    };
    let actor = SessionActor {
        key,
        actor_id,
        registry,
        rx,
        broadcast_tx,
        subscribers: 0,
        state: SessionState::Idle,
        execution_id: None,
        execution_state: None,
        pending: VecDeque::new(),
        active_run: None,
        run_done_tx,
        run_done_rx,
        hitl_approval_done_tx,
        hitl_approval_done_rx,
        run_done_task: None,
        reap_ttl,
        reap_deadline: None,
        history_snapshot: Vec::new(),
        history_warnings: Vec::new(),
        log_entries: None,
        tokens_usage: None,
        session_base: None,
        actor_config,
    };
    (actor, handle)
}

async fn run_actor_turn(params: ActorTurnParams) -> anyhow::Result<harnx_runtime::LoopResult> {
    if let Some(call_fn) = params.call_fn {
        return test_executor::run_local_test_turn(test_executor::LocalTestTurnParams {
            prompt_config: params.prompt_config,
            call_fn,
            abort_signal: params.abort_signal,
            inject_rx: params
                .inject_rx
                .expect("test executor must have an injection receiver"),
            working_dir: params.working_dir,
            event_tx: params.event_tx,
            text: params.text,
            attachment_refs: params.attachment_refs,
            sink: params.sink,
        })
        .await;
    }

    let abort_signal = params.abort_signal.clone();
    let session = match open_actor_nats_session(
        &params.prompt_config,
        &params.local_worker,
        SessionKey {
            agent: params.agent.clone(),
            session: params.session_id.clone(),
        },
        params.abort_signal,
    )
    .await
    {
        Err(_) if abort_signal.aborted() => return Ok(harnx_runtime::LoopResult::Completed),
        result => result?,
    };
    if let Some(admitted) = params.admitted {
        return session
            .follow_admitted_prompt(admitted, params.sink, None, None, Default::default())
            .await
            .map(|_| harnx_runtime::LoopResult::Completed);
    }
    let input = build_input(&params.prompt_config, &params.text, &params.attachment_refs)?;
    let attachments_dir = if params.attachment_refs.is_empty() {
        None
    } else {
        Some(
            Config::session_attachments_dir(SessionAttachmentPath {
                agent_name: &params.agent,
                session_id: &params.session_id,
            })
            .context("invalid session ID for attachment lookup")?,
        )
    };
    session
        .run_turn_input(&input, attachments_dir.as_deref(), params.sink, None)
        .await
        .map(|_| harnx_runtime::LoopResult::Completed)
}

async fn open_actor_nats_session(
    prompt_config: &GlobalConfig,
    local_worker: &Arc<Mutex<Option<LocalWorkerSupervisor>>>,
    key: SessionKey,
    abort_signal: AbortSignal,
) -> anyhow::Result<NatsSession> {
    let activation_route =
        activation_route_for_cluster(LOCAL_CLUSTER_KEY, local_worker, abort_signal.clone()).await?;
    let initializer = {
        let config = prompt_config.read();
        harnx_runtime::SessionInitializer::named_from_config(key.agent, &config)
    };
    NatsSession::from_global_config(
        NatsSessionConfig {
            cluster: LOCAL_CLUSTER_KEY.to_string(),
            initializer,
            session_id: Some(key.session),
            activation_route,
        },
        prompt_config,
        abort_signal,
    )
    .await
}

fn test_injection_channel(
    enabled: bool,
) -> (Option<mpsc::Sender<String>>, Option<mpsc::Receiver<String>>) {
    if enabled {
        let (tx, rx) = mpsc::channel(COMMAND_BUFFER);
        (Some(tx), Some(rx))
    } else {
        (None, None)
    }
}
impl SessionActor {
    fn prompt_config(&self) -> GlobalConfig {
        prompt_config_for_agent_session_from_global(
            &self.actor_config.base_config,
            &self.key,
            self.actor_config.call_fn.is_some(),
        )
    }

    async fn route_hitl_approval_decision(
        actor_config: SessionActorConfig,
        key: SessionKey,
        tool_call_id: String,
        approved: bool,
        note: Option<String>,
    ) -> anyhow::Result<bool> {
        let prompt_config = prompt_config_for_agent_session_from_global(
            &actor_config.base_config,
            &key,
            actor_config.call_fn.is_some(),
        );
        let session = open_actor_nats_session(
            &prompt_config,
            &actor_config.local_worker,
            key,
            create_abort_signal(),
        )
        .await?;
        session
            .decide_hitl_approval(&tool_call_id, approved, note)
            .await
    }

    fn event_context(&self) -> SessionEventContext {
        SessionEventContext::new(
            self.actor_config.base_config.clone(),
            self.key.clone(),
            self.history_snapshot.clone(),
        )
    }

    async fn start(self) {
        let poll = cancellation::poller(self.actor_config.clone(), self.key.session.clone());
        self.run(poll).await;
    }

    async fn run(mut self, mut cancellation_poll: cancellation::CancellationPoller) {
        let far_future = Instant::now() + Duration::from_secs(FAR_FUTURE_SECS);
        let reap_sleep = sleep_until(far_future);
        tokio::pin!(reap_sleep);

        loop {
            tokio::select! {
                result = cancellation_poll.next(), if self.actor_config.call_fn.is_none() => {
                    if let Some(result) = result {
                        self.apply_cancellation_refresh(result);
                    }
                },
                maybe_cmd = self.rx.recv() => {
                    let Some(cmd) = maybe_cmd else {
                        if let Some(active_run) = &self.active_run {
                            active_run.abort_signal.set_ctrlc();
                        }
                        self.deregister();
                        break;
                    };
                    self.handle_command(cmd, &mut reap_sleep).await;
                    cancellation_poll.invalidate();
                }
                maybe_done = self.run_done_rx.recv() => {
                    let Some(done) = maybe_done else {
                        self.deregister();
                        break;
                    };
                    self.handle_run_done(done, &mut reap_sleep).await;
                    cancellation_poll.invalidate();
                }
                Some(done) = self.hitl_approval_done_rx.recv() => {
                    self.refresh_history_snapshot().await;
                    let _ = done.reply.send(done.result);
                    cancellation_poll.invalidate();
                }
                _ = &mut reap_sleep, if self.reap_deadline.is_some() => {
                    if self.reap_now() {
                        break;
                    }
                    if self.subscribers == 0 && !self.is_running() {
                        self.arm_reap(&mut reap_sleep);
                    } else {
                        self.cancel_reap(&mut reap_sleep);
                    }
                }
            }
        }
    }

    async fn handle_command(
        &mut self,
        cmd: SessionCommand,
        reap_sleep: &mut std::pin::Pin<&mut Sleep>,
    ) {
        match cmd {
            SessionCommand::Subscribe { reply } => {
                self.subscribers += 1;
                self.cancel_reap(reap_sleep);
                self.refresh_history_snapshot().await;
                self.refresh_cancellation().await;
                let _ = reply.send(SubscribeResult {
                    snapshot: self.history_snapshot.clone(),
                    history_warnings: self.history_warnings.clone(),
                    state: self.state.clone(),
                    events: self.broadcast_tx.subscribe(),
                    log_entries: self.log_entries.clone(),
                    tokens_usage: self.tokens_usage.clone(),
                    session_base: self.session_base.clone(),
                });
            }
            SessionCommand::Prompt {
                text,
                options,
                reply,
            } => {
                let result = self.handle_prompt(text, options, reap_sleep).await;
                let _ = reply.send(result);
            }
            command @ (SessionCommand::Cancel { .. }
            | SessionCommand::AbandonCancellation { .. }) => {
                self.answer_cancellation_command(command).await
            }
            SessionCommand::HitlApprovalDecision {
                tool_call_id,
                approved,
                note,
                reply,
            } => {
                let actor_config = self.actor_config.clone();
                let key = self.key.clone();
                let done_tx = self.hitl_approval_done_tx.clone();
                tokio::spawn(async move {
                    let result = Self::route_hitl_approval_decision(
                        actor_config,
                        key,
                        tool_call_id,
                        approved,
                        note,
                    )
                    .await
                    .map_err(|error| format!("{error:#}"));
                    let _ = done_tx.send(HitlApprovalFinished { result, reply }).await;
                });
            }
            SessionCommand::Get { reply } => {
                self.refresh_history_snapshot().await;
                self.refresh_cancellation().await;
                let _ = reply.send(self.session_info());
            }
            SessionCommand::Unsubscribe => {
                self.subscribers = self.subscribers.saturating_sub(1);
                if self.subscribers == 0 && !self.is_running() {
                    self.arm_reap(reap_sleep);
                }
            }
            #[cfg(test)]
            SessionCommand::EmitTestEvent { event } => {
                let _ = self.broadcast_tx.send(event);
            }
            #[cfg(test)]
            SessionCommand::Panic => panic!("test-triggered session actor panic"),
        }
    }

    async fn handle_prompt(
        &mut self,
        text: String,
        mut options: SessionPromptOptions,
        reap_sleep: &mut std::pin::Pin<&mut Sleep>,
    ) -> PromptResult {
        self.refresh_cancellation().await;
        if matches!(
            self.state,
            SessionState::Cancelling(_) | SessionState::CancelUnconfirmed(_)
        ) {
            return PromptResult::Rejected {
                reason: "session cancellation is pending; retry cancellation before prompting"
                    .into(),
            };
        }
        if self.actor_config.call_fn.is_none() {
            match self.admit_prompt(&text, &options).await {
                Ok(admitted) => options.admitted = Some(admitted),
                Err(error) => {
                    return PromptResult::Rejected {
                        reason: format!("{error:#}"),
                    }
                }
            }
        }
        let Some(active_run) = &self.active_run else {
            let run_id = self.start_run(text, options, reap_sleep).await;
            return PromptResult::Accepted {
                run_id: run_id.to_string(),
            };
        };

        // Test executors support same-turn injection. Real turns queue the
        // complete prompt for a fresh turn after completion.
        let run_id = active_run.run_id.clone();
        let injected = active_run
            .inject_tx
            .as_ref()
            .is_some_and(|tx| tx.try_send(text.clone()).is_ok());
        if !injected {
            self.pending.push_back(PendingPrompt { text, options });
        }
        PromptResult::Enqueued {
            run_id: run_id.to_string(),
        }
    }

    async fn handle_run_done(
        &mut self,
        done: RunFinished,
        reap_sleep: &mut std::pin::Pin<&mut Sleep>,
    ) {
        self.run_done_task = None;
        self.active_run = None;
        self.refresh_history_snapshot().await;
        match &done.result {
            Ok(harnx_runtime::LoopResult::Completed)
            | Ok(harnx_runtime::LoopResult::AwaitingHitlApproval { .. }) => {
                self.finish_completed_run(&done)
            }
            Ok(harnx_runtime::LoopResult::HandoffRequested {
                agent,
                session_id,
                prompt,
                tool_call_id,
            }) => {
                self.dispatch_handoff_to_target(
                    &done,
                    handoff::HandoffRequest {
                        agent: agent.clone(),
                        session_id: session_id.clone(),
                        prompt: prompt.clone(),
                        handoff_tool_call_id: tool_call_id.clone(),
                    },
                )
                .await;
            }
            Err(err) => {
                done.sink.sink.close_text_segment();
                let _ = self.broadcast_tx.send(Event::RunError(RunErrorEvent {
                    base: base_event(),
                    message: err.to_string(),
                    code: None,
                }));
                self.state = SessionState::Idle;
            }
        }
        self.replay_pending_or_arm_reap(reap_sleep).await;
    }

    fn finish_completed_run(&mut self, done: &RunFinished) {
        let result = match &self.state {
            SessionState::Interrupted { pending } => Some(serde_json::json!({
                "outcome": pending.metadata.clone()
            })),
            SessionState::Idle
            | SessionState::Running { .. }
            | SessionState::Cancelling(_)
            | SessionState::CancelUnconfirmed(_) => None,
        };
        self.finish_run(done, result);
        if matches!(
            self.state,
            SessionState::Idle | SessionState::Running { .. }
        ) {
            self.state = SessionState::Idle;
        }
    }

    fn finish_run(&self, done: &RunFinished, result: Option<serde_json::Value>) {
        done.sink.sink.close_text_segment();
        if let Some(message) = done.sink.sink.take_run_error() {
            let _ = self.broadcast_tx.send(Event::RunError(RunErrorEvent {
                base: base_event(),
                message,
                code: None,
            }));
        } else {
            let _ = self.broadcast_tx.send(Event::RunFinished(RunFinishedEvent {
                base: base_event(),
                thread_id: done.thread_id.clone(),
                run_id: done.run_id.clone(),
                result,
            }));
        }
    }

    async fn replay_pending_or_arm_reap(&mut self, reap_sleep: &mut std::pin::Pin<&mut Sleep>) {
        // Replay after every terminal outcome, including errors, interrupts, and
        // handoffs. Explicit cancellation clears the queue before aborting.
        if let Some(pending) = self.pending.pop_front() {
            self.start_run(pending.text, pending.options, reap_sleep)
                .await;
        } else if self.subscribers == 0 {
            self.arm_reap(reap_sleep);
        }
    }

    async fn start_run(
        &mut self,
        text: String,
        options: SessionPromptOptions,
        reap_sleep: &mut std::pin::Pin<&mut Sleep>,
    ) -> RunId {
        self.cancel_reap(reap_sleep);
        let prompt_config = self.prompt_config();
        let run_id = RunId::random();
        let thread_id = derive_thread_id(&self.key.session);
        let started_at = Utc::now();
        let abort_signal = create_abort_signal();
        let (inject_tx, inject_rx) = test_injection_channel(self.actor_config.call_fn.is_some());

        let _ = self.broadcast_tx.send(Event::RunStarted(RunStartedEvent {
            base: base_event(),
            thread_id: thread_id.clone(),
            run_id: run_id.clone(),
        }));

        let done_tx = self.run_done_tx.clone();
        let run_id_for_task = run_id.clone();
        let thread_id_for_task = thread_id.clone();
        let sink = Arc::new(BroadcastEventSender::new(
            self.broadcast_tx.clone(),
            MessageId::random(),
            self.event_context(),
        ));
        let sink_for_task = sink.clone();
        let attachment_refs = options.attachment_refs.clone();
        let turn = ActorTurnParams {
            admitted: options.admitted,
            prompt_config,
            call_fn: self.actor_config.call_fn.clone(),
            abort_signal: abort_signal.clone(),
            inject_rx,
            working_dir: options.working_dir,
            event_tx: self.broadcast_tx.clone(),
            text,
            attachment_refs: attachment_refs.clone(),
            sink: sink_for_task.clone(),
            local_worker: self.actor_config.local_worker.clone(),
            agent: self.key.agent.clone(),
            session_id: self.key.session.clone(),
        };
        let task = AbortOnDropHandle::new(tokio::spawn(async move {
            let loop_result = run_actor_turn(turn).await;
            let _ = done_tx
                .send(RunFinished {
                    run_id: run_id_for_task,
                    result: loop_result,
                    sink: sink_for_task,
                    thread_id: thread_id_for_task,
                    attachment_refs,
                })
                .await;
        }));

        self.run_done_task = Some(task);
        self.active_run = Some(ActiveRun {
            run_id: run_id.clone(),
            started_at,
            abort_signal,
            inject_tx,
        });
        self.state = SessionState::Running {
            run_id: run_id.to_string(),
            started_at,
        };
        run_id
    }

    fn session_info(&self) -> SessionInfo {
        SessionInfo {
            execution_id: self.execution_id.clone(),
            execution_state: self.execution_state,
            state: self.state.clone(),
            history_snapshot: self.history_snapshot.clone(),
            history_warnings: self.history_warnings.clone(),
            capabilities: SessionCapabilities {
                can_prompt: !matches!(
                    self.state,
                    SessionState::Cancelling(_) | SessionState::CancelUnconfirmed(_)
                ),
                can_cancel: true,
                supports_snapshot: true,
            },
        }
    }

    fn is_running(&self) -> bool {
        matches!(
            self.state,
            SessionState::Running { .. }
                | SessionState::Interrupted { .. }
                | SessionState::Cancelling(_)
                | SessionState::CancelUnconfirmed(_)
        )
    }

    /// Drop this actor's registry entry, leaving any replacement under the same key in place.
    fn deregister(&self) {
        self.registry
            .remove_if(&self.key, |_, handle| handle.actor_id == self.actor_id);
    }

    /// Whether this actor is done: its idle deadline has passed and it managed to deregister,
    /// which is the point of no return for a reap.
    fn reap_now(&self) -> bool {
        self.should_reap() && self.deregister_for_reap()
    }

    /// Deregister for reaping, but only if nobody outside the registry holds a handle.
    ///
    /// `strong_count == 1` means the registry's own sender is the last one, so no caller is
    /// mid-request. Any other count means a caller already cloned the handle and would hit a
    /// closed channel the moment this actor stops, which is exactly the spurious 503 the reap
    /// used to cause. DashMap holds the shard lock across both the check and the removal, and
    /// handing out a handle needs that same lock, so no caller can slip in between the two.
    fn deregister_for_reap(&self) -> bool {
        self.registry
            .remove_if(&self.key, |_, handle| {
                handle.actor_id == self.actor_id && handle.tx.strong_count() == 1
            })
            .is_some()
    }

    fn arm_reap(&mut self, reap_sleep: &mut std::pin::Pin<&mut Sleep>) {
        let deadline = Instant::now() + self.reap_ttl;
        self.reap_deadline = Some(deadline);
        reap_sleep.as_mut().reset(deadline);
    }

    fn cancel_reap(&mut self, reap_sleep: &mut std::pin::Pin<&mut Sleep>) {
        self.reap_deadline = None;
        reap_sleep
            .as_mut()
            .reset(Instant::now() + Duration::from_secs(FAR_FUTURE_SECS));
    }

    fn should_reap(&self) -> bool {
        self.reap_deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
            && self.subscribers == 0
            && !self.is_running()
            && self.rx.is_empty()
    }

    async fn refresh_history_snapshot(&mut self) {
        let (snapshot, warnings, log_entries, session_tokens_usage, session_base) = if self
            .actor_config
            .call_fn
            .is_some()
        {
            let prompt_config = prompt_config_for_agent_session_from_global(
                &self.actor_config.base_config,
                &self.key,
                true,
            );
            let snapshot = prompt_config
                .read()
                .session
                .as_ref()
                .map(|session| crate::ag_ui::history_messages_for_snapshot(&session.messages))
                .unwrap_or_default();
            // Test executor path: no NATS log entries available
            (snapshot, Vec::new(), None, None, None)
        } else {
            match crate::load_nats_session_with_base(
                &self.actor_config.base_config,
                &self.key.session,
            )
            .await
            {
                Ok((session, entries, base_session)) if session.agent_name.as_deref() == Some(self.key.agent.as_str()) => {
                    // Capture session tokens usage for context fields on hydrated usage events
                    let tokens_usage = Some(crate::ag_ui::UsageContextSnapshot::from_session(&session));
                    (
                        crate::ag_ui::history_messages_for_snapshot(&session.messages),
                        session.replay_warnings,
                        Some(entries),
                        tokens_usage,
                        Some(base_session),
                    )
                }
                Ok((session, _entries, _base_session)) => (
                    Vec::new(),
                    vec![format!(
                        "Failed to load session history: session belongs to agent '{}' rather than '{}'",
                        session.agent_name.as_deref().unwrap_or("unknown"),
                        self.key.agent
                    )],
                    None,
                    None,
                    None,
                ),
                Err(error) if error.to_string().contains("Not Found") => {
                    (Vec::new(), Vec::new(), None, None, None)
                }
                Err(error) => (
                    Vec::new(),
                    vec![format!("Failed to load session history: {error:#}")],
                    None,
                    None,
                    None,
                ),
            }
        };
        let derived_interrupt = log_entries
            .as_deref()
            .and_then(crate::ag_ui::derive_hitl_interrupt_outcome)
            .map(|metadata| SessionState::Interrupted {
                pending: Box::new(PendingInterrupt { metadata }),
            });
        if self.active_run.is_none() {
            self.state = derived_interrupt.unwrap_or(SessionState::Idle);
        }
        self.history_snapshot = snapshot;
        self.history_warnings = warnings;
        self.log_entries = log_entries;
        self.tokens_usage = session_tokens_usage;
        self.session_base = session_base;
    }
}

struct BroadcastEventSender {
    sink: AgUiSink,
}

#[derive(Clone)]
struct SessionEventContext {
    base_config: Config,
    key: SessionKey,
    history_snapshot: Vec<AgUiMessage>,
}

impl SessionEventContext {
    fn new(base_config: Config, key: SessionKey, history_snapshot: Vec<AgUiMessage>) -> Self {
        Self {
            base_config,
            key,
            history_snapshot,
        }
    }

    fn history_snapshot(&self) -> Vec<AgUiMessage> {
        self.history_snapshot.clone()
    }

    fn usage_context(&self) -> Option<crate::ag_ui::UsageContextSnapshot> {
        usage_context_snapshot(&self.base_config, &self.key)
    }
}

impl BroadcastEventSender {
    fn new(
        tx: broadcast::Sender<Event>,
        message_id: MessageId,
        session_event_context: SessionEventContext,
    ) -> Self {
        let history_context = session_event_context.clone();
        let history_snapshot = Arc::new(move || history_context.history_snapshot());
        let session_context = Arc::new(move || session_event_context.usage_context());
        Self {
            sink: AgUiSink::new_broadcast_with_snapshot_and_context(
                tx,
                message_id,
                history_snapshot,
                session_context,
            ),
        }
    }
}

impl harnx_core::event::AgentEventSink for BroadcastEventSender {
    fn emit(&self, event: harnx_core::event::AgentEvent) {
        self.sink.emit(event);
    }
}

fn build_loop_ctx(
    prompt_config: GlobalConfig,
    call_fn: Option<AgentCallFn>,
    abort_signal: AbortSignal,
    inject_rx: mpsc::Receiver<String>,
    working_dir: Option<std::path::PathBuf>,
    event_tx: broadcast::Sender<Event>,
) -> AgentLoopContext {
    let shared_injected_text = Arc::new(Mutex::new(inject_rx));
    let on_tool_round: OnToolRoundFn = Arc::new(move |merged_input, _results: &[ToolResult]| {
        let shared_injected_text = shared_injected_text.clone();
        let event_tx = event_tx.clone();
        Box::pin(async move {
            let mut inject_rx = shared_injected_text.lock().await;
            if let Ok(text) = inject_rx.try_recv() {
                merged_input.set_injected_user_text(text.clone());
                let _ = event_tx.send(Event::Custom(ag_ui_core::event::CustomEvent {
                    base: base_event(),
                    name: "pending_message_consumed".to_string(),
                    value: serde_json::json!({ "text": text }),
                }));
            }
            Ok(())
        })
    });
    harnx_session::build_context(
        prompt_config,
        call_fn,
        abort_signal,
        Some(on_tool_round),
        working_dir,
    )
}

fn prompt_config_for_agent_session_from_global(
    base_config: &Config,
    key: &SessionKey,
    test_memory_log: bool,
) -> GlobalConfig {
    let prompt_config = harnx_session::fork_prompt_config(base_config);
    {
        let mut cfg = prompt_config.write();
        cfg.use_agent_by_name(&key.agent).expect("set actor agent");
        if test_memory_log {
            cfg.use_session(Some(&key.session))
                .expect("set actor session");
            if let Some(session) = cfg.session.as_mut() {
                let sink = test_log::test_session_log(key);
                let entries = sink.entries();
                let runtime = std::sync::Arc::new(
                    sink as std::sync::Arc<dyn config::session::SessionAppendSink>,
                );
                if !entries.is_empty() {
                    let raw = entries.into_iter().enumerate().collect::<Vec<_>>();
                    let mut replayed =
                        config::session::replay_log_entries_for_external(&raw, &key.session)
                            .expect("replay test session log");
                    replayed.model = session.model.clone();
                    replayed.model_id = session.model_id.clone();
                    replayed.agent_name = session.agent_name.clone();
                    replayed.agent_instructions = session.agent_instructions.clone();
                    replayed.id = key.session.clone();
                    replayed.session_id = Some(key.session.clone());
                    replayed.runtime = Some(runtime);
                    *session = replayed;
                } else {
                    session.runtime = Some(runtime);
                }
            }
        } else {
            let mut session =
                config::session::new(&cfg, &key.session, None).expect("create NATS actor session");
            session.id = key.session.clone();
            session.session_id = Some(key.session.clone());
            cfg.session = Some(session);
        }
    }
    prompt_config
}

fn build_input(
    prompt_config: &GlobalConfig,
    text: &str,
    attachment_refs: &[String],
) -> anyhow::Result<harnx_core::input::Input> {
    let mut input = config::input::from_str(prompt_config, text, None);
    input.set_attachment_refs(attachment_refs.to_vec());
    Ok(input)
}

fn usage_context_snapshot(
    base_config: &Config,
    key: &SessionKey,
) -> Option<crate::ag_ui::UsageContextSnapshot> {
    let prompt_config = prompt_config_for_agent_session_from_global(base_config, key, false);
    let config = prompt_config.read();
    let session = config.session.as_ref()?;
    Some(crate::ag_ui::UsageContextSnapshot::from_session(session))
}

fn base_event() -> BaseEvent {
    BaseEvent {
        timestamp: None,
        raw_event: None,
    }
}

// Loads the base test config directly from `$HARNX_CONFIG_DIR/config.yaml`.
//
// This deliberately does NOT touch the process-global current directory and
// does NOT acquire the sandbox env lock. `load_from_file` takes an explicit
// path, so there is no shared mutable state to guard, and acquiring the lock
// here would self-deadlock: callers routinely hold a live `TestConfigSandbox`
// (which owns the same env lock guard for its whole lifetime) while calling
// this function.
pub(crate) fn load_base_config_for_tests() -> Config {
    let root = std::env::var_os("HARNX_CONFIG_DIR").expect("HARNX_CONFIG_DIR set");
    let config_file = std::path::PathBuf::from(&root).join("config.yaml");
    Config::load_from_file(&config_file).expect("load config")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{wait_for_state, TestConfigSandbox};
    use anyhow::anyhow;
    use harnx_core::{
        event::{AgentEvent, ContentBlock, ModelEvent},
        message::Message,
        tool::ToolCall,
    };
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::{
        sync::{oneshot, Notify},
        time::sleep,
    };

    fn key(agent: &str, session: &str) -> SessionKey {
        SessionKey {
            agent: agent.to_string(),
            session: session.to_string(),
        }
    }

    async fn subscribe(handle: &SessionHandle) -> SubscribeResult {
        let (reply_tx, reply_rx) = oneshot::channel();
        handle
            .tx
            .send(SessionCommand::Subscribe { reply: reply_tx })
            .await
            .expect("send subscribe");
        reply_rx.await.expect("recv subscribe reply")
    }

    async fn get_info(handle: &SessionHandle) -> SessionInfo {
        let (reply_tx, reply_rx) = oneshot::channel();
        handle
            .tx
            .send(SessionCommand::Get { reply: reply_tx })
            .await
            .expect("send get");
        reply_rx.await.expect("recv get reply")
    }

    async fn prompt(handle: &SessionHandle, text: &str) -> PromptResult {
        prompt_with_options(handle, text, SessionPromptOptions::default()).await
    }

    async fn prompt_with_options(
        handle: &SessionHandle,
        text: &str,
        options: SessionPromptOptions,
    ) -> PromptResult {
        let (reply_tx, reply_rx) = oneshot::channel();
        handle
            .tx
            .send(SessionCommand::Prompt {
                text: text.to_string(),
                options,
                reply: reply_tx,
            })
            .await
            .expect("send prompt");
        reply_rx.await.expect("recv prompt reply")
    }

    async fn cancel(handle: &SessionHandle) {
        let (reply_tx, reply_rx) = oneshot::channel();
        handle
            .tx
            .send(SessionCommand::Cancel {
                reply: reply_tx,
                expected_execution_id: None,
            })
            .await
            .expect("send cancel");
        reply_rx
            .await
            .expect("recv cancel reply")
            .expect("cancellation accepted");
    }

    fn registry_with_call_fn(call_fn: AgentCallFn) -> SessionRegistry {
        SessionRegistry::new_for_tests(
            load_base_config_for_tests(),
            Duration::from_millis(50),
            Some(call_fn),
        )
    }

    #[tokio::test]
    async fn hitl_approval_routing_survives_cancel_command() {
        harnx_core::require_nextest();
        let sandbox = TestConfigSandbox::new();
        sandbox.write_agent("plain", "You are plain.");
        let registry = SessionRegistry::new_for_tests(
            load_base_config_for_tests(),
            Duration::from_millis(50),
            None,
        );
        let local_worker = registry.local_worker_for_tests();
        let handle = registry.get_or_spawn(key("plain", "approval-mailbox"));
        // Establish the broker-backed actor before measuring lock independence;
        // cold broker startup is not part of the cancellation latency contract.
        wait_for_state(&handle, "ready for approval routing", |state| {
            matches!(state, SessionState::Idle)
        })
        .await;
        let worker_guard = local_worker.lock().await;
        let (approval_reply_tx, mut approval_reply_rx) = oneshot::channel();
        handle
            .tx
            .send(SessionCommand::HitlApprovalDecision {
                tool_call_id: "approval-call".to_string(),
                approved: true,
                note: None,
                reply: approval_reply_tx,
            })
            .await
            .expect("send approval decision");

        assert!(
            tokio::time::timeout(Duration::from_millis(20), &mut approval_reply_rx)
                .await
                .is_err(),
            "routing should still be waiting for the worker lock"
        );
        tokio::time::timeout(Duration::from_secs(1), cancel(&handle))
            .await
            .expect("Cancel must remain responsive while approval routing is in flight");

        drop(worker_guard);
        let result = tokio::time::timeout(Duration::from_secs(10), approval_reply_rx)
            .await
            .expect("approval routing must complete after Cancel")
            .expect("Cancel must not drop the approval reply")
            .expect("approval routing succeeds");
        assert!(!result, "the session has no pending approval to apply");
    }
    fn load_session_messages(agent: &str, session_id: &str) -> Vec<Message> {
        super::load_test_session_messages(agent, session_id)
    }

    async fn wait_for_session_messages(
        agent: &str,
        session_id: &str,
        predicate: impl Fn(&[Message]) -> bool,
    ) -> Vec<Message> {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let messages = load_session_messages(agent, session_id);
                if predicate(&messages) {
                    return messages;
                }
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for persisted session {agent}/{session_id}"))
    }

    async fn assert_persisted_user_message(agent: &str, session_id: &str, expected: &str) {
        let messages = wait_for_session_messages(agent, session_id, |messages| {
            messages
                .iter()
                .any(|msg| msg.role.is_user() && msg.content.to_text() == expected)
        })
        .await;
        let user_texts: Vec<String> = messages
            .iter()
            .filter(|msg| msg.role.is_user())
            .map(|msg| msg.content.to_text())
            .collect();
        assert!(user_texts.iter().any(|text| text == expected));
    }

    #[tokio::test]
    async fn session_actor_idle_prompt_runs_and_broadcasts_lifecycle() {
        let _guard = harnx_runtime::client::TestStateGuard::new(None).await;
        let sandbox = TestConfigSandbox::new();
        sandbox.write_agent("plain", "You are plain.");

        let call_fn: AgentCallFn = Arc::new(|_input, _config, _abort| {
            Box::pin(async {
                Ok((
                    "assistant1".to_string(),
                    None,
                    vec![],
                    harnx_runtime::client::CompletionTokenUsage::default(),
                ))
            })
        });
        let registry = registry_with_call_fn(call_fn);
        let handle = registry.get_or_spawn(key("plain", "idle-prompt"));
        let mut sub = subscribe(&handle).await.events;

        let result = prompt(&handle, "hello actor").await;
        let run_id = match result {
            PromptResult::Accepted { run_id } => run_id,
            other => panic!("expected Accepted, got {other:?}"),
        };

        let mut saw_started = false;
        let mut saw_finished = false;
        for _ in 0..4 {
            match sub.recv().await.expect("recv event") {
                Event::RunStarted(event) => {
                    assert_eq!(event.run_id.to_string(), run_id);
                    saw_started = true;
                }
                Event::RunFinished(event) => {
                    assert_eq!(event.run_id.to_string(), run_id);
                    saw_finished = true;
                    break;
                }
                _ => {}
            }
        }
        assert!(saw_started);
        assert!(saw_finished);
    }

    #[tokio::test]
    async fn session_actor_prompt_run_text_events_reach_subscriber() {
        let _guard = harnx_runtime::client::TestStateGuard::new(None).await;
        let sandbox = TestConfigSandbox::new();
        sandbox.write_agent("plain", "You are plain.");

        let call_fn: AgentCallFn = Arc::new(move |_input, _config, _abort| {
            Box::pin(async move {
                harnx_core::sink::emit_agent_event(harnx_core::event::AgentEvent::Model(
                    harnx_core::event::ModelEvent::MessageChunk {
                        blocks: vec![harnx_core::event::ContentBlock::Text(
                            "hello subscriber".to_string(),
                        )],
                    },
                ));
                Ok((
                    "done".to_string(),
                    None,
                    vec![],
                    harnx_runtime::client::CompletionTokenUsage::default(),
                ))
            })
        });
        let registry = registry_with_call_fn(call_fn);
        let handle = registry.get_or_spawn(key("plain", "subscriber-text"));
        let mut sub = subscribe(&handle).await;

        let accepted = prompt(&handle, "emit text").await;
        let run_id = match accepted {
            PromptResult::Accepted { run_id } => run_id,
            other => panic!("expected Accepted, got {other:?}"),
        };

        let mut saw_text = false;
        let mut saw_finished = false;
        for _ in 0..16 {
            let event = tokio::time::timeout(Duration::from_secs(2), sub.events.recv())
                .await
                .expect("recv timeout")
                .expect("event recv");
            match event {
                Event::TextMessageContent(content) => {
                    if content.delta.contains("hello subscriber") {
                        saw_text = true;
                    }
                }
                Event::RunFinished(finished) if finished.run_id.to_string() == run_id => {
                    saw_finished = true;
                    break;
                }
                Event::RunFinished(_) => {}
                _ => {}
            }
        }

        assert!(
            saw_text,
            "expected text content event on subscriber broadcast receiver"
        );
        assert!(saw_finished, "expected run finished event for prompted run");
    }

    #[tokio::test]
    async fn session_actor_running_prompt_injects_mid_loop() {
        let _guard = harnx_runtime::client::TestStateGuard::new(None).await;
        let sandbox = TestConfigSandbox::new();
        sandbox.write_agent("plain", "You are plain.");

        let first_tool_round_started = Arc::new(Notify::new());
        let release_first_tool_round = Arc::new(Notify::new());
        let second_call_release = Arc::new(Notify::new());
        let call_count = Arc::new(AtomicUsize::new(0));
        let call_fn: AgentCallFn = {
            let first_tool_round_started = first_tool_round_started.clone();
            let release_first_tool_round = release_first_tool_round.clone();
            let second_call_release = second_call_release.clone();
            let call_count = call_count.clone();
            Arc::new(move |input, _config, _abort| {
                let first_tool_round_started = first_tool_round_started.clone();
                let release_first_tool_round = release_first_tool_round.clone();
                let second_call_release = second_call_release.clone();
                let injected = input.injected_user_text.clone();
                let n = call_count.fetch_add(1, Ordering::SeqCst);
                Box::pin(async move {
                    if n == 0 {
                        first_tool_round_started.notify_one();
                        release_first_tool_round.notified().await;
                        Ok((
                            "tool round".to_string(),
                            None,
                            vec![ToolCall::new(
                                "noop".to_string(),
                                json!({}),
                                Some("inject-call".to_string()),
                                None,
                            )],
                            harnx_runtime::client::CompletionTokenUsage::default(),
                        ))
                    } else if n == 1 {
                        second_call_release.notified().await;
                        assert_eq!(injected.as_deref(), Some("queued follow-up"));
                        Ok((
                            "done after inject".to_string(),
                            None,
                            vec![],
                            harnx_runtime::client::CompletionTokenUsage::default(),
                        ))
                    } else {
                        Ok((
                            "done".to_string(),
                            None,
                            vec![],
                            harnx_runtime::client::CompletionTokenUsage::default(),
                        ))
                    }
                })
            })
        };
        let registry = registry_with_call_fn(call_fn);
        let handle = registry.get_or_spawn(key("plain", "inject"));
        let _sub = subscribe(&handle).await;

        let _ = prompt(&handle, "initial user request").await;
        first_tool_round_started.notified().await;
        let enqueued = prompt(&handle, "queued follow-up").await;
        assert!(matches!(enqueued, PromptResult::Enqueued { .. }));

        let mut events = subscribe(&handle).await.events;
        let event_watcher = tokio::spawn(async move {
            while let Ok(event) = events.recv().await {
                if let Event::Custom(custom) = event {
                    if custom.name == "pending_message_consumed"
                        && custom.value.get("text").and_then(|v| v.as_str())
                            == Some("queued follow-up")
                    {
                        return true;
                    }
                }
            }
            false
        });

        release_first_tool_round.notify_one();
        tokio::task::yield_now().await;
        second_call_release.notify_one();

        assert!(tokio::time::timeout(Duration::from_secs(5), event_watcher)
            .await
            .expect("timed out waiting for pending message event")
            .expect("watcher task panicked"));

        assert_persisted_user_message("plain", "inject", "queued follow-up").await;
    }

    #[tokio::test]
    async fn session_actor_running_prompt_preserves_multiple_mid_loop_injections_fifo() {
        let _guard = harnx_runtime::client::TestStateGuard::new(None).await;
        let sandbox = TestConfigSandbox::new();
        sandbox.write_agent("plain", "You are plain.");

        let first_tool_round_started = Arc::new(Notify::new());
        let release_first_tool_round = Arc::new(Notify::new());
        let second_tool_round_started = Arc::new(Notify::new());
        let release_second_tool_round = Arc::new(Notify::new());
        let third_tool_round_started = Arc::new(Notify::new());
        let release_third_tool_round = Arc::new(Notify::new());
        let seen_injected = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let call_count = Arc::new(AtomicUsize::new(0));
        let call_fn: AgentCallFn = {
            let first_tool_round_started = first_tool_round_started.clone();
            let release_first_tool_round = release_first_tool_round.clone();
            let second_tool_round_started = second_tool_round_started.clone();
            let release_second_tool_round = release_second_tool_round.clone();
            let third_tool_round_started = third_tool_round_started.clone();
            let release_third_tool_round = release_third_tool_round.clone();
            let seen_injected = seen_injected.clone();
            let call_count = call_count.clone();
            Arc::new(move |input, _config, _abort| {
                let first_tool_round_started = first_tool_round_started.clone();
                let release_first_tool_round = release_first_tool_round.clone();
                let second_tool_round_started = second_tool_round_started.clone();
                let release_second_tool_round = release_second_tool_round.clone();
                let third_tool_round_started = third_tool_round_started.clone();
                let release_third_tool_round = release_third_tool_round.clone();
                let seen_injected = seen_injected.clone();
                let call_count = call_count.clone();
                let injected = input.injected_user_text.clone();
                Box::pin(async move {
                    let n = call_count.fetch_add(1, Ordering::SeqCst);
                    seen_injected.lock().await.push(injected.clone());
                    match n {
                        0 => {
                            first_tool_round_started.notify_one();
                            release_first_tool_round.notified().await;
                            Ok((
                                "tool round one".to_string(),
                                None,
                                vec![ToolCall::new(
                                    "noop".to_string(),
                                    json!({}),
                                    Some("call-1".to_string()),
                                    None,
                                )],
                                harnx_runtime::client::CompletionTokenUsage::default(),
                            ))
                        }
                        1 => {
                            second_tool_round_started.notify_one();
                            release_second_tool_round.notified().await;
                            assert_eq!(injected.as_deref(), Some("second"));
                            Ok((
                                "tool round two".to_string(),
                                None,
                                vec![ToolCall::new(
                                    "noop".to_string(),
                                    json!({}),
                                    Some("call-2".to_string()),
                                    None,
                                )],
                                harnx_runtime::client::CompletionTokenUsage::default(),
                            ))
                        }
                        2 => {
                            third_tool_round_started.notify_one();
                            release_third_tool_round.notified().await;
                            assert_eq!(injected.as_deref(), Some("third"));
                            Ok((
                                "done after third".to_string(),
                                None,
                                vec![],
                                harnx_runtime::client::CompletionTokenUsage::default(),
                            ))
                        }
                        other => panic!("unexpected tool round {other}"),
                    }
                })
            })
        };
        let registry = registry_with_call_fn(call_fn);
        let handle = registry.get_or_spawn(key("plain", "inject-fifo"));
        let _sub = subscribe(&handle).await;

        let accepted = prompt(&handle, "initial user request").await;
        assert!(matches!(accepted, PromptResult::Accepted { .. }));
        first_tool_round_started.notified().await;

        let second_prompt = prompt(&handle, "second").await;
        assert!(matches!(second_prompt, PromptResult::Enqueued { .. }));
        let third_prompt = prompt(&handle, "third").await;
        assert!(matches!(third_prompt, PromptResult::Enqueued { .. }));

        release_first_tool_round.notify_one();
        second_tool_round_started.notified().await;
        release_second_tool_round.notify_one();
        third_tool_round_started.notified().await;
        release_third_tool_round.notify_one();
        wait_for_state(&handle, "idle", |state| *state == SessionState::Idle).await;

        let seen_injected = seen_injected.lock().await.clone();
        assert_eq!(
            seen_injected,
            vec![None, Some("second".to_string()), Some("third".to_string())]
        );

        let user_texts: Vec<String> = load_session_messages("plain", "inject-fifo")
            .iter()
            .filter(|msg| msg.role.is_user())
            .map(|msg| msg.content.to_text())
            .collect();
        assert_eq!(
            user_texts,
            vec![
                "initial user request".to_string(),
                "second".to_string(),
                "third".to_string()
            ]
        );
    }

    #[tokio::test]
    async fn session_actor_model_tool_call_executes_and_persists_results() {
        let _guard = harnx_runtime::client::TestStateGuard::new(None).await;
        let sandbox = TestConfigSandbox::new();
        sandbox.write_agent_with_front_matter(
            "plain",
            "model: openai:gpt-4o\nuse_tools: harnx_agent_session_history_read",
            "You are plain.",
        );

        let call_count = Arc::new(AtomicUsize::new(0));
        let seen_tool_results = Arc::new(tokio::sync::Mutex::new(Vec::<String>::new()));
        let call_fn: AgentCallFn = {
            let call_count = Arc::clone(&call_count);
            let seen_tool_results = Arc::clone(&seen_tool_results);
            Arc::new(move |input, _config, _abort| {
                let call_count = Arc::clone(&call_count);
                let seen_tool_results = Arc::clone(&seen_tool_results);
                let tool_results = input
                    .tool_calls()
                    .as_ref()
                    .map(|calls| calls.tool_results.clone())
                    .unwrap_or_default();
                Box::pin(async move {
                    let round = call_count.fetch_add(1, Ordering::SeqCst);
                    match round {
                        0 => Ok((
                            "searching history".to_string(),
                            None,
                            vec![ToolCall::new(
                                "harnx_agent_session_history_read".to_string(),
                                json!({"entry_type": "message", "limit": 5}),
                                Some("history-1".to_string()),
                                None,
                            )],
                            harnx_runtime::client::CompletionTokenUsage::default(),
                        )),
                        1 => {
                            let outputs = tool_results
                                .iter()
                                .map(|result| result.output.to_string())
                                .collect::<Vec<_>>();
                            *seen_tool_results.lock().await = outputs.clone();
                            assert_eq!(tool_results.len(), 1, "expected merged tool result");
                            assert_eq!(
                                tool_results[0].call.name,
                                "harnx_agent_session_history_read"
                            );
                            assert!(
                                outputs[0].contains("[]"),
                                "new NATS session history should initially be empty: {}",
                                outputs[0]
                            );
                            Ok((
                                "history checked".to_string(),
                                None,
                                vec![],
                                harnx_runtime::client::CompletionTokenUsage::default(),
                            ))
                        }
                        other => panic!("unexpected llm round {other}"),
                    }
                })
            })
        };

        let registry = registry_with_call_fn(call_fn);
        let handle = registry.get_or_spawn(key("plain", "tool-history"));
        let mut sub = subscribe(&handle).await.events;

        let result = prompt(&handle, "hello actor").await;
        let run_id = match result {
            PromptResult::Accepted { run_id } => run_id,
            other => panic!("expected Accepted, got {other:?}"),
        };

        loop {
            match sub.recv().await.expect("recv event") {
                Event::RunFinished(finished) if finished.run_id.to_string() == run_id => break,
                Event::RunFinished(_) => {}
                _ => {}
            }
        }

        assert_eq!(
            call_count.load(Ordering::SeqCst),
            2,
            "tool result must trigger follow-up LLM round"
        );
        let seen_tool_results = seen_tool_results.lock().await.clone();
        assert_eq!(
            seen_tool_results.len(),
            1,
            "expected one executed tool result"
        );
    }

    #[tokio::test]
    async fn session_actor_cancel_persists_and_returns_idle() {
        let _guard = harnx_runtime::client::TestStateGuard::new(None).await;
        let sandbox = TestConfigSandbox::new();
        sandbox.write_agent("plain", "You are plain.");

        let gate_ready = Arc::new(Notify::new());
        let gate_release = Arc::new(Notify::new());
        let call_fn: AgentCallFn = {
            let gate_ready = gate_ready.clone();
            let gate_release = gate_release.clone();
            Arc::new(move |_input, _config, _abort| {
                let gate_ready = gate_ready.clone();
                let gate_release = gate_release.clone();
                Box::pin(async move {
                    gate_ready.notify_one();
                    gate_release.notified().await;
                    Ok((
                        "tool before cancel".to_string(),
                        None,
                        vec![ToolCall::new(
                            "noop".to_string(),
                            json!({}),
                            Some("cancel-call".to_string()),
                            None,
                        )],
                        harnx_runtime::client::CompletionTokenUsage::default(),
                    ))
                })
            })
        };
        let registry = registry_with_call_fn(call_fn);
        let handle = registry.get_or_spawn(key("plain", "cancel"));
        let _sub = subscribe(&handle).await;

        let _ = prompt(&handle, "cancel me").await;
        gate_ready.notified().await;
        let info = get_info(&handle).await;
        assert!(matches!(info.state, SessionState::Running { .. }));
        cancel(&handle).await;
        gate_release.notify_one();

        let info = wait_for_state(&handle, "idle after cancellation", |state| {
            *state == SessionState::Idle
        })
        .await;
        assert_eq!(info.state, SessionState::Idle);

        let persisted = load_session_messages("plain", "cancel");
        let user_texts: Vec<String> = persisted
            .iter()
            .filter(|msg| msg.role.is_user())
            .map(|msg| msg.content.to_text())
            .collect();
        assert!(user_texts.iter().any(|text| text == "cancel me"));
    }

    #[tokio::test]
    async fn session_actor_finish_boundary_prompt_is_not_lost() {
        let _guard = harnx_runtime::client::TestStateGuard::new(None).await;
        let sandbox = TestConfigSandbox::new();
        sandbox.write_agent("plain", "You are plain.");

        let second_turn_started = Arc::new(Notify::new());
        let call_count = Arc::new(AtomicUsize::new(0));
        let call_fn: AgentCallFn = {
            let second_turn_started = second_turn_started.clone();
            let call_count = call_count.clone();
            Arc::new(move |input, _config, _abort| {
                let second_turn_started = second_turn_started.clone();
                let text = input.text();
                let n = call_count.fetch_add(1, Ordering::SeqCst);
                Box::pin(async move {
                    if n == 0 {
                        Ok((
                            "first response".to_string(),
                            None,
                            vec![],
                            harnx_runtime::client::CompletionTokenUsage::default(),
                        ))
                    } else {
                        second_turn_started.notify_one();
                        assert_eq!(text, "boundary prompt");
                        Ok((
                            "second response".to_string(),
                            None,
                            vec![],
                            harnx_runtime::client::CompletionTokenUsage::default(),
                        ))
                    }
                })
            })
        };
        let registry = registry_with_call_fn(call_fn);
        let handle = registry.get_or_spawn(key("plain", "boundary"));
        let _sub = subscribe(&handle).await;

        let _ = prompt(&handle, "first prompt").await;
        sleep(Duration::from_millis(5)).await;
        let enqueued = prompt(&handle, "boundary prompt").await;
        assert!(matches!(
            enqueued,
            PromptResult::Enqueued { .. } | PromptResult::Accepted { .. }
        ));
        second_turn_started.notified().await;
        wait_for_state(&handle, "idle after boundary prompt", |state| {
            *state == SessionState::Idle
        })
        .await;

        let user_texts: Vec<String> = load_session_messages("plain", "boundary")
            .iter()
            .filter(|msg| msg.role.is_user())
            .map(|msg| msg.content.to_text())
            .collect();
        assert!(user_texts.iter().any(|text| text == "boundary prompt"));
        assert!(call_count.load(Ordering::SeqCst) >= 2);
    }

    #[tokio::test]
    async fn session_actor_error_closes_text_segment_before_run_error() {
        let _guard = harnx_runtime::client::TestStateGuard::new(None).await;
        let sandbox = TestConfigSandbox::new();
        sandbox.write_agent("plain", "You are plain.");

        let call_fn: AgentCallFn = Arc::new(move |_input, _config, _abort| {
            Box::pin(async move {
                harnx_core::sink::emit_agent_event(AgentEvent::Model(ModelEvent::MessageChunk {
                    blocks: vec![ContentBlock::Text("partial text".to_string())],
                }));
                Err(anyhow!("plain failure after text"))
            })
        });

        let registry = registry_with_call_fn(call_fn);
        let handle = registry.get_or_spawn(key("plain", "error-close-text"));
        let mut sub = subscribe(&handle).await.events;

        let run_id = match prompt(&handle, "error after text").await {
            PromptResult::Accepted { run_id } => run_id,
            other => panic!("expected Accepted, got {other:?}"),
        };

        let mut event_types = Vec::new();
        tokio::time::timeout(Duration::from_secs(10), async {
            while let Ok(event) = sub.recv().await {
                let event_type = match &event {
                    Event::RunStarted(started) if started.run_id.to_string() == run_id => {
                        Some("RUN_STARTED")
                    }
                    Event::TextMessageContent(_) => Some("TEXT_MESSAGE_CONTENT"),
                    Event::TextMessageEnd(_) => Some("TEXT_MESSAGE_END"),
                    Event::RunError(_) => Some("RUN_ERROR"),
                    _ => None,
                };
                if let Some(event_type) = event_type {
                    event_types.push(event_type);
                    if event_type == "RUN_ERROR" {
                        break;
                    }
                }
            }
        })
        .await
        .expect("timed out waiting for run error");

        let text_end_index = event_types
            .iter()
            .position(|event| *event == "TEXT_MESSAGE_END")
            .expect("expected text segment end before run error");
        let run_error_index = event_types
            .iter()
            .position(|event| *event == "RUN_ERROR")
            .expect("expected run error event");
        assert!(
            text_end_index < run_error_index,
            "TEXT_MESSAGE_END must precede RUN_ERROR: {event_types:?}"
        );
    }

    #[tokio::test]
    async fn session_actor_get_reports_running_then_idle() {
        let _guard = harnx_runtime::client::TestStateGuard::new(None).await;
        let sandbox = TestConfigSandbox::new();
        sandbox.write_agent("plain", "You are plain.");

        let gate_ready = Arc::new(Notify::new());
        let gate_release = Arc::new(Notify::new());
        let call_fn: AgentCallFn = {
            let gate_ready = gate_ready.clone();
            let gate_release = gate_release.clone();
            Arc::new(move |_input, _config, _abort| {
                let gate_ready = gate_ready.clone();
                let gate_release = gate_release.clone();
                Box::pin(async move {
                    gate_ready.notify_one();
                    gate_release.notified().await;
                    Ok((
                        "done".to_string(),
                        None,
                        vec![],
                        harnx_runtime::client::CompletionTokenUsage::default(),
                    ))
                })
            })
        };
        let registry = registry_with_call_fn(call_fn);
        let handle = registry.get_or_spawn(key("plain", "state"));
        let _sub = subscribe(&handle).await;

        let result = prompt(&handle, "state prompt").await;
        let run_id = match result {
            PromptResult::Accepted { run_id } => run_id,
            other => panic!("expected Accepted, got {other:?}"),
        };
        gate_ready.notified().await;

        let running = get_info(&handle).await;
        match running.state {
            SessionState::Running {
                run_id: active_run_id,
                ..
            } => assert_eq!(active_run_id, run_id),
            other => panic!("expected Running state, got {other:?}"),
        }

        gate_release.notify_one();

        let idle = wait_for_state(&handle, "idle", |state| *state == SessionState::Idle).await;
        assert_eq!(idle.state, SessionState::Idle);
    }

    #[tokio::test]
    async fn session_actor_handoff_dispatches_to_target_actor() {
        // When HandoffRequested fires, the serve actor ends its run cleanly
        // and dispatches the handoff prompt to the TARGET session's actor.
        // The target actor then executes the delegated turn.
        //
        // This test uses the real handoff mechanism: the source agent has
        // `use_tools: target-agent_session_handoff` and the call_fn returns
        // a tool call with that name, triggering run_agent_loop to return
        // HandoffRequested. The SessionActor then re-dispatches to the target.
        let _guard = harnx_runtime::client::TestStateGuard::new(None).await;
        let sandbox = TestConfigSandbox::new();

        // Write source agent WITH handoff tool enabled - required for handoff detection
        sandbox.write_agent_with_front_matter(
            "source-agent",
            "model: openai:gpt-4o\nuse_tools: target-agent_session_handoff",
            "You are the source agent.",
        );
        sandbox.write_agent("target-agent", "You are the target agent.");
        if !crate::test_support::ensure_test_nats().await {
            return;
        }

        // Track invocations per agent
        let source_count = Arc::new(AtomicUsize::new(0));
        let target_count = Arc::new(AtomicUsize::new(0));
        let source_count_clone = source_count.clone();
        let target_count_clone = target_count.clone();

        // Stateful call_fn: source returns handoff tool call, target returns text
        let call_fn: AgentCallFn = Arc::new(move |_input, config, _abort| {
            let source_count = source_count_clone.clone();
            let target_count = target_count_clone.clone();
            let agent_name = config
                .read()
                .agent
                .as_ref()
                .map(|a| a.name().to_string())
                .unwrap_or_default();
            Box::pin(async move {
                if agent_name == "source-agent" {
                    source_count.fetch_add(1, Ordering::SeqCst);
                    // Return a handoff tool call - triggers HandoffRequested
                    Ok((
                        "handoff now".to_string(),
                        None,
                        vec![ToolCall::new(
                            "target-agent_session_handoff".to_string(),
                            json!({ "prompt": "delegated work" }),
                            Some("handoff-call-1".to_string()),
                            None,
                        )],
                        harnx_runtime::client::CompletionTokenUsage::default(),
                    ))
                } else if agent_name == "target-agent" {
                    target_count.fetch_add(1, Ordering::SeqCst);
                    // Target completes normally
                    Ok((
                        "target-response".to_string(),
                        None,
                        vec![],
                        harnx_runtime::client::CompletionTokenUsage::default(),
                    ))
                } else {
                    panic!("unexpected agent: {}", agent_name);
                }
            })
        });

        let registry = registry_with_call_fn(call_fn);
        let source_handle = registry.get_or_spawn(key("source-agent", "source-session"));
        let mut source_sub = subscribe(&source_handle).await.events;

        // Prompt the source agent
        let result = prompt(&source_handle, "start handoff").await;
        let source_run_id = match result {
            PromptResult::Accepted { run_id } => run_id,
            other => panic!("expected Accepted, got {other:?}"),
        };

        let mut handoff_session_id = None;

        // Wait for source run to finish (RUN_FINISHED) and observe handoff event
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                match source_sub.recv().await.expect("recv event") {
                    Event::RunFinished(finished) => {
                        if finished.run_id.to_string() == source_run_id {
                            // Source run completed
                            break;
                        }
                    }
                    Event::Custom(event) if event.name == "session_handoff" => {
                        assert_eq!(event.value["agent"].as_str(), Some("target-agent"));
                        handoff_session_id = event.value["session_id"].as_str().map(str::to_string);
                    }
                    _ => {}
                }
            }
        })
        .await
        .expect("timed out waiting for source run to finish");

        // Verify source call_fn was invoked exactly once
        assert_eq!(
            source_count.load(Ordering::SeqCst),
            1,
            "source agent should be invoked once"
        );

        let handoff_session_id =
            handoff_session_id.expect("session_handoff event should be emitted");
        assert_eq!(handoff_session_id.len(), 6);
        assert!(
            harnx_runtime::utils::session_name::decode_timestamp_session_id(&handoff_session_id)
                .is_some(),
            "handoff without an explicit session ID should reserve a short ID"
        );

        assert_persisted_user_message("target-agent", &handoff_session_id, "delegated work").await;
        assert_eq!(
            target_count.load(Ordering::SeqCst),
            1,
            "target agent should be invoked once by handoff dispatch"
        );

        // Verify the target actor was registered (created by handoff)
        assert!(
            registry.has_session(&key("target-agent", &handoff_session_id)),
            "target session should be registered in registry after handoff dispatch"
        );
    }

    #[path = "session_actor_panic_abort_tests.rs"]
    mod panic_abort_tests;

    #[path = "session_actor_registry_tests.rs"]
    mod registry_tests;

    // NATS-backed serve tests spawn a local worker subprocess via
    // `LocalWorkerSupervisor` (Unix process-group management), so they are
    // Unix-only. Gated to avoid unused-import/dead-code errors on Windows.
    #[cfg(unix)]
    #[path = "session_actor_nats_tests.rs"]
    mod nats_tests;
}
