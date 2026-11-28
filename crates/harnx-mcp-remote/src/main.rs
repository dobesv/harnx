use std::time::Duration;

use clap::Parser;
use harnx_mcp_remote::cli;
use harnx_mcp_remote::native;
use harnx_mcp_remote::server::RemoteProxyServer;
use harnx_telemetry::TelemetryOptions;
use rmcp::ServiceExt;
use tokio_util::sync::CancellationToken;

/// Dependency payload targets that must never reach OTLP in client-credentials
/// mode, regardless of how broad the user's `RUST_LOG` is. rmcp logs MCP
/// payloads at INFO/WARN and reqwest/hyper carry request bodies, so severity
/// is not a secrecy boundary here. OTLP rejects spans and events from these
/// targets and their descendants independently of `RUST_LOG`.
const OTEL_EXCLUDED_TARGETS: &[&str] = &["rmcp", "reqwest", "hyper", "hyper_util"];

fn main() -> anyhow::Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let result = rt.block_on(async_main());
    // Tokio stdio can leave a blocking read alive; bound runtime shutdown.
    rt.shutdown_timeout(Duration::from_secs(1));
    result
}

async fn async_main() -> anyhow::Result<()> {
    // Parse before logging/telemetry: client-credentials mode changes both
    // policies, and clap's own error output never touches the logger.
    let cli = cli::Cli::parse();

    // Native NATS mode: per-user credentials, connect pages and per-user
    // registrations. Selected only by --credentials-key-file; without it the
    // stdio proxy path below runs (plain proxy behavior unchanged by default,
    // or opt-in client-credentials mode with --oauth-token-url). The shared
    // runner owns metrics, healthz, telemetry and SIGTERM handling in this
    // mode, so this branch must not initialize any of them first.
    if cli.credentials_key_file.is_some() {
        // Native mode's entry rejects the mixed native+OAuth invocation
        // before its own raw-URL startup log (B3).
        return native::run_native_mode(cli).await;
    }

    let client_credentials = cli.oauth_token_url.is_some();
    if client_credentials {
        // Client-credentials mode owns the stderr boundary: dependency
        // payload logs are never allowed through, whatever the user set
        // HARNX_LOG_FILTER to. Only app-owned (harnx*) targets pass; their
        // messages are static and never contain the configured OAuth URLs.
        let mut settings = harnx_core::logging::settings(harnx_core::logging::LogSink::Stderr);
        settings.filter = "harnx".to_string();
        let _ = harnx_core::logging::init_with(settings)?;
    } else {
        let _ = harnx_core::logging::init(harnx_core::logging::LogSink::Stderr);
    }

    let telemetry = if client_credentials {
        harnx_telemetry::init_telemetry_with_options(
            "harnx-mcp-remote",
            &TelemetryOptions {
                exclude_targets: OTEL_EXCLUDED_TARGETS
                    .iter()
                    .map(|target| target.to_string())
                    .collect(),
            },
        )?
    } else {
        harnx_telemetry::init_telemetry("harnx-mcp-remote")?
    };

    let result = run(&cli).await;
    telemetry.shutdown().await;
    result
}

/// Startup log line, split by mode. Client-credentials mode never logs the
/// configured URLs (either can carry an embedded credential); its short-lived
/// span doubles as the OTLP positive control for that mode's exclusion
/// policy — app-owned, static and safe, so it must arrive at the collector.
fn log_startup(cli: &cli::Cli) {
    if cli.oauth_token_url.is_some() {
        log::info!(
            "harnx-mcp-remote v{}: starting in client-credentials mode",
            env!("CARGO_PKG_VERSION")
        );
        drop(tracing::info_span!(
            target: "harnx_mcp_remote",
            "client_credentials_proxy_started"
        ));
    } else {
        log::info!(
            "harnx-mcp-remote v{}: starting, proxying to {}",
            env!("CARGO_PKG_VERSION"),
            cli.url
        );
    }
}

async fn run(cli: &cli::Cli) -> anyhow::Result<()> {
    // Initialize metrics if configured via --metrics-addr or HARNX_METRICS_ADDR env.
    let flags = harnx_metrics::MetricsFlags {
        metrics_addr: cli.metrics_addr.clone(),
    };
    harnx_metrics::init(&flags)?;

    // Initialize healthz if configured via --healthz-addr or HARNX_HEALTHZ_ADDR env.
    let healthz_flags = harnx_healthz::HealthzFlags {
        healthz_addr: cli.healthz_addr.clone(),
    };
    let readiness = harnx_healthz::init(&healthz_flags).await?;

    log_startup(cli);

    let server = RemoteProxyServer::new(cli.clone());
    let transport = rmcp::transport::stdio();
    let serve_ct = CancellationToken::new();
    let shutdown = harnx_nats_common::shutdown::cancel_token_on_shutdown_signal();

    // Signal readiness when stdio transport starts serving.
    if let Some(ref r) = readiness {
        r.ready();
    }

    tokio::select! {
        service = server.serve_with_ct(transport, serve_ct.clone()) => {
            let service = service?;
            shutdown.cancelled().await;
            if let Some(ref r) = readiness {
                r.not_ready();
            }
            service.service().shutdown_remote().await?;
            service.cancel().await?;
        }
        _ = shutdown.cancelled() => {
            // SIGTERM/SIGINT before initialize completes: cancel the in-progress
            // rmcp initialize wait and exit cleanly.
            if let Some(ref r) = readiness {
                r.not_ready();
            }
            serve_ct.cancel();
        }
    }
    Ok(())
}
