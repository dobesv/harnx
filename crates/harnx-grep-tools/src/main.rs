//! harnx-grep-tools: grep.app toolset server, with MCP stdio back-compat.

use harnx_grep_tools::GrepToolset;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = harnx_core::logging::init(harnx_core::logging::LogSink::Stderr);
    if parse_args()? {
        print_help();
        return Ok(());
    }

    log::info!("harnx-grep-tools v{}: starting", env!("CARGO_PKG_VERSION"));

    harnx_toolset_server::run_toolset_main(GrepToolset::new()).await
}

/// Validate grep-specific arguments. The shared server runner consumes
/// transport and listener arguments, so this parser accepts them without changing toolset setup.
fn parse_args() -> anyhow::Result<bool> {
    let args = std::env::args().collect::<Vec<_>>();
    parse_args_from(&args)
}

fn parse_args_from(args: &[String]) -> anyhow::Result<bool> {
    let mut help = false;
    let mut args = args.iter().skip(1);

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--help" | "-h" => help = true,
            "--mcp-stdio" | "--mcp-http" => {}
            arg if arg == "--metrics-addr" || arg.strip_prefix("--metrics-addr=").is_some() => {
                if arg == "--metrics-addr" {
                    args.next();
                }
            }
            arg if arg == "--healthz-addr" || arg.strip_prefix("--healthz-addr=").is_some() => {
                if arg == "--healthz-addr" {
                    args.next();
                }
            }
            arg if arg == "--host" || arg.strip_prefix("--host=").is_some() => {
                if arg == "--host" {
                    args.next();
                }
            }
            arg if arg == "--port" || arg.strip_prefix("--port=").is_some() => {
                if arg == "--port" {
                    args.next();
                }
            }
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
    eprintln!("  --mcp-stdio             Serve MCP over stdio instead of toolset mode");
    eprintln!("  --mcp-http              Serve MCP over Streamable HTTP instead of toolset mode");
    eprintln!("  --host <HOST>           MCP HTTP bind host (default: 0.0.0.0)");
    eprintln!("  --port <PORT>           MCP HTTP bind port (default: 3004)");
    eprintln!("  --metrics-addr <ADDR>   Serve Prometheus metrics at http://ADDR/metrics.");
    eprintln!("                        Blank host binds 0.0.0.0, e.g. :8456. Unset disables.");
    eprintln!("                        Also honors HARNX_METRICS_ADDR env.");
    eprintln!("  --healthz-addr <ADDR>   Serve readiness checks at http://ADDR/healthz.");
    eprintln!("                        Blank host binds 0.0.0.0, e.g. :8457. Unset disables.");
    eprintln!("                        Also honors HARNX_HEALTHZ_ADDR env.");
    eprintln!("  --help, -h              Show this help message");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_mcp_http_listener_flags_in_both_forms() {
        let args = [
            "harnx-grep-tools",
            "--mcp-http",
            "--host",
            "127.0.0.1",
            "--port=0",
        ]
        .map(str::to_string);

        assert!(!parse_args_from(&args).expect("MCP HTTP listener flags should parse"));
    }

    #[test]
    fn rejects_near_prefix_passthrough_flags() {
        for flag in ["--mcp-http-typo", "--metrics-addr-typo"] {
            let args = ["harnx-grep-tools", flag].map(str::to_string);
            let error = parse_args_from(&args).expect_err("near-prefix flag should be rejected");
            assert!(error
                .to_string()
                .contains(&format!("unknown argument: {flag}")));
        }
    }
}
