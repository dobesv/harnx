#![allow(dead_code)]

#[path = "session_actor_test_executor.rs"]
mod test_executor;

use crate::ag_ui::AgUiSink;
use ag_ui_core::{
    event::{BaseEvent, Event, RunErrorEvent, RunFinishedEvent, RunStartedEvent},
    types::{
        ids::{MessageId, RunId, ThreadId},
        message::Message as AgUiMessage,
    },
};
use chrono::{DateTime, Utc};
use dashmap::{mapref::entry::Entry, DashMap};
use harnx_core::{
    abort::{create_abort_signal, AbortSignal},
    message::MessageContent,
    sink::with_agent_event_sink,
    tool::{ToolCall, ToolResult},
};
use harnx_runtime::{
    config::{self, Config, GlobalConfig, LOCAL_CLUSTER_KEY},
    continue_agent_loop_from_tool_round,
    local_orchestrator::{ensure_local_worker, LocalWorkerSupervisor},
    AgentCallFn, AgentLoopContext, OnToolRoundFn, ThinClientConfig, ThinClientSession,
    ToolApprovalDecision, ToolApprovalInterrupt, ToolUseConfirmation,
};
use std::{
    collections::VecDeque,
    hash::{Hash, Hasher},
    sync::Arc,
    time::Duration,
};
use tokio::{
    sync::{broadcast, mpsc, oneshot, Mutex},
    task::JoinHandle,
    time::{sleep_until, Instant, Sleep},
};

const DEFAULT_REAP_TTL: Duration = Duration::from_secs(5);
const COMMAND_BUFFER: usize = 32;
const BROADCAST_BUFFER: usize = 64;
const FAR_FUTURE_SECS: u64 = 365 * 24 * 60 * 60;

#[derive(Clone, Debug, Eq)]
pub struct SessionKey {
    pub agent: String,
    pub session: String,
}

impl PartialEq for SessionKey {
    fn eq(&self, other: &Self) -> bool {
        self.agent == other.agent && self.session == other.session
    }
}

impl Hash for SessionKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.agent.hash(state);
        self.session.hash(state);
    }
}

