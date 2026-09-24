//! harnx-exa-tools: Exa web tools server, with MCP transport support.

use harnx_exa_tools::ExaToolset;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = harnx_core::logging::init(harnx_core::logging::LogSink::Stderr);
    if parse_args()? {
        print_help();
        return Ok(());
    }

    log::info!("harnx-exa-tools v{}: starting", env!("CARGO_PKG_VERSION"));
    harnx_toolset_server::run_toolset_main(ExaToolset::new()).await
}

fn parse_args() -> anyhow::Result<bool> {
    let args = std::env::args().collect::<Vec<_>>();
    parse_args_from(&args)
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
            "--mcp-stdio" | "--mcp-http" => {}
            arg if is_value_option(arg) => consume_value(arg, &mut args),
            arg if arg == "--enable-tool" || arg.strip_prefix("--enable-tool=").is_some() => {
                handle_enable_tool_arg(arg, &mut args)?;
            }
            _ => anyhow::bail!("harnx-exa-tools: unknown argument: {arg}"),
        }
    }
    Ok(help)
}

fn print_help() {
    eprintln!("harnx-exa-tools - Exa web search and content toolset server");
    eprintln!();
    eprintln!("Usage: harnx-exa-tools [OPTIONS]");
    eprintln!();
    eprintln!("Options:");
    eprintln!("  --mcp-stdio             Serve MCP over stdio instead of toolset mode");
    eprintln!("  --mcp-http              Serve MCP over Streamable HTTP instead of toolset mode");
    eprintln!("  --host <HOST>           MCP HTTP bind host (default: 0.0.0.0)");
    eprintln!("  --port <PORT>           MCP HTTP bind port (default: 3005)");
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
        let args = std::iter::once("harnx-exa-tools")
            .chain(arguments.iter().copied())
            .map(str::to_owned)
            .collect::<Vec<_>>();
        parse_args_from(&args)
    }

    #[test]
    fn accepts_transport_and_listener_flags() {
        assert!(!parse(&[
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
    fn accepts_help_forms() {
        assert!(parse(&["--help"]).unwrap());
        assert!(parse(&["-h"]).unwrap());
    }

    #[test]
    fn rejects_unknown_and_near_prefix_flags() {
        for flag in ["--unknown", "--mcp-http-typo", "--metrics-addr-typo"] {
            let error = parse(&[flag]).expect_err("unknown flag should fail");
            assert!(error
                .to_string()
                .contains(&format!("unknown argument: {flag}")));
        }
    }

    #[test]
    fn accepts_repeatable_enable_tool_forms() {
        assert!(!parse(&["--enable-tool", "web_*", "--enable-tool=web_search_exa"]).unwrap());
    }

    #[test]
    fn rejects_trailing_enable_tool_flag() {
        let error = parse(&["--enable-tool"]).unwrap_err();
        assert!(error
            .to_string()
            .contains("--enable-tool requires a glob pattern argument"));
    }
}
