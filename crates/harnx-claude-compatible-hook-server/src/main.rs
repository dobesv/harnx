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
    let hook = ClaudeCompatibleHook::try_from(Args::parse())?;
    harnx_hookset_server::run_hookset_main(hook).await
}
