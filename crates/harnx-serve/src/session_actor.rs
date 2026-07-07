#![allow(dead_code)]

use crate::ag_ui::AgUiSink;
use ag_ui_core::{
    event::{
        BaseEvent, Event, RunErrorEvent, RunFinishedEvent, RunStartedEvent, TextMessageEndEvent,
        TextMessageStartEvent,
    },
    types::{
        ids::{MessageId, RunId, ThreadId},
        message::{Message as AgUiMessage, Role},
    },
};
use chrono::{DateTime, Utc};
use dashmap::{mapref::entry::Entry, DashMap};
use harnx_core::{
    abort::{create_abort_signal, AbortSignal},
    sink::with_agent_event_sink,
    tool::ToolResult,
};
use harnx_runtime::{
    config::{self, Config, GlobalConfig},
    run_agent_loop, AgentCallFn, AgentLoopContext, OnToolRoundFn,
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionState {
    Idle,
    Running {
        run_id: String,
        started_at: DateTime<Utc>,
    },
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
}

struct ActiveRun {
    run_id: RunId,
    started_at: DateTime<Utc>,
    abort_signal: AbortSignal,
    inject_tx: mpsc::Sender<String>,
}

struct RunFinished {
    run_id: RunId,
    result: anyhow::Result<()>,
    message_id: MessageId,
    thread_id: ThreadId,
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
    pending: VecDeque<String>,
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

impl SessionActor {
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
                let _ = reply.send(SubscribeResult {
                    snapshot: self.history_snapshot.clone(),
                    events: self.broadcast_tx.subscribe(),
                });
            }
            SessionCommand::Prompt {
                text,
                options,
                reply,
            } => match &self.active_run {
                None => {
                    let run_id = self.start_run(text, options, reap_sleep).await;
                    let _ = reply.send(PromptResult::Accepted {
                        run_id: run_id.to_string(),
                    });
                }
                Some(active_run) => {
                    // COMPLETING-STATE invariant: actor stays Running until the run-done
                    // arm fires. If `try_send` fails with Full or Closed, prompt is pushed
                    // to `pending`. Closed covers child-task exit before done is processed,
                    // so prompt survives finish boundary without separate Completing state.
                    let run_id = active_run.run_id.clone();
                    if active_run.inject_tx.try_send(text.clone()).is_err() {
                        self.pending.push_back(text);
                    }
                    let _ = reply.send(PromptResult::Enqueued {
                        run_id: run_id.to_string(),
                    });
                }
            },
            SessionCommand::Cancel { reply } => {
                self.pending.clear();
                if let Some(active_run) = &self.active_run {
                    active_run.abort_signal.set_ctrlc();
                }
                let _ = reply.send(());
            }
            SessionCommand::Get { reply } => {
                self.refresh_history_snapshot();
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

    async fn handle_run_done(
        &mut self,
        done: RunFinished,
        reap_sleep: &mut std::pin::Pin<&mut Sleep>,
    ) {
        self.run_done_task = None;
        self.active_run = None;
        self.refresh_history_snapshot();
        match done.result {
            Ok(()) => {
                let _ = self
                    .broadcast_tx
                    .send(Event::TextMessageEnd(TextMessageEndEvent {
                        base: base_event(),
                        message_id: done.message_id.clone(),
                    }));
                let _ = self.broadcast_tx.send(Event::RunFinished(RunFinishedEvent {
                    base: base_event(),
                    thread_id: done.thread_id,
                    run_id: done.run_id,
                    result: None,
                }));
            }
            Err(err) => {
                let _ = self.broadcast_tx.send(Event::RunError(RunErrorEvent {
                    base: base_event(),
                    message: err.to_string(),
                    code: None,
                }));
            }
        }

        if let Some(next_prompt) = self.pending.pop_front() {
            self.start_run(next_prompt, SessionPromptOptions::default(), reap_sleep)
                .await;
            return;
        }

        self.state = SessionState::Idle;
        if self.subscribers == 0 {
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
        let prompt_config =
            prompt_config_for_agent_session_from_global(&self.actor_config.base_config, &self.key);
        let run_id = RunId::random();
        let thread_id = derive_thread_id(&self.key.session);
        let message_id = MessageId::random();
        let started_at = Utc::now();
        let abort_signal = create_abort_signal();
        let (inject_tx, inject_rx) = mpsc::channel(COMMAND_BUFFER);

        let _ = self.broadcast_tx.send(Event::RunStarted(RunStartedEvent {
            base: base_event(),
            thread_id: thread_id.clone(),
            run_id: run_id.clone(),
        }));
        let _ = self
            .broadcast_tx
            .send(Event::TextMessageStart(TextMessageStartEvent {
                base: base_event(),
                message_id: message_id.clone(),
                role: Role::Assistant,
            }));

        let loop_ctx = build_loop_ctx(
            prompt_config.clone(),
            self.actor_config.call_fn.clone(),
            abort_signal.clone(),
            inject_rx,
            options.working_dir.clone(),
        );
        let event_tx = self.broadcast_tx.clone();
        let input = build_input(&prompt_config, &text).expect("build actor input");
        let done_tx = self.run_done_tx.clone();
        let run_id_for_task = run_id.clone();
        let thread_id_for_task = thread_id.clone();
        let message_id_for_task = message_id.clone();
        let base_config_for_snapshot = self.actor_config.base_config.clone();
        let session_key_for_snapshot = self.key.clone();
        let task = tokio::spawn(async move {
            let history_snapshot = Arc::new(move || {
                load_history_snapshot(&base_config_for_snapshot, &session_key_for_snapshot)
                    .unwrap_or_default()
            });
            let sink = Arc::new(BroadcastEventSender::new(
                event_tx,
                message_id_for_task.clone(),
                history_snapshot,
            ));
            let loop_result = with_agent_event_sink(sink, async {
                Box::pin(run_agent_loop(&loop_ctx, input)).await
            })
            .await;
            let _ = done_tx
                .send(RunFinished {
                    run_id: run_id_for_task,
                    result: loop_result,
                    message_id: message_id_for_task,
                    thread_id: thread_id_for_task,
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
        matches!(self.state, SessionState::Running { .. })
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

    fn refresh_history_snapshot(&mut self) {
        self.history_snapshot =
            load_history_snapshot(&self.actor_config.base_config, &self.key).unwrap_or_default();
    }
}

struct BroadcastEventSender {
    sink: AgUiSink,
}

impl BroadcastEventSender {
    fn new(
        tx: broadcast::Sender<Event>,
        message_id: MessageId,
        history_snapshot: Arc<dyn Fn() -> Vec<AgUiMessage> + Send + Sync>,
    ) -> Self {
        Self {
            sink: AgUiSink::new_broadcast_with_snapshot(tx, message_id, history_snapshot),
        }
    }
}

impl harnx_core::event::AgentEventSink for BroadcastEventSender {
    fn emit(
        &self,
        event: harnx_core::event::AgentEvent,
        source: Option<harnx_core::event::AgentSource>,
    ) {
        self.sink.emit(event, source);
    }
}

fn build_loop_ctx(
    prompt_config: GlobalConfig,
    call_fn: Option<AgentCallFn>,
    abort_signal: AbortSignal,
    inject_rx: mpsc::Receiver<String>,
    working_dir: Option<std::path::PathBuf>,
) -> AgentLoopContext {
    let shared_injected_text = Arc::new(Mutex::new(inject_rx));
    let on_tool_round: OnToolRoundFn = Arc::new(move |merged_input, _results: &[ToolResult]| {
        let shared_injected_text = shared_injected_text.clone();
        Box::pin(async move {
            let mut inject_rx = shared_injected_text.lock().await;
            if let Ok(text) = inject_rx.try_recv() {
                merged_input.set_injected_user_text(text);
            }
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
) -> GlobalConfig {
    let prompt_config = harnx_session::fork_prompt_config(base_config);
    {
        let mut cfg = prompt_config.write();
        cfg.use_agent_by_name(&key.agent).expect("set actor agent");
        cfg.use_session(Some(&key.session))
            .expect("set actor session");
    }
    prompt_config
}

fn build_input(
    prompt_config: &GlobalConfig,
    text: &str,
) -> anyhow::Result<harnx_core::input::Input> {
    Ok(config::input::from_str(prompt_config, text, None))
}

fn load_history_snapshot(
    base_config: &Config,
    key: &SessionKey,
) -> anyhow::Result<Vec<AgUiMessage>> {
    let prompt_config = prompt_config_for_agent_session_from_global(base_config, key);
    let messages = prompt_config
        .read()
        .session
        .as_ref()
        .map(|session| crate::ag_ui::history_messages_for_snapshot(&session.messages))
        .unwrap_or_default();
    Ok(messages)
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
    use crate::test_support::TestConfigSandbox;
    use harnx_core::{message::Message, tool::ToolCall};
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
        let prompt_config = prompt_config_for_agent_session_from_global(&base_config, &key);
        let messages = prompt_config
            .read()
            .session
            .as_ref()
            .expect("session exists")
            .messages
            .clone();
        messages
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
        release_first_tool_round.notify_one();
        tokio::task::yield_now().await;
        second_call_release.notify_one();
        sleep(Duration::from_millis(80)).await;

        let user_texts: Vec<String> = load_session_messages("plain", "inject")
            .iter()
            .filter(|msg| msg.role.is_user())
            .map(|msg| msg.content.to_text())
            .collect();
        assert!(user_texts.iter().any(|text| text == "queued follow-up"));
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
        sleep(Duration::from_millis(80)).await;

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

        let messages = load_session_messages("plain", "tool-history");
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
        sleep(Duration::from_millis(120)).await;

        let info = get_info(&handle).await;
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
        sleep(Duration::from_millis(80)).await;

        let user_texts: Vec<String> = load_session_messages("plain", "boundary")
            .iter()
            .filter(|msg| msg.role.is_user())
            .map(|msg| msg.content.to_text())
            .collect();
        assert!(user_texts.iter().any(|text| text == "boundary prompt"));
        assert!(call_count.load(Ordering::SeqCst) >= 2);
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
            other => panic!("expected running state, got {other:?}"),
        }

        gate_release.notify_one();
        sleep(Duration::from_millis(80)).await;
        let idle = get_info(&handle).await;
        assert_eq!(idle.state, SessionState::Idle);
    }
}
