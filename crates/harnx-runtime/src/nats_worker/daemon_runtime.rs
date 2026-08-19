//! Per-worker runtime state and `SessionActivate` handling: lease
//! acquisition, this session's tool-server refcount, control-plane
//! subscription, and handing the claimed session off to execution.

use super::backend::NatsSessionLogBackend;
use super::control::{control_subject, ControlCommand};
use super::daemon::{should_append_control_log_entry, SessionActivate};
use super::daemon_background::BackgroundServices;
use super::server_reconciler::{tool_servers_for_activation, ServerReconciler};
use crate::config::GlobalConfig;
use crate::nats_lease::{NatsLeaseAcquireParams, NatsLeaseConfig, NatsSessionLease};
use crate::nats_metrics;
use anyhow::{Context, Result};
use async_nats::jetstream;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

/// Longest `handle_activation` waits for this session's own tool servers to
/// start before giving up and continuing anyway.
///
/// Deliberately well under `WORK_NOTIFY_ACK_WAIT`: tool-server starts now run
/// concurrently (see `ServerReconciler::session_started`), so this only needs
/// to cover one server's own startup timeout plus margin, not the sum of
/// several. Comfortably clearing this margin matters because JetStream
/// redelivers an unacked activation at `WORK_NOTIFY_ACK_WAIT` with
/// `max_deliver: -1` — a session that never manages to ack loops forever,
/// each redelivery re-acquiring the lease and fencing the still-running
/// previous attempt.
pub(super) const SESSION_TOOL_SERVER_START_TIMEOUT: Duration = Duration::from_secs(20);

/// Borrowed parameters for [`WorkerRuntime::spawn_control_listener`].
struct ControlListenerCtx<'a> {
    client: &'a async_nats::Client,
    session_id: &'a str,
    lease: &'a Arc<NatsSessionLease>,
    backend: &'a NatsSessionLogBackend,
    abort_signal: &'a crate::utils::AbortSignal,
}

/// Borrowed parameters for [`WorkerRuntime::prepare_and_ack_activation`].
struct ActivationAckCtx<'a> {
    activation: &'a SessionActivate,
    message: &'a async_nats::jetstream::Message,
    lease: &'a Arc<NatsSessionLease>,
    abort_signal: &'a crate::utils::AbortSignal,
}

pub(super) struct WorkerRuntime {
    pub(super) config: GlobalConfig,
    pub(super) instance_id: harnx_core::instance::ServerScope,
    pub(super) _background_services: Arc<Mutex<Option<BackgroundServices>>>,
    pub(super) tools_attempted: tokio::sync::watch::Receiver<bool>,
    /// `None` for a consuming worker, or a managing worker with nothing
    /// configured to spawn anywhere.
    pub(super) server_reconciler: Option<Arc<ServerReconciler>>,
    #[allow(dead_code)]
    pub(super) cluster: String,
    pub(super) manage_servers: bool,
    pub(super) worker_id: String,
    pub(super) identity: crate::worker_identity::WorkerIdentity,
    pub(super) lease: NatsLeaseConfig,
    pub(super) jetstream: jetstream::Context,
    pub(super) session_index: Option<async_nats::jetstream::kv::Store>,
    /// Shared NATS client for control-plane subscriptions (cloned per session
    /// rather than reconnecting on each activation).
    pub(super) client: async_nats::Client,
    pub(super) call_fn: Option<crate::agent_loop::AgentCallFn>,
    pub(super) generation: AtomicU64,
    pub(super) active: Mutex<HashMap<String, JoinHandle<()>>>,
}

impl WorkerRuntime {
    pub(super) async fn already_running(&self, session_id: &str) -> bool {
        let mut active = self.active.lock().await;
        active.retain(|_, handle| !handle.is_finished());
        active.contains_key(session_id)
    }

