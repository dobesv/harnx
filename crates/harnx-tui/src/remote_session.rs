//! Shared construction and recovery operations for broker-backed sessions.

use crate::types::{ExitCancelFactory, ExitWorkerState, Tui};
use anyhow::Result;
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
    let (session_id, initializer) = target_initializer(config, &cluster, &session_id).await?;
    let session = NatsSession::from_global_config(
        NatsSessionConfig {
            cluster,
            initializer,
            session_id: Some(session_id),
            activation_route,
        },
        config,
        abort_signal,
    )
    .await?;
    // A local worker's id changes across restarts, so a wind-up or resume
    // activation addressed to a dead worker is discarded. Attaching here
    // republishes one against the current worker before anything else runs.
    match session.republish_pending_activation().await {
        Ok(republished) => {
            log::info!(
                "attached to session {}: republished pending activation = {republished}",
                session.storage_key()
            );
        }
        Err(error) => {
            log::warn!(
                "attached to session {}: failed to republish pending activation: {error:#}",
                session.storage_key()
            );
        }
    }
    Ok(session)
}

// Targets retain the storage identity even after the user changes agents.
// Reload its canonical owner instead of resolving a local ID in the new agent.
async fn target_initializer(
    config: &GlobalConfig,
    cluster: &str,
    storage_key: &str,
) -> Result<(String, harnx_runtime::SessionInitializer)> {
    use anyhow::Context;
    let snapshot = config.read().clone();
    let replicas = if cluster == LOCAL_CLUSTER_KEY {
        1
    } else {
        snapshot.nats_server(cluster)?.resolved_replicas()
    };
    let jetstream = snapshot.nats_jetstream(cluster).await?;
    let store =
        harnx_runtime::nats_session_metadata::SessionMetadataStore::ensure(&jetstream, replicas)
            .await?;
    let metadata = store
        .get(storage_key)
        .await?
        .context("session target no longer exists")?
        .metadata;
    let initializer = harnx_runtime::SessionInitializer {
        agent: metadata.agent.clone(),
        variables: metadata.variables.clone(),
        overrides: metadata.overrides.clone(),
        tool_context: harnx_runtime::nats_session_metadata::tool_context(&metadata)?,
        parent: metadata.parent.clone(),
    };
    Ok((metadata.session_id, initializer))
}

pub(crate) fn default_exit_cancel_factory() -> ExitCancelFactory {
    Arc::new(|config, local_worker, session_id, cluster| {
        Box::pin(async move {
            // Worker preparation is intentionally outside the durable
            // request's own two-second persistence bound. Local workers
            // consume targeted activations, and readiness can legitimately
            // take longer than the cancellation CAS itself.
            let session =
                nats_session_for_target(&config, &local_worker, session_id, cluster).await?;
            session.interrupt("user interrupt from tui").await
        })
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
        self.exit_after_cancel = true;
        self.start_cancellation(session_id, cluster);
        true
    }

    pub(crate) async fn poll_pending_exit_cancel(&mut self) {
        if self.finish_exit_after_accepted_cancellation() {
            return;
        }
        let result = self
            .pending_exit_cancel
            .as_mut()
            .and_then(FutureExt::now_or_never);
        let Some(result) = result else {
            return;
        };
        self.pending_exit_cancel = None;
        // A join error means the request panicked. Report it like any other
        // failed interrupt so the user can retry or exit.
        let result = result
            .map_err(anyhow::Error::from)
            .and_then(std::convert::identity);
        match result {
            Ok(outcome) => {
                if matches!(
                    self.app.modal,
                    Some(crate::types::ModalState::ConfirmExit { .. })
                ) {
                    self.app.modal = None;
                }
                self.monitor_interrupt(outcome);
                self.finish_exit_after_accepted_cancellation();
            }
            Err(error) => {
                let error = format!("{error:#}");
                self.exit_interrupt_error = Some(error.clone());
                if let Some(tray) = &mut self.cancellation {
                    tray.phase = crate::cancellation::CancellationPhase::Failed(error);
                }
                if let Some(crate::types::ModalState::ConfirmExit { phase, .. }) =
                    &mut self.app.modal
                {
                    *phase = crate::types::ExitPhase::RequestFailed;
                }
            }
        }
    }

    /// The loop has exited without waiting for the cancel: Ctrl+D "exit
    /// anyway", or an error. Stop the request here so it cannot keep the
    /// local worker supervisor busy while the process tears down. A `Cancel`
    /// it already published is durable regardless.
    pub(crate) async fn abandon_pending_exit_cancel(&mut self) {
        if let Some(task) = self.pending_exit_cancel.take() {
            task.abort();
            let _ = task.await;
        }
    }

    fn finish_exit_after_accepted_cancellation(&mut self) -> bool {
        if !self.exit_after_cancel {
            return false;
        }
        if self.pending_exit_cancel.is_some() {
            return false;
        }
        if self.cancellation.is_some() {
            return false;
        }
        self.exit_after_cancel = false;
        self.app.should_quit = true;
        true
    }

    #[cfg(test)]
    #[allow(dead_code)] // Used by exit ordering tests added in the final test task.
    pub(crate) fn set_exit_cancel_factory(&mut self, factory: ExitCancelFactory) {
        self.exit_cancel_factory = factory;
    }

    pub(super) fn cancel_active_remote_session(&mut self) {
        self.clear_tool_confirmation_route();
        if let Some((session_id, cluster)) = self.active_remote_session.clone() {
            self.exit_after_cancel = false;
            self.start_cancellation(session_id, cluster);
        }
    }
}
