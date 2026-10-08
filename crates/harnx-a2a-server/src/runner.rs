//! Detached turn supervision. The registry is a single-replica cache; KV is the
//! durable state; local live snapshots include not-yet-persisted artifact text.
//! Context gates fence admission against stale cancellation.
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

use a2a_lf::{
    Message, StreamResponse, Task, TaskArtifactUpdateEvent, TaskState, TaskStatusUpdateEvent,
};
use anyhow::{Context, Result};
use futures::FutureExt;
use harnx_core::{abort::AbortSignal, input::Input};
use harnx_runtime::{
    config::GlobalConfig, nats_tool_confirmation::ToolConfirmationHandler, NatsSession,
    NatsSessionConfig, RunTurnOptions, SessionActivationRoute, SessionInitializer,
};
use tokio::sync::{broadcast, mpsc, oneshot, watch, Mutex, OwnedMutexGuard};
use tracing::{info, warn};

use crate::{
    exports::Export,
    identity::Principal,
    input_map::{message_to_input, InputLimits},
    store::{
        assert_local_id_no_dot, new_task_id, A2aStore, StoreError, TaskAccess, TaskChanges,
        TaskRecord, TaskSeed, TaskVersion,
    },
};
pub use event_map::A2aEvent;
use event_map::{artifact, status, A2aEventSink, Output, FAILED_MESSAGE};
pub mod event_map;
mod failure;
mod publication;
use publication::{LiveState, StreamChannels};

pub struct SessionRequest<'a> {
    pub export: &'a Export,
    pub owner: &'a Principal,
    pub local_id: Option<&'a str>,
    pub global_config: &'a GlobalConfig,
    pub activation_route: SessionActivationRoute,
    pub abort: AbortSignal,
}

pub struct TurnRequest<'a> {
    pub export: &'a Export,
    pub owner: &'a Principal,
    pub session: NatsSession,
    pub message: Message,
}

struct PendingTurn {
    session: NatsSession,
    message: Message,
    input: Input,
}

pub const EVENT_CAPACITY: usize = 64;
const ARTIFACT_INTERVAL: Duration = Duration::from_millis(100);
const PERSIST_INTERVAL: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub struct ContextKey {
    pub cluster: String,
    pub export: String,
    pub local_id: String,
}
impl ContextKey {
    pub fn new(export: &Export, local_id: &str) -> Self {
        Self {
            cluster: export.cluster.as_deref().unwrap_or("__local__").into(),
            export: export.public_name.clone(),
            local_id: local_id.into(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunnerError {
    Busy,
    Terminal,
}
impl std::fmt::Display for RunnerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Busy => "context already has an active task, retry later",
            Self::Terminal => "task is already terminal",
        })
    }
}
impl std::error::Error for RunnerError {}

struct RunnerHandle {
    task_id: String,
    session: NatsSession,
    cancel_tx: mpsc::Sender<()>,
    events: broadcast::Sender<A2aEvent>,
    done: watch::Receiver<bool>,
    live: Arc<parking_lot::Mutex<LiveState>>,
}

#[derive(Default)]
struct ContextSlot {
    active: Arc<Mutex<Option<Arc<RunnerHandle>>>>,
}

/// One runner per cluster/store. Do not share a store between different brokers.
pub struct Runner {
    store: Arc<A2aStore>,
    contexts: parking_lot::Mutex<HashMap<ContextKey, Arc<ContextSlot>>>,
    shutting_down: AtomicBool,
}

pub struct StartTurnResult {
    /// Persisted, admitted snapshot. May already be superseded by live events.
    pub snapshot: TaskRecord,
    pub events: broadcast::Receiver<A2aEvent>,
}

pub struct Subscription {
    /// Emit this first, then drop events whose sequence is <= snapshot.stream_seq.
    pub snapshot: TaskRecord,
    /// RecvError::Lagged must end this subscriber, never cancel the turn.
    pub events: broadcast::Receiver<A2aEvent>,
}

