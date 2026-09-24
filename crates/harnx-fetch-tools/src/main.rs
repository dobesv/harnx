//! harnx-fetch-tools: native URL fetch toolset server.

use harnx_fetch_tools::FetchToolset;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = harnx_core::logging::init(harnx_core::logging::LogSink::Stderr);
    if parse_args()? {
        print_help();
        return Ok(());
    }

    log::info!("harnx-fetch-tools v{}: starting", env!("CARGO_PKG_VERSION"));
    harnx_toolset_server::run_toolset_main(FetchToolset::new()).await
}

fn parse_args() -> anyhow::Result<bool> {
    parse_args_from(&std::env::args().collect::<Vec<_>>())
}

fn handle_enable_tool_arg(
    arg: &str,
    args: &mut impl Iterator<Item = String>,
) -> anyhow::Result<()> {
    if arg == "--enable-tool" {
        args.next()
            .ok_or_else(|| anyhow::anyhow!("--enable-tool requires a glob pattern argument"))?;
    }
    Ok(())
}

fn is_value_option(arg: &str) -> bool {
    ["--metrics-addr", "--healthz-addr", "--host", "--port"]
        .iter()
        .any(|flag| arg == *flag || arg.strip_prefix(&format!("{flag}=")).is_some())
}

fn consume_value(arg: &str, args: &mut impl Iterator<Item = String>) {
    if !arg.contains('=') {
        args.next();
    }
}

fn parse_args_from(args: &[String]) -> anyhow::Result<bool> {
    let mut help = false;
    let mut args = args.iter().skip(1).cloned();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--help" | "-h" => help = true,
            "--mcp-stdio" | "--mcp-http" | "--allow-private-ip" => {}
            arg if is_value_option(arg) => consume_value(arg, &mut args),
            arg if arg == "--enable-tool" || arg.strip_prefix("--enable-tool=").is_some() => {
                handle_enable_tool_arg(arg, &mut args)?;
            }
            _ => anyhow::bail!("harnx-fetch-tools: unknown argument: {arg}"),
        }
    }
    Ok(help)
}

fn print_help() {
    eprintln!("harnx-fetch-tools - native URL fetching and extraction toolset server");
    eprintln!();
    eprintln!("Usage: harnx-fetch-tools [OPTIONS]");
    eprintln!();
    eprintln!("Options:");
    eprintln!("  --allow-private-ip      Allow private/special IPs and proxy tool arguments");
    eprintln!("  --mcp-stdio             Serve MCP over stdio instead of toolset mode");
    eprintln!("  --mcp-http              Serve MCP over Streamable HTTP instead of toolset mode");
    eprintln!("  --host <HOST>           MCP HTTP bind host (default: 0.0.0.0)");
    eprintln!("  --port <PORT>           MCP HTTP bind port (default: 3006)");
    eprintln!(
        "  --enable-tool <glob>    Enable only tools matching the glob pattern (repeatable)."
    );
    eprintln!("                          If set, only enabled tools are registered and invocable.");
    eprintln!("  --metrics-addr <ADDR>   Serve Prometheus metrics at http://ADDR/metrics.");
    eprintln!("                          Blank host binds 0.0.0.0, e.g. :8456. Unset disables.");
    eprintln!("                          Also honors HARNX_METRICS_ADDR env.");
    eprintln!("  --healthz-addr <ADDR>   Serve readiness checks at http://ADDR/healthz.");
    eprintln!("                          Blank host binds 0.0.0.0, e.g. :8457. Unset disables.");
    eprintln!("                          Also honors HARNX_HEALTHZ_ADDR env.");
    eprintln!("  --help, -h              Show this help message");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(arguments: &[&str]) -> anyhow::Result<bool> {
        let args = std::iter::once("harnx-fetch-tools")
            .chain(arguments.iter().copied())
            .map(str::to_owned)
            .collect::<Vec<_>>();
        parse_args_from(&args)
    }

    #[test]
    fn accepts_transport_listener_and_security_flags() {
        assert!(!parse(&[
            "--allow-private-ip",
            "--mcp-http",
            "--host",
            "127.0.0.1",
            "--port=0",
            "--metrics-addr",
            ":8456",
            "--healthz-addr=:8457",
        ])
        .unwrap());
    }

    #[test]
    fn accepts_help_and_enable_tool_forms() {
        assert!(parse(&["--help"]).unwrap());
        assert!(parse(&["-h"]).unwrap());
        assert!(!parse(&["--enable-tool", "fetch_*", "--enable-tool=fetch_html"]).unwrap());
    }

    #[test]
    fn rejects_unknown_and_missing_values() {
        assert!(parse(&["--unknown"])
            .unwrap_err()
            .to_string()
            .contains("unknown"));
        assert!(parse(&["--enable-tool"])
            .unwrap_err()
            .to_string()
            .contains("requires"));
    }
}