    /// A worker that doesn't manage its own servers, or has nothing
    /// configured to spawn anywhere, has no reconciler and this is a no-op.
    ///
    /// Only the actual server start (`ServerReconciler::start_claimed`) is
    /// bounded by `SESSION_TOOL_SERVER_START_TIMEOUT` and, on timeout, left
    /// running in the background rather than aborted — a session degraded to
    /// fewer tools is far better than an unacked activation that JetStream
    /// redelivers forever. Registering this session as a user
    /// (`ServerReconciler::claim_users`) is awaited directly, unbounded and
    /// un-detached: `end_session_tool_servers` can run as soon as this
    /// activation's ack fails, or when the session ends, and must always see
    /// an accurate registration to release. A registration that instead
    /// landed later, from an abandoned background task, would pin its server
    /// as a "user" that no future `session_ended` call can ever remove.
    pub(super) async fn start_session_tool_servers(&self, activation: &SessionActivate) {
        let Some(reconciler) = self.server_reconciler.clone() else {
            return;
        };
        let servers = tool_servers_for_activation(&self.config, &activation.agent);
        if servers.is_empty() {
            return;
        }
        let to_start = reconciler
            .claim_users(&activation.session_id, servers)
            .await;
        if to_start.is_empty() {
            return;
        }
        let server_names = to_start
            .iter()
            .map(|server| server.name.clone())
            .collect::<Vec<_>>()
            .join(", ");
        let task = tokio::spawn(async move { reconciler.start_claimed(to_start).await });
        match tokio::time::timeout(SESSION_TOOL_SERVER_START_TIMEOUT, task).await {
            Ok(Ok(())) => {}
            Ok(Err(join_error)) => {
                log::warn!(
                    "session '{}' tool-server startup task panicked: {join_error}",
                    activation.session_id
                );
            }
            Err(_) => {
                log::warn!(
                    "session '{}' tool-server startup ({}) exceeded {}s; continuing this \
                     activation without waiting further (still starting in the background)",
                    activation.session_id,
                    server_names,
                    SESSION_TOOL_SERVER_START_TIMEOUT.as_secs()
                );
            }
        }
    }

    pub(super) async fn end_session_tool_servers(&self, session_id: &str) {
        if let Some(reconciler) = &self.server_reconciler {
            reconciler.session_ended(session_id).await;
        }
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

    /// Subscribe control and acknowledge the activation, in that order.
    /// `start_session_tool_servers` already registered this session as a tool-
    /// server user before either step ran; no task exists yet to release that
    /// on completion (that happens inside the spawned task `handle_activation`
    /// creates once this returns `Ok`), so either failure here must release it
    /// itself or the server stays pinned running for the rest of the worker's
    /// lifetime.
    async fn prepare_and_ack_activation(
        &self,
        ctx: ActivationAckCtx<'_>,
    ) -> Result<JoinHandle<()>> {
        let control_task = match self
            .prepare_activation_control(ctx.activation, ctx.lease, ctx.abort_signal)
            .await
        {
            Ok(task) => task,
            Err(error) => {
                self.end_session_tool_servers(&ctx.activation.session_id)
                    .await;
                return Err(error);
            }
        };
        if let Err(error) = ctx.message.ack().await {
            control_task.abort();
            let _ = ctx.lease.release().await;
            self.end_session_tool_servers(&ctx.activation.session_id)
                .await;
            return Err(anyhow::anyhow!("ack SessionActivate: {error}"));
        }
        Ok(control_task)
    }

    pub(super) async fn handle_activation(
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

        // Start (or reuse) this session's own agent's tool servers before the
        // registry snapshot below is taken, so it sees them.
        self.start_session_tool_servers(&activation).await;

        // The session snapshots the tool registry below, so give the first
        // registration round a chance to finish before that snapshot is taken.
        super::daemon_background::await_initial_tool_registration(&self.tools_attempted).await;
        log::info!(
            "session activate claimed: session_id={} worker_id={} worker_pid={} build={} executable={} config={} revision={} epoch={}",
            activation.session_id,
            lease.worker_id(),
            self.identity.pid,
            self.identity.build,
            crate::worker_identity::short_fingerprint(&self.identity.executable_fingerprint),
            crate::worker_identity::short_fingerprint(&self.identity.config_fingerprint),
            lease.fence_token(),
            activation.epoch
        );

        let abort_signal = crate::utils::create_abort_signal();
        // Core-NATS control must be subscribed before activation is
        // acknowledged; either failing releases the lease and this session's
        // tool-server refcount (see `prepare_and_ack_activation`).
        let control_task = self
            .prepare_and_ack_activation(ActivationAckCtx {
                activation: &activation,
                message: &message,
                lease: &lease,
                abort_signal: &abort_signal,
            })
            .await?;
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
            // `active` is only pruned lazily (see `already_running`'s
            // `retain`), so this is the one place a finished session's tool
            // servers can be released — do it before the metrics/log lines
            // below, which already mark the session as done.
            worker.end_session_tool_servers(&task_session_id).await;
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
}
