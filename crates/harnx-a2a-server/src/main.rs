//! A2A server binary entry point.

use anyhow::Result;
use clap::Parser;
use harnx_a2a_server::cli::Args;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();
    harnx_a2a_server::run(args).await
}
