mod server;

use rmcp::ServiceExt;
use server::BashServer;
#[cfg(unix)]
use std::path::Path;
use std::path::PathBuf;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let (roots, sandbox_config) = parse_args()?;

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

    let server = BashServer::new_with_sandbox(roots, sandbox_config);
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
                .map(|path| PathBuf::from(expand_tilde(&path.to_string_lossy())))
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(unix)]
fn parse_env_passthrough() -> Vec<String> {
    std::env::var("HARNX_BASH_ENV_PASSTHROUGH")
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect()
}

#[cfg(unix)]
fn path_is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;

    path.metadata()
        .map(|metadata| metadata.is_file() && (metadata.permissions().mode() & 0o111 != 0))
        .unwrap_or(false)
}

fn expand_tilde(raw: &str) -> String {
    if !raw.starts_with('~') {
        return raw.to_string();
    }

    let home = match std::env::var("HOME") {
        Ok(home) => home,
        Err(_) => return raw.to_string(),
    };

    if raw == "~" {
        home
    } else if let Some(suffix) = raw.strip_prefix("~/") {
        format!("{home}/{suffix}")
    } else {
        raw.to_string()
    }
}

fn push_root(roots: &mut Vec<PathBuf>, raw: &str) {
    let raw = expand_tilde(raw);
    let path = PathBuf::from(&raw);
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
        extra_writable: parse_env_paths("HARNX_BASH_EXTRA_WRITABLE"),
        sandbox_run_path: PathBuf::from("harnx-mcp-bash-sandbox-run"),
        extra_env_passthrough: parse_env_passthrough(),
        env_overrides: vec![],
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
            "--extra-read" => {
                if i + 1 < args.len() {
                    sandbox_config
                        .extra_readable
                        .push(PathBuf::from(expand_tilde(&args[i + 1])));
                    i += 2;
                } else {
                    eprintln!("harnx-mcp-bash: --extra-read requires a path argument");
                    std::process::exit(1);
                }
            }
            "--extra-exec" => {
                if i + 1 < args.len() {
                    sandbox_config
                        .extra_exec
                        .push(PathBuf::from(expand_tilde(&args[i + 1])));
                    i += 2;
                } else {
                    eprintln!("harnx-mcp-bash: --extra-exec requires a path argument");
                    std::process::exit(1);
                }
            }
            "--extra-write" => {
                if i + 1 < args.len() {
                    sandbox_config
                        .extra_writable
                        .push(PathBuf::from(expand_tilde(&args[i + 1])));
                    i += 2;
                } else {
                    eprintln!("harnx-mcp-bash: --extra-write requires a path argument");
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
            "--env" | "-e" => {
                if i + 1 < args.len() {
                    let raw = &args[i + 1];
                    if let Some((key, value)) = raw.split_once('=') {
                        if key.is_empty() {
                            eprintln!("harnx-mcp-bash: --env requires a non-empty variable name");
                            std::process::exit(1);
                        }
                        sandbox_config
                            .env_overrides
                            .push((key.to_string(), value.to_string()));
                    } else {
                        if raw.is_empty() {
                            eprintln!("harnx-mcp-bash: --env requires a non-empty variable name");
                            std::process::exit(1);
                        }
                        sandbox_config.extra_env_passthrough.push(raw.clone());
                    }
                    i += 2;
                } else {
                    eprintln!("harnx-mcp-bash: --env requires an argument");
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
                eprintln!("  --extra-read <path> Add sandbox read-only path (repeatable)");
                eprintln!("  --extra-exec <path>     Add sandbox execute path (repeatable)");
                eprintln!("  --extra-write <path>    Add sandbox writable path (repeatable)");
                eprintln!("  --sandbox-run <path>    Override sandbox helper binary path");
                eprintln!("  --env, -e <VAR>         Pass VAR from host env to child (repeatable)");
                eprintln!("  --env, -e <VAR=VALUE>   Set VAR=VALUE in child env (repeatable)");
                eprintln!("  --help, -h              Show this help message");
                eprintln!();
                eprintln!("Environment:");
                eprintln!(
                    "  HARNX_BASH_EXTRA_READABLE   Colon-separated extra sandbox read-only paths"
                );
                eprintln!(
                    "  HARNX_BASH_EXTRA_EXEC       Colon-separated extra sandbox execute paths"
                );
                eprintln!(
                    "  HARNX_BASH_EXTRA_WRITABLE   Colon-separated extra sandbox writable paths"
                );
                eprintln!(
                    "  HARNX_BASH_ENV_PASSTHROUGH  Comma-separated extra env var names to pass through"
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
        if path_is_executable(&path) {
            sandbox_config.sandbox_run_path = path;
        } else if sandbox_run_override.is_some() {
            anyhow::bail!(
                "harnx-mcp-bash: error: sandbox helper at {} does not exist or is not executable; fix --sandbox-run or pass --no-sandbox to disable sandboxing explicitly",
                path.display()
            );
        } else {
            anyhow::bail!(
                "harnx-mcp-bash: error: sandbox helper at {} does not exist or is not executable; place harnx-mcp-bash-sandbox-run next to harnx-mcp-bash, use --sandbox-run <path>, or pass --no-sandbox to disable sandboxing explicitly",
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
fn parse_env_passthrough() -> Vec<String> {
    std::env::var("HARNX_BASH_ENV_PASSTHROUGH")
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect()
}

#[cfg(not(unix))]
fn parse_args() -> anyhow::Result<(Vec<PathBuf>, server::SandboxConfig)> {
    let args: Vec<String> = std::env::args().collect();
    let mut roots = Vec::new();
    let mut sandbox_config = server::SandboxConfig {
        // Sandbox itself is Unix-only; on Windows these fields are unused.
        enabled: false,
        extra_exec: vec![],
        extra_readable: vec![],
        extra_writable: vec![],
        sandbox_run_path: PathBuf::from("harnx-mcp-bash-sandbox-run"),
        extra_env_passthrough: parse_env_passthrough(),
        env_overrides: vec![],
    };
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
            "--extra-read" => {
                if i + 1 < args.len() {
                    i += 2;
                } else {
                    eprintln!("harnx-mcp-bash: --extra-read requires a path argument");
                    std::process::exit(1);
                }
            }
            "--extra-exec" => {
                if i + 1 < args.len() {
                    i += 2;
                } else {
                    eprintln!("harnx-mcp-bash: --extra-exec requires a path argument");
                    std::process::exit(1);
                }
            }
            "--extra-write" => {
                if i + 1 < args.len() {
                    i += 2;
                } else {
                    eprintln!("harnx-mcp-bash: --extra-write requires a path argument");
                    std::process::exit(1);
                }
            }
            "--env" | "-e" => {
                if i + 1 < args.len() {
                    let raw = &args[i + 1];
                    if let Some((key, value)) = raw.split_once('=') {
                        if key.is_empty() {
                            eprintln!("harnx-mcp-bash: --env requires a non-empty variable name");
                            std::process::exit(1);
                        }
                        sandbox_config
                            .env_overrides
                            .push((key.to_string(), value.to_string()));
                    } else {
                        if raw.is_empty() {
                            eprintln!("harnx-mcp-bash: --env requires a non-empty variable name");
                            std::process::exit(1);
                        }
                        sandbox_config.extra_env_passthrough.push(raw.clone());
                    }
                    i += 2;
                } else {
                    eprintln!("harnx-mcp-bash: --env requires an argument");
                    std::process::exit(1);
                }
            }
            "--help" | "-h" => {
                eprintln!("harnx-mcp-bash: MCP shell command server");
                eprintln!();
                eprintln!("Usage: harnx-mcp-bash [OPTIONS]");
                eprintln!();
                eprintln!("Options:");
                eprintln!("  --root, -r <path>       Add an allowed root directory (repeatable)");
                eprintln!("  --extra-read <path> Accept sandbox read-only path flag (ignored on this platform)");
                eprintln!("  --extra-exec <path>     Accept sandbox execute path flag (ignored on this platform)");
                eprintln!("  --extra-write <path>    Accept sandbox writable path flag (ignored on this platform)");
                eprintln!("  --env, -e <VAR>         Pass VAR from host env to child (repeatable)");
                eprintln!("  --env, -e <VAR=VALUE>   Set VAR=VALUE in child env (repeatable)");
                eprintln!("  --help, -h              Show this help message");
                eprintln!();
                eprintln!("Environment:");
                eprintln!(
                    "  HARNX_BASH_ENV_PASSTHROUGH  Comma-separated extra env var names to pass through"
                );
                eprintln!();
                eprintln!("Sandboxing is Unix-only. On other platforms the child bash process");
                eprintln!("still receives only the curated environment built from the default");
                eprintln!("allowlist plus any --env / passthrough configuration.");
                eprintln!("The server communicates via stdio using the MCP protocol.");
                eprintln!("If no roots are specified, operations are denied until the client provides roots.");
                std::process::exit(0);
            }
            other => {
                eprintln!("harnx-mcp-bash: unknown argument: {}", other);
                eprintln!("Try: harnx-mcp-bash --help");
                std::process::exit(1);
            }
        }
    }

    Ok((roots, sandbox_config))
}

#[cfg(test)]
mod tests {
    use super::expand_tilde;
    use std::ffi::{OsStr, OsString};
    use std::sync::{Mutex, MutexGuard, OnceLock};

    fn env_lock() -> MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        match LOCK.get_or_init(|| Mutex::new(())).lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    struct EnvVar {
        key: String,
        prev: Option<OsString>,
    }

    impl EnvVar {
        fn set(key: &str, value: impl AsRef<OsStr>) -> Self {
            let prev = std::env::var_os(key);
            unsafe { std::env::set_var(key, value.as_ref()) };
            Self {
                key: key.to_string(),
                prev,
            }
        }
    }

    impl Drop for EnvVar {
        fn drop(&mut self) {
            unsafe {
                match self.prev.take() {
                    Some(value) => std::env::set_var(&self.key, value),
                    None => std::env::remove_var(&self.key),
                }
            }
        }
    }

    #[test]
    fn test_expand_tilde_replaces_prefix() {
        let _env_guard = env_lock();
        let _home = EnvVar::set("HOME", "/tmp/test-home");

        assert_eq!(expand_tilde("~/foo"), "/tmp/test-home/foo");
        assert_eq!(expand_tilde("~"), "/tmp/test-home");
        assert_eq!(expand_tilde("/abs/path"), "/abs/path");
    }
}
