//! HTTP frontend for harnx agents using A2A JSON-RPC and SSE.

use anyhow::{Context, Result};
pub mod routes;

mod access;
mod agent_card;
pub mod bearer_verify;
mod bootstrap;
pub mod cli;
mod compat;
mod diagnostics;
pub mod exports;
#[cfg(feature = "fault-injection")]
pub mod fault_injection;
pub mod handler;
mod hitl;
pub mod identity;
pub mod input_map;
mod lifecycle;
mod request_log;
pub mod runner;
mod sse;
pub mod store;
mod web_url;

#[cfg(test)]
mod test_support;

// Share the real broker/worker fixture with private-state regression tests.
#[cfg(test)]
extern crate self as harnx_a2a_server;
#[cfg(test)]
#[allow(dead_code)]
#[path = "../tests/it/support.rs"]
mod runner_test_support;

/// Separate from native toolset ports (3000–3007) and MCP HTTP (3010).
pub const DEFAULT_A2A_HTTP_PORT: u16 = 3020;

/// Bind and serve until Ctrl-C or SIGTERM.
///
/// Unknown agents and export collisions fail before binding the listener.
pub async fn run(args: cli::Args) -> Result<()> {
    harnx_metrics::init(&args.metrics)?;
    let readiness = harnx_healthz::init(&args.healthz).await?;
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
    if let Some(readiness) = &readiness {
        readiness.ready();
    }
    axum::serve(listener, app)
        .with_graceful_shutdown(lifecycle::shutdown(readiness, abort, shutdown_runners))
        .await
        .context("run A2A HTTP server")?;
    drop(bootstrap);
    Ok(())
}

fn startup_identity(args: &cli::Args) -> Result<identity::Identity> {
    let aliases = harnx_runtime::users::load_user_aliases()?;
    let mut identity = identity::Identity::with_memberships(
        &args.user_id_header,
        &args.group_header,
        &args.role_header,
    )?
    .with_user_aliases(aliases);
    if let Some(verifier) = bearer_verifier(args)? {
        identity = identity.with_bearer_verification(verifier);
    }
    identity.validate_bearer_verification()?;
    Ok(identity)
}

/// Build the bearer verifier from the verification flags. All three flags are
/// required together: `--bearer-jwks-url` enables verification, and issuer and
/// audience must be pinned too.
fn bearer_verifier(
    args: &cli::Args,
) -> Result<Option<std::sync::Arc<bearer_verify::BearerVerifier>>> {
    let (Some(url), Some(issuer), Some(audience)) = (
        args.bearer_jwks_url.as_deref(),
        args.bearer_issuer.as_deref(),
        args.bearer_audience.as_deref(),
    ) else {
        let set = [
            args.bearer_jwks_url.is_some(),
            args.bearer_issuer.is_some(),
            args.bearer_audience.is_some(),
        ]
        .into_iter()
        .filter(|flag| *flag)
        .count();
        anyhow::ensure!(
            set == 0,
            "--bearer-jwks-url, --bearer-issuer and --bearer-audience are all required \
             to enable bearer token verification"
        );
        return Ok(None);
    };
    anyhow::ensure!(
        !issuer.trim().is_empty(),
        "--bearer-issuer must not be empty"
    );
    anyhow::ensure!(
        !audience.trim().is_empty(),
        "--bearer-audience must not be empty"
    );
    Ok(Some(std::sync::Arc::new(
        bearer_verify::BearerVerifier::new(bearer_verify::BearerVerificationConfig {
            jwks_url: url.to_owned(),
            issuer: issuer.to_owned(),
            audience: audience.to_owned(),
        })?,
    )))
}

#[cfg(test)]
mod startup_tests;
