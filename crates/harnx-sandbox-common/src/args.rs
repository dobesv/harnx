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

pub fn build_default_sandbox_args(config: &SandboxConfig) -> Vec<OsString> {
    #[cfg(unix)]
    {
        let mut args = Vec::new();
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
            push_flagged_path(&mut args, "--exec", path);
        }
        for path in &config.extra_readable {
            push_flagged_path(&mut args, "--read", path);
        }
        for path in &config.extra_writable {
            push_flagged_path(&mut args, "--write", path);
        }
        for path in &config.extra_rwx {
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

#[cfg(test)]
mod tests {
    use super::*;
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
}
