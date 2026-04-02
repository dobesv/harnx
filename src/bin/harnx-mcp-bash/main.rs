mod server;

use rmcp::ServiceExt;
use server::BashServer;
use std::path::PathBuf;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let roots = parse_args();

    eprintln!(
        "harnx-mcp-bash v{}: starting ({} root{})",
        env!("CARGO_PKG_VERSION"),
        if roots.is_empty() {
            "unrestricted".to_string()
        } else {
            roots.len().to_string()
        },
        if roots.len() == 1 { "" } else { "s" }
    );
    for root in &roots {
        eprintln!("  root: {}", root.display());
    }

    let server = BashServer::new(roots);
    let transport = rmcp::transport::stdio();
    let service = server.serve(transport).await?;
    service.waiting().await?;

    Ok(())
}

fn parse_args() -> Vec<PathBuf> {
    let args: Vec<String> = std::env::args().collect();
    let mut roots = Vec::new();
    let mut i = 1;

    while i < args.len() {
        match args[i].as_str() {
            "--root" | "-r" => {
                if i + 1 < args.len() {
                    let raw = &args[i + 1];
                    let path = PathBuf::from(raw);
                    if path.exists() {
                        roots.push(path.canonicalize().unwrap_or(path));
                    } else {
                        eprintln!("harnx-mcp-bash: warning: root path does not exist: {}", raw);
                    }
                    i += 2;
                } else {
                    eprintln!("harnx-mcp-bash: --root requires a path argument");
                    std::process::exit(1);
                }
            }
            "--help" | "-h" => {
                eprintln!("harnx-mcp-bash: MCP shell command server");
                eprintln!();
                eprintln!("Usage: harnx-mcp-bash [OPTIONS]");
                eprintln!();
                eprintln!("Options:");
                eprintln!("  --root, -r <path>  Add an allowed root directory (repeatable)");
                eprintln!("  --help, -h         Show this help message");
                eprintln!();
                eprintln!("The server communicates via stdio using the MCP protocol.");
                eprintln!("If no roots are specified, all filesystem paths are accessible.");
                eprintln!("Roots can also be provided dynamically by the MCP client.");
                std::process::exit(0);
            }
            other => {
                eprintln!("harnx-mcp-bash: unknown argument: {}", other);
                eprintln!("Try: harnx-mcp-bash --help");
                std::process::exit(1);
            }
        }
    }

    roots
}
