use anyhow::Context;
use harnx_core::instance::{InstanceId, HARNX_INSTANCE_ID};
use harnx_mcp_bridge::{Args, BridgeToolset};
use harnx_toolset_server::serve_over_nats;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Before spawning the child: its stderr is forwarded to `log::debug!`, which
    // goes nowhere until a logger exists.
    harnx_core::server_logging::init_server_logger();
    let args = Args::parse();
    let bridge = BridgeToolset::new(args.name, args.child).await?;
    let child_died = bridge.child_died_token();
    let instance_id = std::env::var(HARNX_INSTANCE_ID)
        .with_context(|| format!("{HARNX_INSTANCE_ID} is required"))?;
    let nats_url = std::env::var("HARNX_NATS_URL").context("HARNX_NATS_URL is required")?;
    let token = std::env::var("HARNX_NATS_TOKEN").context("HARNX_NATS_TOKEN is required")?;

    tokio::select! {
        result = serve_over_nats(bridge, InstanceId::from_string(instance_id), &nats_url, &token) => result,
        _ = child_died.cancelled() => {
            log::warn!("wrapped MCP child exited; shutting down bridge");
            anyhow::bail!("wrapped MCP child exited")
        }
    }
}
