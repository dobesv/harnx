//! harnx-fs-tools: Filesystem toolset server, with MCP stdio back-compat.

use harnx_fs_tools::{FsToolset, NatsAttachmentResolver};
use harnx_tool_allow::{resolve_allowlist, AllowEnv, AllowInputs, ResolvedAllowlist};
use std::path::PathBuf;
use std::sync::Arc;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = harnx_core::logging::init(harnx_core::logging::LogSink::Stderr);
    let (inputs, sandbox_transport) = parse_args();
    let cwd = std::env::current_dir()?;
    let allowlist = resolve_allowlist(&inputs, &cwd, &AllowEnv::from_current_process());

    log::info!(
        "harnx-fs-tools v{}: starting ({} read, {} write allow path{})",
        env!("CARGO_PKG_VERSION"),
        allowlist.read_paths().len(),
        allowlist.write_paths().len(),
        if allowlist.write_paths().len() == 1 {
            ""
        } else {
            "s"
        }
    );

    harnx_toolset_server::run_toolset_main_with_factory_and_http_options(
        move |client| {
            let toolset = build_toolset(allowlist, client);
            Ok(if sandbox_transport {
                toolset.with_sandbox_transport()
            } else {
                toolset
            })
        },
        sandbox_http_options(sandbox_transport),
    )
    .await
}

fn build_toolset(allowlist: ResolvedAllowlist, client: Option<async_nats::Client>) -> FsToolset {
    let toolset = FsToolset::new(allowlist);
    match client {
        Some(client) => toolset.with_attachment_resolver(Arc::new(NatsAttachmentResolver::new(
            async_nats::jetstream::new(client),
        ))),
        None => toolset,
    }
}

fn sandbox_http_options(enabled: bool) -> harnx_toolset_server::McpHttpOptions {
    harnx_toolset_server::McpHttpOptions {
        max_request_body_bytes: enabled
            .then_some(harnx_fs_tools::SANDBOX_HTTP_MAX_REQUEST_BODY_BYTES),
    }
}

fn env_paths(name: &str) -> Vec<PathBuf> {
    std::env::var_os(name)
        .map(|value| {
            std::env::split_paths(&value)
                .filter(|path| !path.as_os_str().is_empty())
                .collect()
        })
        .unwrap_or_default()
}

