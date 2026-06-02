use std::ffi::OsString;

use crate::config::SandboxConfig;

#[cfg(unix)]
use crate::defaults::{
    push_env_relative_defaults, push_home_relative_defaults, system_writable_paths,
    SYSTEM_EXEC_PATHS, SYSTEM_READ_PATHS,
};
#[cfg(unix)]
use std::path::{Path, PathBuf};

#[cfg(unix)]
fn push_flagged_path(args: &mut Vec<OsString>, flag: &str, path: &Path) {
    args.push(OsString::from(flag));
    args.push(path.as_os_str().to_os_string());
}

/// Best-effort name of the running executable, for use in warning messages.
/// Falls back to a generic name when argv[0] is unavailable.
#[cfg(unix)]
fn program_name() -> String {
    std::env::args()
        .next()
        .and_then(|p| {
            Path::new(&p)
                .file_name()
                .and_then(|s| s.to_str().map(|s| s.to_owned()))
        })
        .unwrap_or_else(|| "harnx-sandbox".to_string())
}

pub fn build_default_sandbox_args(config: &SandboxConfig) -> Vec<OsString> {
    #[cfg(unix)]
    {
        let mut args = Vec::new();
        let prog = program_name();
        for path in SYSTEM_EXEC_PATHS {
            args.push(OsString::from("--exec"));
            args.push(OsString::from(path));
        }
        for path in SYSTEM_READ_PATHS {
            args.push(OsString::from("--read"));
            args.push(OsString::from(path));
        }
        for path in system_writable_paths() {
            push_flagged_path(&mut args, "--write", &path);
        }
        if let Some(home) = std::env::var_os("HOME") {
            push_home_relative_defaults(&mut args, &PathBuf::from(home));
        }
        push_env_relative_defaults(&mut args);
        for path in &config.extra_exec {
            if crate::is_home_or_ancestor(path) {
                eprintln!(
                    "{prog}: warning: ignoring --extra-exec {}: would expose home directory",
                    path.display()
                );
                continue;
            }
            push_flagged_path(&mut args, "--exec", path);
        }
        for path in &config.extra_readable {
            if crate::is_home_or_ancestor(path) {
                eprintln!(
                    "{prog}: warning: ignoring --extra-read {}: would expose home directory",
                    path.display()
                );
                continue;
            }
            push_flagged_path(&mut args, "--read", path);
        }
        for path in &config.extra_writable {
            if crate::is_home_or_ancestor(path) {
                eprintln!(
                    "{prog}: warning: ignoring --extra-write {}: would expose home directory",
                    path.display()
                );
                continue;
            }
            push_flagged_path(&mut args, "--write", path);
        }
        for path in &config.extra_rwx {
            if crate::is_home_or_ancestor(path) {
                eprintln!(
                    "{prog}: warning: ignoring --extra-rwx {}: would expose home directory",
                    path.display()
                );
                continue;
            }
            push_flagged_path(&mut args, "--read", path);
            push_flagged_path(&mut args, "--write", path);
            push_flagged_path(&mut args, "--exec", path);
        }
        args
    }
    #[cfg(not(unix))]
    {
        let _ = config;
        Vec::new()
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::test_support::{env_lock, EnvGuard};
    use anyhow::Result;
    use std::path::PathBuf;

    fn test_config() -> SandboxConfig {
        SandboxConfig {
            enabled: true,
            extra_exec: vec![PathBuf::from("/exec")],
            extra_readable: vec![PathBuf::from("/read")],
            extra_writable: vec![PathBuf::from("/write")],
            extra_rwx: vec![PathBuf::from("/rwx")],
            extra_env_passthrough: Vec::new(),
            env_overrides: Vec::new(),
            sandbox_run_path: PathBuf::from("sandbox-run"),
        }
    }

    fn ensure_anyhow_is_linked() -> Result<()> {
        Ok(())
    }

    #[test]
    fn appends_extra_paths_to_defaults() {
        ensure_anyhow_is_linked().expect("anyhow helper should succeed");

        let args = build_default_sandbox_args(&test_config());
        let args: Vec<String> = args
            .into_iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();

        assert!(args.windows(2).any(|w| w == ["--exec", "/exec"]));
        assert!(args.windows(2).any(|w| w == ["--read", "/read"]));
        assert!(args.windows(2).any(|w| w == ["--write", "/write"]));
        assert!(args.windows(2).any(|w| w == ["--read", "/rwx"]));
        assert!(args.windows(2).any(|w| w == ["--write", "/rwx"]));
        assert!(args.windows(2).any(|w| w == ["--exec", "/rwx"]));
    }

    #[test]
    fn drops_home_extra_writable_and_rwx_paths() {
        let _lock = env_lock();
        let _env = EnvGuard::new();
        let temp = tempfile::tempdir().expect("tempdir");
        let home = temp.path().join("home");
        std::fs::create_dir_all(&home).expect("create home");
        unsafe { std::env::set_var("HOME", &home) };

        let rwx = home.clone();
        let writable = home.clone();
        let config = SandboxConfig {
            enabled: true,
            extra_exec: Vec::new(),
            extra_readable: Vec::new(),
            extra_writable: vec![writable.clone()],
            extra_rwx: vec![rwx.clone()],
            extra_env_passthrough: Vec::new(),
            env_overrides: Vec::new(),
            sandbox_run_path: PathBuf::from("sandbox-run"),
        };

        let args: Vec<String> = build_default_sandbox_args(&config)
            .into_iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        let writable = writable.to_string_lossy().into_owned();
        let rwx = rwx.to_string_lossy().into_owned();

        assert!(!args.windows(2).any(|w| w == ["--write", writable.as_str()]));
        assert!(!args.windows(2).any(|w| w == ["--read", rwx.as_str()]));
        assert!(!args.windows(2).any(|w| w == ["--write", rwx.as_str()]));
        assert!(!args.windows(2).any(|w| w == ["--exec", rwx.as_str()]));
    }
}
