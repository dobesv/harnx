//! Sandbox setup delegated to `harnx-sandbox-exec`.
//!
//! Instead of calling birdcage directly (which requires single-threaded context),
//! this module spawns `harnx-sandbox-exec` as a subprocess. This allows the parent
//! process to keep the tokio runtime — and sidecar hooks like `harnx-proxy-auth` —
//! alive for the full duration of the sandboxed command.
//!
//! ## Env var protocol
//!
//! All env vars (defaults, CLI `--env`, hook-injected) are set on the **ambient
//! environment** of the `harnx-sandbox-exec` subprocess via `std::process::Command::env()`.
//! The CLI args to `harnx-sandbox-exec` only carry `--env VAR` (name-only) entries
//! to tell it which vars to whitelist into the sandbox. Values never appear in the
//! process argument list, preventing secrets from leaking into `ps`/`/proc`.

use std::collections::HashMap;
#[cfg(unix)]
use std::ffi::OsString;
#[cfg(unix)]
use std::path::PathBuf;

use anyhow::Result;

use crate::cli::Cli;

/// Find `harnx-sandbox-exec` next to the current executable, falling back to PATH.
fn find_sandbox_exec() -> PathBuf {
    if let Ok(current) = std::env::current_exe() {
        if let Some(dir) = current.parent() {
            let candidate = dir.join("harnx-sandbox-exec");
            if candidate.exists() {
                return candidate;
            }
        }
    }
    PathBuf::from("harnx-sandbox-exec")
}

