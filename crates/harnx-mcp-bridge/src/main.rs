use anyhow::Context;
use harnx_mcp_bridge::{report_tools_filtered, Args, BridgeToolset};
use harnx_nats_common::connect::{NatsConnection, NatsEndpoint};
use harnx_toolset_server::{
    compile_enable_globs, serve_with_config, FilteredToolset, ServeConfig, ServeLifecycle,
};
use std::process::ExitCode;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

#[tokio::main]
async fn main() -> ExitCode {
    // Before spawning the child: its stderr is forwarded to `log::debug!`, which
    // goes nowhere until a logger exists.
    let _ = harnx_core::logging::init(harnx_core::logging::LogSink::Stderr);
    let telemetry = harnx_telemetry::init_telemetry("harnx-mcp-bridge");

    let result = run().await;
    if let Ok(t) = telemetry {
        t.shutdown().await;
    }
    match result {
        Ok(code) => code,
        Err(err) => {
            eprintln!("error: {err:#}");
            ExitCode::from(1)
        }
    }
}

async fn run() -> anyhow::Result<ExitCode> {
    let args = Args::parse();
    // Clap can waive a required argument when it conflicts with another mode.
    anyhow::ensure!(
        args.tool_args.is_none() || args.call_tool.is_some(),
        "--tool-args requires --call-tool"
    );

    harnx_metrics::init(&args.metrics)?;
    let readiness = harnx_healthz::init(&args.healthz).await?;

    // Compile filter globs before starting the bridge
    let filter_set = compile_enable_globs(&args.enable_tool)?;
    let filter: Option<Arc<harnx_toolset_server::globset::GlobSet>> =
        filter_set.as_ref().map(|s| Arc::new(s.clone()));

    if args.list_tools {
        return list_tools(&args, filter_set.as_ref()).await;
    }

    if let Some(tool_name) = &args.call_tool {
        anyhow::ensure!(
            filter_set
                .as_ref()
                .is_none_or(|set| set.is_match(tool_name)),
            "tool '{tool_name}' is excluded by --enable-tool"
        );
        return call_tool_direct(tool_name, &args).await;
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
        result = serve => result?,
        _ = child_died.cancelled() => {
            log::warn!("wrapped MCP child exited; shutting down bridge");
            anyhow::bail!("wrapped MCP child exited")
        }
    };

    Ok(ExitCode::SUCCESS)
}

async fn list_tools(
    args: &Args,
    filter: Option<&harnx_toolset_server::globset::GlobSet>,
) -> anyhow::Result<ExitCode> {
    let name = args.name.as_deref().unwrap_or("mcp-diagnostic");
    let mut bridge = BridgeToolset::new(name, args.child.clone()).await?;
    let report = report_tools_filtered(&bridge, filter);
    bridge.shutdown().await?;
    print!("{report}");
    Ok(ExitCode::SUCCESS)
}

async fn call_tool_direct(tool_name: &str, args: &Args) -> anyhow::Result<ExitCode> {
    let raw = args.tool_args.as_deref().unwrap_or("{}");
    let tool_args: serde_json::Value =
        serde_json::from_str(raw).context("invalid JSON in --tool-args")?;
    anyhow::ensure!(tool_args.is_object(), "--tool-args must be a JSON object");

    let name = args.name.as_deref().unwrap_or("mcp-diagnostic");
    let mut bridge = BridgeToolset::new(name, args.child.clone())
        .await
        .context("failed to connect to MCP child server")?;
    // Always reap after discovery, including unknown tools and protocol failures.
    let result = invoke_direct(&bridge, tool_name, tool_args).await;
    let cleanup = bridge.shutdown().await;
    match (result, cleanup) {
        (Ok(result), Ok(())) => {
            println!("{}", serde_json::to_string_pretty(&result)?);
            anyhow::ensure!(
                result.get("isError").and_then(serde_json::Value::as_bool) != Some(true),
                "tool '{tool_name}' reported isError: true"
            );
            anyhow::ensure!(
                result
                    .get("resultType")
                    .and_then(serde_json::Value::as_str)
                    .is_none_or(|kind| kind == "complete"),
                "tool '{tool_name}' did not return a completed result"
            );
            Ok(ExitCode::SUCCESS)
        }
        (Ok(result), Err(error)) => {
            // Cleanup failure must not discard an otherwise complete tool payload.
            println!("{}", serde_json::to_string_pretty(&result)?);
            Err(error.context("failed to clean up MCP child"))
        }
        (Err(error), Err(cleanup)) => {
            Err(error.context(format!("MCP child cleanup also failed: {cleanup:#}")))
        }
        (Err(error), Ok(())) => Err(error),
    }
}

async fn invoke_direct(
    bridge: &BridgeToolset,
    tool_name: &str,
    arguments: serde_json::Value,
) -> anyhow::Result<serde_json::Value> {
    anyhow::ensure!(
        bridge
            .cached_tools()
            .iter()
            .any(|tool| tool.name == tool_name),
        "tool '{tool_name}' not found in server '{}'",
        bridge.server_name()
    );
    harnx_toolset::Toolset::invoke(bridge, tool_name, arguments, CancellationToken::new())
        .await
        .map_err(anyhow::Error::new)
        .context("tool invocation failed")
}
