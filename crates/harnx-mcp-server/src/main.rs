//! MCP server exposing harnx tools and agents-as-tools.
//!
//! Serves over stdio or Streamable HTTP. Each connection is backed by a
//! harnx session with a worker tool reservation.

use anyhow::{bail, ensure, Result};
use clap::Parser;
#[cfg(test)]
use clap::{CommandFactory, FromArgMatches};
use harnx_core::agent_config::split_tool_selectors;
use harnx_mcp_server::{DEFAULT_MCP_HTTP_PORT, HARNX_MCP_PACKAGE_ENV, HARNX_MCP_USE_TOOLS_ENV};
use std::path::PathBuf;
use tracing_subscriber::EnvFilter;

/// MCP server exposing harnx tools and agents-as-tools over stdio or HTTP.
#[derive(Debug, Parser)]
#[command(name = "harnx-mcp-server", version, about, propagate_version = true)]
struct Args {
    /// Serve MCP over stdio (mutually exclusive with --mcp-http).
    #[arg(long, conflicts_with = "mcp_http")]
    mcp_stdio: bool,

    /// Serve MCP over Streamable HTTP (mutually exclusive with --mcp-stdio).
    #[arg(long, conflicts_with = "mcp_stdio")]
    mcp_http: bool,

    /// HTTP bind host (default: 127.0.0.1; set explicitly to expose externally).
    #[arg(long, default_value = "127.0.0.1", requires = "mcp_http")]
    host: String,

    /// HTTP bind port (default: 3010).
    #[arg(long, requires = "mcp_http")]
    port: Option<u16>,

    /// Tool selectors (repeatable, comma-separated, brace-expansion supported).
    ///
    /// Selects which tools to expose. Required; no default "expose all" mode.
    /// Syntax matches agent `use_tools`: comma-separated with brace expansion,
    /// e.g. "fs_*,bash_exec,bash_spawn" or "fs_{read,write}".
    ///
    /// Also configurable via HARNX_MCP_USE_TOOLS environment variable.
    /// When provided, CLI values replace the environment variable value.
    #[arg(long = "use-tools", value_name = "SELECTORS", env = HARNX_MCP_USE_TOOLS_ENV)]
    use_tools: Vec<String>,

    /// Package context for tool naming.
    ///
    /// When set, same-package tools appear with package-unqualified names
    /// (e.g. `fs_read`, stripping the package prefix, not the server prefix);
    /// cross-package tools use `pkg__server_tool` naming, matching what an agent
    /// in this package would see.
    ///
    /// Also configurable via HARNX_MCP_PACKAGE environment variable.
    #[arg(long, env = HARNX_MCP_PACKAGE_ENV)]
    package: Option<String>,

    /// Target cluster name for shared workers.
    ///
    /// When omitted, uses HARNX_NATS_SERVER env if set, otherwise the local
    /// __local__ cluster (embedded broker + child worker).
    #[arg(long)]
    cluster: Option<String>,

    /// Configuration directory path.
    #[arg(long, value_name = "PATH", env = "HARNX_CONFIG_DIR")]
    config_dir: Option<PathBuf>,
}

impl Args {
    fn validate(&self) -> Result<()> {
        // One of --mcp-stdio or --mcp-http is required
        ensure!(
            self.mcp_stdio || self.mcp_http,
            "one of --mcp-stdio or --mcp-http is required"
        );
        Ok(())
    }

    /// Collect and expand tool selectors from CLI repeat values.
    ///
    /// Each `--use-tools` value is parsed as a comma-separated list with
    /// brace-expansion awareness. Braces protect enclosed commas from splitting,
    /// matching existing `use_tools` syntax in agent configurations.
    fn collect_tool_selectors(&self) -> Vec<String> {
        let mut selectors: Vec<String> = Vec::new();
        for value in &self.use_tools {
            // split_tool_selectors preserves braces, only splitting on
            // top-level commas (commas outside of {…} groups)
            for selector in split_tool_selectors(value) {
                let trimmed = selector.trim();
                if !trimmed.is_empty() {
                    selectors.push(trimmed.to_string());
                }
            }
        }
        selectors
    }

