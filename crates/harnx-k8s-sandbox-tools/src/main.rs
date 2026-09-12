use anyhow::{Context, Result};
use clap::Parser;
use harnx_k8s_sandbox_tools::{
    sandbox_toolsets, KubernetesSandboxApi, McpCaller, McpCallerConfig, SandboxManager,
    SandboxManagerConfig, StreamableHttpMcpCaller,
};
use harnx_nats_common::connect::{NatsConnection, NatsEndpoint};
use harnx_runtime::nats_session_metadata::SessionMetadataStore;
use harnx_toolset_server::{serve_many_with_shutdown, ServeLifecycle};
use std::sync::Arc;
use std::time::Duration;

#[derive(Debug, Parser)]
#[command(about = "Expose Kubernetes Agent Sandbox tools over native Harnx NATS")]
struct Cli {
    #[arg(long, env = "SANDBOX_NAMESPACE", default_value = "agent-sandboxes")]
    sandbox_namespace: String,
    #[arg(long, env = "SANDBOX_TEMPLATE", default_value = "formative-buildbox")]
    sandbox_template: String,
    #[arg(long, env = "DEFAULT_TTL_MINUTES", default_value_t = 4320)]
    default_ttl_minutes: u64,
    #[arg(long, env = "SANDBOX_SCAN_INTERVAL_MINUTES", default_value_t = 15)]
    sandbox_scan_interval_minutes: u64,
    #[arg(long, env = "IDLE_TIMEOUT_MINUTES", default_value_t = 15)]
    idle_timeout_minutes: u64,
    #[arg(long, env = "AUTO_EXTEND_THRESHOLD_HOURS", default_value_t = 48)]
    auto_extend_threshold_hours: u64,
    #[arg(long, env = "AUTO_EXTEND_TTL_HOURS", default_value_t = 72)]
    auto_extend_ttl_hours: u64,
    #[arg(long, env = "K8S_REQUEST_TIMEOUT_SECS", default_value_t = 30)]
    k8s_request_timeout_secs: u64,
    #[arg(long, env = "K8S_OPERATION_TIMEOUT_SECS", default_value_t = 60)]
    k8s_operation_timeout_secs: u64,
    #[arg(long, env = "MCP_PRE_DISPATCH_TIMEOUT_SECS", default_value_t = 30)]
    mcp_pre_dispatch_timeout_secs: u64,
    #[arg(long, env = "MCP_RESPONSE_TIMEOUT_SECS", default_value_t = 90_000)]
    mcp_response_timeout_secs: u64,
    #[arg(long, env = "RETRY_BACKOFF_BASE_MS", default_value_t = 250)]
    retry_backoff_base_ms: u64,
    #[arg(long, env = "RETRY_BACKOFF_CAP_MS", default_value_t = 10_000)]
    retry_backoff_cap_ms: u64,
    #[arg(long, env = "RETRY_MAX_ATTEMPTS", default_value_t = 5)]
    retry_max_attempts: usize,
    #[command(flatten)]
    metrics: harnx_metrics::MetricsFlags,
    #[command(flatten)]
    healthz: harnx_healthz::HealthzFlags,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let _ = harnx_core::logging::init(harnx_core::logging::LogSink::Stderr);
    harnx_metrics::init(&cli.metrics)?;
    let readiness = harnx_healthz::init(&cli.healthz).await?;
    let telemetry = harnx_telemetry::init_telemetry("harnx-k8s-sandbox-tools")?;

    let result = run(cli, readiness).await;
    telemetry.shutdown().await;
    result
}

async fn run(cli: Cli, readiness: Option<harnx_healthz::Readiness>) -> Result<()> {
    validate(&cli)?;
    let scope =
        harnx_core::instance::scope_from_env(harnx_core::instance::StandaloneMode::WorkerLaunched)?;
    let endpoint = NatsEndpoint::from_env()?;
    let client = endpoint.connect().await?;
    let jetstream = async_nats::jetstream::new(client.clone());
    let metadata = SessionMetadataStore::ensure(&jetstream, endpoint.resolved_replicas()).await?;
    let retry = RetrySettings::from_cli(&cli);
    let manager = build_manager(&cli, &retry, kube::Client::try_default().await?)?;
    let caller = build_caller(&cli, &retry)?;
    let shutdown = harnx_nats_common::shutdown::cancel_token_on_shutdown_signal();
    let (watcher, session_cleanup) =
        spawn_idle_watcher(manager.clone(), caller.clone(), shutdown.clone());
    let toolsets = sandbox_toolsets(manager, caller, metadata);
    let result = serve_many_with_shutdown(
        toolsets,
        scope,
        NatsConnection {
            client,
            replicas: endpoint.resolved_replicas(),
        },
        ServeLifecycle::new(shutdown.clone(), readiness),
    )
    .await;
    shutdown.cancel();
    let _ = watcher.await;
    let _ = session_cleanup.await;
    result
}

struct RetrySettings {
    base: Duration,
    cap: Duration,
    max_attempts: usize,
}

impl RetrySettings {
    fn from_cli(cli: &Cli) -> Self {
        Self {
            base: Duration::from_millis(cli.retry_backoff_base_ms),
            cap: Duration::from_millis(cli.retry_backoff_cap_ms),
            max_attempts: cli.retry_max_attempts,
        }
    }
}

