//! Shared construction and recovery operations for broker-backed sessions.

use crate::types::Tui;
use anyhow::{Context, Result};
use harnx_runtime::config::GlobalConfig;
use harnx_runtime::{NatsSession, NatsSessionConfig};
use std::sync::Arc;
use tokio::sync::Mutex;

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

impl Tui {
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