impl Runner {
    pub fn new(store: Arc<A2aStore>) -> Arc<Self> {
        Arc::new(Self {
            store,
            contexts: Default::default(),
            shutting_down: AtomicBool::new(false),
        })
    }

    fn slot(&self, key: &ContextKey) -> Arc<ContextSlot> {
        let mut contexts = self.contexts.lock();
        // Active turns and concurrent callers hold strong references. Retire
        // idle slots so session GC doesn't leave an unbounded process cache.
        contexts.retain(|_, slot| Arc::strong_count(slot) > 1);
        contexts.entry(key.clone()).or_default().clone()
    }

    /// Resolve ownership BEFORE runtime resume, which otherwise creates absent
    /// sessions. The handler obtains activation_route from its worker supervisor.
    pub async fn session(&self, request: SessionRequest<'_>) -> Result<NatsSession> {
        let SessionRequest {
            export,
            owner,
            local_id,
            global_config,
            activation_route,
            abort,
        } = request;
        if let Some(local_id) = local_id {
            self.store
                .resolve_context(export, owner, local_id)
                .await?
                .ok_or(StoreError::NotFound)?;
        }
        let initializer = session_initializer(export, owner, local_id)?;
        let session = Box::pin(NatsSession::from_global_config(
            NatsSessionConfig {
                cluster: export.cluster.clone().unwrap_or_else(|| "__local__".into()),
                initializer,
                session_id: local_id.map(str::to_owned),
                activation_route,
            },
            global_config,
            abort,
        ))
        .await?;
        if local_id.is_none() {
            assert_local_id_no_dot(session.session_id())?;
            self.store
                .bind_context(session.storage_key(), export, owner)
                .await?;
        }
        Ok(session)
    }

    /// Call after identity, binding, dedupe and orphan checks. No client task IDs
    /// are accepted here. The original message is persisted without rewriting it.
    /// Admission and execution survive a dropped caller future/HTTP connection.
    pub async fn start_turn(
        self: &Arc<Self>,
        request: TurnRequest<'_>,
        limits: InputLimits,
    ) -> Result<StartTurnResult> {
        self.authorize_session(request.export, request.owner, &request.session)
            .await?;
        let input = message_to_input(&request.message, limits)?;
        self.start_turn_with_input(request, input).await
    }

    /// HTTP admission renders before session allocation; reuse that validated input.
    pub(crate) async fn start_turn_with_input(
        self: &Arc<Self>,
        request: TurnRequest<'_>,
        input: Input,
    ) -> Result<StartTurnResult> {
        let TurnRequest {
            export,
            owner,
            session,
            message,
        } = request;
        self.authorize_session(export, owner, &session).await?;
        let slot = self.slot(&ContextKey::new(export, session.session_id()));
        let active = slot.active.clone().lock_owned().await;
        anyhow::ensure!(
            !self.shutting_down.load(Ordering::SeqCst),
            "A2A runner is shutting down"
        );
        self.wait_previous_turn(active.as_ref(), session.storage_key())
            .await?;
        self.launch_turn(
            slot,
            active,
            PendingTurn {
                session,
                message,
                input,
            },
        )
        .await
    }

    async fn authorize_session(
        &self,
        export: &Export,
        owner: &Principal,
        session: &NatsSession,
    ) -> Result<()> {
        self.store
            .resolve_context(export, owner, session.session_id())
            .await?
            .filter(|key| key == session.storage_key())
            .ok_or(StoreError::NotFound)?;
        Ok(())
    }

    async fn wait_previous_turn(
        &self,
        handle: Option<&Arc<RunnerHandle>>,
        storage_key: &str,
    ) -> Result<()> {
        let Some(handle) = handle else {
            return Ok(());
        };
        let mut done = handle.done.clone();
        if *done.borrow() {
            return Ok(());
        }
        let current = self.store.get_task(storage_key, &handle.task_id).await?;
        if !current.is_some_and(|record| record.task.status.state.is_terminal()) {
            return Err(RunnerError::Busy.into());
        }
        done.wait_for(|finished| *finished).await?;
        Ok(())
    }