fn validate(cli: &Cli) -> Result<()> {
    anyhow::ensure!(
        cli.sandbox_scan_interval_minutes > 0,
        "sandbox scan interval must be greater than zero"
    );
    anyhow::ensure!(
        cli.idle_timeout_minutes > 0,
        "idle timeout must be greater than zero"
    );
    anyhow::ensure!(
        cli.default_ttl_minutes > 0 && cli.auto_extend_ttl_hours > 0,
        "sandbox TTLs must be greater than zero"
    );
    anyhow::ensure!(
        cli.k8s_request_timeout_secs > 0
            && cli.k8s_operation_timeout_secs > 0
            && cli.mcp_pre_dispatch_timeout_secs > 0,
        "Kubernetes and MCP pre-dispatch timeouts must be greater than zero"
    );
    anyhow::ensure!(
        cli.retry_max_attempts > 0 && cli.retry_backoff_cap_ms >= cli.retry_backoff_base_ms,
        "retry max attempts must be positive and backoff cap must be at least its base"
    );
    Ok(())
}

fn build_manager(
    cli: &Cli,
    retry: &RetrySettings,
    kubernetes: kube::Client,
) -> Result<SandboxManager> {
    let request_timeout = Duration::from_secs(cli.k8s_request_timeout_secs);
    let operation_timeout = Duration::from_secs(cli.k8s_operation_timeout_secs);
    let api = KubernetesSandboxApi::with_timeouts(
        kubernetes,
        &cli.sandbox_namespace,
        request_timeout,
        operation_timeout,
    );
    let config = SandboxManagerConfig {
        template: cli.sandbox_template.clone(),
        default_ttl: minutes(cli.default_ttl_minutes)?,
        scan_interval: minutes(cli.sandbox_scan_interval_minutes)?,
        idle_timeout: minutes(cli.idle_timeout_minutes)?,
        auto_extend_threshold: hours(cli.auto_extend_threshold_hours)?,
        auto_extend_ttl: hours(cli.auto_extend_ttl_hours)?,
        backoff_base: retry.base,
        backoff_cap: retry.cap,
        max_attempts: retry.max_attempts,
        ..SandboxManagerConfig::default()
    };
    Ok(SandboxManager::new(Arc::new(api), config))
}

fn build_caller(cli: &Cli, retry: &RetrySettings) -> Result<Arc<dyn McpCaller>> {
    let response_timeout = mcp_response_timeout(cli.mcp_response_timeout_secs);
    let caller = StreamableHttpMcpCaller::with_config(McpCallerConfig {
        pre_dispatch_timeout: Duration::from_secs(cli.mcp_pre_dispatch_timeout_secs),
        response_timeout,
        backoff_base: retry.base,
        backoff_cap: retry.cap,
        max_attempts: retry.max_attempts,
    })?;
    Ok(Arc::new(caller))
}

fn spawn_idle_watcher(
    manager: SandboxManager,
    caller: Arc<dyn McpCaller>,
    shutdown: tokio_util::sync::CancellationToken,
) -> (tokio::task::JoinHandle<()>, tokio::task::JoinHandle<()>) {
    let watcher_shutdown = shutdown.clone();
    let (hibernated_tx, mut hibernated_rx) = tokio::sync::mpsc::unbounded_channel();
    let watcher = tokio::spawn(async move {
        manager
            .run_idle_watcher(watcher_shutdown, hibernated_tx)
            .await;
    });
    let session_cleanup = tokio::spawn(async move {
        while let Some(sandbox_id) = hibernated_rx.recv().await {
            caller.disconnect(&sandbox_id).await;
        }
    });
    (watcher, session_cleanup)
}

fn mcp_response_timeout(seconds: u64) -> Option<Duration> {
    (seconds > 0).then(|| Duration::from_secs(seconds))
}

fn minutes(value: u64) -> Result<Duration> {
    value
        .checked_mul(60)
        .map(Duration::from_secs)
        .context("minute duration is too large")
}

fn hours(value: u64) -> Result<Duration> {
    value
        .checked_mul(60 * 60)
        .map(Duration::from_secs)
        .context("hour duration is too large")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_cli() -> Cli {
        Cli::try_parse_from(["harnx-k8s-sandbox-tools"]).expect("default CLI must be valid")
    }

    #[test]
    fn default_configuration_is_valid() {
        validate(&valid_cli()).expect("default configuration must pass validation");
    }

    #[test]
    fn bounded_phase_timeouts_must_be_positive() {
        let mut cli = valid_cli();
        cli.k8s_request_timeout_secs = 0;
        assert!(validate(&cli).is_err());

        let mut cli = valid_cli();
        cli.k8s_operation_timeout_secs = 0;
        assert!(validate(&cli).is_err());

        let mut cli = valid_cli();
        cli.mcp_pre_dispatch_timeout_secs = 0;
        assert!(validate(&cli).is_err());
    }

    #[test]
    fn backoff_cap_must_not_be_below_base() {
        let mut cli = valid_cli();
        cli.retry_backoff_base_ms = 501;
        cli.retry_backoff_cap_ms = 500;

        assert!(validate(&cli).is_err());
    }

    #[test]
    fn retry_attempt_budget_must_include_an_initial_attempt() {
        let mut cli = valid_cli();
        cli.retry_max_attempts = 0;

        assert!(validate(&cli).is_err());
    }

    #[test]
    fn zero_response_timeout_disables_response_budget() {
        let mut cli = valid_cli();
        cli.mcp_response_timeout_secs = 0;

        validate(&cli).expect("disabled response budget must be valid");
        assert_eq!(mcp_response_timeout(cli.mcp_response_timeout_secs), None);
        assert_eq!(
            mcp_response_timeout(90_000),
            Some(Duration::from_secs(90_000))
        );
    }
}
