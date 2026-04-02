//! harnx-mcp-fs: High-performance MCP filesystem server.
//!
//! Provides read_file, write_file, edit_file, list_directory, search_files,
//! and find_files tools over the Model Context Protocol (MCP) via stdio transport.
//!
//! Supports the MCP roots feature: roots can be set via CLI flags and are
//! dynamically updated when the client sends roots/list_changed notifications.
//!
//! Usage:
//!   harnx-mcp-fs [--root <path>]...
//!
//! If no roots are specified (via CLI or MCP client), all operations are denied.

mod server;

use rmcp::ServiceExt;
use server::FsServer;
use std::path::PathBuf;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let roots = parse_args();

    eprintln!(
        "harnx-mcp-fs v{}: starting ({} root{})",
        env!("CARGO_PKG_VERSION"),
        if roots.is_empty() {
            "no CLI roots, awaiting client roots".to_string()
        } else {
            roots.len().to_string()
        },
        if roots.len() == 1 { "" } else { "s" }
    );
    for root in &roots {
        eprintln!("  root: {}", root.display());
    }

    let server = FsServer::new(roots);
    let transport = rmcp::transport::stdio();
    let service = server.serve(transport).await?;
    service.waiting().await?;

    Ok(())
}

/// Parse CLI arguments. Supports `--root <path>` (repeatable) and `--help`.
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
                        match path.canonicalize() {
                            Ok(canonical) => roots.push(canonical),
                            Err(err) => {
                                eprintln!(
                                    "warning: failed to canonicalize root '{}': {}",
                                    raw, err
                                );
                            }
                        }
                    } else {
                        eprintln!("harnx-mcp-fs: warning: root path does not exist: {}", raw);
                    }
                    i += 2;
                } else {
                    eprintln!("harnx-mcp-fs: --root requires a path argument");
                    std::process::exit(1);
                }
            }
            "--help" | "-h" => {
                eprintln!("harnx-mcp-fs: High-performance MCP filesystem server");
                eprintln!();
                eprintln!("Usage: harnx-mcp-fs [OPTIONS]");
                eprintln!();
                eprintln!("Options:");
                eprintln!("  --root, -r <path>  Add an allowed root directory (repeatable)");
                eprintln!("  --help, -h         Show this help message");
                eprintln!();
                eprintln!("The server communicates via stdio using the MCP protocol.");
                eprintln!("If no roots are specified, operations are denied until the client provides roots.");
                eprintln!("Roots can also be provided dynamically by the MCP client.");
                std::process::exit(0);
            }
            other => {
                eprintln!("harnx-mcp-fs: unknown argument: {}", other);
                eprintln!("Try: harnx-mcp-fs --help");
                std::process::exit(1);
            }
        }
    }

    roots
}
