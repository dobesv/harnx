//! Shared construction and recovery operations for broker-backed sessions.

use crate::types::{CancellationAction, ExitCancelFactory, ExitWorkerState, Tui};
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

/// Build a read-only cancellation status session without consulting the local
/// worker supervisor. This session never publishes an activation.
pub(super) async fn cancellation_status_session_for_target(
    config: &GlobalConfig,
    session_id: String,
    cluster: String,
) -> Result<NatsSession> {
    let abort_signal = harnx_runtime::utils::create_abort_signal();
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
            activation_route: harnx_runtime::nats_worker::SessionActivationRoute::ClusterShared,
        },
        config,
        abort_signal,
    )
    .await
}

pub(crate) fn default_exit_cancel_factory() -> ExitCancelFactory {
    Arc::new(
        |config, local_worker, session_id, cluster, expected_execution_id, action| {
            Box::pin(async move {
                match action {
                    CancellationAction::Request => {
                        // Worker preparation is intentionally outside the durable
                        // request's own two-second persistence bound. Local workers
                        // consume targeted activations, and readiness can legitimately
                        // take longer than the cancellation CAS itself.
                        let session =
                            nats_session_for_target(&config, &local_worker, session_id, cluster)
                                .await?;
                        session
                            .request_cancel(harnx_execution_control::CancelRequest {
                                expected_execution_id,
                                retry: true,
                            })
                            .await
                    }
                    CancellationAction::Abandon => {
                        // Abandonment is a control-plane override and must not
                        // start a replacement local worker merely to retire the
                        // generation whose owners disappeared.
                        let retire_local_worker = cluster == LOCAL_CLUSTER_KEY;
                        let session =
                            cancellation_status_session_for_target(&config, session_id, cluster)
                                .await?;
                        let receipt = session
                            .abandon_unconfirmed_cancellation(expected_execution_id.as_deref())
                            .await?;
                        if receipt.abandoned && retire_local_worker {
                            // Keep this wait in the background cancellation
                            // future. Polling it from the TUI loop would freeze
                            // the frame before the Abandoning state is drawn.
                            // The composer remains locked until the old process
                            // tree has been retired.
                            local_worker.lock().await.take();
                        }
                        Ok(receipt)
                    }
                }
            })
        },
    )
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
        self.start_cancellation(session_id, cluster, None);
        true
    }

    pub(crate) async fn poll_pending_exit_cancel(&mut self) {
        self.poll_cancellation_status();
        if self.finish_exit_after_confirmed_cancellation() {
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
        match result {
            Ok(receipt) => {
                let locally_owned_worker = matches!(
                    self.app.modal,
                    Some(crate::types::ModalState::ConfirmExit {
                        worker_state: ExitWorkerState::LocalOwnedHere,
                        ..
                    })
                );
                if self.exit_after_cancel && !locally_owned_worker {
                    self.exit_after_cancel = false;
                    self.app.modal = None;
                    self.app.should_quit = true;
                } else {
                    // A local worker and its managed tool/sub-agent servers are
                    // children of this frontend. Keep that process tree alive
                    // until the durable operation graph confirms it stopped;
                    // exiting on acceptance alone can strand descendants in an
                    // unconfirmed state that permanently blocks later prompts.
                    self.app.modal = None;
                    self.monitor_cancellation(receipt);
                    self.finish_exit_after_confirmed_cancellation();
                }
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

    fn finish_exit_after_confirmed_cancellation(&mut self) -> bool {
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
            self.start_cancellation(session_id, cluster, None);
        }
    }
}
