mod cli;
mod client_handler;
mod server;
mod transport;

use std::time::Duration;

use clap::Parser;
use rmcp::ServiceExt;
use server::RemoteProxyServer;
use tokio_util::sync::CancellationToken;

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
    let _ = harnx_core::logging::init(harnx_core::logging::LogSink::Stderr);
    let telemetry = harnx_telemetry::init_telemetry("harnx-mcp-remote")?;

    let result = run().await;
    telemetry.shutdown().await;
    result
}

async fn run() -> anyhow::Result<()> {
    let cli = cli::Cli::parse();

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

    log::info!(
        "harnx-mcp-remote v{}: starting, proxying to {}",
        env!("CARGO_PKG_VERSION"),
        cli.url
    );

    #[cfg(unix)]
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    #[cfg(not(unix))]
    let mut sigterm = ();

    let server = RemoteProxyServer::new(cli);
    let transport = rmcp::transport::stdio();
    let serve_ct = CancellationToken::new();

    // Signal readiness when stdio transport starts serving.
    if let Some(ref r) = readiness {
        r.ready();
    }

    tokio::select! {
        service = server.serve_with_ct(transport, serve_ct.clone()) => {
            let service = service?;
            wait_for_shutdown(&mut sigterm).await;
            if let Some(ref r) = readiness {
                r.not_ready();
            }
            service.service().shutdown_remote().await?;
            service.cancel().await?;
        }
        _ = wait_for_shutdown(&mut sigterm) => {
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

#[cfg(unix)]
async fn wait_for_shutdown(sigterm: &mut tokio::signal::unix::Signal) {
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = sigterm.recv() => {}
    }
}

#[cfg(not(unix))]
async fn wait_for_shutdown(_sigterm: &mut ()) {
    let _ = tokio::signal::ctrl_c().await;
}
