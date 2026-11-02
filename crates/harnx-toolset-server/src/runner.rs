//! CLI startup and transport selection for standalone toolset servers.

use crate::mcp::McpToolsetAdapter;
use crate::{
    compile_enable_globs, enable_tools_from_args, serve_configured, toolset_name_from_args,
    ArcToolsetWrapper, FilteredToolset, NamedToolset, RegistrationIdentity, ServeLifecycle,
    ServeSettings,
};
use anyhow::{Context, Result};
use harnx_core::instance::ServerScope;
use harnx_healthz::Readiness;
use harnx_nats_common::connect::{NatsConnection, NatsEndpoint};
use harnx_toolset::Toolset;
use rmcp::transport::streamable_http_server::session::never::NeverSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::ServiceExt;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// Per-server HTTP limits. Defaults preserve RMCP's request body limit.
#[derive(Debug, Clone, Copy, Default)]
pub struct McpHttpOptions {
    pub max_request_body_bytes: Option<usize>,
}

impl McpHttpOptions {
    fn apply(self, config: StreamableHttpServerConfig) -> StreamableHttpServerConfig {
        match self.max_request_body_bytes {
            Some(bytes) => config.with_max_request_body_bytes(bytes),
            None => config,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum McpMode {
    Stdio,
    Http,
}

pub(crate) fn resolve_mcp_mode(
    args: impl IntoIterator<Item = impl AsRef<std::ffi::OsStr>>,
) -> Result<Option<McpMode>> {
    let mut mcp_stdio = false;
    let mut mcp_http = false;
    for arg in args {
        let arg = arg.as_ref();
        if arg == "--mcp-stdio" {
            mcp_stdio = true;
        } else if arg == "--mcp-http" {
            mcp_http = true;
        }
    }
    if mcp_stdio && mcp_http {
        anyhow::bail!("choose one of --mcp-stdio or --mcp-http, not both");
    }
    if mcp_stdio {
        Ok(Some(McpMode::Stdio))
    } else if mcp_http {
        Ok(Some(McpMode::Http))
    } else {
        Ok(None)
    }
}

fn mcp_http_arg_value(args: &[String], flag: &str) -> Result<Option<String>> {
    let assignment = format!("{flag}=");
    for (index, arg) in args.iter().enumerate() {
        if arg == flag {
            let value = args
                .get(index + 1)
                .with_context(|| format!("missing value for {flag}"))?;
            return Ok(Some(value.clone()));
        }
        if let Some(value) = arg.strip_prefix(&assignment) {
            return Ok(Some(value.to_string()));
        }
    }
    Ok(None)
}

/// Serve a toolset over MCP Streamable HTTP at `/mcp`.
pub async fn run_toolset_mcp_http(
    toolset: Arc<dyn Toolset>,
    host: String,
    port: u16,
    readiness: Option<Readiness>,
) -> Result<()> {
    serve_mcp_http(toolset, (host, port), readiness, McpHttpOptions::default()).await
}

async fn serve_mcp_http(
    toolset: Arc<dyn Toolset>,
    (host, port): (String, u16),
    readiness: Option<Readiness>,
    options: McpHttpOptions,
) -> Result<()> {
    let ct = CancellationToken::new();
    let adapter = McpToolsetAdapter { toolset };
    let config = options.apply(
        StreamableHttpServerConfig::default()
            .with_legacy_session_mode(false)
            .with_json_response(true)
            .with_cancellation_token(ct.child_token())
            .disable_allowed_hosts(),
    );
    let service = StreamableHttpService::new(
        move || Ok(adapter.clone()),
        Arc::new(NeverSessionManager::default()),
        config,
    );
    let app = axum::Router::new()
        .nest_service("/mcp", service)
        .layer(axum::middleware::from_fn(
            harnx_metrics::http_metrics_middleware,
        ));
    let listener = tokio::net::TcpListener::bind((host.as_str(), port))
        .await
        .with_context(|| format!("failed to bind {host}:{port}"))?;
    if let Some(readiness) = readiness.as_ref() {
        readiness.ready();
    }

    let shutdown_ct = ct.clone();
    tokio::spawn(async move {
        harnx_nats_common::shutdown::shutdown_signal().await;
        if let Some(readiness) = readiness.as_ref() {
            readiness.not_ready();
        }
        shutdown_ct.cancel();
    });

    log::info!("serving MCP HTTP at http://{host}:{port}/mcp");
    axum::serve(listener, app)
        .with_graceful_shutdown(async move { ct.cancelled().await })
        .await
        .context("run MCP HTTP server")
}

fn print_toolset_help() {
    eprintln!("Options:");
    eprintln!("  --mcp-stdio               Use MCP stdio transport instead of NATS");
    eprintln!("  --mcp-http                Use MCP Streamable HTTP transport instead of NATS");
    eprintln!("  --host <HOST>             MCP HTTP bind host (default: 0.0.0.0)");
    eprintln!("  --port <PORT>             MCP HTTP bind port (toolset-specific default)");
    eprintln!("  --name <NAME>             Override the registered toolset name");
    eprintln!("  --enable-tool <glob>      Enable only tools matching the glob pattern.");
    eprintln!("                            Repeatable. If set, only enabled tools are");
    eprintln!("                            registered and invocable on this server.");
    eprintln!("  --metrics-addr <ADDR>     Serve Prometheus metrics at http://ADDR/metrics.");
    eprintln!("                            Blank host binds 0.0.0.0, e.g. :8456. Unset disables.");
    eprintln!("                            Also honors HARNX_METRICS_ADDR env.");
    eprintln!("  --healthz-addr <ADDR>     Serve readiness checks at http://ADDR/healthz.");
    eprintln!("                            Blank host binds 0.0.0.0, e.g. :8457. Unset disables.");
    eprintln!("                            Also honors HARNX_HEALTHZ_ADDR env.");
    eprintln!("  --help, -h                Show this help message");
}

fn wrap_toolset_if_filtered(
    toolset: Arc<dyn Toolset>,
    filter_set: Option<globset::GlobSet>,
) -> (Arc<dyn Toolset>, Option<Arc<globset::GlobSet>>) {
    match filter_set {
        Some(set) => {
            let filter = Arc::new(set.clone());
            (
                Arc::new(FilteredToolset::new(ArcToolsetWrapper(toolset), set)),
                Some(filter),
            )
        }
        None => (toolset, None),
    }
}

struct TransportOptions {
    filter: Option<globset::GlobSet>,
    http: McpHttpOptions,
}

enum PreparedTransport {
    Mcp(McpMode),
    Nats {
        instance_id: ServerScope,
        connection: NatsConnection,
    },
}

impl PreparedTransport {
    async fn connect(mode: Option<McpMode>) -> Result<Self> {
        if let Some(mode) = mode {
            return Ok(Self::Mcp(mode));
        }
        let instance_id =
            harnx_core::instance::scope_from_env(harnx_core::instance::StandaloneMode::McpStdio)?;
        log::info!("serving under scope '{}'", instance_id.as_str());
        let endpoint = NatsEndpoint::from_env()?;
        let client = endpoint.connect().await?;
        Ok(Self::Nats {
            instance_id,
            connection: NatsConnection {
                client,
                replicas: endpoint.resolved_replicas(),
            },
        })
    }

    fn nats_client(&self) -> Option<async_nats::Client> {
        match self {
            Self::Mcp(_) => None,
            Self::Nats { connection, .. } => Some(connection.client.clone()),
        }
    }

    async fn serve(
        self,
        toolset: Arc<dyn Toolset>,
        readiness: Option<Readiness>,
        options: TransportOptions,
    ) -> Result<()> {
        let (toolset, filter) = wrap_toolset_if_filtered(toolset, options.filter);
        match self {
            Self::Mcp(McpMode::Stdio) => run_mcp_stdio(toolset, readiness).await,
            Self::Mcp(McpMode::Http) => {
                run_mcp_http_from_args(toolset, readiness, options.http).await
            }
            Self::Nats {
                instance_id,
                connection,
            } => {
                let shutdown = harnx_nats_common::shutdown::cancel_token_on_shutdown_signal();
                serve_configured(
                    toolset,
                    ServeSettings {
                        instance_id,
                        connection,
                        lifecycle: ServeLifecycle::new(shutdown, readiness),
                        identity: RegistrationIdentity::from_env(),
                        started: None,
                        filter,
                    },
                )
                .await
            }
        }
    }
}

async fn prepare_toolset<T, F>(mode: Option<McpMode>, factory: F) -> Result<(T, PreparedTransport)>
where
    F: FnOnce(Option<async_nats::Client>) -> Result<T>,
{
    let transport = PreparedTransport::connect(mode).await?;
    let toolset = factory(transport.nats_client())?;
    Ok((toolset, transport))
}

async fn run_mcp_stdio(toolset: Arc<dyn Toolset>, readiness: Option<Readiness>) -> Result<()> {
    let service = McpToolsetAdapter { toolset }
        .serve(rmcp::transport::stdio())
        .await
        .context("start MCP stdio server")?;
    if let Some(readiness) = readiness.as_ref() {
        readiness.ready();
    }
    let outcome = service.waiting().await.context("run MCP stdio server");
    if let Some(readiness) = readiness.as_ref() {
        readiness.not_ready();
    }
    outcome?;
    Ok(())
}

async fn run_mcp_http_from_args(
    toolset: Arc<dyn Toolset>,
    readiness: Option<Readiness>,
    options: McpHttpOptions,
) -> Result<()> {
    // Front parsers validate and skip these flags; the shared runner reads them.
    let args = std::env::args().collect::<Vec<_>>();
    let host = mcp_http_arg_value(&args, "--host")?.unwrap_or_else(|| "0.0.0.0".to_string());
    let port = match mcp_http_arg_value(&args, "--port")? {
        Some(port) => port
            .parse::<u16>()
            .with_context(|| format!("invalid --port value '{port}'"))?,
        None => toolset.default_mcp_http_port(),
    };
    serve_mcp_http(toolset, (host, port), readiness, options).await
}

/// Run an already constructed toolset. See [`run_toolset_main_with_factory`]
/// when construction needs the hosting NATS connection.
pub async fn run_toolset_main<T>(toolset: T) -> Result<()>
where
    T: Toolset + 'static,
{
    run_toolset_main_with_factory(|_| Ok(toolset)).await
}

/// Construct and host a toolset over NATS, MCP stdio, or MCP HTTP.
///
/// The factory runs once, after transport selection. In NATS mode it receives
/// a clone of the hosting client (the same connection, not a new connection).
/// MCP modes pass None and never connect to NATS. Help exits before construction.
/// The factory may create a JetStream context with `async_nats::jetstream::new`.
/// NATS mode handles SIGTERM/Ctrl+C with graceful registration shutdown.
///
/// **Strict front-parser binaries:** `harnx-{bash,fs,grep,exa,fetch,time,attachment}-tools`
/// reject unknown flags before delegating here. New shared flags must be accepted
/// by each front parser in both `--flag VALUE` and `--flag=VALUE` forms. Match
/// exact names, not prefixes, so typos such as `--flag-typo` remain errors.
pub async fn run_toolset_main_with_factory<T, F>(factory: F) -> Result<()>
where
    T: Toolset + 'static,
    F: FnOnce(Option<async_nats::Client>) -> Result<T>,
{
    run_toolset_main_with_factory_and_http_options(factory, McpHttpOptions::default()).await
}

/// Like [`run_toolset_main_with_factory`], with per-server MCP HTTP limits.
/// Options have no effect in NATS or stdio mode.
pub async fn run_toolset_main_with_factory_and_http_options<T, F>(
    factory: F,
    http_options: McpHttpOptions,
) -> Result<()>
where
    T: Toolset + 'static,
    F: FnOnce(Option<async_nats::Client>) -> Result<T>,
{
    let _ = harnx_core::logging::init(harnx_core::logging::LogSink::Stderr);
    let args = std::env::args_os().collect::<Vec<_>>();
    let name = toolset_name_from_args(&args)?;
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        print_toolset_help();
        return Ok(());
    }
    let filter_set = compile_enable_globs(&enable_tools_from_args(&args)?)?;
    let mode = resolve_mcp_mode(&args)?;
    let (toolset, transport) = prepare_toolset(mode, factory).await?;
    let toolset: Arc<dyn Toolset> = match name {
        Some(name) => Arc::new(NamedToolset::new(toolset, name)?),
        None => Arc::new(toolset),
    };
    run_prepared_toolset(toolset, transport, filter_set, http_options).await
}

async fn run_prepared_toolset(
    toolset: Arc<dyn Toolset>,
    transport: PreparedTransport,
    filter_set: Option<globset::GlobSet>,
    http_options: McpHttpOptions,
) -> Result<()> {
    let metrics_addr = harnx_metrics::metrics_addr_from_args(
        std::env::args_os().map(|arg| arg.to_string_lossy().into_owned()),
    )
    .or_else(|| std::env::var("HARNX_METRICS_ADDR").ok());
    harnx_metrics::init(&harnx_metrics::MetricsFlags { metrics_addr })?;

    let healthz_addr = harnx_healthz::healthz_addr_from_args(std::env::args())
        .or_else(|| std::env::var("HARNX_HEALTHZ_ADDR").ok());
    let readiness = harnx_healthz::init(&harnx_healthz::HealthzFlags { healthz_addr }).await?;
    let service_name = format!("harnx-{}-server", toolset.name());
    let telemetry = harnx_telemetry::init_telemetry(&service_name)?;

    let result = transport
        .serve(
            toolset,
            readiness,
            TransportOptions {
                filter: filter_set,
                http: http_options,
            },
        )
        .await;
    telemetry.shutdown().await;
    result
}

#[cfg(test)]
mod tests;
