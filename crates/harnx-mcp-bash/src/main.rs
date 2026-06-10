mod server;
#[cfg(test)]
mod test_support;

use harnx_sandbox_common::SandboxConfig;
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
fn parse_env_paths(var_name: &str, cwd: &Path) -> Vec<PathBuf> {
    std::env::var_os(var_name)
        .map(|value| {
            std::env::split_paths(&value)
                .filter(|path| !path.as_os_str().is_empty())
                .filter_map(|path| {
                    harnx_sandbox_common::expand_path_var(&path.to_string_lossy(), cwd)
                })
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

fn push_root(roots: &mut Vec<PathBuf>, raw: &str) {
    let raw = harnx_sandbox_common::expand_tilde(raw);
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
fn parse_args() -> anyhow::Result<(Vec<PathBuf>, SandboxConfig)> {
    let args: Vec<String> = std::env::args().collect();
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut roots = Vec::new();
    let mut sandbox_enabled = true;
    let mut sandbox_config = SandboxConfig {
        enabled: true,
        extra_exec: parse_env_paths("HARNX_BASH_EXTRA_EXEC", &cwd),
        extra_readable: parse_env_paths("HARNX_BASH_EXTRA_READABLE", &cwd),
        extra_writable: parse_env_paths("HARNX_BASH_EXTRA_WRITABLE", &cwd),
        extra_rwx: parse_env_paths("HARNX_BASH_EXTRA_RWX", &cwd),
        sandbox_run_path: PathBuf::from("harnx-sandbox-exec"),
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
                    if let Some(path) = harnx_sandbox_common::expand_path_var(&args[i + 1], &cwd) {
                        sandbox_config.extra_readable.push(path);
                    }
                    i += 2;
                } else {
                    eprintln!("harnx-mcp-bash: --extra-read requires a path argument");
                    std::process::exit(1);
                }
            }
            "--extra-exec" => {
                if i + 1 < args.len() {
                    if let Some(path) = harnx_sandbox_common::expand_path_var(&args[i + 1], &cwd) {
                        sandbox_config.extra_exec.push(path);
                    }
                    i += 2;
                } else {
                    eprintln!("harnx-mcp-bash: --extra-exec requires a path argument");
                    std::process::exit(1);
                }
            }
            "--extra-write" => {
                if i + 1 < args.len() {
                    if let Some(path) = harnx_sandbox_common::expand_path_var(&args[i + 1], &cwd) {
                        sandbox_config.extra_writable.push(path);
                    }
                    i += 2;
                } else {
                    eprintln!("harnx-mcp-bash: --extra-write requires a path argument");
                    std::process::exit(1);
                }
            }
            "--extra-rwx" => {
                if i + 1 < args.len() {
                    if let Some(path) = harnx_sandbox_common::expand_path_var(&args[i + 1], &cwd) {
                        sandbox_config.extra_rwx.push(path);
                    }
                    i += 2;
                } else {
                    eprintln!("harnx-mcp-bash: --extra-rwx requires a path argument");
                    std::process::exit(1);
                }
            }
            "--sandbox-run" => {
                if i + 1 < args.len() {
                    sandbox_run_override = Some(PathBuf::from(harnx_sandbox_common::expand_tilde(
                        &args[i + 1],
                    )));
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
                eprintln!(
                    "  --extra-rwx <path>      Add sandbox read/write/exec path (repeatable)"
                );
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
                    "  HARNX_BASH_EXTRA_RWX        Colon-separated extra sandbox read/write/exec paths"
                );
                eprintln!(
                    "  HARNX_BASH_ENV_PASSTHROUGH  Comma-separated extra env var names to pass through"
                );
                eprintln!();
                eprintln!("  $GIT_ROOT, $GIT_COMMON_DIR, $NODE_PROJECT_ROOT, $CARGO_ROOT, $GO_ROOT supported; resolved vs cwd, dropped if absent; any other $ENV_VAR resolves from environment, left literal if unset");
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
        std::env::current_exe()
            .ok()
            .and_then(|path| path.parent().map(|dir| dir.join("harnx-sandbox-exec")))
    });

    if sandbox_enabled {
        let path = resolved_sandbox_run_path.unwrap_or_else(|| PathBuf::from("harnx-sandbox-exec"));
        if path_is_executable(&path) {
            sandbox_config.sandbox_run_path = path;
        } else if sandbox_run_override.is_some() {
            anyhow::bail!(
                "harnx-mcp-bash: error: sandbox helper at {} does not exist or is not executable; fix --sandbox-run or pass --no-sandbox to disable sandboxing explicitly",
                path.display()
            );
        } else {
            anyhow::bail!(
                "harnx-mcp-bash: error: sandbox helper at {} does not exist or is not executable; place harnx-sandbox-exec next to harnx-mcp-bash, use --sandbox-run <path>, or pass --no-sandbox to disable sandboxing explicitly",
                path.display()
            );
        }
    } else {
        sandbox_config.enabled = false;
        sandbox_config.sandbox_run_path =
            resolved_sandbox_run_path.unwrap_or_else(|| PathBuf::from("harnx-sandbox-exec"));
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
fn parse_args() -> anyhow::Result<(Vec<PathBuf>, SandboxConfig)> {
    let args: Vec<String> = std::env::args().collect();
    let mut roots = Vec::new();
    let mut sandbox_config = SandboxConfig {
        // Sandbox itself is Unix-only; on Windows these fields are unused.
        enabled: false,
        extra_exec: vec![],
        extra_readable: vec![],
        extra_writable: vec![],
        extra_rwx: vec![],
        sandbox_run_path: PathBuf::from("harnx-sandbox-exec"),
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
            "--extra-rwx" => {
                if i + 1 < args.len() {
                    i += 2;
                } else {
                    eprintln!("harnx-mcp-bash: --extra-rwx requires a path argument");
                    std::process::exit(1);
                }
            }
            "--no-sandbox" => {
                i += 1;
            }
            "--sandbox-run" => {
                if i + 1 < args.len() {
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
                eprintln!("  --root, -r <path>       Add an allowed root directory (repeatable)");
                eprintln!("  --extra-read <path> Accept sandbox read-only path flag (ignored on this platform)");
                eprintln!("  --extra-exec <path>     Accept sandbox execute path flag (ignored on this platform)");
                eprintln!("  --extra-write <path>    Accept sandbox writable path flag (ignored on this platform)");
                eprintln!("  --extra-rwx <path>      Accept sandbox read/write/exec path flag (ignored on this platform)");
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
    #[cfg(unix)]
    use super::*;
    #[cfg(unix)]
    use crate::test_support::CwdGuard;
    use crate::test_support::{env_lock, EnvVar};

    #[test]
    fn test_expand_tilde_replaces_prefix() {
        let _env_guard = env_lock();
        let _home = EnvVar::set("HOME", "/tmp/test-home");

        assert_eq!(
            harnx_sandbox_common::expand_tilde("~/foo"),
            "/tmp/test-home/foo"
        );
        assert_eq!(harnx_sandbox_common::expand_tilde("~"), "/tmp/test-home");
        assert_eq!(harnx_sandbox_common::expand_tilde("/abs/path"), "/abs/path");
    }

    #[cfg(unix)]
    #[test]
    fn env_extra_rwx_git_root_resolves_inside_repo() {
        let _env_guard = env_lock();
        let _clear_read = EnvVar::unset("HARNX_BASH_EXTRA_READABLE");
        let _clear_write = EnvVar::unset("HARNX_BASH_EXTRA_WRITABLE");
        let _clear_exec = EnvVar::unset("HARNX_BASH_EXTRA_EXEC");
        let _clear_rwx = EnvVar::unset("HARNX_BASH_EXTRA_RWX");
        let manifest_dir =
            PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("manifest dir"));
        let _cwd = CwdGuard::set(&manifest_dir);
        let _extra = EnvVar::set("HARNX_BASH_EXTRA_RWX", "$GIT_ROOT");
        let repo_root = harnx_sandbox_common::detect_project_root(
            harnx_sandbox_common::RootKind::GitRoot,
            &manifest_dir,
        )
        .expect("git root");

        assert_eq!(
            parse_env_paths("HARNX_BASH_EXTRA_RWX", &manifest_dir),
            vec![repo_root]
        );
    }

    #[cfg(unix)]
    #[test]
    fn env_extra_git_root_is_dropped_outside_repo() {
        let _env_guard = env_lock();
        let _clear_read = EnvVar::unset("HARNX_BASH_EXTRA_READABLE");
        let _clear_write = EnvVar::unset("HARNX_BASH_EXTRA_WRITABLE");
        let _clear_exec = EnvVar::unset("HARNX_BASH_EXTRA_EXEC");
        let _clear_rwx = EnvVar::unset("HARNX_BASH_EXTRA_RWX");
        let temp = tempfile::tempdir().expect("tempdir");
        if harnx_sandbox_common::detect_project_root(
            harnx_sandbox_common::RootKind::GitRoot,
            temp.path().parent().unwrap_or(temp.path()),
        )
        .is_some()
        {
            return;
        }
        let _cwd = CwdGuard::set(temp.path());
        let _extra = EnvVar::set("HARNX_BASH_EXTRA_RWX", "$GIT_ROOT");

        assert!(parse_env_paths("HARNX_BASH_EXTRA_RWX", temp.path()).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn env_extra_paths_keep_tilde_and_literal_behavior() {
        let _env_guard = env_lock();
        let _clear_read = EnvVar::unset("HARNX_BASH_EXTRA_READABLE");
        let _clear_write = EnvVar::unset("HARNX_BASH_EXTRA_WRITABLE");
        let _clear_exec = EnvVar::unset("HARNX_BASH_EXTRA_EXEC");
        let _clear_rwx = EnvVar::unset("HARNX_BASH_EXTRA_RWX");
        let temp = tempfile::tempdir().expect("tempdir");
        let home = temp.path().join("home");
        std::fs::create_dir_all(&home).expect("create home");
        let _cwd = CwdGuard::set(temp.path());
        let _home = EnvVar::set("HOME", &home);
        let _read = EnvVar::set(
            "HARNX_BASH_EXTRA_READABLE",
            std::ffi::OsString::from(format!("{}:{}", "~/foo", "/abs")),
        );

        assert_eq!(
            parse_env_paths("HARNX_BASH_EXTRA_READABLE", temp.path()),
            vec![home.join("foo"), PathBuf::from("/abs")]
        );
    }
}
