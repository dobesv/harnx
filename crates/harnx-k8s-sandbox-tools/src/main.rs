use anyhow::{Context, Result};
use clap::Parser;
use harnx_k8s_sandbox_tools::{
    sandbox_toolsets, KubernetesSandboxApi, McpCaller, SandboxManager, SandboxManagerConfig,
    StreamableHttpMcpCaller,
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
    let scope =
        harnx_core::instance::scope_from_env(harnx_core::instance::StandaloneMode::WorkerLaunched)?;
    let endpoint = NatsEndpoint::from_env()?;
    let client = endpoint.connect().await?;
    let jetstream = async_nats::jetstream::new(client.clone());
    let metadata = SessionMetadataStore::ensure(&jetstream, endpoint.resolved_replicas()).await?;
    let kubernetes = kube::Client::try_default().await?;
    let manager = SandboxManager::new(
        Arc::new(KubernetesSandboxApi::new(
            kubernetes,
            &cli.sandbox_namespace,
        )),
        SandboxManagerConfig {
            template: cli.sandbox_template,
            default_ttl: minutes(cli.default_ttl_minutes)?,
            scan_interval: minutes(cli.sandbox_scan_interval_minutes)?,
            idle_timeout: minutes(cli.idle_timeout_minutes)?,
            auto_extend_threshold: hours(cli.auto_extend_threshold_hours)?,
            auto_extend_ttl: hours(cli.auto_extend_ttl_hours)?,
            ..SandboxManagerConfig::default()
        },
    );
    let shutdown = harnx_nats_common::shutdown::cancel_token_on_shutdown_signal();
    let caller: Arc<dyn McpCaller> = Arc::new(StreamableHttpMcpCaller::new()?);
    let watcher_manager = manager.clone();
    let watcher_shutdown = shutdown.clone();
    let watcher_caller = caller.clone();
    let (hibernated_tx, mut hibernated_rx) = tokio::sync::mpsc::unbounded_channel();
    let watcher = tokio::spawn(async move {
        watcher_manager
            .run_idle_watcher(watcher_shutdown, hibernated_tx)
            .await;
    });
    let session_cleanup = tokio::spawn(async move {
        while let Some(sandbox_id) = hibernated_rx.recv().await {
            watcher_caller.disconnect(&sandbox_id).await;
        }
    });
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
