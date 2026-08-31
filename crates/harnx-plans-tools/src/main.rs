//! harnx-plans-tools: File-based plan/task/note management MCP server.
//!
//! Stores plans, tasks, and notes as YAML-frontmatter markdown files in per-plan
//! subdirectories under `.agent/plans/` (configurable via `--dir` or `AGENT_PLANS_PATH`).
//!
//! Layout: `<dir>/<plan>/plan.md`, `<dir>/<plan>/tasks/<id>.md`, `<dir>/<plan>/notes/<id>.md`
//!
//! Provides: list_plans, add_plan, get_plan, update_plan, delete_plan,
//! list_tasks, add_task, get_task, update_task, delete_task,
//! list_notes, add_note, get_note, update_note, delete_note

use anyhow::Context;
use harnx_healthz::Readiness;
use harnx_plans_tools::server::{self, PlansServer};
use harnx_plans_tools::PlansToolset;
use rmcp::transport::streamable_http_server::session::never::NeverSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
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

    fn advance(&mut self, count: usize) {
        self.index += count;
    }

    fn consume_passthrough(&mut self) -> bool {
        let arg = self.current();
        for (flag, assignment) in PASSTHROUGH_FLAGS {
            if arg == flag {
                self.advance(2);
                return true;
            }
            if arg.starts_with(assignment) {
                self.advance(1);
                return true;
            }
        }
        false
    }
}

struct HttpServeConfig {
    plans_dir: PathBuf,
    retention_days: u64,
    host: String,
    port: u16,
    readiness: Option<Readiness>,
}

struct HttpServerLoop {
    plans_dir: PathBuf,
    retention_days: u64,
    listener: tokio::net::TcpListener,
    app: axum::Router,
    cancellation: CancellationToken,
}

struct Args {
    plans_dir: PathBuf,
    retention_days: u64,
    http: bool,
    host: String,
    port: u16,
    metrics_addr: Option<String>,
    healthz_addr: Option<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = harnx_core::logging::init(harnx_core::logging::LogSink::Stderr);
    let Args {
        plans_dir,
        retention_days,
        http,
        host,
        port,
        metrics_addr,
        healthz_addr,
    } = parse_args()?;

    if http {
        harnx_metrics::init(&harnx_metrics::MetricsFlags {
            metrics_addr: metrics_addr.clone(),
        })?;
        let readiness = harnx_healthz::init(&harnx_healthz::HealthzFlags {
            healthz_addr: healthz_addr.clone(),
        })
        .await?;
        return run_http(HttpServeConfig {
            plans_dir,
            retention_days,
            host,
            port,
            readiness,
        })
        .await;
    }

    log::info!(
        "harnx-plans-tools v{}: starting (dir: {}, retention: {} days)",
        env!("CARGO_PKG_VERSION"),
        plans_dir.display(),
        retention_days
    );

    let cleanup = spawn_cleanup(plans_dir.clone(), retention_days);
    let result = harnx_toolset_server::run_toolset_main(PlansToolset::new(plans_dir)).await;
    if let Some(cleanup) = cleanup {
        cleanup.abort();
    }
    result
}

fn spawn_cleanup(plans_dir: PathBuf, retention_days: u64) -> Option<tokio::task::JoinHandle<()>> {
    if retention_days == 0 {
        log::info!("[cleanup] retention disabled");
        None
    } else {
        Some(tokio::spawn(supervise_cleanup(plans_dir, retention_days)))
    }
}

async fn supervise_cleanup(plans_dir: PathBuf, retention_days: u64) {
    const BASE_BACKOFF: Duration = Duration::from_secs(1);
    const MAX_BACKOFF: Duration = Duration::from_secs(300);
    let mut backoff = BASE_BACKOFF;

    loop {
        match tokio::spawn(server::cleanup_loop(plans_dir.clone(), retention_days)).await {
            Err(error) => {
                log::error!("[cleanup] task failed: {error}");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(MAX_BACKOFF);
            }
            Ok(()) => backoff = BASE_BACKOFF,
        }
    }
}