#[derive(Clone)]
pub struct SessionHandle {
    pub tx: mpsc::Sender<SessionCommand>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SessionPromptOptions {
    pub working_dir: Option<std::path::PathBuf>,
    pub attachment_refs: Vec<String>,
    pub resume: Vec<InterruptResume>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InterruptResume {
    pub interrupt_id: String,
    pub status: InterruptResumeStatus,
    pub payload: InterruptResumePayload,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InterruptResumeStatus {
    Approved,
    Denied,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InterruptResumePayload {
    pub approved: bool,
    pub reason: Option<String>,
}

pub enum SessionCommand {
    Subscribe {
        reply: oneshot::Sender<SubscribeResult>,
    },
    Prompt {
        text: String,
        options: SessionPromptOptions,
        reply: oneshot::Sender<PromptResult>,
    },
    Cancel {
        reply: oneshot::Sender<()>,
    },
    Get {
        reply: oneshot::Sender<SessionInfo>,
    },
    Unsubscribe,
    #[cfg(test)]
    EmitTestEvent {
        event: Event,
    },
}

pub struct SubscribeResult {
    pub snapshot: Vec<AgUiMessage>,
    pub events: broadcast::Receiver<Event>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PromptResult {
    Accepted { run_id: String },
    Enqueued { run_id: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionCapabilities {
    pub can_prompt: bool,
    pub can_cancel: bool,
    pub supports_snapshot: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SessionInfo {
    pub state: SessionState,
    pub history_snapshot: Vec<AgUiMessage>,
    pub capabilities: SessionCapabilities,
}

#[derive(Clone, Debug)]
pub enum SessionState {
    Idle,
    Running {
        run_id: String,
        started_at: DateTime<Utc>,
    },
    Interrupted {
        run_id: String,
        started_at: DateTime<Utc>,
        pending: Box<PendingInterruptBatch>,
    },
}

impl PartialEq for SessionState {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Idle, Self::Idle) => true,
            (
                Self::Running {
                    run_id: left_run_id,
                    started_at: left_started_at,
                },
                Self::Running {
                    run_id: right_run_id,
                    started_at: right_started_at,
                },
            ) => left_run_id == right_run_id && left_started_at == right_started_at,
            (
                Self::Interrupted {
                    run_id: left_run_id,
                    started_at: left_started_at,
                    ..
                },
                Self::Interrupted {
                    run_id: right_run_id,
                    started_at: right_started_at,
                    ..
                },
            ) => left_run_id == right_run_id && left_started_at == right_started_at,
            _ => false,
        }
    }
}

/// Pending interrupt batch for tool approval HITL.
#[derive(Clone, Debug)]
pub struct PendingInterruptBatch {
    pub interrupt_run_id: String,
    pub text: String,
    pub attachment_refs: Vec<String>,
    pub completion_output: String,
    pub completion_thought: Option<String>,
    pub tool_calls: Vec<harnx_core::tool::ToolCall>,
    pub interrupts: Vec<ToolApprovalInterruptEntry>,
    pub metadata: serde_json::Value,
}

#[derive(Clone, Debug)]
pub struct ToolApprovalInterruptEntry {
    pub id: String,
    pub tool_call_id: String,
    pub name: String,
    pub arguments: serde_json::Value,
    pub message: String,
    pub reason: Option<String>,
}

#[derive(Clone)]
pub struct SessionRegistry {
    map: Arc<DashMap<SessionKey, SessionHandle>>,
    reap_ttl: Duration,
    actor_config: SessionActorConfig,
}

#[derive(Clone)]
struct SessionActorConfig {
    base_config: Config,
    call_fn: Option<AgentCallFn>,
    /// One supervisor shared by every actor in this long-lived server.
    local_worker: Arc<Mutex<Option<LocalWorkerSupervisor>>>,
}

struct ActiveRun {
    run_id: RunId,
    started_at: DateTime<Utc>,
    abort_signal: AbortSignal,
    inject_tx: Option<mpsc::Sender<String>>,
}

struct PendingPrompt {
    text: String,
    options: SessionPromptOptions,
}

struct RunFinished {
    run_id: RunId,
    result: anyhow::Result<harnx_runtime::LoopResult>,
    sink: Arc<BroadcastEventSender>,
    thread_id: ThreadId,
    attachment_refs: Vec<String>,
}

struct ActorTurnParams {
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

impl SessionRegistry {
    pub fn new(base_config: Config) -> Self {
        Self::with_reap_ttl(base_config, DEFAULT_REAP_TTL)
    }

    pub fn with_reap_ttl(base_config: Config, reap_ttl: Duration) -> Self {
        Self {
            map: Arc::new(DashMap::new()),
            reap_ttl,
            actor_config: SessionActorConfig {
                base_config,
                call_fn: None,
                local_worker: Arc::new(Mutex::new(None)),
            },
        }
    }

    fn with_options(reap_ttl: Duration, actor_config: SessionActorConfig) -> Self {
        Self {
            map: Arc::new(DashMap::new()),
            reap_ttl,
            actor_config,
        }
    }

    pub fn new_for_tests(
        base_config: Config,
        reap_ttl: Duration,
        call_fn: Option<AgentCallFn>,
    ) -> Self {
        Self::with_options(
            reap_ttl,
            SessionActorConfig {
                base_config,
                call_fn,
                local_worker: Arc::new(Mutex::new(None)),
            },
        )
    }

    // Only used by the Unix-only NATS serve tests (`nats_tests`).
    #[cfg(all(test, unix))]
    fn new_with_local_worker_for_tests(
        base_config: Config,
        supervisor: LocalWorkerSupervisor,
    ) -> Self {
        Self::with_options(
            DEFAULT_REAP_TTL,
            SessionActorConfig {
                base_config,
                call_fn: None,
                local_worker: Arc::new(Mutex::new(Some(supervisor))),
            },
        )
    }

    pub fn has_session(&self, key: &SessionKey) -> bool {
        self.map.contains_key(key)
    }

    pub fn get_or_spawn(&self, key: SessionKey) -> SessionHandle {
        match self.map.entry(key.clone()) {
            Entry::Occupied(entry) => entry.get().clone(),
            Entry::Vacant(entry) => {
                let handle = spawn_session_actor(
                    key,
                    Arc::clone(&self.map),
                    self.reap_ttl,
                    self.actor_config.clone(),
                );
                entry.insert(handle.clone());
                handle
            }
        }
    }

    pub fn contains(&self, key: &SessionKey) -> bool {
        self.map.contains_key(key)
    }
}

impl Default for SessionRegistry {
    fn default() -> Self {
        Self::new(Config::default())
    }
}

struct SessionActor {
    key: SessionKey,
    registry: Arc<DashMap<SessionKey, SessionHandle>>,
    rx: mpsc::Receiver<SessionCommand>,
    broadcast_tx: broadcast::Sender<Event>,
    subscribers: usize,
    state: SessionState,
    pending: VecDeque<PendingPrompt>,
    active_run: Option<ActiveRun>,
    run_done_tx: mpsc::Sender<RunFinished>,
    run_done_rx: mpsc::Receiver<RunFinished>,
    run_done_task: Option<JoinHandle<()>>,
    reap_ttl: Duration,
    reap_deadline: Option<Instant>,
    history_snapshot: Vec<AgUiMessage>,
    actor_config: SessionActorConfig,
}

fn spawn_session_actor(
    key: SessionKey,
    registry: Arc<DashMap<SessionKey, SessionHandle>>,
    reap_ttl: Duration,
    actor_config: SessionActorConfig,
) -> SessionHandle {
    let (tx, rx) = mpsc::channel(COMMAND_BUFFER);
    let (broadcast_tx, _) = broadcast::channel(BROADCAST_BUFFER);
    let (run_done_tx, run_done_rx) = mpsc::channel(COMMAND_BUFFER);
    let handle = SessionHandle { tx: tx.clone() };
    let actor = SessionActor {
        key,
        registry,
        rx,
        broadcast_tx,
        subscribers: 0,
        state: SessionState::Idle,
        pending: VecDeque::new(),
        active_run: None,
        run_done_tx,
        run_done_rx,
        run_done_task: None,
        reap_ttl,
        reap_deadline: None,
        history_snapshot: Vec::new(),
        actor_config,
    };
    tokio::spawn(actor.run());
    handle
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

    {
        let mut supervisor = params.local_worker.lock().await;
        ensure_local_worker(&mut supervisor, params.abort_signal.clone()).await?;
    }
    let session = ThinClientSession::from_global_config(
        ThinClientConfig {
            cluster: LOCAL_CLUSTER_KEY.to_string(),
            agent: params.agent,
            session_id: Some(params.session_id),
        },
        &params.prompt_config,
        params.abort_signal,
    )
    .await?;
    session
        .run_turn(&params.text, params.sink, None)
        .await
        .map(|_| harnx_runtime::LoopResult::Completed)
}

fn configure_tool_confirmation(prompt_config: &GlobalConfig, resume: &[InterruptResume]) {
    let decisions = Arc::new(
        resume
            .iter()
            .map(|entry| ToolApprovalDecision {
                tool_call_id: entry.interrupt_id.clone(),
                approved: entry.payload.approved,
                reason: entry.payload.reason.clone(),
            })
            .collect::<Vec<_>>(),
    );
    let confirm_override: Arc<harnx_runtime::ConfirmToolUseFn> = Arc::new(
        move |call: &ToolCall, _args: &serde_json::Value, reason: Option<&str>| {
            let Some(decision) = call.id.as_deref().and_then(|id| {
                decisions
                    .iter()
                    .find(|decision| decision.tool_call_id == id)
            }) else {
                return ToolUseConfirmation::Defer;
            };
            if decision.approved {
                ToolUseConfirmation::Approve
            } else {
                ToolUseConfirmation::Deny {
                    reason: decision
                        .reason
                        .clone()
                        .or_else(|| reason.map(str::to_string)),
                }
            }
        },
    );
    prompt_config
        .write()
        .set_tui_confirm_tool_use(Some(confirm_override));
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

    fn event_context(&self) -> SessionEventContext {
        SessionEventContext::new(
            self.actor_config.base_config.clone(),
            self.key.clone(),
            self.history_snapshot.clone(),
        )
    }

    async fn run(mut self) {
        let far_future = Instant::now() + Duration::from_secs(FAR_FUTURE_SECS);
        let reap_sleep = sleep_until(far_future);
        tokio::pin!(reap_sleep);

        loop {
            tokio::select! {
                maybe_cmd = self.rx.recv() => {
                    let Some(cmd) = maybe_cmd else {
                        if let Some(active_run) = &self.active_run {
                            active_run.abort_signal.set_ctrlc();
                        }
                        self.registry.remove(&self.key);
                        break;
                    };
                    self.handle_command(cmd, &mut reap_sleep).await;
                }
                maybe_done = self.run_done_rx.recv() => {
                    let Some(done) = maybe_done else {
                        self.registry.remove(&self.key);
                        break;
                    };
                    self.handle_run_done(done, &mut reap_sleep).await;
                }
                _ = &mut reap_sleep, if self.reap_deadline.is_some() => {
                    if self.subscribers == 0 && !self.is_running() {
                        if self.should_reap() {
                            self.registry.remove(&self.key);
                            break;
                        }
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
                let _ = reply.send(SubscribeResult {
                    snapshot: self.history_snapshot.clone(),
                    events: self.broadcast_tx.subscribe(),
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
            SessionCommand::Cancel { reply } => {
                self.pending.clear();
                if let Some(active_run) = &self.active_run {
                    active_run.abort_signal.set_ctrlc();
                }
                let _ = reply.send(());
            }
            SessionCommand::Get { reply } => {
                self.refresh_history_snapshot().await;
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
        }
    }

    async fn handle_prompt(
        &mut self,
        text: String,
        options: SessionPromptOptions,
        reap_sleep: &mut std::pin::Pin<&mut Sleep>,
    ) -> PromptResult {
        let Some(active_run) = &self.active_run else {
            let run_id = if let Some(resume) = self.build_resume_continuation(&options.resume) {
                self.start_resume_run(text, options, resume, reap_sleep)
                    .await
            } else {
                self.start_run(text, options, reap_sleep).await
            };
            return PromptResult::Accepted {
                run_id: run_id.to_string(),
            };
        };

        // Test executors support same-turn injection. Production thin-client
        // turns queue the complete prompt for a fresh turn after completion.
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
            Ok(harnx_runtime::LoopResult::Completed) => self.finish_completed_run(&done),
            Ok(harnx_runtime::LoopResult::HandoffRequested {
                agent,
                session_id,
                prompt,
            }) => {
                self.dispatch_handoff_to_target(
                    &done,
                    agent.clone(),
                    session_id.clone(),
                    prompt.clone(),
                )
                .await;
            }
            Err(err) => {
                // Check for typed tool approval interrupt
                if let Some(interrupt) = ToolApprovalInterrupt::from_error(err) {
                    // Build pending interrupt batch
                    if let Some(pending) =
                        self.build_pending_interrupt_batch(&interrupt, &done, &done.attachment_refs)
                    {
                        let result = serde_json::json!({
                            "outcome": pending.metadata.clone()
                        });
                        done.sink.sink.close_text_segment();
                        let _ = self.broadcast_tx.send(Event::RunFinished(RunFinishedEvent {
                            base: base_event(),
                            thread_id: done.thread_id,
                            run_id: done.run_id.clone(),
                            result: Some(result),
                        }));
                        self.state = SessionState::Interrupted {
                            run_id: done.run_id.to_string(),
                            started_at: Utc::now(),
                            pending: Box::new(pending),
                        };
                    } else {
                        done.sink.sink.close_text_segment();
                        let _ = self.broadcast_tx.send(Event::RunError(RunErrorEvent {
                            base: base_event(),
                            message: err.to_string(),
                            code: None,
                        }));
                        self.state = SessionState::Idle;
                    }
                } else {
                    done.sink.sink.close_text_segment();
                    let _ = self.broadcast_tx.send(Event::RunError(RunErrorEvent {
                        base: base_event(),
                        message: err.to_string(),
                        code: None,
                    }));
                    self.state = SessionState::Idle;
                }
            }
        }
        self.replay_pending_or_arm_reap(reap_sleep).await;
    }

    fn finish_completed_run(&mut self, done: &RunFinished) {
        done.sink.sink.close_text_segment();
        let _ = self.broadcast_tx.send(Event::RunFinished(RunFinishedEvent {
            base: base_event(),
            thread_id: done.thread_id.clone(),
            run_id: done.run_id.clone(),
            result: None,
        }));
        self.state = SessionState::Idle;
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

    async fn dispatch_handoff_to_target(
        &mut self,
        done: &RunFinished,
        agent: String,
        session_id: Option<String>,
        prompt: String,
    ) {
        let Some(target_session_id) = self.resolve_handoff_session_id(session_id) else {
            self.state = SessionState::Idle;
            return;
        };

        let target_key = SessionKey {
            agent,
            session: target_session_id,
        };
        let target_handle = self.get_or_spawn_target_session_actor(target_key.clone());
        let (reply_tx, _reply_rx) = oneshot::channel();
        target_handle
            .tx
            .send(SessionCommand::Prompt {
                text: prompt,
                options: SessionPromptOptions::default(),
                reply: reply_tx,
            })
            .await
            .ok();

        done.sink.sink.close_text_segment();
        let _ = self.broadcast_tx.send(Event::RunFinished(RunFinishedEvent {
            base: base_event(),
            thread_id: done.thread_id.clone(),
            run_id: done.run_id.clone(),
            result: None,
        }));
        self.state = SessionState::Idle;
    }

    fn resolve_handoff_session_id(&self, session_id: Option<String>) -> Option<String> {
        match session_id {
            Some(id) => Some(id),
            None => {
                let prompt_config = prompt_config_for_agent_session_from_global(
                    &self.actor_config.base_config,
                    &self.key,
                    self.actor_config.call_fn.is_some(),
                );
                let new_session_id = prompt_config.write().new_session_id();
                match new_session_id {
                    Ok(id) => Some(id),
                    Err(e) => {
                        let _ = self.broadcast_tx.send(Event::RunError(RunErrorEvent {
                            base: base_event(),
                            message: format!("handoff failed: could not allocate session ID: {e}"),
                            code: None,
                        }));
                        None
                    }
                }
            }
        }
    }

    fn get_or_spawn_target_session_actor(&mut self, target_key: SessionKey) -> SessionHandle {
        match self.registry.entry(target_key.clone()) {
            Entry::Occupied(entry) => entry.get().clone(),
            Entry::Vacant(entry) => {
                let handle = spawn_session_actor(
                    target_key,
                    Arc::clone(&self.registry),
                    self.reap_ttl,
                    self.actor_config.clone(),
                );
                entry.insert(handle.clone());
                handle
            }
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
        configure_tool_confirmation(&prompt_config, &options.resume);
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
        let task = tokio::spawn(async move {
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
        });

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
            state: self.state.clone(),
            history_snapshot: self.history_snapshot.clone(),
            capabilities: SessionCapabilities {
                can_prompt: true,
                can_cancel: true,
                supports_snapshot: true,
            },
        }
    }

    fn is_running(&self) -> bool {
        matches!(
            self.state,
            SessionState::Running { .. } | SessionState::Interrupted { .. }
        )
    }

    /// Build a pending interrupt batch from a tool approval interrupt.
    fn build_pending_interrupt_batch(
        &self,
        interrupt: &ToolApprovalInterrupt,
        done: &RunFinished,
        attachment_refs: &[String],
    ) -> Option<PendingInterruptBatch> {
        let prompt_config = self.prompt_config();
        let session = prompt_config.read().session.clone()?;
        let tool_message = session.messages.last()?;
        let MessageContent::ToolCalls(tool_calls) = &tool_message.content else {
            return None;
        };

        let interrupts: Vec<ToolApprovalInterruptEntry> = interrupt
            .deferred_calls
            .iter()
            .map(|deferred| {
                let tool_call_id = deferred.call.id.clone().unwrap_or_default();
                ToolApprovalInterruptEntry {
                    id: tool_call_id.clone(),
                    tool_call_id,
                    name: deferred.call.name.clone(),
                    arguments: deferred.arguments.clone(),
                    message: deferred
                        .reason
                        .clone()
                        .unwrap_or_else(|| format!("Approve tool call `{}`", deferred.call.name)),
                    reason: deferred.reason.clone(),
                }
            })
            .collect();

        let metadata = serde_json::json!({
            "type": "interrupt",
            "interrupts": interrupts.iter().map(|interrupt| serde_json::json!({
                "id": interrupt.id,
                "reason": "tool_call",
                "toolCallId": interrupt.tool_call_id,
                "message": interrupt.message,
                "responseSchema": {
                    "type": "object",
                    "properties": {
                        "approved": { "type": "boolean" },
                        "reason": { "type": "string" }
                    },
                    "required": ["approved"]
                }
            })).collect::<Vec<_>>()
        });

        Some(PendingInterruptBatch {
            interrupt_run_id: done.run_id.to_string(),
            text: session
                .messages
                .iter()
                .rev()
                .find(|message| message.role.is_user())
                .map(|message| message.content.to_text())
                .unwrap_or_default(),
            attachment_refs: attachment_refs.to_vec(),
            completion_output: tool_calls.text.clone(),
            completion_thought: tool_calls.thought.clone(),
            tool_calls: interrupt.tool_calls.clone(),
            interrupts,
            metadata,
        })
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
    }

    async fn refresh_history_snapshot(&mut self) {
        self.history_snapshot = if self.actor_config.call_fn.is_some() {
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
            snapshot
        } else {
            match crate::load_nats_session(&self.actor_config.base_config, &self.key.session).await
            {
                Ok(session) if session.agent_name.as_deref() == Some(self.key.agent.as_str()) => {
                    crate::ag_ui::history_messages_for_snapshot(&session.messages)
                }
                _ => Vec::new(),
            }
        };
    }
}

/// Internal struct for resume continuation.
struct ResumeContinuation {
    pending: PendingInterruptBatch,
    decisions: Vec<ToolApprovalDecision>,
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

impl SessionActor {
    /// Build resume continuation from interrupt resume params.
    fn build_resume_continuation(&self, resume: &[InterruptResume]) -> Option<ResumeContinuation> {
        let SessionState::Interrupted { pending, .. } = &self.state else {
            return None;
        };
        Some(ResumeContinuation {
            pending: pending.as_ref().clone(),
            decisions: resume
                .iter()
                .map(|entry| ToolApprovalDecision {
                    tool_call_id: entry.interrupt_id.clone(),
                    approved: entry.payload.approved
                        && matches!(entry.status, InterruptResumeStatus::Approved),
                    reason: entry.payload.reason.clone(),
                })
                .collect(),
        })
    }

    /// Start a resumed run after tool approval interrupt.
    async fn start_resume_run(
        &mut self,
        _text: String,
        options: SessionPromptOptions,
        resume: ResumeContinuation,
        reap_sleep: &mut std::pin::Pin<&mut Sleep>,
    ) -> RunId {
        self.cancel_reap(reap_sleep);
        let prompt_config = self.prompt_config();

        let run_id = RunId::random();
        let thread_id = derive_thread_id(&self.key.session);
        let started_at = Utc::now();
        let abort_signal = create_abort_signal();

        let _ = self.broadcast_tx.send(Event::RunStarted(RunStartedEvent {
            base: base_event(),
            thread_id: thread_id.clone(),
            run_id: run_id.clone(),
        }));

        let loop_ctx = build_loop_ctx(
            prompt_config.clone(),
            self.actor_config.call_fn.clone(),
            abort_signal.clone(),
            mpsc::channel(COMMAND_BUFFER).1,
            None,
            self.broadcast_tx.clone(),
        );
        let event_tx = self.broadcast_tx.clone();
        let input = build_input(
            &prompt_config,
            &resume.pending.text,
            &resume.pending.attachment_refs,
        )
        .expect("build actor input for resume");
        let done_tx = self.run_done_tx.clone();
        let run_id_for_task = run_id.clone();
        let thread_id_for_task = thread_id.clone();
        let session_event_context = self.event_context();
        let sink = Arc::new(BroadcastEventSender::new(
            event_tx,
            MessageId::random(),
            session_event_context,
        ));
        let sink_for_task = sink.clone();
        let abort_signal_for_task = abort_signal.clone();
        let task = tokio::spawn(async move {
            let loop_result = with_agent_event_sink(sink_for_task.clone(), async {
                let mut input = input;
                loop {
                    match continue_agent_loop_from_tool_round(
                        &loop_ctx,
                        input,
                        resume.pending.completion_output.clone(),
                        resume.pending.completion_thought.clone(),
                        resume.pending.tool_calls.clone(),
                        resume.decisions.clone(),
                        resume
                            .pending
                            .interrupts
                            .iter()
                            .map(|i| i.tool_call_id.clone())
                            .collect(),
                    )
                    .await
                    {
                        Ok(harnx_runtime::LoopResult::Completed) => {
                            break Ok(harnx_runtime::LoopResult::Completed)
                        }
                        Ok(harnx_runtime::LoopResult::HandoffRequested {
                            agent,
                            session_id,
                            prompt,
                        }) => {
                            prompt_config.write().exit_agent_with_lock(None)?;
                            harnx_runtime::config::Config::use_agent(
                                &prompt_config,
                                &agent,
                                session_id.as_deref(),
                                abort_signal_for_task.clone(),
                            )
                            .await?;
                            if prompt_config.read().session.is_some() {
                                prompt_config.write().empty_session_with_lock(None)?;
                            }
                            input = harnx_runtime::config::input::from_str(
                                &prompt_config,
                                &prompt,
                                None,
                            );
                        }
                        Err(e) => break Err(e),
                    }
                }
            })
            .await;
            let _ = done_tx
                .send(RunFinished {
                    run_id: run_id_for_task,
                    result: loop_result,
                    sink: sink_for_task,
                    thread_id: thread_id_for_task,
                    attachment_refs: options.attachment_refs.clone(),
                })
                .await;
        });

        self.run_done_task = Some(task);
        self.active_run = Some(ActiveRun {
            run_id: run_id.clone(),
            started_at,
            abort_signal,
            inject_tx: None,
        });
        self.state = SessionState::Running {
            run_id: run_id.to_string(),
            started_at,
        };
        run_id
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
        })
    });
    let context = harnx_session::build_context(
        prompt_config.clone(),
        call_fn,
        abort_signal,
        Some(on_tool_round),
        working_dir,
    );
    #[cfg(test)]
    let context = {
        let mut context = context;
        context.nats_hook_provider = test_hook_provider(&prompt_config);
        context
    };
    context
}

#[cfg(test)]
fn test_hook_provider(
    config: &GlobalConfig,
) -> Option<Arc<harnx_runtime::nats_hook_provider::NatsHookProvider>> {
    use harnx_core::hooks::{HookOutcome, HookResult, HookResultControl};
    use harnx_hookset::{FailPolicy, HookSpec};
    use harnx_runtime::nats_hook_provider::{DiscoveredHook, NatsHookProvider};

    let hooks = config.read().resolved_hooks().entries;
    if hooks.is_empty() {
        return None;
    }
    let outcomes = hooks
        .iter()
        .enumerate()
        .map(|(index, hook)| {
            let reason = hook
                .command
                .split("permissionDecisionReason\":\"")
                .nth(1)
                .and_then(|tail| tail.split('\"').next())
                .map(str::to_string);
            (format!("test-hook-{index}"), reason)
        })
        .collect::<std::collections::HashMap<_, _>>();
    let discovered = hooks
        .into_iter()
        .enumerate()
        .map(|(index, hook)| {
            let words: Vec<_> = hook.command.split_whitespace().collect();
            let runner_arg = |name: &str| {
                words
                    .iter()
                    .position(|word| *word == name)
                    .and_then(|position| words.get(position + 1))
                    .map(|value| value.trim_matches('\'').to_string())
            };
            DiscoveredHook {
                server: format!("test-hook-{index}"),
                display_label: hook.status_message,
                spec: HookSpec {
                    event: runner_arg("--event").expect("test hook command must declare --event"),
                    matcher: runner_arg("--matcher"),
                    priority: index as i32,
                    timeout_secs: runner_arg("--timeout").and_then(|value| value.parse().ok()),
                    fail_policy: FailPolicy::Closed,
                },
            }
        })
        .collect();
    let handler = Arc::new(move |subject: &str, _payload| {
        let reason = outcomes
            .iter()
            .find(|(server, _)| subject.split('.').nth(4) == Some(server.as_str()))
            .and_then(|(_, reason)| reason.clone());
        HookOutcome {
            control: HookResultControl::Ask { reason },
            result: HookResult::default(),
        }
    });
    Some(Arc::new(NatsHookProvider::from_request_handler(
        harnx_core::instance::ServerScope::from_string("session-actor-test"),
        discovered,
        handler,
    )))
}

fn prompt_config_for_agent_session_from_global(
    base_config: &Config,
    key: &SessionKey,
    filesystem: bool,
) -> GlobalConfig {
    let prompt_config = harnx_session::fork_prompt_config(base_config);
    {
        let mut cfg = prompt_config.write();
        cfg.use_agent_by_name(&key.agent).expect("set actor agent");
        if filesystem {
            cfg.use_session(Some(&key.session))
                .expect("set actor session");
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
    let (context_tokens, percent) = session.tokens_usage();
    let max_context_tokens = session.model().max_input_tokens();
    Some(crate::ag_ui::UsageContextSnapshot {
        context_tokens,
        max_context_tokens,
        context_percent: max_context_tokens.map(|_| percent),
    })
}

fn derive_thread_id(session: &str) -> ThreadId {
    use uuid::Uuid;
    ThreadId::from(Uuid::new_v5(&Uuid::NAMESPACE_URL, session.as_bytes()))
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
    use std::{
        fs,
        sync::atomic::{AtomicUsize, Ordering},
    };
    use tokio::{sync::Notify, time::sleep};

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
            .send(SessionCommand::Cancel { reply: reply_tx })
            .await
            .expect("send cancel");
        reply_rx.await.expect("recv cancel reply");
    }

    fn registry_with_call_fn(call_fn: AgentCallFn) -> SessionRegistry {
        SessionRegistry::new_for_tests(
            load_base_config_for_tests(),
            Duration::from_millis(50),
            Some(call_fn),
        )
    }

    fn load_session_messages(agent: &str, session_id: &str) -> Vec<Message> {
        let key = key(agent, session_id);
        let base_config = load_base_config_for_tests();
        let prompt_config = prompt_config_for_agent_session_from_global(&base_config, &key, true);
        let messages = prompt_config
            .read()
            .session
            .as_ref()
            .expect("session exists")
            .messages
            .clone();
        messages
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
                                outputs[0].contains("session has not been saved yet"),
                                "tool result should surface real built-in tool execution error: {}",
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

        let messages = wait_for_session_messages("plain", "tool-history", |messages| {
            messages
                .iter()
                .any(|msg| msg.role.is_assistant() && msg.content.to_text() == "history checked")
        })
        .await;
        let assistant_messages: Vec<String> = messages
            .iter()
            .filter(|msg| msg.role.is_assistant())
            .map(|msg| msg.content.to_text())
            .collect();
        assert!(assistant_messages
            .iter()
            .any(|text| text == "history checked"));

        let session_path = {
            let mut config = load_base_config_for_tests();
            config.use_agent_by_name("plain").expect("set agent");
            config.session_file("tool-history")
        };
        let persisted = fs::read_to_string(session_path).expect("read persisted session");
        assert!(persisted.contains("type: tool_calls"));
        assert!(persisted.contains("type: tool_results"));
        assert!(persisted.contains("harnx_agent_session_history_read"));
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
    async fn session_actor_interrupt_emits_run_finished_and_interrupted_state() {
        let _guard = harnx_runtime::client::TestStateGuard::new(None).await;
        let sandbox = TestConfigSandbox::new();
        sandbox.write_agent_with_front_matter(
            "plain",
            "model: openai:gpt-4o\nuse_tools: harnx_agent_session_history_read\nhooks:\n  entries:\n    - command: |\n        harnx-claude-compatible-hook-server --event PreToolUse --matcher '^harnx_agent_session_history_read$' -- printf '\"'\"'{\"hookSpecificOutput\":{\"permissionDecision\":\"ask\",\"permissionDecisionReason\":\"approval needed\"}}'\"'\"''",
            "You are plain.",
        );

        let call_count = Arc::new(AtomicUsize::new(0));
        let call_fn: AgentCallFn = {
            let call_count = Arc::clone(&call_count);
            Arc::new(move |_input, _config, _abort| {
                let call_count = Arc::clone(&call_count);
                Box::pin(async move {
                    let round = call_count.fetch_add(1, Ordering::SeqCst);
                    Ok(if round == 0 {
                        (
                            "approval needed".to_string(),
                            None,
                            vec![ToolCall::new(
                                "harnx_agent_session_history_read".to_string(),
                                json!({}),
                                Some("call-interrupt".to_string()),
                                None,
                            )],
                            harnx_runtime::client::CompletionTokenUsage::default(),
                        )
                    } else {
                        panic!("unexpected second round while interrupted");
                    })
                })
            })
        };

        let registry = registry_with_call_fn(call_fn);
        let handle = registry.get_or_spawn(key("plain", "interrupt-state"));
        let mut sub = subscribe(&handle).await.events;

        let run_id = match prompt(&handle, "interrupt me").await {
            PromptResult::Accepted { run_id } => run_id,
            other => panic!("expected Accepted, got {other:?}"),
        };

        let mut finished_result = None;
        tokio::time::timeout(Duration::from_secs(10), async {
            while let Ok(event) = sub.recv().await {
                if let Event::RunFinished(finished) = event {
                    if finished.run_id.to_string() == run_id {
                        finished_result = finished.result;
                        break;
                    }
                }
            }
        })
        .await
        .expect("timed out waiting for run finished");

        let result = finished_result.expect("run finished result");
        assert_eq!(result["outcome"]["type"], "interrupt");
        assert_eq!(
            result["outcome"]["interrupts"][0]["toolCallId"],
            "call-interrupt"
        );

        let info = get_info(&handle).await;
        match info.state {
            SessionState::Interrupted {
                run_id: state_run_id,
                pending,
                ..
            } => {
                assert_eq!(state_run_id, run_id);
                assert_eq!(pending.interrupt_run_id, run_id);
                assert_eq!(pending.text, "interrupt me");
                assert_eq!(pending.interrupts.len(), 1);
                assert_eq!(pending.interrupts[0].id, "call-interrupt");
            }
            other => panic!("expected interrupted state, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn session_actor_interrupt_closes_text_segment_before_run_finished() {
        let _guard = harnx_runtime::client::TestStateGuard::new(None).await;
        let sandbox = TestConfigSandbox::new();
        sandbox.write_agent_with_front_matter(
            "plain",
            "model: openai:gpt-4o\nuse_tools: harnx_agent_session_history_read\nhooks:\n  entries:\n    - command: |\n        harnx-claude-compatible-hook-server --event PreToolUse --matcher '^harnx_agent_session_history_read$' -- printf '\"'\"'{\"hookSpecificOutput\":{\"permissionDecision\":\"ask\",\"permissionDecisionReason\":\"approval needed\"}}'\"'\"''",
            "You are plain.",
        );

        let call_fn: AgentCallFn = Arc::new(move |_input, _config, _abort| {
            Box::pin(async move {
                harnx_core::sink::emit_agent_event(AgentEvent::Model(ModelEvent::MessageChunk {
                    blocks: vec![ContentBlock::Text("partial text".to_string())],
                }));
                Ok((
                    "approval needed".to_string(),
                    None,
                    vec![ToolCall::new(
                        "harnx_agent_session_history_read".to_string(),
                        json!({}),
                        Some("call-interrupt-close".to_string()),
                        None,
                    )],
                    harnx_runtime::client::CompletionTokenUsage::default(),
                ))
            })
        });

        let registry = registry_with_call_fn(call_fn);
        let handle = registry.get_or_spawn(key("plain", "interrupt-close-text"));
        let mut sub = subscribe(&handle).await.events;

        let run_id = match prompt(&handle, "interrupt after text").await {
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
                    Event::RunFinished(finished) if finished.run_id.to_string() == run_id => {
                        Some("RUN_FINISHED")
                    }
                    _ => None,
                };
                if let Some(event_type) = event_type {
                    event_types.push(event_type);
                    if event_type == "RUN_FINISHED" {
                        break;
                    }
                }
            }
        })
        .await
        .expect("timed out waiting for run finished");

        let text_end_index = event_types
            .iter()
            .position(|event| *event == "TEXT_MESSAGE_END")
            .expect("expected text segment end before interruption finish");
        let run_finished_index = event_types
            .iter()
            .position(|event| *event == "RUN_FINISHED")
            .expect("expected run finished event");
        assert!(
            text_end_index < run_finished_index,
            "TEXT_MESSAGE_END must precede RUN_FINISHED: {event_types:?}"
        );
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
    async fn session_actor_resume_preserves_full_tool_round() {
        let _guard = harnx_runtime::client::TestStateGuard::new(None).await;
        let sandbox = TestConfigSandbox::new();
        sandbox.write_agent_with_front_matter(
            "plain",
            "model: openai:gpt-4o\nuse_tools: harnx_agent_session_history_read\nhooks:\n  entries:\n    - command: |\n        harnx-claude-compatible-hook-server --event PreToolUse --matcher '^harnx_agent_session_history_read$' -- printf '\"'\"'{\"hookSpecificOutput\":{\"permissionDecision\":\"ask\",\"permissionDecisionReason\":\"batch approval needed\"}}'\"'\"''",
            "You are plain.",
        );

        let round = Arc::new(AtomicUsize::new(0));
        let call_fn: AgentCallFn = {
            let round = Arc::clone(&round);
            Arc::new(move |_input, _config, _abort| {
                let round = Arc::clone(&round);
                Box::pin(async move {
                    let turn = round.fetch_add(1, Ordering::SeqCst);
                    Ok(match turn {
                        0 => (
                            "batch approval needed".to_string(),
                            None,
                            vec![
                                ToolCall::new(
                                    "harnx_agent_session_history_read".to_string(),
                                    json!({}),
                                    Some("call-a".to_string()),
                                    None,
                                ),
                                ToolCall::new(
                                    "harnx_agent_session_history_read".to_string(),
                                    json!({}),
                                    Some("call-b".to_string()),
                                    None,
                                ),
                            ],
                            harnx_runtime::client::CompletionTokenUsage::default(),
                        ),
                        1 => (
                            "resume complete".to_string(),
                            None,
                            vec![],
                            harnx_runtime::client::CompletionTokenUsage::default(),
                        ),
                        other => panic!("unexpected LLM round {other}"),
                    })
                })
            })
        };

        let registry = registry_with_call_fn(call_fn);
        let handle = registry.get_or_spawn(key("plain", "interrupt-batch"));
        let mut sub = subscribe(&handle).await.events;

        let first_run_id = match prompt(&handle, "batch interrupt").await {
            PromptResult::Accepted { run_id } => run_id,
            other => panic!("expected Accepted, got {other:?}"),
        };
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                match sub.recv().await.expect("recv event") {
                    Event::RunFinished(finished) if finished.run_id.to_string() == first_run_id => {
                        break
                    }
                    _ => {}
                }
            }
        })
        .await
        .expect("timed out waiting for initial run finished");

        let resumed = prompt_with_options(
            &handle,
            "batch interrupt",
            SessionPromptOptions {
                resume: vec![
                    InterruptResume {
                        interrupt_id: "call-a".to_string(),
                        status: InterruptResumeStatus::Approved,
                        payload: InterruptResumePayload {
                            approved: true,
                            reason: None,
                        },
                    },
                    InterruptResume {
                        interrupt_id: "call-b".to_string(),
                        status: InterruptResumeStatus::Denied,
                        payload: InterruptResumePayload {
                            approved: false,
                            reason: Some("denied by test".to_string()),
                        },
                    },
                ],
                ..Default::default()
            },
        )
        .await;
        let resumed_run_id = match resumed {
            PromptResult::Accepted { run_id } => run_id,
            other => panic!("expected Accepted resume, got {other:?}"),
        };
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                match sub.recv().await.expect("recv event") {
                    Event::RunFinished(finished)
                        if finished.run_id.to_string() == resumed_run_id =>
                    {
                        break
                    }
                    _ => {}
                }
            }
        })
        .await
        .expect("timed out waiting for resumed run finished");

        let info = get_info(&handle).await;
        assert_eq!(
            round.load(Ordering::SeqCst),
            2,
            "resume should continue loop exactly once"
        );
        assert!(matches!(info.state, SessionState::Idle));
        let messages = load_session_messages("plain", "interrupt-batch");
        let transcript = messages
            .iter()
            .map(|message| message.content.to_text())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(transcript.contains("resume complete"));
        let tool_turn = messages
            .iter()
            .find_map(|message| match &message.content {
                MessageContent::ToolCalls(tool_calls) => Some(tool_calls),
                _ => None,
            })
            .expect("tool turn persisted");
        assert_eq!(
            tool_turn.tool_results.len(),
            2,
            "full round should persist both tool results"
        );
        let outputs_by_id: std::collections::BTreeMap<_, _> = tool_turn
            .tool_results
            .iter()
            .map(|result| {
                (
                    result.call.id.clone().expect("tool result id"),
                    result.output.clone(),
                )
            })
            .collect();
        assert_eq!(
            outputs_by_id.get("call-a"),
            Some(&serde_json::json!({
                "content": [{
                    "type": "text",
                    "text": "[{\"seq\":0,\"type\":\"header\",\"text\":\"model: openai:gpt-4o\"},{\"seq\":1,\"type\":\"message\",\"text\":\"batch interrupt\",\"role\":\"user\"},{\"seq\":2,\"type\":\"tool_calls\",\"text\":\"batch approval needed\\nharnx_agent_session_history_read({})\\nharnx_agent_session_history_read({})\",\"tool_names\":[\"harnx_agent_session_history_read\",\"harnx_agent_session_history_read\"]}]"
                }]
            })),
            "auto-approved call should execute normally"
        );
        assert_eq!(
            outputs_by_id.get("call-b"),
            Some(&serde_json::json!({
                "error": "denied by test",
                "blocked_by_hook": true,
            })),
            "deferred denied call should persist denial result"
        );
    }
    #[tokio::test]
    async fn session_actor_resume_with_mixed_round_approves_non_pending_calls_without_reinterrupt()
    {
        let _guard = harnx_runtime::client::TestStateGuard::new(None).await;
        let sandbox = TestConfigSandbox::new();
        sandbox.write_agent_with_front_matter(
            "plain",
            "model: openai:gpt-4o\nuse_tools: harnx_agent_session_history_read, harnx_agent_session_history_search\nhooks:\n  entries:\n    - command: |\n        harnx-claude-compatible-hook-server --event PreToolUse --matcher '^harnx_agent_session_history_search$' -- printf '\"'\"'{\"hookSpecificOutput\":{\"permissionDecision\":\"ask\",\"permissionDecisionReason\":\"approval needed\"}}'\"'\"''",
            "You are plain.",
        );

        let round = Arc::new(AtomicUsize::new(0));
        let call_fn: AgentCallFn = {
            let round = Arc::clone(&round);
            Arc::new(move |_input, _config, _abort| {
                let round = Arc::clone(&round);
                Box::pin(async move {
                    let turn = round.fetch_add(1, Ordering::SeqCst);
                    Ok(match turn {
                        0 => (
                            "approval required".to_string(),
                            None,
                            vec![
                                ToolCall::new(
                                    "harnx_agent_session_history_read".to_string(),
                                    json!({}),
                                    Some("auto-call".to_string()),
                                    None,
                                ),
                                ToolCall::new(
                                    "harnx_agent_session_history_search".to_string(),
                                    json!({}),
                                    Some("deferred-call".to_string()),
                                    None,
                                ),
                            ],
                            harnx_runtime::client::CompletionTokenUsage::default(),
                        ),
                        1 => (
                            "resume complete".to_string(),
                            None,
                            vec![],
                            harnx_runtime::client::CompletionTokenUsage::default(),
                        ),
                        other => panic!("unexpected LLM round {other}"),
                    })
                })
            })
        };

        let registry = registry_with_call_fn(call_fn);
        let handle = registry.get_or_spawn(key("plain", "mixed-no-reinterrupt"));
        let mut sub = subscribe(&handle).await.events;

        let first_run_id = match prompt(&handle, "batch interrupt").await {
            PromptResult::Accepted { run_id } => run_id,
            other => panic!("expected Accepted, got {other:?}"),
        };
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                match sub.recv().await.expect("recv event") {
                    Event::RunFinished(finished) if finished.run_id.to_string() == first_run_id => {
                        let outcome = finished.result.expect("interrupt outcome");
                        assert_eq!(
                            outcome["outcome"]["interrupts"].as_array().unwrap().len(),
                            1
                        );
                        assert_eq!(outcome["outcome"]["interrupts"][0]["id"], "deferred-call");
                        break;
                    }
                    _ => {}
                }
            }
        })
        .await
        .expect("timed out waiting for initial interrupt");

        let resumed_run_id = match prompt_with_options(
            &handle,
            "batch interrupt",
            SessionPromptOptions {
                resume: vec![InterruptResume {
                    interrupt_id: "deferred-call".to_string(),
                    status: InterruptResumeStatus::Approved,
                    payload: InterruptResumePayload {
                        approved: true,
                        reason: None,
                    },
                }],
                ..Default::default()
            },
        )
        .await
        {
            PromptResult::Accepted { run_id } => run_id,
            other => panic!("expected Accepted resume, got {other:?}"),
        };

        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                match sub.recv().await.expect("recv event") {
                    Event::RunFinished(finished)
                        if finished.run_id.to_string() == resumed_run_id =>
                    {
                        assert!(
                            finished
                                .result
                                .as_ref()
                                .and_then(|result| result.get("outcome"))
                                .is_none(),
                            "resume should complete, not re-interrupt"
                        );
                        break;
                    }
                    _ => {}
                }
            }
        })
        .await
        .expect("timed out waiting for resumed run finished");

        assert_eq!(
            round.load(Ordering::SeqCst),
            2,
            "mixed round should complete after single resume"
        );
    }