    async fn launch_turn(
        self: &Arc<Self>,
        slot: Arc<ContextSlot>,
        mut active: OwnedMutexGuard<Option<Arc<RunnerHandle>>>,
        pending: PendingTurn,
    ) -> Result<StartTurnResult> {
        let task_id = new_task_id(pending.session.session_id());
        let (events, rx) = broadcast::channel(EVENT_CAPACITY);
        let (cancel_tx, cancel_rx) = mpsc::channel(1);
        let (done_tx, done) = watch::channel(false);
        let live = Arc::new(parking_lot::Mutex::new(LiveState::default()));
        *active = Some(Arc::new(RunnerHandle {
            task_id: task_id.clone(),
            session: pending.session.clone(),
            cancel_tx,
            events: events.clone(),
            done,
            live: live.clone(),
        }));
        let (admitted_tx, admitted_rx) = oneshot::channel();
        let publisher = Publisher::new(
            self.store.clone(),
            pending.session.storage_key(),
            StreamChannels { events, live },
            active,
        );
        let detached = DetachedTurn {
            publisher,
            slot,
            pending,
            task_id,
            cancel_rx,
            admitted_tx,
            done_tx,
        };
        tokio::spawn(detached.run());
        let snapshot = admitted_rx
            .await
            .context("turn stopped before admission")??;
        Ok(StartTurnResult {
            snapshot,
            events: rx,
        })
    }

    /// Settle admitted turns before the process drops its managed worker.
    pub async fn shutdown(&self) {
        // The check under each context gate prevents admissions arriving after
        // this snapshot from escaping shutdown. Existing admissions hold slots.
        self.shutting_down.store(true, Ordering::SeqCst);
        let slots: Vec<_> = self.contexts.lock().values().cloned().collect();
        for slot in slots {
            let active = slot.active.lock().await;
            let Some(handle) = active.as_ref() else {
                continue;
            };
            shutdown_turn(handle).await;
        }
    }

    pub async fn is_busy(&self, key: &ContextKey) -> bool {
        self.slot(key)
            .active
            .lock()
            .await
            .as_ref()
            .is_some_and(|handle| !*handle.done.borrow())
    }

    pub async fn subscribe(
        &self,
        export: &Export,
        owner: &Principal,
        task_id: &str,
    ) -> Result<Subscription> {
        let (snapshot, events) = self.stream_snapshot(export, owner, task_id).await?;
        if snapshot.task.status.state.is_terminal() {
            return Err(RunnerError::Terminal.into());
        }
        Ok(Subscription {
            snapshot,
            events: events.ok_or(RunnerError::Busy)?,
        })
    }

    /// Snapshot and subscribe under the publishing lock: events at or below the
    /// snapshot cursor are already represented, even when KV persistence is throttled.
    pub(crate) async fn stream_snapshot(
        &self,
        export: &Export,
        owner: &Principal,
        task_id: &str,
    ) -> Result<(TaskRecord, Option<broadcast::Receiver<A2aEvent>>)> {
        let (context, _) =
            crate::store::parse_task_id(task_id).map_err(|_| StoreError::NotFound)?;
        let key = self
            .store
            .resolve_context(export, owner, context)
            .await?
            .ok_or(StoreError::NotFound)?;
        let slot = self.slot(&ContextKey::new(export, context));
        let active = slot.active.lock().await;
        if let Some(handle) = active.as_ref().filter(|h| h.task_id == task_id) {
            let live = handle.live.lock();
            let events = handle.events.subscribe();
            let snapshot = live.snapshot().context("missing admitted live snapshot")?;
            return Ok((snapshot, Some(events)));
        }
        let events = None;
        let snapshot = self
            .store
            .get_task(&key, task_id)
            .await?
            .ok_or(StoreError::NotFound)?;
        if !snapshot.task.status.state.is_terminal() && events.is_none() {
            return Err(RunnerError::Busy.into());
        }
        Ok((snapshot, events))
    }

