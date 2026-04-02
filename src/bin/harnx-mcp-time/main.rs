mod server;

use rmcp::ServiceExt;
use server::TimeServer;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    eprintln!(
        "harnx-mcp-time v{}: starting",
        env!("CARGO_PKG_VERSION"),
    );

    let server = TimeServer::new();
    let transport = rmcp::transport::stdio();
    let service = server.serve(transport).await?;
    service.waiting().await?;

    Ok(())
}
