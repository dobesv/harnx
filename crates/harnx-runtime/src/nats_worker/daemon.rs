//! Worker daemon and session activation.

use super::agent_loop::{
    build_mid_turn_injection_callback, fold_new_user_messages_since,
    run_agent_loop_with_nats_inner, RunAgentLoopArgs,
};
use super::backend::NatsSessionLogBackend;
use super::control::{control_subject, ControlCommand};
use super::tool_supervisor::{ToolServerStartConfig, ToolServerSupervisor};
use crate::config::{resolve_local_nats_server_config, GlobalConfig, Input, LOCAL_CLUSTER_KEY};
use crate::nats_lease::{NatsLeaseAcquireParams, NatsLeaseConfig, NatsSessionLease};
use crate::nats_metrics;
use crate::nats_session_index;
use anyhow::{Context, Result};
use async_nats::header::{HeaderValue, NATS_MESSAGE_ID};
use async_nats::jetstream::{
    self,
    consumer::{pull, DeliverPolicy},
    stream::{Config as StreamConfig, RetentionPolicy, StorageType},
};
use chrono::Utc;
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

// ---------------------------------------------------------------------------
// P2.1 worker daemon + SessionActivate dispatch
// ---------------------------------------------------------------------------

const WORK_NOTIFY_STREAM_PREFIX: &str = "WORK_NOTIFY_";
const WORK_NOTIFY_CONSUMER_PREFIX: &str = "worker-";
const WORK_NOTIFY_ACK_WAIT: Duration = Duration::from_secs(30);
const WORK_NOTIFY_INACTIVE_THRESHOLD: Duration = Duration::from_secs(60 * 60);

/// Configuration for a worker daemon instance.
#[derive(Debug, Clone)]
pub struct WorkerDaemonConfig {
    pub cluster: String,
    pub worker_id: String,
    pub lease: NatsLeaseConfig,
}

impl WorkerDaemonConfig {
    pub fn new(cluster: impl Into<String>, worker_id: impl Into<String>) -> Self {
        Self {
            cluster: cluster.into(),
            worker_id: worker_id.into(),
            lease: NatsLeaseConfig::default(),
        }
    }
}

/// Activation request published by a client to wake/claim a session.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionActivate {
    pub session_id: String,
    pub agent: String,
    pub epoch: String,
}

impl SessionActivate {
    pub fn new(session_id: impl Into<String>, agent: impl Into<String>) -> Self {
        Self {
            session_id: session_id.into(),
            agent: agent.into(),
            epoch: Utc::now().to_rfc3339(),
        }
    }

    /// Dedup id for the notify stream (`Nats-Msg-Id`): session + epoch.
    pub fn msg_id(&self) -> String {
        format!("{}:{}", self.session_id, self.epoch)
    }
}

/// Generate a fresh remote session id (uuid v7, time-ordered).
pub fn new_remote_session_id() -> String {
    uuid::Uuid::now_v7().to_string()
}

/// Notify subject for a cluster's session activations.
pub fn notify_subject(cluster: &str) -> String {
    format!("cluster.{cluster}.sessions.notify")
}

/// Core-NATS subject used by workers to announce that their activation pull
/// consumer exists and can receive [`SessionActivate`] notifications.
pub fn worker_ready_subject(cluster: &str) -> String {
    format!("cluster.{cluster}.worker.ready")
}

fn notify_stream_name(cluster: &str) -> String {
    format!(
        "{WORK_NOTIFY_STREAM_PREFIX}{}",
        sanitize_name_component(cluster)
    )
}

fn durable_consumer_name(worker_id: &str) -> String {
    format!(
        "{WORK_NOTIFY_CONSUMER_PREFIX}{}",
        sanitize_name_component(worker_id)
    )
}

pub(crate) fn should_append_control_log_entry(lease: &NatsSessionLease) -> bool {
    lease.is_held()
}

