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

fn is_value_option(arg: &str) -> bool {
    [
        "--host",
        "--port",
        "--metrics-addr",
        "--healthz-addr",
        "--name",
        "--enable-tool",
        "--format",
    ]
    .iter()
    .any(|flag| arg == *flag || arg.strip_prefix(&format!("{flag}=")).is_some())
}

fn parse_args_from(arguments: &[String]) -> anyhow::Result<bool> {
    let mut help = false;
    let mut args = arguments.iter().skip(1);

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--help" | "-h" => help = true,
            "--mcp-stdio" | "--mcp-http" | "--list-tools" => {}
            arg if is_value_option(arg) => {
                if !arg.contains('=') {
                    args.next();
                }
            }
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
    eprintln!("  --list-tools               Print the advertised tools and exit");
    eprintln!("  --format <text|json>       Output shape for --list-tools");
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

    #[test]
    fn accepts_list_tools_flags() {
        assert!(!parse(&["--list-tools"]).unwrap());
        assert!(!parse(&["--list-tools", "--format", "json"]).unwrap());
        assert!(!parse(&["--list-tools", "--format=json"]).unwrap());
    }

    #[test]
    fn rejects_format_typo() {
        assert!(parse(&["--list-tools", "--formatt", "json"]).is_err());
        assert!(parse(&["--list-tools", "--formatt=json"]).is_err());
    }
}
