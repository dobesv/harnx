mod cli;
mod server;

use rmcp::ServiceExt;
use server::{HooksProxyConfig, HooksProxyServer};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let (hooks_config, child_cmd_args) = cli::parse_args()?;

    if child_cmd_args.is_empty() {
        anyhow::bail!("no child command specified");
    }
    let child_command = child_cmd_args[0].clone();
    let child_args = child_cmd_args[1..].to_vec();

    let config = HooksProxyConfig {
        hooks: hooks_config,
        child_command,
        child_args,
        session_id: uuid::Uuid::new_v4().to_string(),
        cwd: std::env::current_dir()?,
    };

    eprintln!(
        "harnx-mcp-hooks-proxy v{}: starting ...",
        env!("CARGO_PKG_VERSION")
    );

    let server = HooksProxyServer::new(config);
    let transport = rmcp::transport::stdio();
    let service = server.serve(transport).await?;
    service.waiting().await?;

    Ok(())
}
