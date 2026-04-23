mod server;

use rmcp::ServiceExt;
use server::Repro249Server;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let server = Repro249Server;
    let transport = rmcp::transport::stdio();
    let service = server.serve(transport).await?;
    service.waiting().await?;
    Ok(())
}
