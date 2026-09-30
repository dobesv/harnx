//! harnx-attachment-tools: Attachment toolset server.
//!
//! Provides `attachment_read` and `attachment_create` tools for NATS-backed
//! blob storage via cid: URLs.

use harnx_attachment_tools::AttachmentToolset;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = harnx_core::logging::init(harnx_core::logging::LogSink::Stderr);
    if parse_args()? {
        print_help();
        return Ok(());
    }

    log::info!(
        "harnx-attachment-tools v{}: starting",
        env!("CARGO_PKG_VERSION")
    );

    harnx_toolset_server::run_toolset_main(AttachmentToolset::new()).await
}

/// Validate attachment-specific arguments.
fn parse_args() -> anyhow::Result<bool> {
    let args = std::env::args().collect::<Vec<_>>();
    parse_args_from(&args)
}

fn handle_enable_tool_arg(
    arg: &str,
    args: &mut impl Iterator<Item = String>,
) -> anyhow::Result<bool> {
    let pattern = if arg == "--enable-tool" {
        args.next()
            .ok_or_else(|| anyhow::anyhow!("--enable-tool requires a glob pattern argument"))?
    } else {
        arg.strip_prefix("--enable-tool=")
            .unwrap_or_default()
            .to_owned()
    };
    let _ = globset::Glob::new(&pattern)
        .map_err(|e| anyhow::anyhow!("invalid glob pattern '{}': {}", pattern, e))?;
    Ok(true)
}

fn is_value_option(arg: &str) -> bool {
    ["--nats-url", "--metrics-addr", "--healthz-addr", "--name"]
        .iter()
        .any(|flag| arg == *flag || arg.strip_prefix(&format!("{flag}=")).is_some())
}

fn consume_value(arg: &str, args: &mut impl Iterator<Item = String>) {
    if !arg.contains('=') {
        args.next();
    }
}

fn parse_args_from(args: &[String]) -> anyhow::Result<bool> {
    let mut args = args.iter().skip(1).cloned();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--help" | "-h" => return Ok(true),
            "--mcp-stdio" | "--mcp-http" => {}
            "--mcp-http-listener" => consume_value(&arg, &mut args),
            arg if arg.starts_with("--mcp-http-listener=") => {}
            arg if arg.starts_with("--mcp-http-") => {
                anyhow::bail!("unknown argument: {arg}")
            }
            arg if arg == "--enable-tool" || arg.starts_with("--enable-tool=") => {
                handle_enable_tool_arg(arg, &mut args)?;
            }
            arg if is_value_option(arg) => consume_value(arg, &mut args),
            _ => anyhow::bail!("unknown argument: {arg}"),
        }
    }
    Ok(false)
}

fn print_help() {
    eprintln!("harnx-attachment-tools: Attachment toolset server");
    eprintln!();
    eprintln!("Usage: harnx-attachment-tools [OPTIONS]");
    eprintln!();
    eprintln!("Options:");
    eprintln!("  --mcp-stdio             Run as MCP server over stdio");
    eprintln!("  --mcp-http [--mcp-http-listener <ADDR>]");
    eprintln!("                          Run as MCP server over HTTP");
    eprintln!("  --nats-url <URL>        NATS server URL (default: $HARNX_NATS_URL or nats://127.0.0.1:4222)");
    eprintln!("  --name <NAME>           Override the registered toolset name");
    eprintln!("  --enable-tool <GLOB>    Only publish tools matching glob pattern");
    eprintln!("  --metrics-addr <ADDR>   Serve Prometheus metrics at http://ADDR/metrics");
    eprintln!("                          Blank host binds 0.0.0.0. Unset disables.");
    eprintln!("  --healthz-addr <ADDR>   Serve readiness checks at http://ADDR/healthz.");
    eprintln!("                          Blank host binds 0.0.0.0. Unset disables.");
    eprintln!("  --help, -h              Show this help message");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(arguments: &[&str]) -> anyhow::Result<bool> {
        let args = std::iter::once("harnx-attachment-tools")
            .chain(arguments.iter().copied())
            .map(str::to_owned)
            .collect::<Vec<_>>();
        parse_args_from(&args)
    }

    #[test]
    fn accepts_mcp_http_flag() {
        let args = ["harnx-attachment-tools", "--mcp-http"].map(str::to_string);
        assert!(!parse_args_from(&args).expect("MCP HTTP flag should parse"));
    }

    #[test]
    fn rejects_unknown_flags() {
        let args = ["harnx-attachment-tools", "--mcp-http-typo"].map(str::to_string);
        let error = parse_args_from(&args).expect_err("unknown flag should be rejected");
        assert!(error
            .to_string()
            .contains("unknown argument: --mcp-http-typo"));
    }

    #[test]
    fn accepts_enable_tool_space_form_without_help_or_unknown_error() {
        assert!(!parse(&["--enable-tool", "attachment_read"]).unwrap());
    }

    #[test]
    fn accepts_enable_tool_equals_form() {
        assert!(!parse(&["--enable-tool=attachment_read"]).unwrap());
    }

    #[test]
    fn accepts_trailing_enable_tool_flag() {
        let error = parse(&["--enable-tool"]).expect_err("missing pattern should be rejected");
        assert!(error
            .to_string()
            .contains("--enable-tool requires a glob pattern argument"));
    }

    #[test]
    fn accepts_name_override_forms() {
        assert!(!parse(&["--name", "review"]).unwrap());
        assert!(!parse(&["--name=review"]).unwrap());
    }
}
