use anyhow::Context;
use harnx_core::instance::{ServerScope, HARNX_SERVER_SCOPE};
use harnx_mcp_bridge::{report_tools, Args, BridgeToolset};
use harnx_nats_common::connect::{NatsConnection, NatsEndpoint};
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
    let instance_id = std::env::var(HARNX_SERVER_SCOPE).map_err(|_| {
        anyhow::anyhow!(harnx_core::instance::missing_scope_message(
            harnx_core::instance::StandaloneMode::ListTools
        ))
    })?;
    // Keep the connect attempt inside the same race as `serve_with_client`:
    // a slow/unreachable NATS cluster (bad DNS, stalled TLS handshake) must
    // not block the bridge from noticing the wrapped child has already died.
    let serve = async {
        let endpoint = NatsEndpoint::from_env()?;
        let client = endpoint.connect().await?;
        let connection = NatsConnection {
            client,
            replicas: endpoint.resolved_replicas(),
        };
        serve_with_client(
            Arc::new(bridge),
            ServerScope::from_string(instance_id),
            connection,
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
