//! `harnx-worker` — NATS worker daemon that executes agent turns.
//!
//! Front-ends (`harnx`, `harnx-serve`) publish session activations to a NATS
//! cluster; a worker leases the session and runs the agent loop. Shipping the
//! worker as its own binary keeps it out of the front-end's dep graph and lets
//! deployments run workers without the TUI.

use anyhow::{bail, Result};
use clap::{ArgGroup, Parser};
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
    about = "Run a persistent-cluster or frontend-managed local worker daemon",
    long_about = None,
    group(ArgGroup::new("connection_mode").required(true).args(["cluster", "session_scope"]))
)]
struct Cli {
    /// Persistent cluster key from nats_servers/<name>.yaml.
    #[arg(long, conflicts_with = "session_scope")]
    cluster: Option<String>,
    /// Frontend-managed local session scope using the NATS environment handoff.
    #[arg(long, conflicts_with = "cluster")]
    session_scope: Option<String>,
    /// Worker identity for leases and dispatch. Required for local serving;
    /// generated when omitted for a persistent cluster.
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
    /// Prometheus metrics endpoint address (e.g., "127.0.0.1:9109" or ":0" for
    /// any free port). If omitted, no metrics listener is started.
    #[command(flatten)]
    metrics: harnx_metrics::MetricsFlags,
    /// HTTP readiness endpoint address. If omitted, no healthz listener is started.
    #[command(flatten)]
    healthz: harnx_healthz::HealthzFlags,
}

impl Cli {
    fn validate(&self) -> Result<()> {
        if self.cluster.as_deref() == Some(harnx_runtime::config::LOCAL_CLUSTER_KEY) {
            bail!(
                "--cluster {} is reserved for frontend-managed workers; use --session-scope {}",
                harnx_runtime::config::LOCAL_CLUSTER_KEY,
                harnx_runtime::config::LOCAL_CLUSTER_KEY
            );
        }
        if let Some(scope) = &self.session_scope {
            self.validate_local_scope(scope)?;
        }
        if self.diagnose && self.session_scope.is_none() {
            bail!(
                "--diagnose is available through --session-scope {}",
                harnx_runtime::config::LOCAL_CLUSTER_KEY
            );
        }
        Ok(())
    }

    fn validate_local_scope(&self, scope: &str) -> Result<()> {
        if scope != harnx_runtime::config::LOCAL_CLUSTER_KEY {
            bail!(
                "--session-scope currently accepts only {}",
                harnx_runtime::config::LOCAL_CLUSTER_KEY
            );
        }
        if !self.diagnose {
            self.validate_local_serving()?;
        }
        Ok(())
    }

    fn validate_local_serving(&self) -> Result<()> {
        if self.worker_id.is_none() {
            bail!("local serving requires --worker-id");
        }
        if !self.manage_servers {
            bail!("local serving requires --manage-servers");
        }
        Ok(())
    }

    fn daemon_config(&self) -> Result<harnx_runtime::nats_worker::WorkerDaemonConfig> {
        self.validate()?;
        if self.session_scope.is_some() {
            return harnx_runtime::nats_worker::WorkerDaemonConfig::local(
                self.worker_id
                    .clone()
                    .expect("validated local serving worker id"),
            );
        }
        let cluster = self
            .cluster
            .clone()
            .expect("clap requires a connection mode");
        let worker_id = self
            .worker_id
            .clone()
            .unwrap_or_else(harnx_runtime::nats_worker::new_worker_id);
        Ok(if self.manage_servers {
            harnx_runtime::nats_worker::WorkerDaemonConfig::managing(cluster, worker_id)
        } else {
            harnx_runtime::nats_worker::WorkerDaemonConfig::new(cluster, worker_id)
        })
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    load_env_file()?;
    let cli = Cli::parse();
    cli.validate()?;
    setup_logger(LogSink::Stderr)?;
    let telemetry = harnx_telemetry::init_telemetry("harnx-worker")?;
    harnx_core::alloc_guard::init_from_env();

    let result = run(cli).await;
    telemetry.shutdown().await;
    result
}

async fn run(cli: Cli) -> Result<()> {
    harnx_metrics::init(&cli.metrics)?;
    let readiness = harnx_healthz::init(&cli.healthz).await?;
    let config = Arc::new(RwLock::new(
        Config::init_headless(WorkingMode::Cmd, true).await?,
    ));
    config.write().agent_variables = collect_agent_variables(&cli.agent_variable)?;
    if cli.diagnose {
        print!(
            "{}",
            harnx_runtime::nats_worker::diagnose_tool_servers(&config).await?
        );
        return Ok(());
    }

    let daemon = cli.daemon_config()?;
    // `None` selects the agent loop's default call path
    // (`call_with_retry_and_fallback`), which is what the worker wants.
    harnx_runtime::nats_worker::run_worker_daemon(config, daemon, None, readiness).await
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
        assert_eq!(cli.cluster.as_deref(), Some("prod"));
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
    fn parses_local_diagnose() {
        let cli = Cli::try_parse_from([
            "harnx-worker",
            "--session-scope",
            "__local__",
            "--worker-id",
            "local",
            "--diagnose",
        ])
        .unwrap();
        assert_eq!(cli.worker_id.as_deref(), Some("local"));
        assert!(cli.diagnose);
        cli.validate().expect("local diagnose is supported");
    }

    #[test]
    fn manage_servers_defaults_to_off_and_is_settable() {
        let cli = Cli::try_parse_from(["harnx-worker", "--cluster", "prod"]).unwrap();
        assert!(!cli.manage_servers);

        let cli =
            Cli::try_parse_from(["harnx-worker", "--cluster", "prod", "--manage-servers"]).unwrap();
        assert!(cli.manage_servers);
    }

    #[test]
    fn connection_modes_are_mutually_exclusive() {
        assert!(Cli::try_parse_from([
            "harnx-worker",
            "--cluster",
            "prod",
            "--session-scope",
            "__local__"
        ])
        .is_err());
    }

    #[test]
    fn rejects_reserved_local_cluster() {
        let cli = Cli::try_parse_from(["harnx-worker", "--cluster", "__local__"]).unwrap();
        let error = cli.validate().expect_err("reserved cluster must fail");
        assert!(error.to_string().contains("--session-scope __local__"));
    }

    #[test]
    fn local_serving_requires_worker_id_and_manage_servers() {
        let missing_both =
            Cli::try_parse_from(["harnx-worker", "--session-scope", "__local__"]).unwrap();
        assert!(missing_both
            .validate()
            .unwrap_err()
            .to_string()
            .contains("--worker-id"));

        let missing_manage = Cli::try_parse_from([
            "harnx-worker",
            "--session-scope",
            "__local__",
            "--worker-id",
            "local-test",
        ])
        .unwrap();
        assert!(missing_manage
            .validate()
            .unwrap_err()
            .to_string()
            .contains("--manage-servers"));
    }

    #[test]
    fn local_scope_rejects_other_values() {
        let cli =
            Cli::try_parse_from(["harnx-worker", "--session-scope", "prod", "--diagnose"]).unwrap();
        assert!(cli.validate().is_err());
    }

    #[test]
    fn diagnose_uses_the_local_session_scope_mode() {
        let cli = Cli::try_parse_from(["harnx-worker", "--cluster", "prod", "--diagnose"]).unwrap();
        assert!(cli
            .validate()
            .unwrap_err()
            .to_string()
            .contains("--session-scope __local__"));
    }
}