/// Build the argv for `harnx-sandbox-exec` from CLI flags and the default whitelist.
///
/// Env var *names* are passed as `--env VAR` (no value — `harnx-sandbox-exec` reads
/// them from its ambient environment, which the caller sets via `.env()`).
#[cfg(unix)]
fn build_exec_args(cli: &Cli, all_env_keys: &[String], use_defaults: bool) -> Vec<OsString> {
    use harnx_sandbox_common::{
        is_home_or_ancestor, resolve_path, system_writable_paths, HOME_EXEC_PATHS, HOME_READ_PATHS,
        HOME_RWX_PATHS, HOME_WRITE_PATHS, SYSTEM_EXEC_PATHS, SYSTEM_READ_PATHS,
    };

    let mut args: Vec<OsString> = Vec::new();

    // Default whitelist — same paths that harnx-mcp-bash uses
    if use_defaults {
        for path in SYSTEM_EXEC_PATHS {
            let p = std::path::PathBuf::from(path);
            if p.exists() {
                args.push("--exec".into());
                args.push(p.into_os_string());
            }
        }
        for path in SYSTEM_READ_PATHS {
            let p = std::path::PathBuf::from(path);
            if p.exists() {
                args.push("--read".into());
                args.push(p.into_os_string());
            }
        }
        for p in system_writable_paths() {
            if p.exists() {
                args.push("--write".into());
                args.push(p.into_os_string());
            }
        }
        if let Some(home) = std::env::var_os("HOME") {
            let home = std::path::PathBuf::from(home);
            for sub in HOME_READ_PATHS {
                let p = home.join(sub);
                if p.exists() {
                    args.push("--read".into());
                    args.push(p.into_os_string());
                }
            }
            for sub in HOME_EXEC_PATHS {
                let p = home.join(sub);
                if p.exists() {
                    args.push("--exec".into());
                    args.push(p.into_os_string());
                }
            }
            for sub in HOME_WRITE_PATHS {
                let p = home.join(sub);
                if p.exists() {
                    args.push("--write".into());
                    args.push(p.into_os_string());
                }
            }
            for sub in HOME_RWX_PATHS {
                let p = home.join(sub);
                if p.exists() {
                    args.push("--read".into());
                    args.push(p.clone().into_os_string());
                    args.push("--write".into());
                    args.push(p.clone().into_os_string());
                    args.push("--exec".into());
                    args.push(p.into_os_string());
                }
            }
        }
        // CARGO_HOME, GOROOT, GOPATH, GOBIN
        if let Some(cargo_home) = std::env::var_os("CARGO_HOME") {
            let bin = std::path::PathBuf::from(cargo_home).join("bin");
            if bin.exists() {
                args.push("--exec".into());
                args.push(bin.into_os_string());
            }
        }
        if let Some(goroot) = std::env::var_os("GOROOT") {
            let p = std::path::PathBuf::from(goroot);
            if p.exists() {
                args.push("--exec".into());
                args.push(p.into_os_string());
            }
        }
        if let Some(gopath) = std::env::var_os("GOPATH") {
            let gopath = std::path::PathBuf::from(gopath);
            let bin = gopath.join("bin");
            let pkg = gopath.join("pkg");
            if bin.exists() {
                args.push("--exec".into());
                args.push(bin.into_os_string());
            }
            if pkg.exists() {
                args.push("--read".into());
                args.push(pkg.clone().into_os_string());
                args.push("--write".into());
                args.push(pkg.into_os_string());
            }
        }
        if let Some(gobin) = std::env::var_os("GOBIN") {
            let p = std::path::PathBuf::from(gobin);
            if p.exists() {
                args.push("--exec".into());
                args.push(p.into_os_string());
            }
        }
    }

    // CLI-provided extra paths
    for path in &cli.extra_read {
        let resolved = resolve_path(path);
        if is_home_or_ancestor(&resolved) {
            eprintln!(
                "harnx-sandbox-run: warning: ignoring --extra-read {}: would expose home directory",
                path.display()
            );
            continue;
        }
        args.push("--read".into());
        args.push(resolved.into_os_string());
    }
    for path in &cli.extra_write {
        let resolved = resolve_path(path);
        if is_home_or_ancestor(&resolved) {
            eprintln!(
                "harnx-sandbox-run: warning: ignoring --extra-write {}: would expose home directory",
                path.display()
            );
            continue;
        }
        args.push("--write".into());
        args.push(resolved.into_os_string());
    }
    for path in &cli.extra_exec {
        let resolved = resolve_path(path);
        if is_home_or_ancestor(&resolved) {
            eprintln!(
                "harnx-sandbox-run: warning: ignoring --extra-exec {}: would expose home directory",
                path.display()
            );
            continue;
        }
        args.push("--exec".into());
        args.push(resolved.into_os_string());
    }
    for path in &cli.extra_rwx {
        let resolved = resolve_path(path);
        if is_home_or_ancestor(&resolved) {
            eprintln!(
                "harnx-sandbox-run: warning: ignoring --extra-rwx {}: would expose home directory",
                path.display()
            );
            continue;
        }
        args.push("--read".into());
        args.push(resolved.clone().into_os_string());
        args.push("--write".into());
        args.push(resolved.clone().into_os_string());
        args.push("--exec".into());
        args.push(resolved.into_os_string());
    }

    if cli.no_network {
        args.push("--no-network".into());
    }

    if let Some(dir) = &cli.working_dir {
        args.push("--working-dir".into());
        args.push(dir.clone().into_os_string());
    }

    // Env var names — values are in the ambient env, not in args
    for key in all_env_keys {
        args.push("--env".into());
        args.push(key.clone().into());
    }

    args.push("--".into());
    args.extend(cli.command.iter().cloned());

    args
}

/// Collect all env vars to pass through, returning (key, value) pairs for the
/// ambient environment and the list of key names for `--env` args.
fn collect_env(
    cli: &Cli,
    hook_env: HashMap<String, String>,
) -> (Vec<(String, String)>, Vec<String>) {
    const DEFAULT_PASSTHROUGH: &[&str] = &[
        "HOME", "USER", "LOGNAME", "SHELL", "TERM", "LANG", "LC_ALL", "LC_CTYPE", "PATH", "TMPDIR",
    ];

    let mut env: Vec<(String, String)> = Vec::new();

    // 1. Baseline passthrough
    for name in DEFAULT_PASSTHROUGH {
        if let Ok(value) = std::env::var(name) {
            env.push((name.to_string(), value));
        }
    }
    // XDG_*
    for (name, value) in std::env::vars() {
        if name.starts_with("XDG_") && !env.iter().any(|(k, _)| k == &name) {
            env.push((name, value));
        }
    }

    // 2. CLI --env (override defaults)
    for raw in &cli.env_vars {
        if let Some((key, value)) = raw.split_once('=') {
            env.retain(|(k, _)| k != key);
            env.push((key.to_string(), value.to_string()));
        } else if let Ok(value) = std::env::var(raw) {
            env.retain(|(k, _)| k != raw.as_str());
            env.push((raw.clone(), value));
        }
    }

    // 3. Hook env (highest priority — never appears in args)
    for (key, value) in hook_env {
        env.retain(|(k, _)| k != &key);
        env.push((key, value));
    }

    let keys: Vec<String> = env.iter().map(|(k, _)| k.clone()).collect();
    (env, keys)
}

