//! A2A server binary entry point.

use anyhow::Result;
use clap::Parser;
use harnx_a2a_server::cli::Args;

mod logging;

#[tokio::main]
async fn main() -> Result<()> {
    harnx_runtime::config::load_env_file()?;
    let args = Args::parse();
    logging::init()?;
    harnx_a2a_server::run(args).await
}
