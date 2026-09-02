//! Shared construction and recovery operations for broker-backed sessions.

use crate::types::{ExitCancelFactory, ExitWorkerState, Tui};
use anyhow::{Context, Result};
use futures_util::FutureExt;
use harnx_runtime::config::{GlobalConfig, LOCAL_CLUSTER_KEY};
use harnx_runtime::nats_lease::{lease_holder_in, NatsLeaseConfig};
use harnx_runtime::{NatsSession, NatsSessionConfig};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

const EXIT_WORKER_STATE_TIMEOUT: Duration = Duration::from_secs(2);

/// Classify which client owns the local worker for exit-prompt messaging.
///
/// A TUI attaching to a `__local__` session ALWAYS spawns its own
/// `LocalWorkerSupervisor`, so `local_worker.is_some()` is always true for
/// local sessions. To distinguish "owned by this client" (shuts down on exit)
/// from "owned by another client" (keeps running), we must compare the lease
/// holder's worker ID from the `harnx_leases` KV bucket against our own
/// `route().worker_id()`.
pub(crate) fn classify_exit_worker_state(
    cluster: &str,
    own_worker_id: Option<&str>,
    lease_holder: std::result::Result<Option<&str>, ()>,
) -> ExitWorkerState {
    if cluster != LOCAL_CLUSTER_KEY {
        return ExitWorkerState::Remote;
    }
    let Some(own_worker_id) = own_worker_id else {
        return ExitWorkerState::LocalOwnedElsewhere;
    };
    match lease_holder {
        Ok(Some(lease_holder)) if lease_holder == own_worker_id => ExitWorkerState::LocalOwnedHere,
        Ok(_) => ExitWorkerState::LocalOwnedElsewhere,
        Err(()) => ExitWorkerState::Unknown,
    }
}

pub(super) async fn nats_session_for_target(
    config: &GlobalConfig,
    local_worker: &Arc<Mutex<Option<harnx_runtime::local_orchestrator::LocalWorkerSupervisor>>>,
    session_id: String,
    cluster: String,
) -> Result<NatsSession> {
    let abort_signal = harnx_runtime::utils::create_abort_signal();
    let activation_route = harnx_runtime::local_orchestrator::activation_route_for_cluster(
        &cluster,
        local_worker,
        abort_signal.clone(),
    )
    .await?;
    let initializer = {
        let config = config.read();
        let agent = config
            .remote_agent
            .as_ref()
            .map(|(agent, _)| agent.clone())
            .or_else(|| config.agent.as_ref().map(|agent| agent.name().to_string()))
            .unwrap_or_default();
        harnx_runtime::SessionInitializer::named_from_config(agent, &config)
    };
    NatsSession::from_global_config(
        NatsSessionConfig {
            cluster,
            initializer,
            session_id: Some(session_id),
            activation_route,
        },
        config,
        abort_signal,
    )
    .await
}

async fn cancel_remote_session(
    config: &GlobalConfig,
    local_worker: &Arc<Mutex<Option<harnx_runtime::local_orchestrator::LocalWorkerSupervisor>>>,
    session_id: String,
    cluster: String,
) -> Result<()> {
    let session = nats_session_for_target(config, local_worker, session_id.clone(), cluster)
        .await
        .with_context(|| format!("prepare NATS session {session_id} for cancellation"))?;
    session
        .cancel_pending_turn()
        .await
        .with_context(|| format!("durably cancel NATS session {session_id}"))?;
    Ok(())
}

pub(crate) fn default_exit_cancel_factory() -> ExitCancelFactory {
    Arc::new(|config, local_worker, session_id, cluster| {
        Box::pin(
            async move { cancel_remote_session(&config, &local_worker, session_id, cluster).await },
        )
    })
}

impl Tui {
    pub(crate) async fn exit_worker_state(&self) -> ExitWorkerState {
        let Some((session_id, cluster)) = self.active_remote_session.as_ref() else {
            return ExitWorkerState::Unknown;
        };
        if cluster != LOCAL_CLUSTER_KEY {
            return ExitWorkerState::Remote;
        }

        let own_worker_id = {
            let local_worker = self.local_worker.lock().await;
            local_worker
                .as_ref()
                .map(|worker| worker.route().worker_id().to_string())
        };
        let Some(own_worker_id) = own_worker_id else {
            return classify_exit_worker_state(cluster, None, Ok(None));
        };

        let config = self.config.read().clone();
        let lease_config = NatsLeaseConfig::default();
        let lease_holder = tokio::time::timeout(EXIT_WORKER_STATE_TIMEOUT, async {
            let bucket = config.nats_kv_bucket(cluster, &lease_config.bucket).await?;
            lease_holder_in(&bucket, &lease_config, session_id).await
        })
        .await;
        match lease_holder {
            Ok(Ok(record)) => classify_exit_worker_state(
                cluster,
                Some(&own_worker_id),
                Ok(record.as_ref().map(|record| record.worker_id.as_str())),
            ),
            Ok(Err(_)) | Err(_) => {
                classify_exit_worker_state(cluster, Some(&own_worker_id), Err(()))
            }
        }
    }

    pub(crate) fn start_exit_cancel(&mut self) -> bool {
        let Some((session_id, cluster)) = self.active_remote_session.clone() else {
            return false;
        };
        let cancel = (self.exit_cancel_factory)(
            self.config.clone(),
            self.local_worker.clone(),
            session_id,
            cluster,
        );
        self.exit_interrupt_error = None;
        self.pending_exit_cancel = Some(cancel);
        true
    }

    /// Poll the in-flight cancel future, completing exit only if ready.
    ///
    /// Uses `now_or_never` for a single non-blocking poll. A pending future is
    /// preserved (not dropped) so the cancel continues across event-loop ticks.
    /// Once complete, clears the modal and sets `should_quit`, recording any
    /// error for post-exit warning.
    pub(crate) async fn poll_pending_exit_cancel(&mut self) {
        let result = self
            .pending_exit_cancel
            .as_mut()
            .and_then(FutureExt::now_or_never);
        let Some(result) = result else {
            return;
        };

        self.pending_exit_cancel = None;
        if let Err(error) = result {
            self.exit_interrupt_error = Some(format!("{error:#}"));
        }
        self.app.modal = None;
        self.app.should_quit = true;
    }

    #[cfg(test)]
    #[allow(dead_code)] // Used by exit ordering tests added in the final test task.
    pub(crate) fn set_exit_cancel_factory(&mut self, factory: ExitCancelFactory) {
        self.exit_cancel_factory = factory;
    }

    /// Fire-and-forget cancel for in-TUI Ctrl+C that stays in the event loop.
    ///
    /// This detached-spawn path is ONLY appropriate when the TUI remains alive
    /// after cancel (Ctrl+C interrupts but keeps the session open). Exit paths
    /// that quit afterward MUST use `start_exit_cancel` + `poll_pending_exit_cancel`
    /// to await completion before shutting down the `LocalWorkerSupervisor`.
    /// Process exit drops the supervisor, which kills the worker and can race
    /// the cancel request, losing the durable `Cancel` tombstone.
    pub(super) fn cancel_active_remote_session(&self) {
        self.clear_tool_confirmation_route();
        let Some((session_id, cluster)) = self.active_remote_session.clone() else {
            return;
        };
        let config = self.config.clone();
        let local_worker = self.local_worker.clone();
        tokio::spawn(async move {
            if let Err(error) =
                cancel_remote_session(&config, &local_worker, session_id, cluster).await
            {
                log::warn!("Failed to cancel active NATS session: {error:#}");
            }
        });
    }
}
