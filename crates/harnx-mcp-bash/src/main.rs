mod server;

use rmcp::ServiceExt;
use server::BashServer;
use std::path::PathBuf;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    #[cfg(unix)]
    let (roots, sandbox_config) = parse_args()?;
    #[cfg(not(unix))]
    let roots = parse_args();

    eprintln!(
        "harnx-mcp-bash v{}: starting ({} root{})",
        env!("CARGO_PKG_VERSION"),
        if roots.is_empty() {
            "no CLI roots, awaiting client roots".to_string()
        } else {
            roots.len().to_string()
        },
        if roots.len() == 1 { "" } else { "s" }
    );
    for root in &roots {
        eprintln!("  root: {}", root.display());
    }

    #[cfg(unix)]
    {
        if sandbox_config.enabled {
            eprintln!(
                "  sandbox: enabled (helper: {})",
                sandbox_config.sandbox_run_path.display()
            );
        } else {
            eprintln!("  sandbox: disabled");
        }
    }

    #[cfg(unix)]
    let server = BashServer::new_with_sandbox(roots, sandbox_config);
    #[cfg(not(unix))]
    let server = BashServer::new(roots);
    let cleanup_server = server.clone();
    let transport = rmcp::transport::stdio();
    let service = server.serve(transport).await?;
    let wait_result = service.waiting().await;
    if let Err(err) = cleanup_server.cleanup_log_dir() {
        eprintln!("harnx-mcp-bash: warning: failed to clean temp log dir: {err}");
    }
    wait_result?;

    Ok(())
}

#[cfg(unix)]
fn parse_env_paths(var_name: &str) -> Vec<PathBuf> {
    std::env::var_os(var_name)
        .map(|value| {
            std::env::split_paths(&value)
                .filter(|path| !path.as_os_str().is_empty())
                .collect()
        })
        .unwrap_or_default()
}

fn push_root(roots: &mut Vec<PathBuf>, raw: &str) {
    let path = PathBuf::from(raw);
    if path.exists() {
        match path.canonicalize() {
            Ok(canonical) => roots.push(canonical),
            Err(err) => {
                eprintln!("warning: failed to canonicalize root '{}': {}", raw, err);
            }
        }
    } else {
        eprintln!("harnx-mcp-bash: warning: root path does not exist: {}", raw);
    }
}

