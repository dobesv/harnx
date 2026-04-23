//! `harnx-acp-server` — standalone ACP agent binary for headless deployments
//! that don't need the TUI or HTTP server. Speaks the ACP protocol over
//! stdin/stdout to a host coordinator (Zed, Superpowers, etc.).
//!
//! All advanced features (session management, macros, model picker, etc.)
//! remain available through the primary `harnx --acp=<name>` path.

use anyhow::{Context, Result};
use clap::Parser;
use harnx_render::render_error;
use harnx_runtime::bootstrap::setup_logger;
use harnx_runtime::config::{load_env_file, Config, WorkingMode};
use parking_lot::RwLock;
use std::sync::Arc;

#[derive(Parser, Debug)]
#[command(author, version, about = "harnx ACP agent server (stdio)", long_about = None)]
struct Cli {
    /// Agent name to serve (must exist in agents/)
    agent: String,
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
        Config::init(
            WorkingMode::Acp(cli.agent.clone()),
            false,
            cli.mcp_root.clone(),
        )
        .await
        .context("Failed to init Config")?,
    ));

    if cli.dry_run {
        config.write().dry_run = true;
    }
    if let Some(model_id) = &cli.model {
        config.write().set_model(model_id)?;
    }

    if let Err(err) = harnx_acp_server::run(config, cli.agent).await {
        render_error(err);
        std::process::exit(1);
    }
    Ok(())
}
