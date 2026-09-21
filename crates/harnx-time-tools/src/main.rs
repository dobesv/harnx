use harnx_time_tools::TimeToolset;
use harnx_toolset_server::run_toolset_main;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    if parse_args()? {
        print_help();
        return Ok(());
    }

    run_toolset_main(TimeToolset::new()).await
}

fn parse_args() -> anyhow::Result<bool> {
    let mut help = false;
    let mut args = std::env::args().skip(1);

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--help" | "-h" => help = true,
            "--mcp-stdio" | "--mcp-http" => {}
            "--host" | "--port" | "--metrics-addr" | "--healthz-addr" => {
                args.next();
            }
            arg if arg.strip_prefix("--host=").is_some()
                || arg.strip_prefix("--port=").is_some()
                || arg.strip_prefix("--metrics-addr=").is_some()
                || arg.strip_prefix("--healthz-addr=").is_some() => {}
            _ => anyhow::bail!("harnx-time-tools: unknown argument: {arg}"),
        }
    }

    Ok(help)
}

fn print_help() {
    eprintln!("harnx-time-tools - time toolset server");
    eprintln!();
    eprintln!("Usage: harnx-time-tools [OPTIONS]");
    eprintln!();
    eprintln!("Options:");
    eprintln!("  --mcp-stdio                Serve MCP over stdio");
    eprintln!("  --mcp-http                 Serve MCP over Streamable HTTP");
    eprintln!("  --host <HOST>              MCP HTTP bind host (default: 0.0.0.0)");
    eprintln!("  --port <PORT>              MCP HTTP bind port (default: 3001)");
    eprintln!("  --metrics-addr <ADDR>      Serve Prometheus metrics");
    eprintln!("  --healthz-addr <ADDR>      Serve readiness checks");
    eprintln!("  --help, -h                 Show this help message");
}
