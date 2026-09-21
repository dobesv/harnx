//! `harnx-serve` — standalone HTTP server binary for headless deployments
//! that don't need the TUI. Pulls in a narrower dep graph (no ratatui,
//! crossterm UI, or agent-client-protocol) than the full `harnx` CLI.
//!
//! All advanced features (agents, sessions, macros, dry-run echo, interactive
//! model selection) remain available through this `harnx-serve` binary.

use anyhow::{Context, Result};
use clap::Parser;
use harnx_core::agent_config::collect_agent_variables;
use harnx_core::logging::LogSink;
use harnx_render::render_error;
use harnx_runtime::bootstrap::setup_logger;
use harnx_runtime::config::{load_env_file, Config, WorkingMode};
use parking_lot::RwLock;
use std::{path::PathBuf, sync::Arc, time::Duration};

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
    /// Directory of web-ui static assets to serve
    /// (default: ~/.local/share/harnx/web-assets, XDG-aware)
    #[clap(long, value_name = "PATH", env = "HARNX_WEB_ASSETS")]
    web_assets: Option<PathBuf>,
    /// Maximum seconds to wait for client HTTP connections during shutdown
    #[clap(long, value_name = "SECONDS", default_value_t = 25)]
    drain_timeout_seconds: u64,
    /// Minimum per-stream shutdown jitter in milliseconds
    #[clap(long, value_name = "MILLISECONDS", default_value_t = 1_000)]
    stream_drain_min_jitter_ms: u64,
    /// Maximum per-stream shutdown jitter in milliseconds
    #[clap(long, value_name = "MILLISECONDS", default_value_t = 20_000)]
    stream_drain_max_jitter_ms: u64,
    /// Set agent variable pairs (format: --agent-variable key value or -x key value); can be repeated
    #[clap(short = 'x', long, value_names = ["KEY", "VALUE"], num_args = 2, action = clap::ArgAction::Append)]
    pub agent_variable: Vec<String>,
    #[command(flatten)]
    metrics: harnx_metrics::MetricsFlags,
    #[command(flatten)]
    healthz: harnx_healthz::HealthzFlags,
}

#[tokio::main]
async fn main() -> Result<()> {
    load_env_file()?;
    let cli = Cli::parse();

    setup_logger(LogSink::Stderr)?;
    let telemetry = harnx_telemetry::init_telemetry("harnx-serve")?;

    let result = run(cli).await;
    telemetry.shutdown().await;
    if let Some(err) = result? {
        render_error(err);
        std::process::exit(1);
    }
    Ok(())
}

async fn run(cli: Cli) -> Result<Option<anyhow::Error>> {
    harnx_metrics::init(&cli.metrics)?;

    let readiness = harnx_healthz::init(&cli.healthz).await?;
    let mut config = Config::init_headless(WorkingMode::Serve, false)
        .await
        .context("Failed to init Config")?;
    config.apply_frontend_nats_routing();
    let config = Arc::new(RwLock::new(config));

    if cli.dry_run {
        config.write().dry_run = true;
    }
    if let Some(model_id) = &cli.model {
        config.write().set_model(model_id)?;
    }
    config.write().agent_variables = collect_agent_variables(&cli.agent_variable)?;

    let stream_drain = harnx_serve::StreamDrainConfig::new(
        Duration::from_millis(cli.stream_drain_min_jitter_ms),
        Duration::from_millis(cli.stream_drain_max_jitter_ms),
    )?;
    Ok(harnx_serve::run_with_shutdown_config(
        config,
        cli.addr,
        cli.web_assets,
        readiness,
        Duration::from_secs(cli.drain_timeout_seconds),
        stream_drain,
    )
    .await
    .err())
}

#[cfg(test)]
mod tests {
    use super::Cli;
    use clap::Parser;

    #[test]
    fn drain_timeout_defaults_to_25_seconds_and_is_configurable() {
        let default_cli = Cli::try_parse_from(["harnx-serve"]).unwrap();
        assert_eq!(default_cli.drain_timeout_seconds, 25);
        assert_eq!(default_cli.stream_drain_min_jitter_ms, 1_000);
        assert_eq!(default_cli.stream_drain_max_jitter_ms, 20_000);

        let configured = Cli::try_parse_from([
            "harnx-serve",
            "--drain-timeout-seconds",
            "7",
            "--stream-drain-min-jitter-ms",
            "10",
            "--stream-drain-max-jitter-ms",
            "50",
        ])
        .unwrap();
        assert_eq!(configured.drain_timeout_seconds, 7);
        assert_eq!(configured.stream_drain_min_jitter_ms, 10);
        assert_eq!(configured.stream_drain_max_jitter_ms, 50);
    }

    #[test]
    fn parses_agent_variables_flag() {
        let cli = Cli::try_parse_from([
            "harnx-serve",
            "--agent-variable",
            "cloud_env",
            "true",
            "--agent-variable",
            "debug",
            "false",
        ])
        .unwrap();
        assert_eq!(
            cli.agent_variable,
            vec!["cloud_env", "true", "debug", "false"]
        );
    }
}
