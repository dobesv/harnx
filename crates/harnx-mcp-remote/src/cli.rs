use std::path::PathBuf;

use clap::{ArgAction, Parser};

#[derive(Debug, Clone, Parser)]
#[command(name = "harnx-mcp-remote")]
#[command(about = "Stdio MCP proxy for remote HTTP MCP servers")]
pub struct Cli {
    #[arg(long, env = "MCP_REMOTE_URL", required = true)]
    pub url: String,

    #[arg(long, env = "MCP_REMOTE_BEARER_TOKEN", hide_env_values = true)]
    pub bearer_token: Option<String>,

    #[arg(
        long,
        env = "MCP_REMOTE_INSECURE",
        help = "Allow bearer tokens over non-HTTPS transport for non-loopback hosts"
    )]
    pub insecure: bool,

    #[arg(long, action = ArgAction::Append)]
    pub header: Vec<String>,

    #[arg(long, env = "MCP_REMOTE_TLS_CERT")]
    pub tls_cert: Option<PathBuf>,

    #[arg(long, env = "MCP_REMOTE_TLS_KEY")]
    pub tls_key: Option<PathBuf>,

    #[arg(long, env = "MCP_REMOTE_TLS_CA")]
    pub tls_ca: Option<PathBuf>,

    #[arg(
        long,
        env = "MCP_REMOTE_STRICT_SESSION",
        help = "Require stateful remote MCP sessions and disable stateless fallback"
    )]
    pub strict_session: bool,

    /// Prometheus metrics endpoint address (e.g., "0.0.0.0:9090").
    /// Also reads from HARNX_METRICS_ADDR env.
    #[arg(long, env = "HARNX_METRICS_ADDR")]
    pub metrics_addr: Option<String>,

    /// Healthz readiness endpoint address (e.g., "0.0.0.0:8080").
    /// Also reads from HARNX_HEALTHZ_ADDR env.
    #[arg(long, env = "HARNX_HEALTHZ_ADDR")]
    pub healthz_addr: Option<String>,
}
