//! NATS worker startup and readiness synchronization.

use anyhow::{Context, Result};
use futures::StreamExt;
use harnx_runtime::config::GlobalConfig;
use harnx_runtime::nats_worker::{run_worker_daemon, worker_ready_subject, WorkerDaemonConfig};
use harnx_runtime::AgentCallFn;

use super::{CLUSTER, TEST_TIMEOUT};

pub(crate) async fn spawn_worker(
    config: GlobalConfig,
    call_fn: AgentCallFn,
) -> Result<tokio::task::JoinHandle<Result<()>>> {
    let client = {
        let config = config.read().clone();
        config.nats_client(CLUSTER).await?
    };
    let mut readiness = client.subscribe(worker_ready_subject(CLUSTER)).await?;
    client.flush().await?;

    let mut daemon = tokio::spawn(run_worker_daemon(
        config,
        WorkerDaemonConfig::managing(CLUSTER, "acp-integration-worker"),
        Some(call_fn),
        None,
    ));
    tokio::select! {
        ready = tokio::time::timeout(TEST_TIMEOUT, readiness.next()) => {
            ready.context("worker did not announce readiness")?
                .context("worker readiness subscription closed")?;
        }
        stopped = &mut daemon => anyhow::bail!("worker stopped before readiness: {stopped:?}"),
    }
    Ok(daemon)
}