/// Spawn `harnx-sandbox-exec` and wait for it to exit.
pub fn setup_and_spawn(
    cli: &Cli,
    hook_env: HashMap<String, String>,
    use_defaults: bool,
) -> Result<i32> {
    let sandbox_exec = find_sandbox_exec();

    let (env_pairs, env_keys) = collect_env(cli, hook_env);

    #[cfg(unix)]
    let exec_args = build_exec_args(cli, &env_keys, use_defaults);

    #[cfg(not(unix))]
    {
        let _ = (sandbox_exec, env_pairs, env_keys, use_defaults);
        anyhow::bail!("harnx-sandbox-run is only supported on Unix platforms");
    }

    #[cfg(unix)]
    {
        let status = std::process::Command::new(&sandbox_exec)
            .args(&exec_args)
            // Set all env vars as ambient environment — values never in args
            .env_clear()
            .envs(env_pairs.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .status()
            .map_err(|e| {
                anyhow::anyhow!(
                    "failed to spawn {}: {e} — is harnx-sandbox-exec installed?",
                    sandbox_exec.display()
                )
            })?;

        Ok(status.code().unwrap_or(1))
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn collect_env_hook_overrides_cli() {
        let cli = Cli {
            env_vars: vec!["MYVAR=from_cli".to_string()],
            extra_read: vec![],
            extra_write: vec![],
            extra_exec: vec![],
            extra_rwx: vec![],
            no_network: false,
            working_dir: None,
            no_defaults: false,
            command: vec![],
        };
        let hook_env = HashMap::from([("MYVAR".to_string(), "from_hook".to_string())]);
        let (env, keys) = collect_env(&cli, hook_env);
        let val = env
            .iter()
            .find(|(k, _)| k == "MYVAR")
            .map(|(_, v)| v.as_str());
        assert_eq!(val, Some("from_hook"));
        assert!(keys.contains(&"MYVAR".to_string()));
    }

    #[test]
    fn collect_env_no_values_in_keys_only() {
        // Keys list must contain the key name — the test just checks it's present
        let cli = Cli {
            env_vars: vec!["HOME".to_string()],
            extra_read: vec![],
            extra_write: vec![],
            extra_exec: vec![],
            extra_rwx: vec![],
            no_network: false,
            working_dir: None,
            no_defaults: false,
            command: vec![],
        };
        let (_, keys) = collect_env(&cli, HashMap::new());
        assert!(keys.contains(&"HOME".to_string()));
    }

    #[test]
    fn build_exec_args_filters_home_and_resolves_allowed_paths() {
        let temp = tempfile::tempdir().expect("tempdir");
        let cwd = temp.path().join("cwd");
        let home = temp.path().join("home");
        let child = home.join("projects");
        std::fs::create_dir_all(&cwd).expect("create cwd");
        std::fs::create_dir_all(&child).expect("create child");

        let old_cwd = std::env::current_dir().expect("current_dir");
        std::env::set_current_dir(&cwd).expect("set cwd");
        unsafe { std::env::set_var("HOME", &home) };

        let cli = Cli {
            env_vars: vec![],
            extra_read: vec![PathBuf::from(".")],
            extra_write: vec![home.clone()],
            extra_exec: vec![PathBuf::from("/")],
            extra_rwx: vec![child.clone()],
            no_network: false,
            working_dir: None,
            no_defaults: false,
            command: vec![],
        };

        let args = build_exec_args(&cli, &[], false);
        let args: Vec<String> = args
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();

        // Use canonicalize for expected paths: on macOS /var/… resolves to
        // /private/var/… through a symlink, matching what resolve_path returns.
        let canon = |p: &std::path::Path| {
            std::fs::canonicalize(p)
                .unwrap_or_else(|_| p.to_path_buf())
                .to_string_lossy()
                .into_owned()
        };
        assert_eq!(
            args,
            vec![
                "--read".to_string(),
                canon(&cwd),
                "--read".to_string(),
                canon(&child),
                "--write".to_string(),
                canon(&child),
                "--exec".to_string(),
                canon(&child),
                "--".to_string(),
            ]
        );

        std::env::set_current_dir(old_cwd).expect("restore cwd");
    }
}