    /// Ownership must already have been checked by the caller's point read.
    pub async fn live_record(&self, export: &Export, record: TaskRecord) -> TaskRecord {
        let slot = self.slot(&ContextKey::new(export, &record.task.context_id));
        let active = slot.active.lock().await;
        active
            .as_ref()
            .filter(|h| h.task_id == record.task.id)
            .and_then(|h| h.live.lock().snapshot())
            .unwrap_or(record)
    }

    /// A cloned watch keeps waiting independent of the admission/cancellation gate.
    pub(crate) async fn completion(
        &self,
        export: &Export,
        record: &TaskRecord,
    ) -> Option<watch::Receiver<bool>> {
        let slot = self.slot(&ContextKey::new(export, &record.task.context_id));
        let active = slot.active.lock().await;
        active
            .as_ref()
            .filter(|h| h.task_id == record.task.id)
            .map(|h| h.done.clone())
    }

    /// Caller rejects terminal tasks. Completion racing cancellation wins.
    pub async fn cancel_task(
        &self,
        export: &Export,
        owner: &Principal,
        task_id: &str,
    ) -> Result<TaskRecord> {
        let record = self
            .store
            .get_task_for_export(export, owner, task_id)
            .await?
            .ok_or(StoreError::NotFound)?;
        let slot = self.slot(&ContextKey::new(export, &record.task.context_id));
        let active = slot.active.lock().await;
        let current = self
            .store
            .get_task_for_export(export, owner, task_id)
            .await?
            .ok_or(StoreError::NotFound)?;
        if current.task.status.state.is_terminal() {
            return Ok(current);
        }
        let handle = active
            .as_ref()
            .filter(|h| h.task_id == task_id)
            .ok_or(StoreError::NotFound)?;
        let mut done = handle.done.clone();
        if !*done.borrow() {
            let _ = handle.cancel_tx.try_send(());
            if let Err(error) = handle.session.cancel_pending_turn().await {
                warn!(%task_id, %error, "CancelTask remote cancellation failed");
            }
            done.wait_for(|finished| *finished).await?;
        }
        self.store
            .get_task_for_export(export, owner, task_id)
            .await?
            .ok_or_else(|| StoreError::NotFound.into())
    }

    /// Handler calls this AFTER dedupe, BEFORE busy/terminal rejection or admit.
    /// Missing live runner doesn't prove the worker stopped. Fence it first.
    pub async fn reconcile_orphan(
        &self,
        access: TaskAccess<'_>,
        session: &NatsSession,
    ) -> Result<TaskRecord> {
        let TaskAccess {
            export,
            owner,
            task_id,
        } = access;
        self.authorize_session(export, owner, session).await?;
        let slot = self.slot(&ContextKey::new(export, session.session_id()));
        let mut active = slot.active.lock().await;
        if active
            .as_ref()
            .is_some_and(|handle| handle.done.has_changed().is_err() && !*handle.done.borrow())
        {
            // The supervisor vanished without settling. Fence its worker as an orphan.
            *active = None;
        }
        // A live turn may have completed while this caller waited for the gate.
        let record = self
            .store
            .get_task_for_export(export, owner, task_id)
            .await?
            .filter(|r| r.task.context_id == session.session_id())
            .ok_or(StoreError::NotFound)?;
        if record.task.status.state.is_terminal() {
            return Ok(record);
        }
        if let Some(handle) = active.as_ref().filter(|handle| !*handle.done.borrow()) {
            if handle.task_id == task_id {
                return Ok(handle.live.lock().snapshot().unwrap_or(record));
            }
            // Never send a session-wide cancel at a newer live task.
            return Err(RunnerError::Busy.into());
        }
        // A completed supervisor with nonterminal KV state failed persistence.
        // Treat it as abandoned, rather than waiting for a writer that has stopped.
        *active = None;
        if let Err(error) = session.cancel_pending_turn().await {
            warn!(%task_id, %error, "orphan remote cancellation failed");
        }
        self.store
            .update_task(
                TaskVersion {
                    storage_key: session.storage_key(),
                    task_id,
                    revision: record.revision,
                },
                TaskChanges {
                    status: Some(status(
                        TaskState::Failed,
                        Some("interrupted by server restart"),
                    )),
                    ..Default::default()
                },
            )
            .await
    }
}

