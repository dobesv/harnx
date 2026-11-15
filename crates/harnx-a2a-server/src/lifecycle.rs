//! Stop advertising readiness before aborting and draining runners.
use crate::runner::Runner;
use anyhow::Result;
use harnx_core::abort::AbortSignal;
use harnx_healthz::Readiness;
use std::sync::Arc;

pub(crate) async fn shutdown(
    readiness: Option<Readiness>,
    abort: AbortSignal,
    runners: Vec<Arc<Runner>>,
) {
    if let Err(error) = shutdown_signal().await {
        tracing::warn!(%error, "failed to listen for shutdown signal");
    }
    if let Some(readiness) = readiness {
        readiness.not_ready();
    }
    abort.set_ctrlc();
    for runner in runners {
        runner.shutdown().await;
    }
}

async fn shutdown_signal() -> Result<()> {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => result?,
            _ = terminate.recv() => {},
        }
    }
    #[cfg(not(unix))]
    tokio::signal::ctrl_c().await?;
    Ok(())
}
