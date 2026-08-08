use anyhow::Context;
use harnx_core::instance::{InstanceId, HARNX_INSTANCE_ID};
use harnx_mcp_bridge::{report_tools, Args, BridgeToolset};
use harnx_nats_common::connect::NatsEndpoint;
use harnx_toolset_server::serve_with_client;
use std::sync::Arc;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Before spawning the child: its stderr is forwarded to `log::debug!`, which
    // goes nowhere until a logger exists.
    harnx_core::server_logging::init_server_logger();
    let args = Args::parse();

    if args.list_tools {
        let name = args.name.unwrap_or_else(|| "mcp-diagnostic".to_string());
        let bridge = BridgeToolset::new(name, args.child).await?;
        print!("{}", report_tools(&bridge));
        return Ok(());
    }

    let name = args
        .name
        .context("--name is required when serving over NATS")?;
    let bridge = BridgeToolset::new(name, args.child).await?;
    let child_died = bridge.child_died_token();
    let instance_id = std::env::var(HARNX_INSTANCE_ID).map_err(|_| {
        anyhow::anyhow!(harnx_core::instance::missing_scope_message(
            harnx_core::instance::StandaloneMode::ListTools
        ))
    })?;
    let client = NatsEndpoint::from_env()?.connect().await?;

    tokio::select! {
        result = serve_with_client(Arc::new(bridge), InstanceId::from_string(instance_id), client) => result,
        _ = child_died.cancelled() => {
            log::warn!("wrapped MCP child exited; shutting down bridge");
            anyhow::bail!("wrapped MCP child exited")
        }
    }
}
