mod server;

use anyhow::Context;
use harnx_healthz::Readiness;
use rmcp::transport::streamable_http_server::session::never::NeverSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::ServiceExt;
use server::TimeServer;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

const PASSTHROUGH_FLAGS: [(&str, &str); 2] = [
    ("--metrics-addr", "--metrics-addr="),
    ("--healthz-addr", "--healthz-addr="),
];

struct ParseState<'a> {
    args: &'a [String],
    index: usize,
}

impl ParseState<'_> {
    fn current(&self) -> &str {
        self.args[self.index].as_str()
    }

    fn consume_passthrough(&mut self) -> bool {
        let arg = self.current();
        for (flag, assignment) in PASSTHROUGH_FLAGS {
            if arg == flag {
                self.index += 2;
                return true;
            }
            if arg.starts_with(assignment) {
                self.index += 1;
                return true;
            }
        }
        false
    }
}

struct Args {
    http: bool,
    host: String,
    port: u16,
    metrics_addr: Option<String>,
    healthz_addr: Option<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = harnx_core::logging::init(harnx_core::logging::LogSink::Stderr);
    let telemetry = harnx_telemetry::init_telemetry("harnx-mcp-time")?;

    let result = run().await;
    telemetry.shutdown().await;
    result
}

async fn run() -> anyhow::Result<()> {
    let args = parse_args()?;

    // Init metrics recorder (idempotent via OnceLock)
    let metrics_flags = harnx_metrics::MetricsFlags {
        metrics_addr: args.metrics_addr.clone(),
    };
    harnx_metrics::init(&metrics_flags)?;

    let readiness = harnx_healthz::init(&harnx_healthz::HealthzFlags {
        healthz_addr: args.healthz_addr.clone(),
    })
    .await?;

    if args.http {
        run_http(args, readiness).await
    } else {
        log::info!("harnx-mcp-time v{}: starting", env!("CARGO_PKG_VERSION"));

        let server = TimeServer::new();
        let service = server.serve(rmcp::transport::stdio()).await?;
        if let Some(readiness) = &readiness {
            readiness.ready();
        }
        service.waiting().await?;

        Ok(())
    }
}

fn parse_args() -> anyhow::Result<Args> {
    let mut http = false;
    let mut host = "0.0.0.0".to_string();
    let mut port = 3000;
    let args: Vec<String> = std::env::args().collect();
    let mut state = ParseState {
        args: &args,
        index: 1,
    };

    while state.index < args.len() {
        if state.consume_passthrough() {
            continue;
        }
        match state.current() {
            "--http" => {
                http = true;
                state.index += 1;
            }
            "--host" => {
                if state.index + 1 >= args.len() {
                    anyhow::bail!("harnx-mcp-time: --host requires an address argument");
                }
                host = args[state.index + 1].clone();
                state.index += 2;
            }
            "--port" => {
                if state.index + 1 >= args.len() {
                    anyhow::bail!("harnx-mcp-time: --port requires a number argument");
                }
                match args[state.index + 1].parse::<u16>() {
                    Ok(value) => {
                        port = value;
                        state.index += 2;
                    }
                    Err(_) => {
                        anyhow::bail!(
                            "harnx-mcp-time: --port requires a valid u16 port (got: {})",
                            args[state.index + 1]
                        );
                    }
                }
            }
            "--help" | "-h" => {
                print_help();
                std::process::exit(0);
            }
            unknown => anyhow::bail!(
                "harnx-mcp-time: unknown argument: {unknown}\nTry: harnx-mcp-time --help"
            ),
        }
    }

    // Use shared helper for metrics-addr (supports both --metrics-addr VAL and --metrics-addr=VAL)
    let metrics_addr = harnx_metrics::metrics_addr_from_args(args.clone())
        .or_else(|| std::env::var("HARNX_METRICS_ADDR").ok());
    let healthz_addr = harnx_healthz::healthz_addr_from_args(args.clone())
        .or_else(|| std::env::var("HARNX_HEALTHZ_ADDR").ok());

    Ok(Args {
        http,
        host,
        port,
        metrics_addr,
        healthz_addr,
    })
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
    eprintln!("  --metrics-addr <ADDR>   Serve Prometheus metrics at http://ADDR/metrics.");
    eprintln!("                        Blank host binds 0.0.0.0, e.g. :8456. Unset disables.");
    eprintln!("                        Also honors HARNX_METRICS_ADDR env.");
    eprintln!("  --healthz-addr <ADDR>   Serve readiness checks at http://ADDR/healthz.");
    eprintln!("                        Blank host binds 0.0.0.0, e.g. :8457. Unset disables.");
    eprintln!("                        Also honors HARNX_HEALTHZ_ADDR env.");
    eprintln!("  --help, -h          Show this help message");
}

async fn run_http(args: Args, readiness: Option<Readiness>) -> anyhow::Result<()> {
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
    let app =
        axum::Router::new()
            .nest_service("/mcp", mcp_service)
            .layer(axum::middleware::from_fn(
                harnx_metrics::http_metrics_middleware,
            ));
    let listener = tokio::net::TcpListener::bind((host.as_str(), port))
        .await
        .with_context(|| format!("harnx-mcp-time: failed to bind {host}:{port}"))?;
    if let Some(readiness) = &readiness {
        readiness.ready();
    }

    spawn_shutdown_handler(ct.clone(), readiness);

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

fn spawn_shutdown_handler(ct: CancellationToken, readiness: Option<Readiness>) {
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

        if let Some(readiness) = &readiness {
            readiness.not_ready();
        }
        ct.cancel();
    });
}
