//! Detached turn supervision with CAS-owned admission and task writes.
//! Local slots hold delivery handles, not ownership. Snapshots and update cursors
//! become visible through the shared authority and durable task stream.
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
    identity::{Principal, RequestIdentity},
    input_map::{message_to_input, InputLimits},
    store::{
        assert_local_id_no_dot, A2aStore, StoreError, TaskAccess, TaskChanges, TaskRecord,
        TaskVersion,
    },
};
pub use event_map::A2aEvent;
use event_map::{artifact, status, A2aEventSink, Output, FAILED_MESSAGE};
mod admission;
pub(crate) use admission::AllocatedAdmission;
mod authority;
mod cancellation;
mod coalescing;
pub mod event_map;
mod failure;
mod fixed_prompt;
mod inbox;
mod limits;
mod outbox;
mod persistence;
mod reconciler;
mod recovery;
mod remote;
mod supervision;
use authority::OwnedTask;
pub use supervision::SupervisionConfig;
mod publication;
mod publisher;
#[cfg(test)]
mod registry_tests;
mod stored_session;
use publication::{LiveState, StreamChannels};
use publisher::Publisher;

pub struct SessionRequest<'a> {
    pub export: &'a Export,
    pub owner: &'a RequestIdentity,
    pub local_id: Option<&'a str>,
    pub global_config: &'a GlobalConfig,
    pub activation_route: SessionActivationRoute,
    pub abort: AbortSignal,
}

pub struct TurnRequest<'a> {
    pub export: &'a Export,
    pub owner: &'a RequestIdentity,
    pub session: NatsSession,
    pub message: Message,
}

struct PendingTurn {
    session: NatsSession,
    input: Input,
    allocation: crate::store::TaskAllocation,
    authority: OwnedTask,
}

pub const EVENT_CAPACITY: usize = 64;
const ARTIFACT_INTERVAL: Duration = Duration::from_millis(100);

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
    fence: crate::store::context::OwnerFence,
    lease: Arc<harnx_runtime::nats_lease::NatsSessionLease>,
    session: NatsSession,
    cancel_tx: mpsc::Sender<()>,
    done: watch::Receiver<bool>,
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
    boot_id: String,
    #[cfg(feature = "fault-injection")]
    lease_config_override: parking_lot::Mutex<Option<harnx_runtime::nats_lease::NatsLeaseConfig>>,
    #[cfg(feature = "fault-injection")]
    hooks: Arc<crate::fault_injection::FaultHooks>,
}

