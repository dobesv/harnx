mod server;

use anyhow::Context;
use rmcp::transport::streamable_http_server::session::never::NeverSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::ServiceExt;
use server::TimeServer;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

struct Args {
    http: bool,
    host: String,
    port: u16,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = parse_args()?;

    if args.http {
        run_http(args).await
    } else {
        eprintln!("harnx-mcp-time v{}: starting", env!("CARGO_PKG_VERSION"));

        let server = TimeServer::new();
        let service = server.serve(rmcp::transport::stdio()).await?;
        service.waiting().await?;

        Ok(())
    }
}

fn parse_args() -> anyhow::Result<Args> {
    let mut http = false;
    let mut host = "0.0.0.0".to_string();
    let mut port = 3000;

    let args: Vec<String> = std::env::args().collect();
    let mut i = 1;

    while i < args.len() {
        match args[i].as_str() {
            "--http" => {
                http = true;
                i += 1;
            }
            "--host" => {
                if i + 1 >= args.len() {
                    anyhow::bail!("harnx-mcp-time: --host requires an address argument");
                }
                host = args[i + 1].clone();
                i += 2;
            }
            "--port" => {
                if i + 1 >= args.len() {
                    anyhow::bail!("harnx-mcp-time: --port requires a number argument");
                }
                match args[i + 1].parse::<u16>() {
                    Ok(value) => {
                        port = value;
                        i += 2;
                    }
                    Err(_) => {
                        anyhow::bail!(
                            "harnx-mcp-time: --port requires a valid u16 port (got: {})",
                            args[i + 1]
                        );
                    }
                }
            }
            "--help" | "-h" => {
                print_help();
                std::process::exit(0);
            }
            other => {
                anyhow::bail!(
                    "harnx-mcp-time: unknown argument: {other}\nTry: harnx-mcp-time --help"
                );
            }
        }
    }

    Ok(Args { http, host, port })
}

fn print_help() {
    eprintln!("harnx-mcp-time: MCP time utilities server");
    eprintln!();
    eprintln!("Usage: harnx-mcp-time [OPTIONS]");
    eprintln!();
    eprintln!("Options:");
    eprintln!("  --http              Serve MCP over Streamable HTTP at /mcp");
    eprintln!("  --host <addr>       Bind address for HTTP mode (default: 0.0.0.0)");
    eprintln!("  --port <N>          Bind port for HTTP mode (default: 3000)");
    eprintln!("  --help, -h          Show this help message");
}

async fn run_http(args: Args) -> anyhow::Result<()> {
    let Args { host, port, .. } = args;
    let ct = CancellationToken::new();
    let config = StreamableHttpServerConfig::default()
        .with_legacy_session_mode(false)
        .with_json_response(true)
        .with_cancellation_token(ct.child_token())
        // Empty allowlist = accept any Host header. Required so external
        // (e.g. Kubernetes) Host values aren't rejected by rmcp's default
        // loopback-only allowlist. Deploy behind a trusted ingress/network.
        .disable_allowed_hosts();
    let mcp_service = StreamableHttpService::new(
        || Ok(TimeServer::new()),
        Arc::new(NeverSessionManager::default()),
        config,
    );
    let app = axum::Router::new().nest_service("/mcp", mcp_service);
    let listener = tokio::net::TcpListener::bind((host.as_str(), port))
        .await
        .with_context(|| format!("harnx-mcp-time: failed to bind {host}:{port}"))?;

    spawn_shutdown_handler(ct.clone());

    eprintln!(
        "harnx-mcp-time v{}: listening on http://{}:{}/mcp",
        env!("CARGO_PKG_VERSION"),
        host,
        port
    );

    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            ct.cancelled().await;
        })
        .await?;

    Ok(())
}

fn spawn_shutdown_handler(ct: CancellationToken) {
    tokio::spawn(async move {
        #[cfg(unix)]
        {
            match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                Ok(mut sigterm) => {
                    tokio::select! {
                        _ = tokio::signal::ctrl_c() => {}
                        _ = sigterm.recv() => {}
                    }
                }
                Err(e) => {
                    eprintln!(
                        "harnx-mcp-time: failed to install SIGTERM handler ({e}); \
                         falling back to Ctrl-C only"
                    );
                    let _ = tokio::signal::ctrl_c().await;
                }
            }
        }

        #[cfg(not(unix))]
        {
            let _ = tokio::signal::ctrl_c().await;
        }

        ct.cancel();
    });
}