fn session_initializer(
    export: &Export,
    owner: &Principal,
    local_id: Option<&str>,
) -> Result<SessionInitializer> {
    let initializer = SessionInitializer::named(export.agent.clone(), Default::default());
    if local_id.is_some() {
        return Ok(initializer);
    }
    let Some(user_id) = owner.user_id() else {
        return Ok(initializer);
    };
    Ok(initializer.with_properties(serde_json::from_value(
        serde_json::json!({"user_id": {"value": user_id, "inherit": true}}),
    )?))
}

async fn shutdown_turn(handle: &RunnerHandle) {
    let mut done = handle.done.clone();
    if *done.borrow() {
        return;
    }
    let _ = handle.cancel_tx.try_send(());
    if let Err(error) = handle.session.cancel_pending_turn().await {
        warn!(%error, "shutdown remote cancellation failed");
    }
    let _ = done.wait_for(|finished| *finished).await;
}

struct ExecutionRequest<'a> {
    session: &'a NatsSession,
    task_id: String,
    message: Message,
    input: Input,
    cancel_rx: mpsc::Receiver<()>,
    admitted_tx: oneshot::Sender<Result<TaskRecord>>,
}

struct AdmittedTurn {
    input: Input,
    cancel_rx: mpsc::Receiver<()>,
    admitted_tx: oneshot::Sender<Result<TaskRecord>>,
}

struct DetachedTurn {
    publisher: Publisher,
    slot: Arc<ContextSlot>,
    pending: PendingTurn,
    task_id: String,
    cancel_rx: mpsc::Receiver<()>,
    admitted_tx: oneshot::Sender<Result<TaskRecord>>,
    done_tx: watch::Sender<bool>,
}

impl DetachedTurn {
    async fn run(self) {
        let Self {
            mut publisher,
            slot,
            pending,
            task_id,
            cancel_rx,
            admitted_tx,
            done_tx,
        } = self;
        let PendingTurn {
            session,
            message,
            input,
        } = pending;
        let request = ExecutionRequest {
            session: &session,
            task_id: task_id.clone(),
            message,
            input,
            cancel_rx,
            admitted_tx,
        };
        let result = std::panic::AssertUnwindSafe(publisher.execute(request))
            .catch_unwind()
            .await
            .unwrap_or_else(|_| Err(anyhow::anyhow!("turn runner panicked")));
        if let Err(error) = result {
            warn!(%task_id, %error, "A2A turn failed");
            publisher.fail_turn(&session, &task_id).await;
        }
        publisher.admission_guard.take();
        done_tx.send_replace(true);
        // CancelTask holds the gate until remote cancellation and final state settle.
        let mut active = slot.active.lock().await;
        if active
            .as_ref()
            .is_some_and(|handle| handle.task_id == task_id)
        {
            *active = None;
        }
    }
}

fn turn_outcome(
    result: harnx_runtime::NatsTurnResult,
    output: &str,
) -> (TaskState, Option<String>) {
    if result.was_cancelled {
        return (TaskState::Canceled, None);
    }
    if let Some(error) = result.error {
        warn!(%error, "A2A worker turn failed");
        return (TaskState::Failed, Some(FAILED_MESSAGE.to_owned()));
    }
    (
        TaskState::Completed,
        Some(result.response.unwrap_or_else(|| output.to_owned())),
    )
}