pub struct StartTurnResult {
    /// Persisted, admitted snapshot. May already be superseded by live events.
    pub snapshot: TaskRecord,
    pub events: broadcast::Receiver<A2aEvent>,
    pub deduped: bool,
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
            boot_id: uuid::Uuid::new_v4().to_string(),
            #[cfg(feature = "fault-injection")]
            lease_config_override: Default::default(),
            #[cfg(feature = "fault-injection")]
            hooks: Default::default(),
        })
    }

    #[cfg(feature = "fault-injection")]
    pub fn fault_hooks(&self) -> Arc<crate::fault_injection::FaultHooks> {
        self.hooks.clone()
    }

    fn slot(&self, key: &ContextKey) -> Arc<ContextSlot> {
        let mut contexts = self.contexts.lock();
        // Active turns and concurrent callers hold strong references. Retire
        // idle slots so session GC doesn't leave an unbounded process cache.
        // Verified by nats_slot_pruning_retains_running_turn_and_removes_idle_slots.
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
        let initializer = session_initializer(export, &owner.principal, local_id)?;
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
                .bind_context(session.storage_key(), export, &owner.principal)
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

    async fn authorize_session(
        &self,
        export: &Export,
        owner: &RequestIdentity,
        session: &NatsSession,
    ) -> Result<()> {
        self.store
            .resolve_context(export, owner, session.session_id())
            .await?
            .filter(|key| key == session.storage_key())
            .ok_or(StoreError::NotFound)?;
        Ok(())
    }

    async fn launch_turn(
        self: &Arc<Self>,
        slot: Arc<ContextSlot>,
        mut active: OwnedMutexGuard<Option<Arc<RunnerHandle>>>,
        pending: PendingTurn,
    ) -> Result<StartTurnResult> {
        let PendingTurn {
            session,
            input,
            allocation,
            authority,
        } = pending;
        let task_id = allocation.task_id.clone();
        let (events, rx) = broadcast::channel(EVENT_CAPACITY);
        let (cancel_tx, cancel_rx) = mpsc::channel(1);
        let (done_tx, done) = watch::channel(false);
        let live = Arc::new(parking_lot::Mutex::new(LiveState::default()));
        *active = Some(Arc::new(RunnerHandle {
            task_id: task_id.clone(),
            lease: authority.lease.clone(),
            fence: authority.context.version()?.owner,
            session: session
                .clone()
                .with_admission_id(allocation.invocation_id.clone()),
            cancel_tx,
            done,
        }));
        let (admitted_tx, admitted_rx) = oneshot::channel();
        let publisher = Publisher::new(
            self.store.clone(),
            &session,
            StreamChannels { events, live },
            AdmissionOwnership {
                guard: active,
                authority,
            },
        );
        #[cfg(feature = "fault-injection")]
        let mut publisher = publisher;
        #[cfg(feature = "fault-injection")]
        {
            publisher.hooks = self.hooks.clone();
        }
        let detached = DetachedTurn {
            publisher,
            slot,
            session,
            input,
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
            deduped: false,
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
        owner: &RequestIdentity,
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

    /// Global watermark before authoritative snapshot, then an independent cursor.
    pub(crate) async fn stream_snapshot(
        &self,
        export: &Export,
        owner: &RequestIdentity,
        task_id: &str,
    ) -> Result<(TaskRecord, Option<broadcast::Receiver<A2aEvent>>)> {
        self.shared_snapshot(export, owner, task_id).await
    }

    /// Caller point read is authoritative; local coalescing isn't visible until CAS.
    pub async fn live_record(&self, export: &Export, record: TaskRecord) -> TaskRecord {
        let storage =
            harnx_core::session_identity::session_key(Some(&export.agent), &record.task.context_id);
        self.store
            .get_task(&storage, &record.task.id)
            .await
            .ok()
            .flatten()
            .unwrap_or(record)
    }

    /// A cloned watch keeps waiting independent of the admission/cancellation gate.
    pub(crate) async fn completion(
        &self,
        export: &Export,
        record: &TaskRecord,
    ) -> Option<watch::Receiver<bool>> {
        let key =
            harnx_core::session_identity::session_key(Some(&export.agent), &record.task.context_id);
        let authority = self.store.read_context(&key).await.ok().flatten()?;
        let slot = self.slot(&ContextKey::new(export, &record.task.context_id));
        let active = slot.active.lock().await;
        active
            .as_ref()
            .filter(|h| {
                h.task_id == record.task.id && authority.document.owner.as_ref() == Some(&h.fence)
            })
            .map(|h| h.done.clone())
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
        if self
            .store
            .read_context(session.storage_key())
            .await?
            .is_some()
        {
            let record = self
                .store
                .get_task_for_export(export, owner, task_id)
                .await?
                .ok_or(StoreError::NotFound)?;
            return self.reconcile_coordinated(export, session, record).await;
        }
        self.reconcile_legacy_orphan(
            TaskAccess {
                export,
                owner,
                task_id,
            },
            session,
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
    if let Err(error) = handle
        .session
        .interrupt_admitted_invocation("A2A shutdown")
        .await
    {
        warn!(%error, "shutdown remote cancellation failed");
    }
    let _ = done.wait_for(|finished| *finished).await;
}

struct ExecutionRequest<'a> {
    session: &'a NatsSession,
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
    session: NatsSession,
    input: Input,
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
            session,
            input,
            task_id,
            cancel_rx,
            admitted_tx,
            done_tx,
        } = self;
        let request = ExecutionRequest {
            session: &session,
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
        if let Err(error) = publisher
            .authority
            .release(&publisher.store, &publisher.storage_key)
            .await
        {
            warn!(%task_id, %error, "A2A context release unresolved");
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

struct AdmissionOwnership {
    guard: OwnedMutexGuard<Option<Arc<RunnerHandle>>>,
    authority: OwnedTask,
}

fn denial_confirmation_handler() -> Arc<ToolConfirmationHandler> {
    Arc::new(|request| {
        Box::pin(async move {
            info!(tool = %request.tool_name, tool_call_id = ?request.tool_call_id, "A2A tool confirmation denied");
            false
        })
    })
}
