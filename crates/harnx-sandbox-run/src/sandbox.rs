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
        expand_path_var, is_home_or_ancestor, push_env_relative_defaults, resolve_path,
        system_writable_paths, HOME_EXEC_PATHS, HOME_READ_PATHS, HOME_RWX_PATHS, HOME_WRITE_PATHS,
        SYSTEM_EXEC_PATHS, SYSTEM_READ_PATHS,
    };

    let mut args: Vec<OsString> = Vec::new();
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));

    // Default whitelist — same paths that harnx-bash-tools uses
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
                    // Emit --read alongside --write to match the shared default
                    // logic in harnx-sandbox-common::push_home_relative_defaults.
                    args.push("--read".into());
                    args.push(p.clone().into_os_string());
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
        push_env_relative_defaults(&mut args);
    }

    // CLI-provided extra paths
    for path in &cli.allow_read {
        let raw = path.to_string_lossy();
        let Some(expanded) = expand_path_var(&raw, &cwd) else {
            continue;
        };
        let resolved = resolve_path(&expanded);
        if is_home_or_ancestor(&resolved) {
            eprintln!(
                "harnx-sandbox-run: warning: ignoring --allow-read {}: would expose home directory",
                path.display()
            );
            continue;
        }
        args.push("--read".into());
        args.push(resolved.into_os_string());
    }
    for path in &cli.allow_write {
        let raw = path.to_string_lossy();
        let Some(expanded) = expand_path_var(&raw, &cwd) else {
            continue;
        };
        let resolved = resolve_path(&expanded);
        if is_home_or_ancestor(&resolved) {
            eprintln!(
                "harnx-sandbox-run: warning: ignoring --allow-write {}: would expose home directory",
                path.display()
            );
            continue;
        }
        args.push("--write".into());
        args.push(resolved.into_os_string());
    }
    for path in &cli.allow_exec {
        let raw = path.to_string_lossy();
        let Some(expanded) = expand_path_var(&raw, &cwd) else {
            continue;
        };
        let resolved = resolve_path(&expanded);
        if is_home_or_ancestor(&resolved) {
            eprintln!(
                "harnx-sandbox-run: warning: ignoring --allow-exec {}: would expose home directory",
                path.display()
            );
            continue;
        }
        args.push("--exec".into());
        args.push(resolved.into_os_string());
    }
    for path in &cli.allow_rwx {
        let raw = path.to_string_lossy();
        let Some(expanded) = expand_path_var(&raw, &cwd) else {
            continue;
        };
        let resolved = resolve_path(&expanded);
        if is_home_or_ancestor(&resolved) {
            eprintln!(
                "harnx-sandbox-run: warning: ignoring --allow-rwx {}: would expose home directory",
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
        "HOME",
        "USER",
        "LOGNAME",
        "SHELL",
        "TERM",
        "LANG",
        "LC_ALL",
        "LC_CTYPE",
        "PATH",
        "TMPDIR",
        // Forward Go cache locations so sandboxed `go` honors custom cache dirs
        // that were also whitelisted by push_env_relative_defaults().
        "GOMODCACHE",
        "GOCACHE",
    ];

    let mut env: Vec<(String, String)> = Vec::new();

    // 1. Baseline passthrough
    for name in DEFAULT_PASSTHROUGH {
        if let Ok(value) = std::env::var(name) {
            env.push((name.to_string(), value));
        }
    }
    // Safe XDG Base Directory Specification vars only (deny-by-default whitelist).
    for name in harnx_sandbox_common::SAFE_XDG_VARS {
        if let Ok(value) = std::env::var(name) {
            if !env.iter().any(|(k, _)| k == name) {
                env.push(((*name).to_string(), value));
            }
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
    use std::sync::{Mutex, OnceLock};

    fn env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    struct EnvGuard {
        saved_home: Option<std::ffi::OsString>,
        saved_cwd: PathBuf,
    }

    impl EnvGuard {
        fn new() -> Self {
            Self {
                saved_home: std::env::var_os("HOME"),
                saved_cwd: std::env::current_dir().expect("current_dir"),
            }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            let _ = std::env::set_current_dir(&self.saved_cwd);
            match &self.saved_home {
                Some(home) => unsafe { std::env::set_var("HOME", home) },
                None => unsafe { std::env::remove_var("HOME") },
            }
        }
    }

    #[test]
    fn collect_env_hook_overrides_cli() {
        let cli = Cli {
            env_vars: vec!["MYVAR=from_cli".to_string()],
            allow_read: vec![],
            allow_write: vec![],
            allow_exec: vec![],
            allow_rwx: vec![],
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
            allow_read: vec![],
            allow_write: vec![],
            allow_exec: vec![],
            allow_rwx: vec![],
            no_network: false,
            working_dir: None,
            no_defaults: false,
            command: vec![],
        };
        let (_, keys) = collect_env(&cli, HashMap::new());
        assert!(keys.contains(&"HOME".to_string()));
    }

    #[test]
    fn collect_env_whitelists_safe_xdg_vars() {
        // Serialize against other env-mutating tests in this binary.
        let _lock = env_lock().lock().expect("lock poisoned");

        // Save + restore the XDG vars this test mutates, regardless of outcome.
        struct XdgGuard([(&'static str, Option<std::ffi::OsString>); 3]);
        impl Drop for XdgGuard {
            fn drop(&mut self) {
                for (name, saved) in &self.0 {
                    match saved {
                        Some(v) => unsafe { std::env::set_var(name, v) },
                        None => unsafe { std::env::remove_var(name) },
                    }
                }
            }
        }
        let _guard = XdgGuard([
            ("XDG_CONFIG_HOME", std::env::var_os("XDG_CONFIG_HOME")),
            ("XDG_RUNTIME_DIR", std::env::var_os("XDG_RUNTIME_DIR")),
            ("XDG_SESSION_ID", std::env::var_os("XDG_SESSION_ID")),
        ]);

        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", "/tmp/xdg-config");
            std::env::set_var("XDG_RUNTIME_DIR", "/run/user/9999");
            std::env::set_var("XDG_SESSION_ID", "c-test");
        }

        let cli = Cli {
            env_vars: vec![],
            allow_read: vec![],
            allow_write: vec![],
            allow_exec: vec![],
            allow_rwx: vec![],
            no_network: false,
            working_dir: None,
            no_defaults: false,
            command: vec![],
        };
        let (env, _keys) = collect_env(&cli, HashMap::new());
        let has = |k: &str| env.iter().any(|(name, _)| name == k);

        // Base-directory spec var is forwarded.
        assert!(
            has("XDG_CONFIG_HOME"),
            "XDG_CONFIG_HOME should pass through"
        );
        // The keyring/DBus-locating runtime dir is NOT forwarded.
        assert!(
            !has("XDG_RUNTIME_DIR"),
            "XDG_RUNTIME_DIR must be excluded from the sandbox env"
        );
        // Desktop-session plumbing is NOT forwarded.
        assert!(
            !has("XDG_SESSION_ID"),
            "XDG_SESSION_ID must not be forwarded"
        );
    }

    #[test]
    fn build_exec_args_filters_home_and_resolves_allowed_paths() {
        let _lock = env_lock().lock().expect("lock poisoned");
        let _env = EnvGuard::new();
        let temp = tempfile::tempdir().expect("tempdir");
        let cwd = temp.path().join("cwd");
        let home = temp.path().join("home");
        let child = home.join("projects");
        std::fs::create_dir_all(&cwd).expect("create cwd");
        std::fs::create_dir_all(&child).expect("create child");

        std::env::set_current_dir(&cwd).expect("set cwd");
        unsafe { std::env::set_var("HOME", &home) };

        let cli = Cli {
            env_vars: vec![],
            allow_read: vec![PathBuf::from(".")],
            allow_write: vec![home.clone()],
            allow_exec: vec![PathBuf::from("/")],
            allow_rwx: vec![child.clone()],
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
    }

    #[test]
    fn build_exec_args_skips_home_expanded_from_env_var() {
        let _lock = env_lock().lock().expect("lock poisoned");
        let _env = EnvGuard::new();
        let temp = tempfile::tempdir().expect("tempdir");
        let cwd = temp.path().join("cwd");
        let home = temp.path().join("home");
        std::fs::create_dir_all(&cwd).expect("create cwd");
        std::fs::create_dir_all(&home).expect("create home");

        std::env::set_current_dir(&cwd).expect("set cwd");
        unsafe { std::env::set_var("HOME", &home) };
        unsafe { std::env::set_var("HARNX_TEST_HOME_VAR", &home) };

        let cli = Cli {
            env_vars: vec![],
            allow_read: vec![],
            allow_write: vec![PathBuf::from("$HARNX_TEST_HOME_VAR")],
            allow_exec: vec![],
            allow_rwx: vec![],
            no_network: false,
            working_dir: None,
            no_defaults: false,
            command: vec![],
        };

        let args: Vec<String> = build_exec_args(&cli, &[], false)
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        let home = std::fs::canonicalize(&home)
            .unwrap_or_else(|_| home.clone())
            .to_string_lossy()
            .into_owned();

        assert!(!args.windows(2).any(|w| w == ["--write", home.as_str()]));
        assert_eq!(args, vec!["--".to_string()]);

        unsafe { std::env::remove_var("HARNX_TEST_HOME_VAR") };
    }

    #[test]
    fn build_exec_args_expands_git_root_rwx_inside_repo() {
        let _lock = env_lock().lock().expect("lock poisoned");
        let _env = EnvGuard::new();
        let manifest_dir =
            PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
        std::env::set_current_dir(&manifest_dir).expect("set cwd");
        let repo_root = harnx_sandbox_common::detect_project_root(
            harnx_sandbox_common::RootKind::GitRoot,
            &manifest_dir,
        )
        .expect("git root");
        let expected = harnx_sandbox_common::resolve_path(&repo_root)
            .to_string_lossy()
            .into_owned();

        let cli = Cli {
            env_vars: vec![],
            allow_read: vec![],
            allow_write: vec![],
            allow_exec: vec![],
            allow_rwx: vec![PathBuf::from("$GIT_ROOT")],
            no_network: false,
            working_dir: None,
            no_defaults: false,
            command: vec![],
        };

        let args: Vec<String> = build_exec_args(&cli, &[], false)
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();

        assert_eq!(
            args,
            vec![
                "--read".to_string(),
                expected.clone(),
                "--write".to_string(),
                expected.clone(),
                "--exec".to_string(),
                expected,
                "--".to_string(),
            ]
        );
    }

    #[test]
    fn build_exec_args_skips_git_root_rwx_outside_repo() {
        let _lock = env_lock().lock().expect("lock poisoned");
        let _env = EnvGuard::new();
        let temp = tempfile::tempdir().expect("tempdir");
        if harnx_sandbox_common::detect_project_root(
            harnx_sandbox_common::RootKind::GitRoot,
            temp.path().parent().unwrap_or(temp.path()),
        )
        .is_some()
        {
            return;
        }
        std::env::set_current_dir(temp.path()).expect("set cwd");

        let cli = Cli {
            env_vars: vec![],
            allow_read: vec![],
            allow_write: vec![],
            allow_exec: vec![],
            allow_rwx: vec![PathBuf::from("$GIT_ROOT")],
            no_network: false,
            working_dir: None,
            no_defaults: false,
            command: vec![],
        };

        let args: Vec<String> = build_exec_args(&cli, &[], false)
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();

        assert_eq!(args, vec!["--".to_string()]);
    }
}