fn env_toggle(name: &str) -> bool {
    std::env::var(name)
        .map(|value| {
            matches!(
                value.to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}

/// Parse filesystem-specific CLI arguments. Shared server runner consumes
/// MCP transport arguments, so this parser accepts them without changing allowlist setup.
fn parse_args() -> (AllowInputs, bool) {
    let args: Vec<String> = std::env::args().collect();
    parse_args_from(
        &args,
        initial_allow_inputs(),
        env_toggle("HARNX_FS_SANDBOX_TRANSPORT"),
    )
    .unwrap_or_else(|error| {
        eprintln!("{error}");
        std::process::exit(1);
    })
}

fn initial_allow_inputs() -> AllowInputs {
    AllowInputs {
        read: env_paths("HARNX_TOOLS_ALLOW_READ"),
        write: env_paths("HARNX_TOOLS_ALLOW_WRITE"),
        exec: env_paths("HARNX_TOOLS_ALLOW_EXEC"),
        rwx: env_paths("HARNX_TOOLS_ALLOW_RWX"),
        common_default: env_toggle("HARNX_TOOLS_ALLOW_COMMON_DEFAULT"),
        dev_tools: env_toggle("HARNX_TOOLS_ALLOW_DEV_TOOLS"),
        repo_work: env_toggle("HARNX_TOOLS_ALLOW_REPO_WORK"),
        all: env_toggle("HARNX_TOOLS_ALLOW_ALL"),
    }
}

const PASSTHROUGH_FLAGS: [(&str, &str); 6] = [
    ("--metrics-addr", "--metrics-addr="),
    ("--healthz-addr", "--healthz-addr="),
    ("--host", "--host="),
    ("--port", "--port="),
    ("--name", "--name="),
    ("--enable-tool", "--enable-tool="),
];

struct ParseState<'a> {
    args: &'a [String],
    index: usize,
}

impl ParseState<'_> {
    fn current(&self) -> &str {
        self.args[self.index].as_str()
    }

    fn consume_passthrough(&mut self) -> bool {
        let arg = self.current();
        for (flag, assignment) in PASSTHROUGH_FLAGS {
            if arg == flag {
                self.index += 2;
                return true;
            }
            if arg.strip_prefix(assignment).is_some() {
                self.index += 1;
                return true;
            }
        }
        false
    }
}

fn parse_args_from(
    args: &[String],
    mut inputs: AllowInputs,
    mut sandbox_transport: bool,
) -> Result<(AllowInputs, bool), String> {
    let mut state = ParseState { args, index: 1 };
    let mut mcp_http = false;

    while state.index < args.len() {
        if state.consume_passthrough() {
            continue;
        }
        let target = match state.current() {
            "--allow-read" => Some(&mut inputs.read),
            "--allow-write" => Some(&mut inputs.write),
            "--allow-exec" => Some(&mut inputs.exec),
            "--allow-rwx" => Some(&mut inputs.rwx),
            "--allow-common-default" => {
                inputs.common_default = true;
                None
            }
            "--allow-dev-tools" => {
                inputs.dev_tools = true;
                None
            }
            "--allow-repo-work" => {
                inputs.repo_work = true;
                None
            }
            "--allow-all" => {
                inputs.all = true;
                None
            }
            "--sandbox-transport" => {
                sandbox_transport = true;
                None
            }
            "--mcp-http" => {
                mcp_http = true;
                None
            }
            "--mcp-stdio" => None,
            "--help" | "-h" => print_help_and_exit(),
            other => {
                return Err(format!(
                    "harnx-fs-tools: unknown argument: {other}\nTry: harnx-fs-tools --help"
                ));
            }
        };

        if let Some(paths) = target {
            let path = args.get(state.index + 1).ok_or_else(|| {
                format!(
                    "harnx-fs-tools: {} requires a path argument",
                    args[state.index]
                )
            })?;
            paths.push(PathBuf::from(path));
            state.index += 2;
        } else {
            state.index += 1;
        }
    }

    if sandbox_transport && !mcp_http {
        return Err(
            "harnx-fs-tools: --sandbox-transport / HARNX_FS_SANDBOX_TRANSPORT requires --mcp-http; private write_bytes is only for the sandbox gateway, not NATS or MCP stdio"
                .to_string(),
        );
    }
    Ok((inputs, sandbox_transport))
}

fn print_help_and_exit() -> ! {
    eprintln!("harnx-fs-tools: Filesystem toolset server");
    eprintln!();
    eprintln!("Usage: harnx-fs-tools [OPTIONS]");
    eprintln!();
    eprintln!("Options:");
    eprintln!("  --allow-read <path>       Allow filesystem reads (repeatable)");
    eprintln!("  --allow-write <path>      Allow reads and writes (repeatable)");
    eprintln!("  --allow-exec <path>       Allow reads; fs has no exec operation (repeatable)");
    eprintln!("  --allow-rwx <path>        Allow reads and writes (repeatable)");
    eprintln!("  --allow-common-default    Allow common operating-system paths");
    eprintln!("  --allow-dev-tools         Allow development tool paths");
    eprintln!("  --allow-repo-work         Allow detected project roots and current directory");
    eprintln!("  --allow-all               Allow all filesystem paths");
    eprintln!("  --mcp-stdio               Serve MCP over stdio instead of toolset mode");
    eprintln!("  --mcp-http                Serve MCP over Streamable HTTP instead of toolset mode");
    eprintln!("  --sandbox-transport       Expose private write_bytes for the sandbox gateway");
    eprintln!("                            Requires --mcp-http; not NATS or MCP stdio");
    eprintln!("                            Also honors HARNX_FS_SANDBOX_TRANSPORT");
    eprintln!("  --host <HOST>             MCP HTTP bind host (default: 0.0.0.0)");
    eprintln!("  --name <NAME>             Override the registered toolset name");
    eprintln!("  --port <PORT>             MCP HTTP bind port (default: 3003)");
    eprintln!(
        "  --enable-tool <glob>      Enable only tools matching the glob pattern (repeatable)."
    );
    eprintln!(
        "                            If set, only enabled tools are registered and invocable."
    );
    eprintln!("  --metrics-addr <ADDR>     Serve Prometheus metrics at http://ADDR/metrics.");
    eprintln!("                            Blank host binds 0.0.0.0, e.g. :8456. Unset disables.");
    eprintln!("                            Also honors HARNX_METRICS_ADDR env.");
    eprintln!("  --healthz-addr <ADDR>     Serve readiness checks at http://ADDR/healthz.");
    eprintln!("                            Blank host binds 0.0.0.0, e.g. :8457. Unset disables.");
    eprintln!("                            Also honors HARNX_HEALTHZ_ADDR env.");
    eprintln!("  --help, -h                Show this help message");
    std::process::exit(0);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_toolset_has_no_attachment_resolver() {
        let toolset = build_toolset(ResolvedAllowlist::new(), None);
        assert!(toolset.attachment_resolver().is_none());
    }

    #[tokio::test]
    async fn nats_toolset_has_attachment_resolver() -> anyhow::Result<()> {
        let Some(server) = harnx_test_bins::spawn_nats_server().await? else {
            return Ok(());
        };
        let client = async_nats::connect(server.url()).await?;
        let toolset = build_toolset(ResolvedAllowlist::new(), Some(client));
        let resolver = toolset
            .attachment_resolver()
            .expect("NATS resolver configured");
        assert!(matches!(
            resolver.resolve("", 1024).await,
            Err(harnx_fs_tools::AttachmentResolveError::InvalidUrl(_))
        ));
        Ok(())
    }

    #[test]
    fn accepts_mcp_http_listener_flags() {
        let args = [
            "harnx-fs-tools",
            "--mcp-http",
            "--host",
            "127.0.0.1",
            "--port=0",
            "--allow-all",
        ]
        .map(str::to_string);

        let (inputs, sandbox_transport) = parse_args_from(&args, AllowInputs::default(), false)
            .expect("MCP HTTP listener flags should parse");
        assert!(!sandbox_transport);
        assert!(inputs.all, "boolean flag after --mcp-http was swallowed");
    }

    #[test]
    fn rejects_near_prefix_passthrough_flags() {
        for flag in ["--mcp-http-typo", "--metrics-addr-typo"] {
            let args = ["harnx-fs-tools", flag].map(str::to_string);
            let error = parse_args_from(&args, AllowInputs::default(), false)
                .expect_err("near-prefix flag should be rejected");
            assert!(error.contains(&format!("unknown argument: {flag}")));
        }
    }

    #[test]
    fn rejects_legacy_allowlist_flags() {
        let legacy_flags = [
            ["--", "root"].concat(),
            ["--extra", "-read"].concat(),
            ["--default", "-root", "-cwd"].concat(),
        ];

        for flag in legacy_flags {
            let args = vec!["harnx-fs-tools".to_string(), flag.clone()];
            let error = parse_args_from(&args, AllowInputs::default(), false)
                .expect_err("legacy flag should be rejected");
            assert!(error.contains(&format!("unknown argument: {flag}")));
        }
    }

    #[test]
    fn accepts_name_override_forms() {
        for name_args in [
            ["--name", "review"].as_slice(),
            ["--name=review"].as_slice(),
        ] {
            let args = std::iter::once("harnx-fs-tools")
                .chain(name_args.iter().copied())
                .map(str::to_string)
                .collect::<Vec<_>>();
            parse_args_from(&args, AllowInputs::default(), false)
                .expect("name override should pass through");
        }
    }
}

#[cfg(test)]
mod sandbox_transport_tests {
    use super::*;

    #[test]
    fn sandbox_transport_flag_is_explicit_and_preserves_allowlist_flags() {
        let args = [
            "harnx-fs-tools",
            "--mcp-http",
            "--sandbox-transport",
            "--allow-all",
        ]
        .map(str::to_string);
        let (allow, enabled) = parse_args_from(&args, AllowInputs::default(), false).unwrap();
        assert!(enabled);
        assert!(allow.all);
        assert_eq!(
            sandbox_http_options(enabled).max_request_body_bytes,
            Some(harnx_fs_tools::SANDBOX_HTTP_MAX_REQUEST_BODY_BYTES),
        );
        assert_eq!(sandbox_http_options(false).max_request_body_bytes, None);
    }

    #[test]
    fn sandbox_transport_flag_does_not_match_values_or_typos() {
        let args = ["harnx-fs-tools", "--allow-write", "--sandbox-transport"].map(str::to_string);
        let (allow, enabled) = parse_args_from(&args, AllowInputs::default(), false).unwrap();
        assert!(!enabled);
        assert_eq!(allow.write, vec![PathBuf::from("--sandbox-transport")]);
        let args = ["harnx-fs-tools", "--sandbox-transport-typo"].map(str::to_string);
        assert!(parse_args_from(&args, AllowInputs::default(), false).is_err());
    }
}