fn sanitize_name_component(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if is_valid_name_component_char(ch) {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

fn is_valid_name_component_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_')
}

async fn ensure_notify_stream(
    jetstream: &jetstream::Context,
    cluster: &str,
    subject: &str,
) -> Result<jetstream::stream::Stream> {
    let name = notify_stream_name(cluster);
    if let Ok(stream) = jetstream.get_stream(&name).await {
        return Ok(stream);
    }
    match jetstream
        .create_stream(StreamConfig {
            name: name.clone(),
            description: Some("session activation work queue".to_string()),
            subjects: vec![subject.to_string()],
            retention: RetentionPolicy::WorkQueue,
            storage: StorageType::File,
            ..Default::default()
        })
        .await
    {
        Ok(stream) => Ok(stream),
        Err(_) => jetstream
            .get_stream(&name)
            .await
            .map_err(anyhow::Error::from)
            .with_context(|| format!("Failed to create notify stream for cluster '{cluster}'")),
    }
}

/// Publish a `SessionActivate` notification (idempotent via `Nats-Msg-Id`).
pub async fn publish_session_activate(
    jetstream: &jetstream::Context,
    cluster: &str,
    activation: &SessionActivate,
) -> Result<u64> {
    let subject = notify_subject(cluster);
    ensure_notify_stream(jetstream, cluster, &subject).await?;
    let payload = serde_json::to_vec(activation).context("serialize SessionActivate")?;
    let mut headers = async_nats::HeaderMap::new();
    headers.insert(NATS_MESSAGE_ID, HeaderValue::from(activation.msg_id()));
    let ack = jetstream
        .publish_with_headers(subject, headers, payload.into())
        .await
        .context("publish SessionActivate")?
        .await
        .context("ack SessionActivate")?;
    Ok(ack.sequence)
}

/// Shared, borrowed context for the `derive_*_turn_input` helpers. Groups the
/// three references they all thread through so each helper stays within the
/// function-argument budget.
#[derive(Clone, Copy)]
struct TurnInputCtx<'a> {
    activation: &'a SessionActivate,
    per_session: &'a GlobalConfig,
    backend: &'a NatsSessionLogBackend,
}

struct WorkerStartup {
    jetstream: jetstream::Context,
    client: async_nats::Client,
    consumer: jetstream::consumer::Consumer<pull::Config>,
}

async fn prepare_worker_startup(
    config: &GlobalConfig,
    daemon: &WorkerDaemonConfig,
) -> Result<WorkerStartup> {
    let (jetstream, client) = {
        let cfg = config.read().clone();
        let jetstream = cfg.nats_jetstream(&daemon.cluster).await?;
        let client = cfg.nats_client(&daemon.cluster).await?;
        (jetstream, client)
    };
    let subject = notify_subject(&daemon.cluster);
    let stream = ensure_notify_stream(&jetstream, &daemon.cluster, &subject).await?;
    let consumer_name = durable_consumer_name(&daemon.worker_id);
    let consumer = stream
        .get_or_create_consumer(
            &consumer_name,
            pull::Config {
                durable_name: Some(consumer_name.clone()),
                deliver_policy: DeliverPolicy::All,
                ack_wait: WORK_NOTIFY_ACK_WAIT,
                filter_subject: subject,
                inactive_threshold: WORK_NOTIFY_INACTIVE_THRESHOLD,
                max_deliver: -1,
                ..Default::default()
            },
        )
        .await
        .with_context(|| format!("create worker consumer '{consumer_name}'"))?;
    Ok(WorkerStartup {
        jetstream,
        client,
        consumer,
    })
}

