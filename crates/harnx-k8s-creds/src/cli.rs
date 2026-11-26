use clap::Parser;
use std::path::PathBuf;

#[derive(Debug, Parser)]
pub(super) struct Args {
    #[arg(long, num_args = 1..)]
    pub context: Vec<String>,
    #[arg(long)]
    pub kubeconfig: Option<PathBuf>,

    /// Optional legacy compatibility flag; tool server mode is now default.
    #[arg(long)]
    pub tool_server: bool,
    #[arg(long, conflicts_with = "mcp_http")]
    pub mcp_stdio: bool,
    #[arg(long)]
    pub mcp_http: bool,
    /// MCP HTTP bind host (default: 0.0.0.0).
    #[arg(long, requires = "mcp_http")]
    pub host: Option<String>,
    /// MCP HTTP port (default: 3011).
    #[arg(long, requires = "mcp_http")]
    pub port: Option<u16>,
    /// Print the advertised tools and exit. Starts nothing and needs no broker.
    #[arg(long, conflicts_with_all = ["mcp_stdio", "mcp_http"])]
    pub list_tools: bool,
    /// Output shape for `--list-tools`.
    #[arg(long, requires = "list_tools", value_parser = ["text", "json"])]
    pub format: Option<String>,
    #[arg(long)]
    pub name: Option<String>,
    #[arg(long)]
    pub enable_tool: Vec<String>,
    #[command(flatten)]
    pub metrics: harnx_metrics::MetricsFlags,
    #[command(flatten)]
    pub healthz: harnx_healthz::HealthzFlags,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_tool_transport_flags_and_legacy_contexts() {
        assert!(Args::try_parse_from(["k8s", "--context", "test"]).is_ok());
        assert!(Args::try_parse_from([
            "k8s",
            "--context",
            "test",
            "--tool-server",
            "--mcp-stdio",
            "--name=k8s_creds",
            "--enable-tool=environment",
        ])
        .is_ok());
        assert!(Args::try_parse_from([
            "k8s",
            "--context",
            "test",
            "--mcp-stdio",
            "--name=k8s_creds",
            "--enable-tool=environment",
        ])
        .is_ok());
        assert!(Args::try_parse_from([
            "k8s",
            "--context",
            "test",
            "--tool-server",
            "--mcp-http",
            "--host=127.0.0.1",
            "--port=3011",
        ])
        .is_ok());
        for args in [
            vec![
                "k8s",
                "--context",
                "test",
                "--tool-server",
                "--mcp-stdio",
                "--mcp-http",
            ],
            vec!["k8s", "--context", "test", "--mcp-stdio", "--mcp-http"],
            vec!["k8s", "--context", "test", "--port=3011"],
        ] {
            assert!(Args::try_parse_from(args).is_err());
        }
    }

    #[test]
    fn list_tools_flags_parse_and_stay_strict() {
        for args in [
            vec!["x", "--list-tools"],
            vec!["x", "--list-tools", "--format", "json"],
            vec!["x", "--list-tools", "--format=json"],
        ] {
            assert!(Args::try_parse_from(args).is_ok());
        }
        for args in [
            vec!["x", "--format", "json"],
            vec!["x", "--list-tools", "--format", "yaml"],
            vec!["x", "--list-tools", "--mcp-stdio"],
            vec!["x", "--list-tools", "--formatt", "json"],
        ] {
            assert!(Args::try_parse_from(args).is_err());
        }
    }
}
