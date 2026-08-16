//! `harnx-worker` — NATS worker daemon that executes agent turns.
//!
//! Front-ends (`harnx`, `harnx-serve`) publish session activations to a NATS
//! cluster; a worker leases the session and runs the agent loop. Shipping the
//! worker as its own binary keeps it out of the front-end's dep graph and lets
//! deployments run workers without the TUI.

use anyhow::Result;
use clap::Parser;
use harnx_core::agent_config::collect_agent_variables;
use harnx_core::logging::LogSink;
use harnx_runtime::bootstrap::setup_logger;
use harnx_runtime::config::{load_env_file, Config, WorkingMode};
use parking_lot::RwLock;
use std::sync::Arc;

/// Heap-usage guard installed as the process allocator: aborts with a backtrace
/// if live heap exceeds `HARNX_HEAP_LIMIT_MB`. Disarmed (plain passthrough to
/// the system allocator) when that env var is unset.
#[global_allocator]
static GLOBAL_ALLOC: harnx_core::alloc_guard::HeapGuard = harnx_core::alloc_guard::HeapGuard;

#[derive(Parser, Debug, PartialEq, Eq)]
#[command(
    author,
    version,
    about = "Run a worker daemon for a configured or shared-local NATS cluster",
    long_about = None
)]
struct Cli {
    /// Cluster key from nats_servers/<name>.yaml, or __local__ with
    /// HARNX_NATS_URL and HARNX_NATS_TOKEN handoff
    #[arg(long)]
    cluster: String,
    /// Stable worker identity for leases and the durable consumer name.
    /// Defaults to a generated id if omitted.
    #[arg(long)]
    worker_id: Option<String>,
    /// Set agent variable pairs (format: --agent-variable key value or -x key value); can be repeated
    #[arg(short = 'x', long, value_names = ["KEY", "VALUE"], num_args = 2, action = clap::ArgAction::Append)]
    agent_variable: Vec<String>,
    /// Start this worker's tool servers, report which ones registered, and
    /// exit without serving sessions.
    #[arg(long)]
    diagnose: bool,
    /// Launch this worker's tool and hook servers as child processes instead of
    /// discovering independently deployed ones. Local runs set this; cloud
    /// deployments leave it off and supply HARNX_SERVER_SCOPE.
    #[arg(long)]
    manage_servers: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    load_env_file()?;
    let cli = Cli::parse();
    setup_logger(LogSink::Stderr)?;
    harnx_core::alloc_guard::init_from_env();

    let config = Arc::new(RwLock::new(Config::init(WorkingMode::Cmd, true).await?));
    config.write().agent_variables = collect_agent_variables(&cli.agent_variable)?;
    if cli.diagnose {
        print!(
            "{}",
            harnx_runtime::nats_worker::diagnose_tool_servers(&config).await?
        );
        return Ok(());
    }

    let worker_id = cli
        .worker_id
        .unwrap_or_else(harnx_runtime::nats_worker::new_remote_session_id);
    let daemon = if cli.manage_servers {
        harnx_runtime::nats_worker::WorkerDaemonConfig::managing(cli.cluster, worker_id)
    } else {
        harnx_runtime::nats_worker::WorkerDaemonConfig::new(cli.cluster, worker_id)
    };
    // `None` selects the agent loop's default call path
    // (`call_with_retry_and_fallback`), which is what the worker wants.
    harnx_runtime::nats_worker::run_worker_daemon(config, daemon, None).await
}

#[cfg(test)]
mod tests {
    use super::Cli;
    use clap::Parser;

    #[test]
    fn parses_agent_variables() {
        let cli = Cli::try_parse_from([
            "harnx-worker",
            "--cluster",
            "prod",
            "--agent-variable",
            "cloud_env",
            "true",
            "--agent-variable",
            "debug",
            "false",
        ])
        .unwrap();
        assert_eq!(cli.cluster, "prod");
        assert_eq!(cli.worker_id, None);
        assert_eq!(
            cli.agent_variable,
            vec!["cloud_env", "true", "debug", "false"]
        );
        assert!(!cli.diagnose);
    }

    #[test]
    fn requires_cluster() {
        assert!(Cli::try_parse_from(["harnx-worker"]).is_err());
    }

    #[test]
    fn parses_worker_id_and_diagnose() {
        let cli = Cli::try_parse_from([
            "harnx-worker",
            "--cluster",
            "__local__",
            "--worker-id",
            "local",
            "--diagnose",
        ])
        .unwrap();
        assert_eq!(cli.worker_id.as_deref(), Some("local"));
        assert!(cli.diagnose);
    }

    #[test]
    fn manage_servers_defaults_to_off_and_is_settable() {
        let cli = Cli::try_parse_from(["harnx-worker", "--cluster", "prod"]).unwrap();
        assert!(!cli.manage_servers);

        let cli =
            Cli::try_parse_from(["harnx-worker", "--cluster", "prod", "--manage-servers"]).unwrap();
        assert!(cli.manage_servers);
    }
}