    #[cfg(test)]
    fn parse_from<I, T>(args: I) -> Result<Self>
    where
        I: IntoIterator<Item = T>,
        T: Into<std::ffi::OsString> + Clone,
    {
        let matches = Self::command().try_get_matches_from(args)?;
        Ok(Self::from_arg_matches(&matches)?)
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize tracing to stderr
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();

    let args = Args::parse();
    args.validate()?;

    let selectors = args.collect_tool_selectors();

    // Reject absent/empty selectors with clear error
    if selectors.is_empty() {
        bail!(
            "no tool selectors specified; set --use-tools or {}",
            HARNX_MCP_USE_TOOLS_ENV
        );
    }

    tracing::info!(
        selectors = ?selectors,
        package = ?args.package,
        cluster = ?args.cluster,
        "harnx-mcp-server starting"
    );

    let bootstrap = harnx_mcp_server::bootstrap::Bootstrap::new(
        args.cluster,
        args.config_dir,
        harnx_core::abort::create_abort_signal(),
    )
    .await?;

    let bootstrap = std::sync::Arc::new(bootstrap);
    let view = harnx_runtime::nats_worker::tool_reservation::ToolReservationView {
        package: args.package,
        use_tools: selectors,
    };
    if args.mcp_stdio {
        harnx_mcp_server::transport::run_stdio(bootstrap, view).await
    } else {
        let listener = tokio::net::TcpListener::bind((
            args.host.as_str(),
            args.port.unwrap_or(DEFAULT_MCP_HTTP_PORT),
        ))
        .await?;
        let shutdown = tokio_util::sync::CancellationToken::new();
        let signal = shutdown.clone();
        let _signal_task = tokio_util::task::AbortOnDropHandle::new(tokio::spawn(async move {
            if let Err(error) = shutdown_signal().await {
                tracing::warn!(%error, "failed to listen for shutdown signal");
            }
            signal.cancel();
        }));
        harnx_mcp_server::transport::run_http(
            bootstrap,
            view,
            harnx_mcp_server::transport::HttpOptions {
                listener,
                session_config: Default::default(),
                shutdown,
            },
        )
        .await
    }
}

async fn shutdown_signal() -> Result<()> {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => result?,
            _ = terminate.recv() => {},
        }
    }
    #[cfg(not(unix))]
    tokio::signal::ctrl_c().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_stdio_transport() {
        let args = Args::parse_from(["harnx-mcp-server", "--mcp-stdio", "--use-tools", "fs_*"])
            .expect("parse succeeds");
        assert!(args.mcp_stdio);
        assert!(!args.mcp_http);
        assert_eq!(args.use_tools, vec!["fs_*"]);
        assert!(args.validate().is_ok());
    }

    #[test]
    fn parse_http_transport() {
        let args = Args::parse_from(["harnx-mcp-server", "--mcp-http", "--use-tools", "fs_read"])
            .expect("parse succeeds");
        assert!(!args.mcp_stdio);
        assert!(args.mcp_http);
        assert_eq!(args.use_tools, vec!["fs_read"]);
        assert!(args.validate().is_ok());
    }

    #[test]
    fn http_host_defaults_to_loopback_and_explicit_external_host_is_preserved() {
        let default_args =
            Args::parse_from(["harnx-mcp-server", "--mcp-http", "--use-tools", "fs_read"])
                .expect("default HTTP arguments parse");
        assert_eq!(default_args.host, "127.0.0.1");

        let external_args = Args::parse_from([
            "harnx-mcp-server",
            "--mcp-http",
            "--host",
            "0.0.0.0",
            "--use-tools",
            "fs_read",
        ])
        .expect("explicit external host parses");
        assert_eq!(external_args.host, "0.0.0.0");
    }

    #[test]
    fn parse_http_with_host_port() {
        let args = Args::parse_from([
            "harnx-mcp-server",
            "--mcp-http",
            "--host",
            "127.0.0.1",
            "--port",
            "8080",
            "--use-tools",
            "bash_*",
        ])
        .expect("parse succeeds");
        assert!(args.mcp_http);
        assert_eq!(args.host, "127.0.0.1");
        assert_eq!(args.port, Some(8080));
    }

    #[test]
    fn reject_both_transports() {
        let result = Args::parse_from([
            "harnx-mcp-server",
            "--mcp-stdio",
            "--mcp-http",
            "--use-tools",
            "fs_*",
        ]);
        assert!(result.is_err(), "should reject both transports");
    }

    #[test]
    fn reject_no_transport() {
        let args =
            Args::parse_from(["harnx-mcp-server", "--use-tools", "fs_*"]).expect("parse succeeds");
        let err = args.validate().expect_err("should require transport");
        assert!(err.to_string().contains("--mcp-stdio") || err.to_string().contains("--mcp-http"));
    }

    fn parse_stdio_with(extra_args: &[&str]) -> Args {
        let mut full_args = vec!["harnx-mcp-server", "--mcp-stdio"];
        full_args.extend_from_slice(extra_args);
        Args::parse_from(full_args).expect("parse succeeds")
    }

    fn assert_parsed_selectors(extra_args: &[&str], expected: &[&str]) {
        let args = parse_stdio_with(extra_args);
        assert_eq!(args.collect_tool_selectors(), expected);
    }

    fn assert_optional_field<F>(flag: &str, value: &str, field_extractor: F)
    where
        F: Fn(&Args) -> Option<&str>,
    {
        let default_args = parse_stdio_with(&["--use-tools", "fs_*"]);
        assert!(field_extractor(&default_args).is_none());

        let configured_args = parse_stdio_with(&["--use-tools", "fs_*", flag, value]);
        assert_eq!(field_extractor(&configured_args), Some(value));
    }

    #[test]
    fn collect_tool_selectors_comma_separated() {
        assert_parsed_selectors(
            &["--use-tools", "fs_*,bash_exec,bash_spawn"],
            &["fs_*", "bash_exec", "bash_spawn"],
        );
    }

    #[test]
    fn collect_tool_selectors_repeatable() {
        assert_parsed_selectors(
            &["--use-tools", "fs_*", "--use-tools", "bash_exec"],
            &["fs_*", "bash_exec"],
        );
    }

    #[test]
    fn collect_tool_selectors_cli_brace_intact() {
        // Real CLI invocation: braces protect enclosed commas.
        assert_parsed_selectors(
            &["--use-tools", "fs_{read,write},bash_exec"],
            &["fs_{read,write}", "bash_exec"],
        );
    }

    #[test]
    fn collect_tool_selectors_brace_expansion() {
        assert_parsed_selectors(&["--use-tools", "fs_{read,write}"], &["fs_{read,write}"]);
    }

    #[test]
    fn collect_tool_selectors_brace_expansion_mixed() {
        assert_parsed_selectors(
            &["--use-tools", "fs_{read,write},bash_exec"],
            &["fs_{read,write}", "bash_exec"],
        );
    }

    #[test]
    fn collect_tool_selectors_env_brace_value() {
        assert_parsed_selectors(
            &["--use-tools", "bash_{exec,spawn},fs_*"],
            &["bash_{exec,spawn}", "fs_*"],
        );
    }

    #[test]
    fn collect_tool_selectors_empty_selectors_rejected() {
        let args = parse_stdio_with(&[]);
        assert!(args.collect_tool_selectors().is_empty());
    }

    #[test]
    fn env_var_use_tools() {
        use clap::CommandFactory;
        let cmd = Args::command();
        let arg = cmd
            .get_arguments()
            .find(|a| a.get_id() == "use_tools")
            .expect("use_tools arg exists");
        assert!(arg.get_env().is_some());
    }

    #[test]
    fn package_optional() {
        assert_optional_field("--package", "my-package", |a| a.package.as_deref());
    }

    #[test]
    fn cluster_optional() {
        assert_optional_field("--cluster", "production", |a| a.cluster.as_deref());
    }
}