fn spawn_readiness_publisher(client: async_nats::Client, daemon: &WorkerDaemonConfig) {
    let subject = worker_ready_subject(&daemon.cluster);
    let worker_id = daemon.worker_id.clone();
    tokio::spawn(async move {
        loop {
            if let Err(error) = client
                .publish(subject.clone(), worker_id.clone().into())
                .await
            {
                log::warn!("failed to publish worker readiness marker: {error}");
                return;
            }
            if let Err(error) = client.flush().await {
                log::warn!("failed to flush worker readiness marker: {error}");
                return;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    });
}

async fn optional_session_index(
    jetstream: &jetstream::Context,
) -> Option<async_nats::jetstream::kv::Store> {
    match nats_session_index::ensure_index_bucket(jetstream).await {
        Ok(store) => Some(store),
        Err(error) => {
            log::warn!(
                "session index disabled: failed to ensure harnx_sessions index bucket: {:#}",
                error
            );
            None
        }
    }
}

async fn start_local_tool_servers(
    daemon: &WorkerDaemonConfig,
    client: async_nats::Client,
    instance_id: &harnx_core::instance::InstanceId,
) -> Option<ToolServerSupervisor> {
    if daemon.cluster != LOCAL_CLUSTER_KEY {
        return None;
    }
    let result = async {
        let server = resolve_local_nats_server_config().await?;
        let token = server
            .token
            .as_deref()
            .context("local NATS tool servers require HARNX_NATS_TOKEN")?;
        let start = ToolServerStartConfig::new(client, instance_id.clone(), &server.url, token);
        ToolServerSupervisor::start_local(start)
            .await
            .context("start local NATS tool servers")
    }
    .await;
    optional_tool_server(result)
}

fn optional_tool_server<T>(result: Result<T>) -> Option<T> {
    match result {
        Ok(supervisor) => Some(supervisor),
        Err(error) => {
            log::warn!("local NATS tool servers disabled; continuing with stdio tools: {error:#}");
            None
        }
    }
}

/// Run a worker daemon: pull `SessionActivate` notifications, claim each via a
/// KV lease, and execute the session (exactly one worker per session).
pub async fn run_worker_daemon(
    config: GlobalConfig,
    daemon: WorkerDaemonConfig,
    call_fn: Option<crate::agent_loop::AgentCallFn>,
) -> Result<()> {
    let instance_id = harnx_core::instance::InstanceId::new();
    let startup = prepare_worker_startup(&config, &daemon).await?;
    let tool_supervisor =
        start_local_tool_servers(&daemon, startup.client.clone(), &instance_id).await;
    // Attempt optional tool startup before readiness so successful pilots are available on turn one.
    spawn_readiness_publisher(startup.client.clone(), &daemon);
    let session_index = optional_session_index(&startup.jetstream).await;
    let runtime = Arc::new(WorkerRuntime {
        config,
        instance_id,
        _tool_supervisor: tool_supervisor,
        cluster: daemon.cluster.clone(),
        worker_id: daemon.worker_id.clone(),
        lease: daemon.lease,
        jetstream: startup.jetstream,
        session_index,
        client: startup.client,
        call_fn,
        generation: AtomicU64::new(1),
        active: Mutex::new(HashMap::new()),
    });

    let mut messages = startup
        .consumer
        .messages()
        .await
        .context("worker notify message stream")?;
    while let Some(message) = messages.next().await {
        let message = message.context("receive activation")?;
        if let Err(error) = runtime.handle_activation(message).await {
            log::warn!("worker activation handling failed: {error:#}");
        }
    }
    Ok(())
}

/// Borrowed parameters for [`WorkerRuntime::spawn_control_listener`].
struct ControlListenerCtx<'a> {
    client: &'a async_nats::Client,
    session_id: &'a str,
    lease: &'a Arc<NatsSessionLease>,
    backend: &'a NatsSessionLogBackend,
    abort_signal: &'a crate::utils::AbortSignal,
}

struct WorkerRuntime {
    config: GlobalConfig,
    instance_id: harnx_core::instance::InstanceId,
    _tool_supervisor: Option<ToolServerSupervisor>,
    #[allow(dead_code)]
    cluster: String,
    worker_id: String,
    lease: NatsLeaseConfig,
    jetstream: jetstream::Context,
    session_index: Option<async_nats::jetstream::kv::Store>,
    /// Shared NATS client for control-plane subscriptions (cloned per session
    /// rather than reconnecting on each activation).
    client: async_nats::Client,
    call_fn: Option<crate::agent_loop::AgentCallFn>,
    generation: AtomicU64,
    active: Mutex<HashMap<String, JoinHandle<()>>>,
}

impl WorkerRuntime {
    async fn already_running(&self, session_id: &str) -> bool {
        let mut active = self.active.lock().await;
        active.retain(|_, handle| !handle.is_finished());
        active.contains_key(session_id)
    }

    async fn acquire_activation_lease(
        &self,
        activation: &SessionActivate,
    ) -> Result<Option<Arc<NatsSessionLease>>> {
        let generation = self.generation.fetch_add(1, Ordering::SeqCst);
        let lease = NatsSessionLease::acquire(NatsLeaseAcquireParams {
            jetstream: self.jetstream.clone(),
            session_id: &activation.session_id,
            worker_id: self.worker_id.clone(),
            generation,
            config: self.lease.clone(),
            session_index: self.session_index.clone(),
        })
        .await?;
        Ok(lease.map(Arc::new))
    }

    async fn prepare_activation_control(
        &self,
        activation: &SessionActivate,
        lease: &Arc<NatsSessionLease>,
        abort_signal: &crate::utils::AbortSignal,
    ) -> Result<JoinHandle<()>> {
        let backend = NatsSessionLogBackend::new(self.jetstream.clone(), &activation.session_id);
        match Self::spawn_control_listener(ControlListenerCtx {
            client: &self.client,
            session_id: &activation.session_id,
            lease,
            backend: &backend,
            abort_signal,
        })
        .await
        {
            Ok(task) => Ok(task),
            Err(error) => {
                let _ = lease.release().await;
                Err(error)
            }
        }
    }

    async fn handle_activation(
        self: &Arc<Self>,
        message: async_nats::jetstream::Message,
    ) -> Result<()> {
        let activation: SessionActivate =
            serde_json::from_slice(&message.payload).context("decode SessionActivate")?;

        // Re-activation of an already-running session is a no-op.
        if self.already_running(&activation.session_id).await {
            let _ = message.ack().await;
            return Ok(());
        }

        // A lease loser leaves the message unacked for its current holder.
        let Some(lease) = self.acquire_activation_lease(&activation).await? else {
            return Ok(());
        };
        log::info!(
            "session activate claimed: session_id={} worker_id={} revision={} epoch={}",
            activation.session_id,
            lease.worker_id(),
            lease.fence_token(),
            activation.epoch
        );

        let abort_signal = crate::utils::create_abort_signal();
        // Core-NATS control must be subscribed before activation is acknowledged.
        let control_task = self
            .prepare_activation_control(&activation, &lease, &abort_signal)
            .await?;

        // We hold the lease and control is ready: ack activation and spawn execution.
        if let Err(error) = message.ack().await {
            control_task.abort();
            let _ = lease.release().await;
            return Err(anyhow::anyhow!("ack SessionActivate: {error}"));
        }
        let worker = Arc::clone(self);
        let session_id = activation.session_id.clone();
        let task_session_id = session_id.clone();
        nats_metrics::active_session_started();
        let handle = tokio::spawn(async move {
            let snapshot = nats_metrics::snapshot();
            log::info!(
                "active session started: session_id={} worker_id={} revision={} active_sessions_per_worker={}",
                activation.session_id,
                lease.worker_id(),
                lease.fence_token(),
                snapshot.active_sessions_per_worker
            );
            let result = worker
                .execute_session(activation, Arc::clone(&lease), abort_signal, control_task)
                .await;
            nats_metrics::active_session_finished();
            let snapshot = nats_metrics::snapshot();
            log::info!(
                "active session finished: session_id={} worker_id={} revision={} active_sessions_per_worker={}",
                task_session_id,
                lease.worker_id(),
                lease.fence_token(),
                snapshot.active_sessions_per_worker
            );
            if let Err(error) = result {
                log::warn!("worker session execution failed: {error:#}");
            }
        });
        self.active.lock().await.insert(session_id, handle);
        Ok(())
    }

    async fn derive_continuation_turn_input(
        &self,
        ctx: TurnInputCtx<'_>,
        high_water: Option<u64>,
    ) -> Option<(Input, Option<u64>)> {
        // Continuation turns (high_water set) derive input CURSOR-based, not
        // barrier-based. A user message that arrives DURING a turn is logged at
        // a seq BELOW that turn's assistant barrier, so the barrier-based
        // reconstruct would treat it as already-answered and never fold it. The
        // cursor (high-water mark of messages already fed this activation) is the
        // authoritative "unanswered" boundary and matches the drain decision.
        let hw = high_water?;
        let tail = match ctx.backend.load_events_latest_async().await {
            Ok(entries) => entries,
            Err(err) => {
                log::warn!(
                    "failed to load session log for continuation drain: session_id={} worker_id={} err={err}",
                    ctx.backend.session_id(),
                    self.worker_id,
                );
                Vec::new()
            }
        };
        let (new_messages, latest_seq) = fold_new_user_messages_since(&tail, Some(hw));
        if new_messages.is_empty() {
            return None;
        }
        let (mut input, seed) = self
            .derive_idle_turn_input(ctx.activation, ctx.per_session, new_messages)
            .await;
        input.with_session = true;
        input.skip_user_log_append = true;
        Some((input, seed.or(latest_seq)))
    }

    /// Derive the turn input for an activation from the reconstructed session
    /// state, honoring cancel tombstones, resumable turns, and pending messages.
    async fn derive_turn_input(
        &self,
        activation: &SessionActivate,
        lease: &NatsSessionLease,
        per_session: &GlobalConfig,
        backend: &NatsSessionLogBackend,
        _session_index: Option<&async_nats::jetstream::kv::Store>,
        high_water: Option<u64>,
    ) -> (Input, Option<u64>) {
        let ctx = TurnInputCtx {
            activation,
            per_session,
            backend,
        };
        if let Some(continuation) = self.derive_continuation_turn_input(ctx, high_water).await {
            return continuation;
        }

        let reconstructed = self.reconstruct_session_state(backend).await;
        log::debug!(
            "derive_turn_input: session_id={} turn_status={:?} next_turn_count={} resumable_ctx={}",
            activation.session_id,
            reconstructed.turn_status,
            reconstructed.next_turn_messages.len(),
            reconstructed.resumable_ctx.is_some(),
        );
        let (mut input, seed_cursor) = match reconstructed.turn_status {
            harnx_core::session_reconstruct::TurnStatus::InFlightCancelled => {
                // Terminal: Cancel tombstone prevents resume. Do NOT consume pending.
                // The turn is idle; wait for new user input or activation.
                log::info!(
                    "session has cancelled turn tombstone; not resuming (session_id={})",
                    activation.session_id
                );
                (crate::config::input::from_str(per_session, "", None), None)
            }
            harnx_core::session_reconstruct::TurnStatus::InFlightResumable => {
                // Resume existing turn (orphan repair handled inside run_agent_loop_with_nats_inner).
                log::info!(
                    "resume state: session_id={} worker_id={} revision={} mode=resumable",
                    activation.session_id,
                    lease.worker_id(),
                    lease.fence_token()
                );
                // Extract the last_user Message to get both its text (for Input) and log_seq (for cursor).
                let last_user_msg = reconstructed
                    .resumable_ctx
                    .as_ref()
                    .and_then(|ctx| ctx.last_user.as_ref());
                let input = if let Some(last_user) = last_user_msg {
                    crate::config::input::from_str(per_session, &last_user.content.to_text(), None)
                } else {
                    crate::config::input::from_str(per_session, "", None)
                };
                // Cursor: the log_seq of the last user message that kicked off this resumable turn.
                // Any messages appended AFTER this (seq > cursor) will be folded mid-turn.
                let seed_cursor = last_user_msg
                    .and_then(|msg| msg.log_seq.and_then(|seq| u64::try_from(seq).ok()));
                (input, seed_cursor)
            }
            harnx_core::session_reconstruct::TurnStatus::Idle => {
                let msg_count = reconstructed.next_turn_messages.len();
                let result = self
                    .derive_idle_turn_input(
                        activation,
                        per_session,
                        reconstructed.next_turn_messages,
                    )
                    .await;
                // DEBUG: log the seed_cursor for this path
                log::debug!(
                    "derive_turn_input idle: session_id={} seed_cursor={:?} messages_count={}",
                    activation.session_id,
                    result.1,
                    msg_count,
                );
                result
            }
        };
        // The NATS worker ALWAYS operates on a session. The input is derived
        // before `run_agent_loop_with_nats_inner` attaches the session to the
        // per-session config, so `from_str` sees no session and leaves
        // `with_session=false`. That would make `save_message` a no-op and the
        // turn's assistant barrier would never be persisted. Force it true.
        input.with_session = true;
        // The folded user messages are ALREADY durable in the log (clients
        // append them directly) and loaded into `session.messages`. The worker
        // must not re-append them: doing so duplicates the user message and
        // reorders the assistant barrier past concurrently-arrived messages,
        // burying them so they are never folded into a continuation turn.
        input.skip_user_log_append = true;
        (input, seed_cursor)
    }

    /// Idle-state input: fold queued next-turn messages in log order.
    async fn derive_idle_turn_input(
        &self,
        _activation: &SessionActivate,
        per_session: &GlobalConfig,
        next_turn_messages: Vec<harnx_core::message::Message>,
    ) -> (Input, Option<u64>) {
        let seed_cursor = next_turn_messages
            .last()
            .and_then(|message| message.log_seq.and_then(|seq| u64::try_from(seq).ok()));
        let folded = next_turn_messages
            .into_iter()
            .map(|message| message.content.to_text())
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join("\n");
        (
            crate::config::input::from_str(per_session, &folded, None),
            seed_cursor,
        )
    }

    async fn execute_session(
        &self,
        activation: SessionActivate,
        lease: Arc<NatsSessionLease>,
        abort_signal: crate::utils::AbortSignal,
        control_task: JoinHandle<()>,
    ) -> Result<()> {
        // Per-session config clone with the requested agent (loaded from the
        // worker's OWN config) and the session selected.
        let per_session = {
            let base = self.config.read().clone();
            Arc::new(parking_lot::RwLock::new(base))
        };
        if let Ok(agent) = self.config.read().retrieve_agent(&activation.agent) {
            let mut cfg = per_session.write();
            let _ = cfg.use_agent_obj(agent);
        }

        // Create event sink for live fan-out. `new` seeds `after_seq` from stream once.
        let event_sink = crate::nats_event_sink::NatsEventSink::new(
            self.client.clone(),
            self.jetstream.clone(),
            activation.session_id.clone(),
        )
        .await;
        let after_seq_observer = event_sink.after_seq_handle();
        let event_sink = Arc::new(event_sink);

        // Build the backend for control-plane operations and state reconstruction.
        // Share the `after_seq` high-water mark for event-sink fan-out advisories;
        // worker tail reads themselves use leader-authoritative `load_events_latest_async`.
        let backend = NatsSessionLogBackend::new(self.jetstream.clone(), &activation.session_id)
            .with_after_seq_observer(Arc::clone(&after_seq_observer));

        // Abort turns promptly if lease is lost.
        let watch_task =
            Self::spawn_lease_loss_watch(&lease, &abort_signal, &activation.session_id);

        let result = harnx_core::sink::with_agent_event_sink(event_sink, async {
            // Activation high-water cursor: max log seq of ANY user message we've fed into this
            // activation. Includes messages folded into turn inputs AND mid-round injections.
            // A continuation turn only runs if there's a user message with seq > this cursor.
            let mut activation_high_water: Option<u64> = None;

            loop {
                if !lease.is_held() {
                    break Ok(());
                }

                let (input, seed_cursor) = self
                    .derive_turn_input(
                        &activation,
                        &lease,
                        &per_session,
                        &backend,
                        self.session_index.as_ref(),
                        activation_high_water,
                    )
                    .await;

                log::info!(
                    "execute_session turn: session_id={} seed_cursor={:?}",
                    activation.session_id,
                    seed_cursor,
                );

                // Initialize cursor for this turn. Use seed_cursor (from derive_turn_input)
                // which is the max seq of messages folded into the input.
                let turn_cursor = Arc::new(AtomicU64::new(seed_cursor.unwrap_or(0)));

                // Update activation high-water if seed_cursor is higher.
                if let Some(seed) = seed_cursor {
                    activation_high_water = Some(activation_high_water.map_or(seed, |h| h.max(seed)));
                }

                let on_tool_round =
                    build_mid_turn_injection_callback(backend.clone(), Arc::clone(&turn_cursor));

                // Per-turn observer for the S2 header-insert migration seq. Only
                // the first activation of a headerless session migrates; the
                // migration re-maps the leading-user block onto this seq, which
                // the turn answers. We must advance the activation high-water
                // past it so the end-of-turn drain does not re-fold the remapped
                // (already-answered) users and spuriously re-run the turn (S3).
                let header_insert_seq = Arc::new(AtomicU64::new(0));

                run_agent_loop_with_nats_inner(
                    RunAgentLoopArgs {
                        cluster_key: &self.cluster,
                        session_id: &activation.session_id,
                        config: per_session.clone(),
                        instance_id: self.instance_id.clone(),
                        initial_input: input,
                        abort_signal: abort_signal.clone(),
                        call_fn: self.call_fn.clone(),
                        lease: None,
                        lease_config: self.lease.clone(),
                        after_seq_observer: None,
                        header_insert_observer: None,
                        session_index: self.session_index.as_ref(),
                        on_tool_round: Some(on_tool_round),
                        working_dir: None,
                    }
                    .with_lease(Arc::clone(&lease))
                    .with_after_seq_observer(Arc::clone(&after_seq_observer))
                    .with_header_insert_observer(Arc::clone(&header_insert_seq)),
                )
                .await?;

                // After turn completes, update activation high-water from turn_cursor.
                // turn_cursor was updated by mid-round injection callback for any messages
                // injected during multi-round tool execution.
                let turn_cursor_val = turn_cursor.load(Ordering::SeqCst);
                if turn_cursor_val > 0 {
                    activation_high_water = Some(activation_high_water.map_or(turn_cursor_val, |h| h.max(turn_cursor_val)));
                }

                // Advance past the header-insert migration seq (if any). The
                // migration remaps this turn's leading-user block onto this seq,
                // so treat it as consumed by this turn's answer.
                let header_insert_val = header_insert_seq.load(Ordering::SeqCst);
                if header_insert_val > 0 {
                    activation_high_water = Some(
                        activation_high_water.map_or(header_insert_val, |h| h.max(header_insert_val)),
                    );
                }

                log::info!(
                    "execute_session turn complete: session_id={} turn_cursor={} activation_high_water={:?}",
                    activation.session_id,
                    turn_cursor_val,
                    activation_high_water,
                );

                if !lease.is_held() {
                    break Ok(());
                }

                // DRAIN DECISION: cursor-based, not barrier-based.
                // Re-run another turn ONLY if there's a user message with seq > activation_high_water.
                // This prevents re-running when we've already consumed everything.
                // Use the fresh leader-authoritative load so this re-read reflects
                // both the worker's own just-persisted turn barrier and any client
                // edit/retract committed just before the read (otherwise it would
                // re-fold already-answered or retracted messages).
                let tail = backend.load_events_latest_async().await?;

                // Check for resumable in-flight tool rounds (multi-turn tool execution).
                // Use reconstruct_state_from_nats to preserve NATS seqs for EditEntries resolution.
                let reconstructed = harnx_core::session_reconstruct::reconstruct_state_from_nats(&tail);
                let has_resumable = reconstructed.resumable_ctx.is_some();

                // Check for new user messages beyond the high-water cursor.
                let (new_messages, latest_new_seq) = fold_new_user_messages_since(&tail, activation_high_water);

                log::info!(
                    "execute_session drain check: session_id={} new_messages_count={} has_resumable={} latest_new_seq={:?}",
                    activation.session_id,
                    new_messages.len(),
                    has_resumable,
                    latest_new_seq,
                );

                // Continue only if there are genuinely new user messages OR a resumable tool context.
                // A completed turn with nothing new => exactly one execution.
                //
                // Do NOT advance `activation_high_water` here. The drain only
                // DETECTS that unanswered messages exist; the continuation turn
                // CONSUMES them and advances the high-water from its own
                // `seed_cursor` at the top of the loop. Advancing here would mark
                // the messages consumed before the turn runs, so the continuation
                // turn would derive an empty input and never answer them.
                if new_messages.is_empty() && !has_resumable {
                    break Ok(());
                }
            }
        })
        .await;

        if !lease.is_held() {
            log::warn!(
                "session execution ended after failover: session_id={} worker_id={} revision={}",
                activation.session_id,
                lease.worker_id(),
                lease.fence_token()
            );
        }

        watch_task.abort();
        control_task.abort();
        let _ = tokio::time::timeout(std::time::Duration::from_millis(100), watch_task).await;
        let _ = tokio::time::timeout(std::time::Duration::from_millis(100), control_task).await;

        let _ = lease.release().await;
        result
    }

    /// Spawn a task that watches for lease loss and aborts on loss.
    fn spawn_lease_loss_watch(
        lease: &Arc<NatsSessionLease>,
        abort_signal: &crate::utils::AbortSignal,
        session_id: &str,
    ) -> tokio::task::JoinHandle<()> {
        let mut lost = lease.lost_watch();
        let abort_for_watch = abort_signal.clone();
        let watch_session_id = session_id.to_string();
        let watch_lease = Arc::clone(lease);
        tokio::spawn(async move {
            while lost.changed().await.is_ok() {
                if !*lost.borrow() {
                    log::warn!(
                        "failover abort: session_id={} worker_id={} revision={} reason=lease_lost",
                        watch_session_id,
                        watch_lease.worker_id(),
                        watch_lease.fence_token()
                    );
                    abort_for_watch.set_ctrlc();
                    break;
                }
            }
        })
    }

    /// Subscribe to control before spawning its listener task.
    ///
    /// Returning only after `subscribe` completes is the ordering barrier used by
    /// activation handling before it acknowledges the non-durable work message.
    async fn spawn_control_listener(ctx: ControlListenerCtx<'_>) -> Result<JoinHandle<()>> {
        let ctrl_subject = control_subject(ctx.session_id);
        let subscriber = ctx
            .client
            .subscribe(ctrl_subject)
            .await
            .context("subscribe to session control subject")?;
        // Flush the SUB protocol command so broker-side interest exists before
        // activation is acknowledged and clients can observe that readiness.
        ctx.client
            .flush()
            .await
            .context("flush session control subscription")?;
        let ctrl_abort = ctx.abort_signal.clone();
        let ctrl_lease = Arc::clone(ctx.lease);
        let ctrl_backend = ctx.backend.clone();
        Ok(tokio::spawn(async move {
            use futures_util::StreamExt;
            let mut messages = subscriber;
            while let Some(msg) = messages.next().await {
                // Parse the control command.
                let Ok(command) = ControlCommand::from_bytes(&msg.payload) else {
                    log::debug!("invalid control command payload, ignoring");
                    continue;
                };
                match command {
                    ControlCommand::Cancel => {
                        // Worker-originated: append Cancel entry (worker's fence token),
                        // THEN fire AbortSignal.
                        if should_append_control_log_entry(&ctrl_lease) {
                            let cancel_entry = harnx_core::session::SessionLogEntry::Cancel {
                                fence_token: ctrl_lease.fence_token(),
                            };
                            // Append BEFORE firing abort; if append fails, still abort (safe).
                            if let Err(e) = ctrl_backend.append_event_blocking(&cancel_entry) {
                                log::warn!("failed to append Cancel entry: {e}");
                            }
                        }
                        ctrl_abort.set_ctrlc();
                    }
                }
            }
        }))
    }

    /// Reconstruct session state using the canonical algorithm.
    ///
    /// Returns the session's turn status, effective pending message, and
    /// resumable context for driving the agent loop correctly.
    async fn reconstruct_session_state(
        &self,
        backend: &NatsSessionLogBackend,
    ) -> harnx_core::session_reconstruct::ReconstructedState {
        match backend.load_events_latest_async().await {
            Ok(entries) => harnx_core::session_reconstruct::reconstruct_state_from_nats(&entries),
            Err(err) => {
                log::warn!(
                    "failed to load session log for reconstruction: session_id={} worker_id={} err={err}",
                    backend.session_id(),
                    self.worker_id,
                );
                harnx_core::session_reconstruct::ReconstructedState {
                    turn_status: harnx_core::session_reconstruct::TurnStatus::Idle,
                    next_turn_messages: Vec::new(),
                    resumable_ctx: None,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::optional_tool_server;

    #[test]
    fn worker_startup_continues_when_pilot_binary_is_missing() {
        harnx_core::require_nextest();
        let missing = Err(anyhow::anyhow!(
            "HARNX_TIME_SERVER_BIN points to a missing tool-server binary"
        ));

        assert!(optional_tool_server::<()>(missing).is_none());
    }
}
