use clap::Parser;
use harnx_metrics::MetricsFlags;

#[derive(Debug, Parser)]
pub struct Args {
    #[command(flatten)]
    pub metrics: MetricsFlags,
    #[command(flatten)]
    pub healthz: harnx_healthz::HealthzFlags,
    /// Run the environment tool service (NATS by default).
    #[arg(long)]
    pub tool_server: bool,
    /// Use MCP stdio transport for the tool service.
    #[arg(long, conflicts_with = "mcp_http")]
    pub mcp_stdio: bool,
    /// Use MCP Streamable HTTP transport for the tool service.
    #[arg(long)]
    pub mcp_http: bool,
    /// MCP HTTP bind host (default: 0.0.0.0).
    #[arg(long, requires = "mcp_http")]
    pub host: Option<String>,
    /// MCP HTTP port (default: 3009).
    #[arg(long, requires = "mcp_http")]
    pub port: Option<u16>,
    /// Enable only matching tools in tool-server mode (repeatable glob).
    #[arg(long)]
    pub enable_tool: Vec<String>,
    /// Server name assigned by the supervisor.
    #[arg(long, value_name = "NAME")]
    pub name: Option<String>,

    /// jq/jaq filter or exec hook applied to each request. Input is a JSON
    /// object with fields: host, path, method, headers (object of lowercase
    /// header names). Output should be same object, optionally with headers
    /// modified. A `--hook` value starting with `#!` becomes an inline resident
    /// exec stage and must include a shebang so temp file is runnable. Values
    /// starting with `/`, `./`, or `../` are treated as executable file paths
    /// relative to proxy CWD; they only need execute permission and may be any
    /// language or compiled binary. All other values are jaq stages. Elements
    /// apply in CLI order.
    /// Example: 'if .host == "github.com" then .headers.authorization = "Bearer \(env.GITHUB_TOKEN)" else . end'
    #[arg(long, value_name = "HOOK")]
    pub hook: Vec<String>,

    /// Per-request timeout in seconds for exec `--hook` stages.
    #[arg(long, default_value_t = 30)]
    pub hook_timeout_secs: u64,

    /// Write a JSON log line for every proxied request. Each line is a JSON
    /// object with fields: host, method, path, auth, changed.
    /// `auth` is a truncated summary of the outgoing Authorization header
    /// (never the full token) and `changed` lists header names the hook
    /// added, replaced, or removed. Useful for debugging auth injection
    /// without exposing full tokens.
    #[arg(long, value_name = "PATH")]
    pub log_file: Option<std::path::PathBuf>,

    /// jq/jaq filter that receives sentinel values as jaq variables:
    /// `$fake_uuid_key`, `$fake_base64_key`, `$fake_url_base64_key`,
    /// `$fake_hex_key`, `$fake_email`, `$temp_file_root`, and after proxy
    /// startup `$proxy_port` as a string. Must output a JSON object.
    /// Multiple `--env` flags run in order and merge (later keys win).
    #[arg(long, value_name = "JQ_FILTER")]
    pub env: Vec<String>,

    /// jq/jaq filter that must output an object mapping relative file paths to
    /// file contents. Each value may be a string, object/array (written as JSON),
    /// or null to skip. Files are written under a fresh temporary root (unique
    /// per proxy instance, auto-deleted on exit) whose path is available to
    /// `--env`/`--fs`/`--hook` jq filters as `$temp_file_root` and to executable
    /// hooks as `vars.temp_file_root` on each request.
    /// Multiple `--fs` flags run in order and merge (later keys win).
    #[arg(long, value_name = "JQ_FILTER")]
    pub fs: Vec<String>,

    /// Load a YAML file into a jaq variable before evaluating hooks.
    /// Format: name=path
    #[arg(long = "load-yaml", value_name = "NAME=PATH")]
    pub load_yaml: Vec<String>,

    /// Load a JSON file into a jaq variable before evaluating hooks.
    /// Format: name=path
    #[arg(long = "load-json", value_name = "NAME=PATH")]
    pub load_json: Vec<String>,

    /// Load a raw text file into a jaq variable before evaluating hooks.
    /// Format: name=path
    #[arg(long = "load-raw", value_name = "NAME=PATH")]
    pub load_raw: Vec<String>,

    /// Execute a shell command and load stdout as a jaq variable.
    /// Format: name=command
    #[arg(long = "load-exec", value_name = "NAME=COMMAND")]
    pub load_exec: Vec<String>,
}

impl Args {
    /// Tool-server argv isn't shell-expanded. Expand only this explicit package
    /// prefix; never substitute variables inside jaq filters or inline scripts.
    pub fn resolved_hooks(&self) -> anyhow::Result<Vec<String>> {
        let package_dir = std::env::var_os("HARNX_PACKAGE_DIR");
        self.hook
            .iter()
            .map(|hook| resolve_package_hook(hook, package_dir.as_deref()))
            .collect()
    }
}

fn resolve_package_hook(
    hook: &str,
    package_dir: Option<&std::ffi::OsStr>,
) -> anyhow::Result<String> {
    let Some(relative) = hook.strip_prefix("$HARNX_PACKAGE_DIR/") else {
        return Ok(hook.to_owned());
    };
    let dir =
        package_dir.ok_or_else(|| anyhow::anyhow!("package --hook requires HARNX_PACKAGE_DIR"))?;
    let dir = std::path::Path::new(dir);
    anyhow::ensure!(dir.is_absolute(), "HARNX_PACKAGE_DIR must be absolute");
    Ok(dir.join(relative).to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn package_hook_expansion_does_not_interpret_filters_or_shell_syntax() {
        let dir = std::env::current_dir().unwrap().join("package with spaces");
        let hook = "$HARNX_PACKAGE_DIR/hooks/jira-auth-hook.py";
        assert_eq!(
            resolve_package_hook(hook, Some(dir.as_os_str())).unwrap(),
            dir.join("hooks/jira-auth-hook.py").to_string_lossy()
        );
        assert!(resolve_package_hook(hook, None).is_err());
        assert!(resolve_package_hook(hook, Some(std::ffi::OsStr::new("relative"))).is_err());
        for source in [
            ".headers.auth = env.GITHUB_TOKEN",
            "{path: $temp_file_root}",
            "#!/bin/sh\necho $HARNX_PACKAGE_DIR",
            "./script.py",
        ] {
            assert_eq!(resolve_package_hook(source, None).unwrap(), source);
        }
    }

    #[test]
    fn tool_transport_flags_are_strict_and_keep_assignment_forms() {
        for args in [
            vec![
                "proxy",
                "--tool-server",
                "--mcp-stdio",
                "--name=proxy_auth",
                "--enable-tool=environment",
            ],
            vec![
                "proxy",
                "--mcp-stdio",
                "--name=proxy_auth",
                "--enable-tool=environment",
            ],
            vec![
                "proxy",
                "--tool-server",
                "--mcp-http",
                "--host=127.0.0.1",
                "--port=3009",
            ],
            vec!["proxy", "--mcp-http", "--host=127.0.0.1", "--port=3009"],
        ] {
            assert!(Args::try_parse_from(args).is_ok());
        }
        for args in [
            vec!["proxy", "--mcp-stdio", "--mcp-http"],
            vec!["proxy", "--tool-server", "--mcp-stdio", "--mcp-http"],
            vec!["proxy", "--tool-server", "--mcp-typo"],
        ] {
            assert!(Args::try_parse_from(args).is_err());
        }
    }
}