fn print_help() {
    eprintln!("harnx-plans-tools: File-based todo/plan management MCP server");
    eprintln!();
    eprintln!("Usage: harnx-plans-tools [OPTIONS]");
    eprintln!();
    eprintln!("Options:");
    eprintln!("  --dir, -d <path>           Set the plans directory (default: .agent/plans)");
    eprintln!("  --retention-days, -r <N>   Set retention period in days (default: 14)");
    eprintln!("  --http                     Serve MCP over Streamable HTTP at /mcp");
    eprintln!("  --mcp-stdio                Serve MCP over stdio instead of native NATS mode");
    eprintln!("  --host <addr>              Bind address for HTTP mode (default: 0.0.0.0)");
    eprintln!("  --port <N>                 Bind port for HTTP mode (default: 3000)");
    eprintln!("  --metrics-addr <ADDR>      Serve Prometheus metrics at http://ADDR/metrics.");
    eprintln!("                             Blank host binds 0.0.0.0, e.g. :8456. Unset disables.");
    eprintln!("                             Also honors HARNX_METRICS_ADDR env.");
    eprintln!("  --healthz-addr <ADDR>      Serve readiness checks at http://ADDR/healthz.");
    eprintln!("                             Blank host binds 0.0.0.0, e.g. :8457. Unset disables.");
    eprintln!("                             Also honors HARNX_HEALTHZ_ADDR env.");
    eprintln!("  --help, -h                 Show this help message");
    eprintln!();
    eprintln!("Env:");
    eprintln!("  AGENT_PLANS_PATH         Overrides the default directory.");
    eprintln!("  AGENT_PLANS_RETENTION_DAYS  Overrides the default retention days.");
}

fn parse_args() -> anyhow::Result<Args> {
    let args: Vec<String> = std::env::args().collect();
    let mut plans_dir: Option<PathBuf> = None;
    let mut retention_days: Option<u64> = None;
    let mut http = false;
    let mut host = None::<String>;
    let mut port = None::<u16>;
    let mut state = ParseState {
        args: &args,
        index: 1,
    };
    while state.index < args.len() {
        if state.consume_passthrough() {
            continue;
        }
        match state.current() {
            "--dir" | "-d" => {
                if state.index + 1 < args.len() {
                    plans_dir = Some(PathBuf::from(&args[state.index + 1]));
                    state.index += 2;
                } else {
                    anyhow::bail!("harnx-plans-tools: --dir requires a path argument");
                }
            }
            "--retention-days" | "-r" => {
                if state.index + 1 < args.len() {
                    match args[state.index + 1].parse::<u64>() {
                        Ok(days) => {
                            retention_days = Some(days);
                            state.index += 2;
                        }
                        Err(_) => {
                            anyhow::bail!(
                                "harnx-plans-tools: --retention-days requires a non-negative integer (got: {})",
                                args[state.index + 1]
                            );
                        }
                    }
                } else {
                    anyhow::bail!("harnx-plans-tools: --retention-days requires a number argument");
                }
            }
            "--http" => {
                http = true;
                state.index += 1;
            }
            "--mcp-stdio" => {
                state.index += 1;
            }
            "--host" => {
                if state.index + 1 < args.len() {
                    host = Some(args[state.index + 1].clone());
                    state.index += 2;
                } else {
                    anyhow::bail!("harnx-plans-tools: --host requires an address argument");
                }
            }
            "--port" => {
                if state.index + 1 < args.len() {
                    match args[state.index + 1].parse::<u16>() {
                        Ok(p) => {
                            port = Some(p);
                            state.index += 2;
                        }
                        Err(_) => {
                            anyhow::bail!(
                                "harnx-plans-tools: --port requires a port number (got: {})",
                                args[state.index + 1]
                            );
                        }
                    }
                } else {
                    anyhow::bail!("harnx-plans-tools: --port requires a number argument");
                }
            }
            "--help" | "-h" => {
                print_help();
                std::process::exit(0);
            }
            unknown => anyhow::bail!(
                "harnx-plans-tools: unknown argument: {unknown}\nTry: harnx-plans-tools --help"
            ),
        }
    }

    // Use shared helper for metrics-addr (supports both --metrics-addr VAL and --metrics-addr=VAL)
    let metrics_addr = harnx_metrics::metrics_addr_from_args(args.clone())
        .or_else(|| std::env::var("HARNX_METRICS_ADDR").ok());
    let healthz_addr = harnx_healthz::healthz_addr_from_args(args.clone())
        .or_else(|| std::env::var("HARNX_HEALTHZ_ADDR").ok());

    // Resolve retention_days: CLI flag > env var > default (14)
    let retention_days = if let Some(days) = retention_days {
        days
    } else if let Ok(env_days) = std::env::var("AGENT_PLANS_RETENTION_DAYS") {
        match env_days.trim().parse::<u64>() {
            Ok(days) => days,
            Err(_) => {
                anyhow::bail!(
                    "harnx-plans-tools: AGENT_PLANS_RETENTION_DAYS must be a non-negative integer (got: {})",
                    env_days.trim()
                );
            }
        }
    } else {
        14
    };

    // Resolve plans_dir: CLI flag > env var > default
    let plans_dir = if let Some(dir) = plans_dir {
        dir
    } else if let Ok(env_path) = std::env::var("AGENT_PLANS_PATH") {
        if !env_path.trim().is_empty() {
            PathBuf::from(env_path.trim())
        } else {
            PathBuf::from(".agent/plans")
        }
    } else {
        PathBuf::from(".agent/plans")
    };

    // Resolve HTTP options
    let host = host.unwrap_or_else(|| "0.0.0.0".to_string());
    let port = port.unwrap_or(3000);

    Ok(Args {
        plans_dir,
        retention_days,
        http,
        host,
        port,
        metrics_addr,
        healthz_addr,
    })
}