struct Publisher {
    store: Arc<A2aStore>,
    storage_key: String,
    events: broadcast::Sender<A2aEvent>,
    record: Option<Arc<TaskRecord>>,
    live: Arc<parking_lot::Mutex<LiveState>>,
    last_persist: tokio::time::Instant,
    output: Output,
    admission_guard: Option<OwnedMutexGuard<Option<Arc<RunnerHandle>>>>,
}
impl Publisher {
    fn new(
        store: Arc<A2aStore>,
        key: &str,
        channels: StreamChannels,
        admission_guard: OwnedMutexGuard<Option<Arc<RunnerHandle>>>,
    ) -> Self {
        Self {
            store,
            storage_key: key.into(),
            events: channels.events,
            record: None,
            live: channels.live,
            last_persist: tokio::time::Instant::now(),
            output: Output::default(),
            admission_guard: Some(admission_guard),
        }
    }
    fn record(&self) -> &TaskRecord {
        self.record
            .as_deref()
            .expect("task persisted before admission")
    }
    fn publish_snapshot(&mut self) {
        let mut live = self.live.lock();
        live.record = self.record.clone();
    }
    fn send(&mut self, response: StreamResponse) {
        // Updating the snapshot and sending must share the subscription lock.
        let mut live = self.live.lock();
        live.sequence += 1;
        live.record = self.record.clone();
        live.apply_response(&response);
        let _ = self.events.send(A2aEvent {
            sequence: live.sequence,
            response,
        });
    }
    async fn set_status(&mut self, state: TaskState, text: Option<&str>) -> Result<()> {
        let record = self.record();
        let status = status(state, text);
        // Completed history includes the same final agent message as status.
        // Keep original inbound parts unchanged and persist both atomically.
        let history = status
            .message
            .as_ref()
            .filter(|_| status.state == TaskState::Completed)
            .map(|message| {
                let mut history = record.task.history.clone().unwrap_or_default();
                history.push(message.clone());
                history
            });
        self.record = Some(Arc::new(
            self.store
                .update_task(
                    TaskVersion {
                        storage_key: &self.storage_key,
                        task_id: &record.task.id,
                        revision: record.revision,
                    },
                    TaskChanges {
                        status: Some(status),
                        history,
                        artifacts: if self.output.sent || !self.output.text.is_empty() {
                            Some(vec![artifact(self.output.text.clone())])
                        } else {
                            record.task.artifacts.clone()
                        },
                    },
                )
                .await?,
        ));
        if self.record().task.status.state.is_terminal() && !self.output.pending.is_empty() {
            // A failed final flush may have left unpublished text. The status
            // write includes it durably, so publish its replacement before status.
            self.send(StreamResponse::ArtifactUpdate(TaskArtifactUpdateEvent {
                task_id: self.record().task.id.clone(),
                context_id: self.record().task.context_id.clone(),
                artifact: artifact(self.output.text.clone()),
                append: Some(false),
                last_chunk: Some(true),
                metadata: None,
            }));
            self.output.pending.clear();
            self.output.sent = true;
        }
        self.send_status();
        Ok(())
    }

