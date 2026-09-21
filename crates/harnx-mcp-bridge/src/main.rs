use anyhow::Context;
use harnx_mcp_bridge::{report_tools_filtered, Args, BridgeToolset};
use harnx_nats_common::connect::{NatsConnection, NatsEndpoint};
use harnx_toolset_server::{
    compile_enable_globs, serve_with_config, FilteredToolset, ServeConfig, ServeLifecycle,
};
use std::sync::Arc;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Before spawning the child: its stderr is forwarded to `log::debug!`, which
    // goes nowhere until a logger exists.
    let _ = harnx_core::logging::init(harnx_core::logging::LogSink::Stderr);
    let telemetry = harnx_telemetry::init_telemetry("harnx-mcp-bridge")?;

    let result = run().await;
    telemetry.shutdown().await;
    result
}

async fn run() -> anyhow::Result<()> {
    let args = Args::parse();

    harnx_metrics::init(&args.metrics)?;
    let readiness = harnx_healthz::init(&args.healthz).await?;

    // Compile filter globs before starting the bridge
    let filter_set = compile_enable_globs(&args.enable_tool)?;
    let filter: Option<Arc<harnx_toolset_server::globset::GlobSet>> =
        filter_set.as_ref().map(|s| Arc::new(s.clone()));

    if args.list_tools {
        let name = args.name.unwrap_or_else(|| "mcp-diagnostic".to_string());
        let bridge = BridgeToolset::new(name, args.child).await?;
        print!("{}", report_tools_filtered(&bridge, filter_set.as_ref()));
        return Ok(());
    }

    let name = args
        .name
        .context("--name is required when serving over NATS")?;
    let bridge = BridgeToolset::new(name, args.child).await?;
    let child_died = bridge.child_died_token();
    let scope =
        harnx_core::instance::scope_from_env(harnx_core::instance::StandaloneMode::ListTools)?;
    log::info!("serving under scope '{}'", scope.as_str());
    // SIGTERM/Ctrl+C get a chance to deregister before the process exits, the
    // same as the toolset/hookset binaries this bridge otherwise mirrors: an
    // independently deployed bridge pod has no parent supervisor to clean up
    // after it, and Kubernetes terminates pods with SIGTERM.
    let shutdown = harnx_nats_common::shutdown::cancel_token_on_shutdown_signal();

    // Apply filter to bridge toolset if specified
    let toolset: Arc<dyn harnx_toolset::Toolset> = match &filter_set {
        Some(set) => Arc::new(FilteredToolset::new(bridge, set.clone())),
        None => Arc::new(bridge),
    };

    // Keep the connect attempt inside the same race as the signal above:
    // a slow/unreachable NATS cluster (bad DNS, stalled TLS handshake) must
    // not block the bridge from noticing the wrapped child has already died.
    let serve = async {
        let endpoint = NatsEndpoint::from_env()?;
        let client = endpoint.connect().await?;
        let connection = NatsConnection {
            client,
            replicas: endpoint.resolved_replicas(),
        };
        serve_with_config(
            toolset,
            ServeConfig {
                instance_id: scope,
                connection,
                lifecycle: ServeLifecycle::new(shutdown, readiness),
                filter,
            },
        )
        .await
    };

    tokio::select! {
        result = serve => result,
        _ = child_died.cancelled() => {
            log::warn!("wrapped MCP child exited; shutting down bridge");
            anyhow::bail!("wrapped MCP child exited")
        }
    }
}
