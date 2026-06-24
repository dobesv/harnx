//! Worker daemon and session activation.

use super::agent_loop::{
    build_mid_turn_injection_callback, fold_new_user_messages_since,
    run_agent_loop_with_nats_inner, RunAgentLoopArgs,
};
use super::backend::NatsSessionLogBackend;
use super::control::{control_subject, ControlCommand};
use crate::config::{GlobalConfig, Input};
use crate::nats_lease::{NatsLeaseAcquireParams, NatsLeaseConfig, NatsSessionLease};
use crate::nats_metrics;
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

/// Run a worker daemon: pull `SessionActivate` notifications, claim each via a
/// KV lease, and execute the session (exactly one worker per session).
pub async fn run_worker_daemon(
    config: GlobalConfig,
    daemon: WorkerDaemonConfig,
    call_fn: Option<crate::agent_loop::AgentCallFn>,
) -> Result<()> {
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
                filter_subject: subject.clone(),
                inactive_threshold: WORK_NOTIFY_INACTIVE_THRESHOLD,
                max_deliver: -1,
                ..Default::default()
            },
        )
        .await
        .with_context(|| format!("create worker consumer '{consumer_name}'"))?;

    let runtime = Arc::new(WorkerRuntime {
        config,
        cluster: daemon.cluster.clone(),
        worker_id: daemon.worker_id.clone(),
        lease: daemon.lease,
        jetstream: jetstream.clone(),
        client,
        call_fn,
        generation: AtomicU64::new(1),
        active: Mutex::new(HashMap::new()),
    });

    let mut messages = consumer
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
    #[allow(dead_code)]
    cluster: String,
    worker_id: String,
    lease: NatsLeaseConfig,
    jetstream: jetstream::Context,
    /// Shared NATS client for control-plane subscriptions (cloned per session
    /// rather than reconnecting on each activation).
    client: async_nats::Client,
    call_fn: Option<crate::agent_loop::AgentCallFn>,
    generation: AtomicU64,
    active: Mutex<HashMap<String, JoinHandle<()>>>,
}

impl WorkerRuntime {
    async fn handle_activation(
        self: &Arc<Self>,
        message: async_nats::jetstream::Message,
    ) -> Result<()> {
        let activation: SessionActivate =
            serde_json::from_slice(&message.payload).context("decode SessionActivate")?;

        // Re-activation of an already-running session is a no-op.
        {
            let mut active = self.active.lock().await;
            active.retain(|_, handle| !handle.is_finished());
            if active.contains_key(&activation.session_id) {
                let _ = message.ack().await;
                return Ok(());
            }
        }

        // Try to claim the session via the KV lease. Loser drops (does not ack,
        // so the message stays for the holder to ack; another worker holds it).
        let generation = self.generation.fetch_add(1, Ordering::SeqCst);
        let lease = match NatsSessionLease::acquire(NatsLeaseAcquireParams {
            jetstream: self.jetstream.clone(),
            session_id: &activation.session_id,
            worker_id: self.worker_id.clone(),
            generation,
            config: self.lease.clone(),
        })
        .await?
        {
            Some(lease) => lease,
            None => return Ok(()),
        };
        log::info!(
            "session activate claimed: session_id={} worker_id={} revision={} epoch={}",
            activation.session_id,
            lease.worker_id(),
            lease.fence_token(),
            activation.epoch
        );

        // We hold the lease: ack the activation and spawn execution.
        let _ = message.ack().await;
        let worker = Arc::clone(self);
        let session_id = activation.session_id.clone();
        let task_session_id = session_id.clone();
        let lease = Arc::new(lease);
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
            let result = worker.execute_session(activation, Arc::clone(&lease)).await;
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
        let tail = match ctx.backend.load_events_consistent_blocking() {
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
                let seed_cursor = last_user_msg.and_then(|msg| msg.log_seq.map(|seq| seq as u64));
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
            .and_then(|message| message.log_seq.map(|seq| seq as u64));
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

        let abort_signal = crate::utils::create_abort_signal();

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
        // Share the `after_seq` high-water mark so the drain re-read can enforce
        // read-your-writes consistency on the worker's own turn-output appends.
        let backend = NatsSessionLogBackend::new(self.jetstream.clone(), &activation.session_id)
            .with_after_seq_observer(Arc::clone(&after_seq_observer));

        // Abort turns promptly if lease is lost.
        let watch_task =
            Self::spawn_lease_loss_watch(&lease, &abort_signal, &activation.session_id);

        // Subscribe to control commands for this session.
        let control_task = Self::spawn_control_listener(ControlListenerCtx {
            client: &self.client,
            session_id: &activation.session_id,
            lease: &lease,
            backend: &backend,
            abort_signal: &abort_signal,
        });

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

                run_agent_loop_with_nats_inner(
                    RunAgentLoopArgs {
                        cluster_key: &self.cluster,
                        session_id: &activation.session_id,
                        config: per_session.clone(),
                        initial_input: input,
                        abort_signal: abort_signal.clone(),
                        call_fn: self.call_fn.clone(),
                        lease: None,
                        after_seq_observer: None,
                        on_tool_round: Some(on_tool_round),
                    }
                    .with_lease(Arc::clone(&lease))
                    .with_after_seq_observer(Arc::clone(&after_seq_observer)),
                )
                .await?;

                // After turn completes, update activation high-water from turn_cursor.
                // turn_cursor was updated by mid-round injection callback for any messages
                // injected during multi-round tool execution.
                let turn_cursor_val = turn_cursor.load(Ordering::SeqCst);
                if turn_cursor_val > 0 {
                    activation_high_water = Some(activation_high_water.map_or(turn_cursor_val, |h| h.max(turn_cursor_val)));
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
                // Use the read-your-writes consistent load so this re-read reflects
                // the worker's own just-persisted turn barrier (otherwise it would
                // re-fold already-answered messages).
                let tail = backend.load_events_consistent_blocking()?;

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

    /// Spawn a task that listens for control commands and handles them.
    fn spawn_control_listener(ctx: ControlListenerCtx<'_>) -> tokio::task::JoinHandle<()> {
        let ctrl_subject = control_subject(ctx.session_id);
        let ctrl_abort = ctx.abort_signal.clone();
        let ctrl_lease = Arc::clone(ctx.lease);
        let ctrl_backend = ctx.backend.clone();
        let client = ctx.client.clone();
        tokio::spawn(async move {
            let subscriber = match client.subscribe(ctrl_subject).await {
                Ok(s) => s,
                Err(e) => {
                    log::warn!("failed to subscribe to control subject: {e}");
                    return;
                }
            };
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
        })
    }

    /// Reconstruct session state using the canonical algorithm.
    ///
    /// Returns the session's turn status, effective pending message, and
    /// resumable context for driving the agent loop correctly.
    async fn reconstruct_session_state(
        &self,
        backend: &NatsSessionLogBackend,
    ) -> harnx_core::session_reconstruct::ReconstructedState {
        match backend.load_events_consistent_blocking() {
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