    fn attachment_capture_call_fn(
        seen_attachments: Arc<tokio::sync::Mutex<Vec<Vec<String>>>>,
    ) -> AgentCallFn {
        let round = Arc::new(AtomicUsize::new(0));
        Arc::new(move |input, _config, _abort| {
            let round = Arc::clone(&round);
            let seen_attachments = Arc::clone(&seen_attachments);
            Box::pin(async move {
                seen_attachments
                    .lock()
                    .await
                    .push(input.attachment_refs.clone());
                let turn = round.fetch_add(1, Ordering::SeqCst);
                Ok(match turn {
                    0 => (
                        "approval required".to_string(),
                        None,
                        vec![ToolCall::new(
                            "harnx_agent_session_history_read".to_string(),
                            json!({}),
                            Some("attach-call".to_string()),
                            None,
                        )],
                        harnx_runtime::client::CompletionTokenUsage::default(),
                    ),
                    1 => (
                        "resume complete".to_string(),
                        None,
                        vec![],
                        harnx_runtime::client::CompletionTokenUsage::default(),
                    ),
                    other => panic!("unexpected LLM round {other}"),
                })
            })
        })
    }

    async fn assert_attachment_resume(handle: &SessionHandle, attachment_refs: &[String]) {
        let started = prompt_with_options(
            handle,
            "attachment prompt",
            SessionPromptOptions {
                attachment_refs: attachment_refs.to_vec(),
                ..Default::default()
            },
        )
        .await;
        assert!(matches!(started, PromptResult::Accepted { .. }));

        let info = wait_for_state(handle, "interrupted", |state| {
            matches!(state, SessionState::Interrupted { .. })
        })
        .await;
        let pending = match info.state {
            SessionState::Interrupted { pending, .. } => pending,
            other => panic!("expected interrupted state, got {other:?}"),
        };
        assert_eq!(pending.attachment_refs, attachment_refs);

        let resumed = prompt_with_options(
            handle,
            "attachment prompt",
            SessionPromptOptions {
                resume: vec![InterruptResume {
                    interrupt_id: "attach-call".to_string(),
                    status: InterruptResumeStatus::Approved,
                    payload: InterruptResumePayload {
                        approved: true,
                        reason: None,
                    },
                }],
                ..Default::default()
            },
        )
        .await;
        assert!(matches!(resumed, PromptResult::Accepted { .. }));
        wait_for_state(handle, "idle after resume", |state| {
            *state == SessionState::Idle
        })
        .await;
    }

