//! harnx-plans-tools: NATS-backed plan/task/note toolset server.

use harnx_plans_tools::PlansToolset;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = harnx_core::logging::init(harnx_core::logging::LogSink::Stderr);
    log::info!("harnx-plans-tools v{}: starting", env!("CARGO_PKG_VERSION"));
    harnx_toolset_server::run_toolset_main(PlansToolset::new()).await
}
