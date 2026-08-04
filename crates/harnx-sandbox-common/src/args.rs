use crate::config::SandboxConfig;
use std::ffi::OsString;

#[cfg(unix)]
use std::path::Path;

#[cfg(unix)]
fn push_flagged_path(args: &mut Vec<OsString>, flag: &str, path: &Path) {
    args.push(OsString::from(flag));
    args.push(path.as_os_str().to_os_string());
}

/// Build sandbox filesystem arguments directly from the resolved allowlist.
///
/// No paths are granted implicitly. An empty allowlist emits no filesystem
/// flags, leaving the sandbox deny-all.
pub fn build_default_sandbox_args(config: &SandboxConfig) -> Vec<OsString> {
    #[cfg(unix)]
    {
        let mut args = Vec::new();
        for path in config.allowlist.read_paths() {
            push_flagged_path(&mut args, "--read", path);
        }
        for path in config.allowlist.write_paths() {
            push_flagged_path(&mut args, "--write", path);
        }
        for path in config.allowlist.exec_paths() {
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
    use harnx_tool_allow::{resolve_allowlist, AllowEnv, AllowInputs, ResolvedAllowlist};
    use std::path::PathBuf;
    use std::sync::Arc;

    fn config(allowlist: ResolvedAllowlist) -> SandboxConfig {
        SandboxConfig {
            enabled: true,
            allowlist: Arc::new(allowlist),
            extra_env_passthrough: Vec::new(),
            env_overrides: Vec::new(),
            sandbox_run_path: PathBuf::from("sandbox-run"),
        }
    }

    fn string_args(config: &SandboxConfig) -> Vec<String> {
        build_default_sandbox_args(config)
            .into_iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn empty_allowlist_emits_no_filesystem_grants() {
        assert!(build_default_sandbox_args(&config(ResolvedAllowlist::new())).is_empty());
    }

    #[test]
    fn emits_resolved_permission_sets() {
        let mut allowlist = ResolvedAllowlist::new();
        allowlist.insert_read("/read");
        allowlist.insert_write("/write");
        allowlist.insert_exec("/exec");
        allowlist.insert_rwx("/rwx");
        let args = string_args(&config(allowlist));

        assert!(args.windows(2).any(|w| w == ["--read", "/read"]));
        assert!(args.windows(2).any(|w| w == ["--write", "/write"]));
        assert!(args.windows(2).any(|w| w == ["--exec", "/exec"]));
        assert!(args.windows(2).any(|w| w == ["--read", "/rwx"]));
        assert!(args.windows(2).any(|w| w == ["--write", "/rwx"]));
        assert!(args.windows(2).any(|w| w == ["--exec", "/rwx"]));
    }

    #[test]
    fn common_default_batch_emits_system_paths() {
        let inputs = AllowInputs {
            common_default: true,
            ..AllowInputs::default()
        };
        let allowlist = resolve_allowlist(&inputs, Path::new("/workspace"), &AllowEnv::default());
        let args = string_args(&config(allowlist));

        #[cfg(target_os = "linux")]
        {
            assert!(args.windows(2).any(|w| w == ["--read", "/usr/bin"]));
            assert!(args.windows(2).any(|w| w == ["--exec", "/usr/bin"]));
            assert!(args.windows(2).any(|w| w == ["--write", "/tmp"]));
        }
        #[cfg(target_os = "macos")]
        {
            assert!(args.windows(2).any(|w| w == ["--read", "/usr/bin"]));
            assert!(args.windows(2).any(|w| w == ["--exec", "/usr/bin"]));
            assert!(args.windows(2).any(|w| w == ["--write", "/private/tmp"]));
        }
    }

    #[test]
    fn resolved_home_guard_never_emits_home_write_or_exec() {
        let home_guard = tempfile::tempdir().expect("home");
        let home = home_guard.path().canonicalize().expect("canonical home");
        let env = AllowEnv {
            home: Some(home.clone()),
            ..AllowEnv::default()
        };
        let inputs = AllowInputs {
            rwx: vec![home.clone()],
            ..AllowInputs::default()
        };
        let allowlist = resolve_allowlist(&inputs, Path::new("/workspace"), &env);
        let args = string_args(&config(allowlist));
        let home = home.to_string_lossy();

        assert!(args.windows(2).any(|w| w == ["--read", home.as_ref()]));
        assert!(!args.windows(2).any(|w| w == ["--write", home.as_ref()]));
        assert!(!args.windows(2).any(|w| w == ["--exec", home.as_ref()]));
    }
}