/// Extracts MCP service configuration for HTTP mode.
fn build_mcp_service(
    plans_dir: PathBuf,
    ct: CancellationToken,
) -> StreamableHttpService<PlansServer, NeverSessionManager> {
    let config = StreamableHttpServerConfig::default()
        .with_legacy_session_mode(false)
        .with_json_response(true)
        .with_cancellation_token(ct.child_token())
        // Empty allowlist = accept any Host header. Required so external
        // (e.g. Kubernetes) Host values aren't rejected by rmcp's default
        // loopback-only allowlist. Deploy behind a trusted ingress/network.
        .disable_allowed_hosts();
    StreamableHttpService::new(
        move || Ok(PlansServer::new(plans_dir.clone())),
        Arc::new(NeverSessionManager::default()),
        config,
    )
}

async fn run_http_server_loop(config: HttpServerLoop) -> anyhow::Result<()> {
    let HttpServerLoop {
        plans_dir,
        retention_days,
        listener,
        app,
        cancellation: ct,
    } = config;
    if retention_days == 0 {
        log::info!("[cleanup] retention disabled");
        axum::serve(listener, app)
            .with_graceful_shutdown(async move { ct.cancelled().await })
            .await?;
        return Ok(());
    }

    let cleanup_dir = plans_dir.clone();
    let mut cleanup_handle =
        tokio::spawn(server::cleanup_loop(cleanup_dir.clone(), retention_days));
    const BASE_BACKOFF: Duration = Duration::from_secs(1);
    const MAX_BACKOFF: Duration = Duration::from_secs(300);
    let mut backoff = BASE_BACKOFF;

    let shutdown_ct = ct.clone();
    let server_handle = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async move { shutdown_ct.cancelled().await })
            .await
    });
    tokio::pin!(server_handle);

    loop {
        tokio::select! {
            result = &mut *server_handle => {
                cleanup_handle.abort();
                result??;
                break;
            }
            result = &mut cleanup_handle => {
                match result {
                    Err(e) => {
                        log::error!("[cleanup] task failed: {e}");
                        tokio::time::sleep(backoff).await;
                        backoff = (backoff * 2).min(MAX_BACKOFF);
                    }
                    Ok(()) => { backoff = BASE_BACKOFF; }
                }
                cleanup_handle = tokio::spawn(server::cleanup_loop(cleanup_dir.clone(), retention_days));
            }
        }
    }

    Ok(())
}

async fn run_http(config: HttpServeConfig) -> anyhow::Result<()> {
    let HttpServeConfig {
        plans_dir,
        retention_days,
        host,
        port,
        readiness,
    } = config;
    let ct = CancellationToken::new();
    let mcp_service = build_mcp_service(plans_dir.clone(), ct.child_token());
    let app =
        axum::Router::new()
            .nest_service("/mcp", mcp_service)
            .layer(axum::middleware::from_fn(
                harnx_metrics::http_metrics_middleware,
            ));
    let listener = tokio::net::TcpListener::bind((host.as_str(), port))
        .await
        .with_context(|| format!("harnx-plans-tools: failed to bind {host}:{port}"))?;
    if let Some(readiness) = &readiness {
        readiness.ready();
    }

    spawn_shutdown_handler(ct.clone(), readiness);

    log::info!(
        "harnx-plans-tools v{}: listening on http://{}:{}/mcp (dir: {}, retention: {} days)",
        env!("CARGO_PKG_VERSION"),
        host,
        port,
        plans_dir.display(),
        retention_days,
    );

    run_http_server_loop(HttpServerLoop {
        plans_dir,
        retention_days,
        listener,
        app,
        cancellation: ct,
    })
    .await
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
                        "harnx-plans-tools: failed to install SIGTERM handler ({e}); \
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
