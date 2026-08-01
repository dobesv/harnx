use anyhow::Result;
use clap::Parser;
use harnx_claude_compatible_hook_server::{Args, ClaudeCompatibleHook};

#[tokio::main]
async fn main() -> Result<()> {
    let hook = ClaudeCompatibleHook::try_from(Args::parse())?;
    harnx_hookset_server::run_hookset_main(hook).await
}
