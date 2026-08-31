use anyhow::Result;
use clap::Parser;
use harnx_claude_compatible_hook_server::{Args, ClaudeCompatibleHook};

#[tokio::main]
async fn main() -> Result<()> {
    let _ = harnx_core::logging::init(harnx_core::logging::LogSink::Stderr);
    let telemetry = harnx_telemetry::init_telemetry("harnx-claude-compatible-hook-server")?;

    let result = run().await;
    telemetry.shutdown().await;
    result
}

async fn run() -> Result<()> {
    let args = Args::parse();
    let readiness = harnx_healthz::init(&args.healthz).await?;
    let hook = ClaudeCompatibleHook::try_from(args)?;
    harnx_hookset_server::run_hookset_main_with_readiness(hook, readiness).await
}
