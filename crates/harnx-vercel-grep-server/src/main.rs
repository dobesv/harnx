use anyhow::Context;
use harnx_vercel_grep_server::server::GrepServer;
use rmcp::ServiceExt;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    if parse_args()? {
        print_help();
        return Ok(());
    }

    eprintln!(
        "harnx-vercel-grep-server v{}: starting",
        env!("CARGO_PKG_VERSION")
    );

    let server = GrepServer::new();
    let service = server
        .serve(rmcp::transport::stdio())
        .await
        .context("harnx-vercel-grep-server: failed to start stdio service")?;
    service
        .waiting()
        .await
        .context("harnx-vercel-grep-server: stdio service failed")?;

    Ok(())
}

fn parse_args() -> anyhow::Result<bool> {
    let mut help = false;

    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--help" | "-h" => help = true,
            _ => anyhow::bail!("harnx-vercel-grep-server: unknown argument: {arg}"),
        }
    }

    Ok(help)
}

fn print_help() {
    eprintln!("harnx-vercel-grep-server - GitHub code search MCP server");
    eprintln!();
    eprintln!("Usage: harnx-vercel-grep-server [--help]");
    eprintln!();
    eprintln!("Options:");
    eprintln!("  --help, -h          Show this help message");
}
