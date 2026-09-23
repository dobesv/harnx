//! Binary entry point for harnx-acp-server.
//!
//! Usage: harnx-acp-server --agent <name>
//!
//! All protocol output goes to stdout; all logs go to stderr.

use std::io::IsTerminal;

use anyhow::Result;
use clap::Parser;
use tracing_subscriber::EnvFilter;

/// harnx ACP server — exposes harnx agents via Agent Client Protocol.
#[derive(Parser, Debug)]
#[command(name = "harnx-acp-server", version, about)]
struct Args {
    /// Agent name to expose over ACP.
    #[arg(short, long, default_value = "default")]
    agent: String,

    /// Log level (trace, debug, info, warn, error).
    #[arg(short, long, default_value = "info")]
    log_level: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    // Initialize tracing to stderr only. Never use stdout for logs.
    let env_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(&args.log_level));

    // Only show fancy formatting in a terminal; use JSON otherwise.
    let use_ansi = std::io::stderr().is_terminal();
    tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .with_writer(std::io::stderr)
        .with_ansi(use_ansi)
        .with_target(false)
        .with_level(false)
        .init();

    tracing::info!("starting harnx-acp-server for agent {}", args.agent);

    harnx_acp_server::run(args.agent).await
}
