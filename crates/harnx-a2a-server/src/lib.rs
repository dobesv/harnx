//! HTTP frontend for harnx agents using A2A JSON-RPC and SSE.

use anyhow::{Context, Result};
pub mod routes;

mod access;
mod agent_card;
mod bootstrap;
pub mod cli;
mod compat;
pub mod exports;
pub mod handler;
pub mod identity;
pub mod input_map;
pub mod runner;
mod sse;
pub mod store;
mod web_url;

#[cfg(test)]
mod test_support;

/// Separate from native toolset ports (3000–3007) and MCP HTTP (3010).
pub const DEFAULT_A2A_HTTP_PORT: u16 = 3020;

/// Bind and serve until Ctrl-C or SIGTERM.
///
/// Unknown agents and export collisions fail before binding the listener.
pub async fn run(args: cli::Args) -> Result<()> {
    // Runtime path helpers read HARNX_CONFIG_DIR during turns as well as startup.
    if let Some(dir) = &args.config_dir {
        std::env::set_var("HARNX_CONFIG_DIR", dir);
    }
    let access_rules = harnx_runtime::access::load_access_rules(args.access_rules.clone())?;
    let identity = startup_identity(&args)?;
    identity.validate_access_rules(access_rules.is_some())?;
    let exports = exports::resolve_exports(
        &args.agents,
        args.cluster.as_deref(),
        args.config_dir.as_deref(),
    )
    .await
    .context("resolve agent exports")?;
    let abort = harnx_core::abort::create_abort_signal();
    let bootstrap = bootstrap::Bootstrap::new(
        &exports,
        args.config_dir.as_deref(),
        abort.clone(),
        access_rules.clone(),
    )
    .await?;
    let export_count = exports.len();
    let app = routes::router_with_access_rules(
        &exports,
        args.public_base_url.as_deref(),
        identity,
        access_rules,
        |export, identity| {
            std::sync::Arc::new(handler::HarnxHandler::new(
                export.clone(),
                identity,
                bootstrap.backends[export
                    .cluster
                    .as_deref()
                    .unwrap_or(harnx_runtime::config::LOCAL_CLUSTER_KEY)]
                .clone(),
                input_map::InputLimits {
                    max_data_part_bytes: args.max_data_part_bytes,
                },
            ))
        },
    )?;

    let listener = tokio::net::TcpListener::bind((args.host.as_str(), args.port))
        .await
        .context("bind A2A HTTP listener")?;
    tracing::info!(address = %listener.local_addr()?, exports = export_count, "serving A2A HTTP");
    let shutdown_runners: Vec<_> = bootstrap
        .backends
        .values()
        .map(|backend| backend.runner.clone())
        .collect();
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            if let Err(error) = shutdown_signal().await {
                tracing::warn!(%error, "failed to listen for shutdown signal");
            }
            abort.set_ctrlc();
            for runner in shutdown_runners {
                runner.shutdown().await;
            }
        })
        .await
        .context("run A2A HTTP server")?;
    drop(bootstrap);
    Ok(())
}

fn startup_identity(args: &cli::Args) -> Result<identity::Identity> {
    identity::Identity::with_memberships(
        &args.user_id_header,
        &args.group_header,
        &args.role_header,
    )
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
