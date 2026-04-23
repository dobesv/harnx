//! `harnx-serve` — standalone HTTP server binary for headless deployments
//! that don't need the TUI. Pulls in a narrower dep graph (no ratatui,
//! crossterm UI, or agent-client-protocol) than the full `harnx` CLI.
//!
//! All advanced features (agents, sessions, macros, dry-run echo, interactive
//! model selection) remain available through the primary `harnx --serve` path.

use anyhow::{Context, Result};
use clap::Parser;
use harnx_render::render_error;
use harnx_runtime::bootstrap::setup_logger;
use harnx_runtime::config::{load_env_file, Config, WorkingMode};
use parking_lot::RwLock;
use std::sync::Arc;

#[derive(Parser, Debug)]
#[command(author, version, about = "harnx HTTP server", long_about = None)]
struct Cli {
    /// Listen address (default from config.yaml or 127.0.0.1:8000)
    #[clap(short = 'a', long, value_name = "ADDRESS")]
    addr: Option<String>,
    /// Select an LLM model
    #[clap(short = 'm', long)]
    model: Option<String>,
    /// Echo prompts instead of sending them to the LLM
    #[clap(long)]
    dry_run: bool,
    /// Add MCP roots (comma-separated)
    #[clap(long, value_name = "PATH", value_delimiter = ',')]
    mcp_root: Vec<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    load_env_file()?;
    let cli = Cli::parse();

    setup_logger(true)?;

    let config = Arc::new(RwLock::new(
        Config::init(WorkingMode::Serve, false, cli.mcp_root.clone())
            .await
            .context("Failed to init Config")?,
    ));

    if cli.dry_run {
        config.write().dry_run = true;
    }
    if let Some(model_id) = &cli.model {
        config.write().set_model(model_id)?;
    }

    if let Err(err) = harnx_serve::run(config, cli.addr).await {
        render_error(err);
        std::process::exit(1);
    }
    Ok(())
}
