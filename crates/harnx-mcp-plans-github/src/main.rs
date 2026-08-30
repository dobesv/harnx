use anyhow::Result;
use harnx_mcp_plans_github::config::AppConfig;

fn main() -> Result<()> {
    // Parse config OUTSIDE async runtime — git auto-detection makes synchronous blocking calls.
    let config = AppConfig::parse_from_env_and_args()?;
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async move {
        // Init metrics inside the runtime (recorder listener needs live tokio runtime)
        let metrics_flags = harnx_metrics::MetricsFlags {
            metrics_addr: config.metrics_addr.clone(),
        };
        harnx_metrics::init(&metrics_flags)?;

        let telemetry = harnx_telemetry::init_telemetry("harnx-mcp-plans-github")?;
        let result = harnx_mcp_plans_github::runtime::run(config).await;
        telemetry.shutdown().await;
        result
    })
}
