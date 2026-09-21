use harnx_time_tools::TimeToolset;
use harnx_toolset_server::run_toolset_main;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    run_toolset_main(TimeToolset::new()).await
}