    fn send_status(&mut self) {
        let record = self.record();
        self.send(StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
            task_id: record.task.id.clone(),
            context_id: record.task.context_id.clone(),
            status: record.task.status.clone(),
            metadata: None,
        }));
    }

    async fn flush_artifact(&mut self, last: bool, replace: bool) -> Result<()> {
        if !last && self.output.pending.is_empty() {
            return Ok(());
        }
        if last || self.last_persist.elapsed() >= PERSIST_INTERVAL {
            let record = self.record();
            self.record = Some(Arc::new(
                self.store
                    .update_task(
                        TaskVersion {
                            storage_key: &self.storage_key,
                            task_id: &record.task.id,
                            revision: record.revision,
                        },
                        TaskChanges {
                            artifacts: Some(vec![artifact(self.output.text.clone())]),
                            ..Default::default()
                        },
                    )
                    .await?,
            ));
            self.last_persist = tokio::time::Instant::now();
        }
        let text = if replace {
            self.output.text.clone()
        } else {
            std::mem::take(&mut self.output.pending)
        };
        let record = self.record();
        self.send(StreamResponse::ArtifactUpdate(TaskArtifactUpdateEvent {
            task_id: record.task.id.clone(),
            context_id: record.task.context_id.clone(),
            artifact: artifact(text),
            append: Some(self.output.sent && !replace),
            last_chunk: Some(last),
            metadata: None,
        }));
        self.output.sent = true;
        self.output.pending.clear();
        Ok(())
    }
    async fn execute(&mut self, request: ExecutionRequest<'_>) -> Result<()> {
        let ExecutionRequest {
            session,
            task_id,
            message,
            input,
            cancel_rx,
            admitted_tx,
        } = request;
        let task = Task {
            id: task_id,
            context_id: session.session_id().into(),
            status: status(TaskState::Submitted, None),
            artifacts: None,
            history: Some(vec![message]),
            metadata: None,
        };
        self.record = Some(Arc::new(
            self.store
                .create_task(
                    &self.storage_key,
                    TaskSeed {
                        task,
                        user_msg_id: String::new(),
                        user_msg_seq: 0,
                        execution_id: String::new(),
                    },
                )
                .await?,
        ));
        self.set_status(TaskState::Working, None).await?;
        let route = session
            .tool_confirmation_route(denial_confirmation_handler())
            .await?;
        let session = session.clone().with_external_admission();
        let result = self
            .follow_turn(
                &session,
                &route,
                AdmittedTurn {
                    input,
                    cancel_rx,
                    admitted_tx,
                },
            )
            .await;
        route.close().await;
        result
    }

    async fn follow_turn(
        &mut self,
        session: &NatsSession,
        route: &harnx_runtime::nats_tool_confirmation::ToolConfirmationRoute,
        turn: AdmittedTurn,
    ) -> Result<()> {
        let AdmittedTurn {
            input,
            cancel_rx,
            admitted_tx,
        } = turn;
        let appended = session
            .admit_input_with_tool_confirmation_route(&input, route)
            .await?;
        let record = self.record();
        self.record = Some(Arc::new(
            self.store
                .update_admission(
                    TaskVersion {
                        storage_key: &self.storage_key,
                        task_id: &record.task.id,
                        revision: record.revision,
                    },
                    &appended,
                )
                .await?,
        ));
        self.publish_snapshot();
        self.last_persist = tokio::time::Instant::now();
        // Cancellation must not land before the admitted user message.
        self.admission_guard.take();
        let snapshot = self.live.lock().snapshot().expect("admitted snapshot");
        let _ = admitted_tx.send(Ok(snapshot));
        let (tx, mut rx) = mpsc::unbounded_channel();
        let follow = session.follow_admitted_prompt(
            appended,
            Arc::new(A2aEventSink(tx)),
            Some(cancel_rx),
            Some(route.subject()),
            RunTurnOptions::default(),
        );
        tokio::pin!(follow);
        let mut interval = tokio::time::interval(ARTIFACT_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let result = loop {
            tokio::select! {
                result = &mut follow => break result?,
                Some(event) = rx.recv() => self.output.accept(event),
                _ = interval.tick() => self.flush_artifact(false, false).await?,
            }
        };
        while let Ok(event) = rx.try_recv() {
            self.output.accept(event);
        }
        self.complete_turn(result).await
    }

    async fn complete_turn(&mut self, result: harnx_runtime::NatsTurnResult) -> Result<()> {
        let (state, text) = turn_outcome(result, &self.output.text);
        let replace = state == TaskState::Completed && text.as_deref() != Some(&self.output.text);
        if replace {
            self.output.text = text.clone().unwrap_or_default();
        }
        self.flush_artifact(true, replace).await?;
        self.set_status(state, text.as_deref()).await
    }
}

fn denial_confirmation_handler() -> Arc<ToolConfirmationHandler> {
    Arc::new(|request| {
        Box::pin(async move {
            info!(tool = %request.tool_name, tool_call_id = ?request.tool_call_id, "A2A tool confirmation denied");
            false
        })
    })
}
