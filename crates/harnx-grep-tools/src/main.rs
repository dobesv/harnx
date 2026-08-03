//! harnx-grep-tools: grep.app toolset server, with MCP stdio back-compat.

use harnx_grep_tools::GrepToolset;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    if parse_args()? {
        print_help();
        return Ok(());
    }

    eprintln!("harnx-grep-tools v{}: starting", env!("CARGO_PKG_VERSION"));

    harnx_toolset_server::run_toolset_main(GrepToolset::new()).await
}

/// Validate grep-specific arguments. The shared server runner consumes
/// `--mcp-stdio`, so this parser accepts it without changing toolset setup.
fn parse_args() -> anyhow::Result<bool> {
    let mut help = false;

    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--help" | "-h" => help = true,
            "--mcp-stdio" => {}
            _ => anyhow::bail!("harnx-grep-tools: unknown argument: {arg}"),
        }
    }

    Ok(help)
}

fn print_help() {
    eprintln!("harnx-grep-tools - GitHub code search toolset server");
    eprintln!();
    eprintln!("Usage: harnx-grep-tools [OPTIONS]");
    eprintln!();
    eprintln!("Options:");
    eprintln!("  --mcp-stdio         Serve MCP over stdio instead of the default toolset mode");
    eprintln!("  --help, -h          Show this help message");
}