#[cfg(unix)]
fn parse_args() -> anyhow::Result<(Vec<PathBuf>, server::SandboxConfig)> {
    let args: Vec<String> = std::env::args().collect();
    let mut roots = Vec::new();
    let mut sandbox_enabled = true;
    let mut sandbox_config = server::SandboxConfig {
        enabled: true,
        extra_exec: parse_env_paths("HARNX_BASH_EXTRA_EXEC"),
        extra_readable: parse_env_paths("HARNX_BASH_EXTRA_READABLE"),
        sandbox_run_path: PathBuf::from("harnx-mcp-bash-sandbox-run"),
    };
    let mut sandbox_run_override = None;
    let mut i = 1;

    while i < args.len() {
        match args[i].as_str() {
            "--root" | "-r" => {
                if i + 1 < args.len() {
                    push_root(&mut roots, &args[i + 1]);
                    i += 2;
                } else {
                    eprintln!("harnx-mcp-bash: --root requires a path argument");
                    std::process::exit(1);
                }
            }
            "--no-sandbox" => {
                sandbox_enabled = false;
                sandbox_config.enabled = false;
                i += 1;
            }
            "--extra-readable" => {
                if i + 1 < args.len() {
                    sandbox_config
                        .extra_readable
                        .push(PathBuf::from(&args[i + 1]));
                    i += 2;
                } else {
                    eprintln!("harnx-mcp-bash: --extra-readable requires a path argument");
                    std::process::exit(1);
                }
            }
            "--extra-exec" => {
                if i + 1 < args.len() {
                    sandbox_config.extra_exec.push(PathBuf::from(&args[i + 1]));
                    i += 2;
                } else {
                    eprintln!("harnx-mcp-bash: --extra-exec requires a path argument");
                    std::process::exit(1);
                }
            }
            "--sandbox-run" => {
                if i + 1 < args.len() {
                    sandbox_run_override = Some(PathBuf::from(&args[i + 1]));
                    i += 2;
                } else {
                    eprintln!("harnx-mcp-bash: --sandbox-run requires a path argument");
                    std::process::exit(1);
                }
            }
            "--help" | "-h" => {
                eprintln!("harnx-mcp-bash: MCP shell command server");
                eprintln!();
                eprintln!("Usage: harnx-mcp-bash [OPTIONS]");
                eprintln!();
                eprintln!("Options:");
                eprintln!("  --root, -r <path>        Add an allowed root directory (repeatable)");
                eprintln!("  --no-sandbox            Disable filesystem sandboxing explicitly");
                eprintln!("  --extra-readable <path> Add sandbox read-only path (repeatable)");
                eprintln!("  --extra-exec <path>     Add sandbox execute path (repeatable)");
                eprintln!("  --sandbox-run <path>    Override sandbox helper binary path");
                eprintln!("  --help, -h              Show this help message");
                eprintln!();
                eprintln!("Environment:");
                eprintln!(
                    "  HARNX_BASH_EXTRA_READABLE  Colon-separated extra sandbox read-only paths"
                );
                eprintln!(
                    "  HARNX_BASH_EXTRA_EXEC      Colon-separated extra sandbox execute paths"
                );
                eprintln!();
                eprintln!("Sandboxing is enabled by default on Unix. Use --no-sandbox to disable it explicitly.");
                eprintln!("The server communicates via stdio using the MCP protocol.");
                eprintln!("If no roots are specified, operations are denied until the client provides roots.");
                eprintln!("Roots can also be provided dynamically by the MCP client.");
                std::process::exit(0);
            }
            other => {
                eprintln!("harnx-mcp-bash: unknown argument: {}", other);
                eprintln!("Try: harnx-mcp-bash --help");
                std::process::exit(1);
            }
        }
    }

    let resolved_sandbox_run_path = sandbox_run_override.clone().or_else(|| {
        std::env::current_exe().ok().and_then(|path| {
            path.parent()
                .map(|dir| dir.join("harnx-mcp-bash-sandbox-run"))
        })
    });

    if sandbox_enabled {
        let path = resolved_sandbox_run_path
            .unwrap_or_else(|| PathBuf::from("harnx-mcp-bash-sandbox-run"));
        if path.is_file() {
            sandbox_config.sandbox_run_path = path;
        } else if sandbox_run_override.is_some() {
            anyhow::bail!(
                "harnx-mcp-bash: error: sandbox helper not found at {}; fix --sandbox-run or pass --no-sandbox to disable sandboxing explicitly",
                path.display()
            );
        } else {
            anyhow::bail!(
                "harnx-mcp-bash: error: sandbox helper not found at {}; place harnx-mcp-bash-sandbox-run next to harnx-mcp-bash, use --sandbox-run <path>, or pass --no-sandbox to disable sandboxing explicitly",
                path.display()
            );
        }
    } else {
        sandbox_config.enabled = false;
        sandbox_config.sandbox_run_path = resolved_sandbox_run_path
            .unwrap_or_else(|| PathBuf::from("harnx-mcp-bash-sandbox-run"));
    }

    Ok((roots, sandbox_config))
}

#[cfg(not(unix))]
fn parse_args() -> Vec<PathBuf> {
    let args: Vec<String> = std::env::args().collect();
    let mut roots = Vec::new();
    let mut i = 1;

    while i < args.len() {
        match args[i].as_str() {
            "--root" | "-r" => {
                if i + 1 < args.len() {
                    push_root(&mut roots, &args[i + 1]);
                    i += 2;
                } else {
                    eprintln!("harnx-mcp-bash: --root requires a path argument");
                    std::process::exit(1);
                }
            }
            "--help" | "-h" => {
                eprintln!("harnx-mcp-bash: MCP shell command server");
                eprintln!();
                eprintln!("Usage: harnx-mcp-bash [OPTIONS]");
                eprintln!();
                eprintln!("Options:");
                eprintln!("  --root, -r <path>  Add an allowed root directory (repeatable)");
                eprintln!("  --help, -h         Show this help message");
                eprintln!();
                eprintln!("Sandboxing is enabled by default on Unix. Use --no-sandbox to disable it explicitly.");
                eprintln!("The server communicates via stdio using the MCP protocol.");
                eprintln!("If no roots are specified, operations are denied until the client provides roots.");
                eprintln!("Roots can also be provided dynamically by the MCP client.");
                std::process::exit(0);
            }
            other => {
                eprintln!("harnx-mcp-bash: unknown argument: {}", other);
                eprintln!("Try: harnx-mcp-bash --help");
                std::process::exit(1);
            }
        }
    }

    roots
}
