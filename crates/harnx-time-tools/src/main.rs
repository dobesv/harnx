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
    parse_args_from(&std::env::args().collect::<Vec<_>>())
}

fn parse_args_from(arguments: &[String]) -> anyhow::Result<bool> {
    let mut help = false;
    let mut args = arguments.iter().skip(1);

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--help" | "-h" => help = true,
            "--mcp-stdio" | "--mcp-http" => {}
            "--host" | "--port" | "--metrics-addr" | "--healthz-addr" | "--name"
            | "--enable-tool" => {
                args.next();
            }
            arg if arg.strip_prefix("--host=").is_some()
                || arg.strip_prefix("--port=").is_some()
                || arg.strip_prefix("--metrics-addr=").is_some()
                || arg.strip_prefix("--healthz-addr=").is_some()
                || arg.strip_prefix("--name=").is_some()
                || arg.strip_prefix("--enable-tool=").is_some() => {}
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
    eprintln!("  --name <NAME>              Override the registered toolset name");
    eprintln!("  --enable-tool <GLOB>       Enable only tools matching the glob pattern");
    eprintln!("  --metrics-addr <ADDR>      Serve Prometheus metrics");
    eprintln!("  --healthz-addr <ADDR>      Serve readiness checks");
    eprintln!("  --help, -h                 Show this help message");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(arguments: &[&str]) -> anyhow::Result<bool> {
        let args = std::iter::once("harnx-time-tools")
            .chain(arguments.iter().copied())
            .map(str::to_string)
            .collect::<Vec<_>>();
        parse_args_from(&args)
    }

    #[test]
    fn accepts_name_override_forms() {
        assert!(!parse(&["--name", "review", "--enable-tool", "wait"]).unwrap());
        assert!(!parse(&["--name=review", "--enable-tool=wait"]).unwrap());
    }
}
