use super::{serve_configured, RegistrationIdentity, ServeLifecycle, ServeSettings};
use anyhow::Result;
use harnx_core::instance::ServerScope;
use harnx_nats_common::connect::NatsConnection;
use harnx_toolset::Toolset;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::oneshot;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

const REGISTRATION_TIMEOUT: Duration = Duration::from_secs(10);

/// Serve several independently named toolsets over one NATS connection.
///
/// The process becomes ready only after every registration is visible. If any
/// constituent server exits, the remaining servers are stopped and the
/// aggregate server exits too.
pub async fn serve_many_with_shutdown(
    toolsets: Vec<Arc<dyn Toolset>>,
    instance_id: ServerScope,
    connection: NatsConnection,
    lifecycle: ServeLifecycle,
) -> Result<()> {
    validate_toolsets(&toolsets)?;
    let (shutdown, readiness) = lifecycle.into_parts();
    let child_shutdown = CancellationToken::new();
    let (mut servers, started) =
        start_servers(toolsets, instance_id, connection, child_shutdown.clone());

    let outcome = run_aggregate(&shutdown, readiness.as_ref(), &mut servers, started).await;
    if let Some(readiness) = readiness.as_ref() {
        readiness.not_ready();
    }
    stop_servers(child_shutdown, &mut servers).await;
    outcome
}

fn validate_toolsets(toolsets: &[Arc<dyn Toolset>]) -> Result<()> {
    anyhow::ensure!(!toolsets.is_empty(), "at least one toolset is required");
    let mut names = HashSet::new();
    for toolset in toolsets {
        anyhow::ensure!(
            names.insert(toolset.name().to_string()),
            "duplicate toolset name '{}'",
            toolset.name()
        );
    }
    Ok(())
}

fn start_servers(
    toolsets: Vec<Arc<dyn Toolset>>,
    instance_id: ServerScope,
    connection: NatsConnection,
    shutdown: CancellationToken,
) -> (JoinSet<Result<()>>, Vec<oneshot::Receiver<()>>) {
    let identity = RegistrationIdentity::from_env();
    let mut servers = JoinSet::new();
    let mut started = Vec::with_capacity(toolsets.len());
    for toolset in toolsets {
        let (started_tx, started_rx) = oneshot::channel();
        started.push(started_rx);
        servers.spawn(serve_configured(
            toolset,
            ServeSettings {
                instance_id: instance_id.clone(),
                connection: connection.clone(),
                lifecycle: ServeLifecycle::new(shutdown.clone(), None),
                identity: identity.clone(),
                started: Some(started_tx),
            },
        ));
    }
    (servers, started)
}

async fn run_aggregate(
    shutdown: &CancellationToken,
    readiness: Option<&harnx_healthz::Readiness>,
    servers: &mut JoinSet<Result<()>>,
    started: Vec<oneshot::Receiver<()>>,
) -> Result<()> {
    if !await_startup(shutdown, servers, started).await? {
        return Ok(());
    }
    if let Some(readiness) = readiness {
        readiness.ready();
    }
    wait_until_shutdown(shutdown, servers).await
}

async fn await_startup(
    shutdown: &CancellationToken,
    servers: &mut JoinSet<Result<()>>,
    started: Vec<oneshot::Receiver<()>>,
) -> Result<bool> {
    let registrations = tokio::time::timeout(
        REGISTRATION_TIMEOUT,
        futures_util::future::try_join_all(started),
    );
    tokio::pin!(registrations);
    tokio::select! {
        _ = shutdown.cancelled() => Ok(false),
        joined = servers.join_next() => Err(server_exit_error(joined)),
        registrations = &mut registrations => map_startup_result(registrations),
    }
}

fn map_startup_result(
    result: Result<Result<Vec<()>, oneshot::error::RecvError>, tokio::time::error::Elapsed>,
) -> Result<bool> {
    match result {
        Ok(Ok(_)) => Ok(true),
        Ok(Err(error)) => Err(anyhow::Error::from(error).context("toolset startup signal closed")),
        Err(_) => anyhow::bail!("timed out waiting for all toolset registrations"),
    }
}

async fn wait_until_shutdown(
    shutdown: &CancellationToken,
    servers: &mut JoinSet<Result<()>>,
) -> Result<()> {
    tokio::select! {
        _ = shutdown.cancelled() => Ok(()),
        joined = servers.join_next() => Err(server_exit_error(joined)),
    }
}

async fn stop_servers(shutdown: CancellationToken, servers: &mut JoinSet<Result<()>>) {
    shutdown.cancel();
    while let Some(joined) = servers.join_next().await {
        if let Ok(Err(error)) = joined {
            log::debug!("constituent toolset server stopped with an error: {error:#}");
        }
    }
}

fn server_exit_error(joined: Option<Result<Result<()>, tokio::task::JoinError>>) -> anyhow::Error {
    match joined {
        Some(Ok(Ok(()))) => anyhow::anyhow!("constituent toolset server exited unexpectedly"),
        Some(Ok(Err(error))) => error.context("constituent toolset server failed"),
        Some(Err(error)) => anyhow::Error::from(error).context("join constituent toolset server"),
        None => anyhow::anyhow!("all constituent toolset servers exited unexpectedly"),
    }
}