    #[tokio::test]
    async fn session_actor_resume_preserves_attachment_refs() {
        let _guard = harnx_runtime::client::TestStateGuard::new(None).await;
        let sandbox = TestConfigSandbox::new();
        sandbox.write_agent_with_front_matter(
            "plain",
            "model: openai:gpt-4o\nuse_tools: harnx_agent_session_history_read\nhooks:\n  entries:\n    - command: |\n        harnx-claude-compatible-hook-server --event PreToolUse --matcher '^harnx_agent_session_history_read$' -- printf '\"'\"'{\"hookSpecificOutput\":{\"permissionDecision\":\"ask\",\"permissionDecisionReason\":\"approval needed\"}}'\"'\"''",
            "You are plain.",
        );

        let seen_attachments: Arc<tokio::sync::Mutex<Vec<Vec<String>>>> =
            Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let registry =
            registry_with_call_fn(attachment_capture_call_fn(Arc::clone(&seen_attachments)));
        let handle = registry.get_or_spawn(key("plain", "resume-attachments"));
        let attachment_refs = vec!["cid:test-attachment".to_string()];

        assert_attachment_resume(&handle, &attachment_refs).await;

        let captured = seen_attachments.lock().await.clone();
        assert_eq!(
            captured.len(),
            2,
            "call_fn should see initial and resumed inputs"
        );
        assert_eq!(captured[0], attachment_refs);
        assert_eq!(captured[1], vec!["cid:test-attachment".to_string()]);
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
    async fn session_actor_resume_missing_decision_for_pending_tool_causes_reinterrupt() {
        let _guard = harnx_runtime::client::TestStateGuard::new(None).await;
        let sandbox = TestConfigSandbox::new();
        sandbox.write_agent_with_front_matter(
            "plain",
            "model: openai:gpt-4o\nuse_tools: harnx_agent_session_history_read\nhooks:\n  entries:\n    - command: |\n        harnx-claude-compatible-hook-server --event PreToolUse --matcher '^harnx_agent_session_history_read$' -- printf '\"'\"'{\"hookSpecificOutput\":{\"permissionDecision\":\"ask\",\"permissionDecisionReason\":\"approval needed\"}}'\"'\"''",
            "You are plain.",
        );

        let round = Arc::new(AtomicUsize::new(0));
        let call_fn: AgentCallFn = {
            let round = Arc::clone(&round);
            Arc::new(move |_input, _config, _abort| {
                let round = Arc::clone(&round);
                Box::pin(async move {
                    let turn = round.fetch_add(1, Ordering::SeqCst);
                    Ok(match turn {
                        0 => (
                            "approval required".to_string(),
                            None,
                            vec![
                                ToolCall::new(
                                    "harnx_agent_session_history_read".to_string(),
                                    json!({}),
                                    Some("pending-call-1".to_string()),
                                    None,
                                ),
                                ToolCall::new(
                                    "harnx_agent_session_history_read".to_string(),
                                    json!({}),
                                    Some("pending-call-2".to_string()),
                                    None,
                                ),
                            ],
                            harnx_runtime::client::CompletionTokenUsage::default(),
                        ),
                        _ => panic!("unexpected LLM round"),
                    })
                })
            })
        };

        let registry = registry_with_call_fn(call_fn);
        let handle = registry.get_or_spawn(key("plain", "missing-decision-reinterrupt"));
        let mut sub = subscribe(&handle).await.events;

        let first_run_id = match prompt(&handle, "start").await {
            PromptResult::Accepted { run_id } => run_id,
            other => panic!("expected Accepted, got {other:?}"),
        };

        // Wait for first interrupt (both tools pending)
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Event::RunFinished(finished) = sub.recv().await.expect("recv event") {
                    if finished.run_id.to_string() == first_run_id {
                        let outcome = finished.result.expect("interrupt outcome");
                        assert_eq!(
                            outcome["outcome"]["interrupts"].as_array().unwrap().len(),
                            2
                        );
                        break;
                    }
                }
            }
        })
        .await
        .expect("timed out waiting for initial interrupt");

        // Resume with decision for only ONE of the tools
        let resumed_run_id = match prompt_with_options(
            &handle,
            "start",
            SessionPromptOptions {
                resume: vec![InterruptResume {
                    interrupt_id: "pending-call-1".to_string(),
                    status: InterruptResumeStatus::Approved,
                    payload: InterruptResumePayload {
                        approved: true,
                        reason: None,
                    },
                }],
                ..Default::default()
            },
        )
        .await
        {
            PromptResult::Accepted { run_id } => run_id,
            other => panic!("expected Accepted resume, got {other:?}"),
        };

        // Expect second interrupt because "pending-call-2" was missing a decision
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Event::RunFinished(finished) = sub.recv().await.expect("recv event") {
                    if finished.run_id.to_string() == resumed_run_id {
                        let outcome = finished.result.expect("interrupt outcome");
                        let interrupts = outcome["outcome"]["interrupts"].as_array().unwrap();
                        assert_eq!(
                            interrupts.len(),
                            1,
                            "should re-interrupt for the remaining pending call"
                        );
                        assert_eq!(interrupts[0]["id"], "pending-call-2");
                        break;
                    }
                }
            }
        })
        .await
        .expect("timed out waiting for re-interrupt");
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
                            json!({
                                "prompt": "delegated work",
                                "session_id": "handoff-target-session"
                            }),
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

        // Track whether we saw session_handoff event
        let saw_handoff_event = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let saw_handoff_event_clone = saw_handoff_event.clone();

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
                        // Verify the handoff event payload
                        assert_eq!(event.value["agent"].as_str(), Some("target-agent"));
                        assert_eq!(
                            event.value["session_id"].as_str(),
                            Some("handoff-target-session")
                        );
                        saw_handoff_event_clone.store(true, Ordering::SeqCst);
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

        // Verify session_handoff event was emitted
        assert!(
            saw_handoff_event.load(Ordering::SeqCst),
            "session_handoff event should be emitted"
        );

        // Verify target actor was actually invoked - poll with timeout
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let count = target_count.load(Ordering::SeqCst);
                if count >= 1 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("timed out waiting for target agent to run");

        assert_eq!(
            target_count.load(Ordering::SeqCst),
            1,
            "target agent should be invoked once by handoff dispatch"
        );

        // Verify the target actor was registered (created by handoff)
        assert!(
            registry.has_session(&key("target-agent", "handoff-target-session")),
            "target session should be registered in registry after handoff dispatch"
        );
    }

    // NATS-backed serve tests spawn a local worker subprocess via
    // `LocalWorkerSupervisor` (Unix process-group management), so they are
    // Unix-only. Gated to avoid unused-import/dead-code errors on Windows.
    #[cfg(unix)]
    #[path = "session_actor_nats_tests.rs"]
    mod nats_tests;
}
