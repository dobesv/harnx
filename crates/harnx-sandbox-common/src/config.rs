use std::path::PathBuf;

/// Shared sandbox and child-env configuration.
///
/// Sandboxing fields are honoured only on Unix; on other platforms they are
/// accepted for API compatibility and ignored.
///
/// Env-control fields are honoured on every platform.
#[derive(Clone, Debug)]
pub struct SandboxConfig {
    #[cfg_attr(not(unix), allow(dead_code))]
    pub enabled: bool,
    #[cfg_attr(not(unix), allow(dead_code))]
    pub extra_exec: Vec<PathBuf>,
    #[cfg_attr(not(unix), allow(dead_code))]
    pub extra_readable: Vec<PathBuf>,
    #[cfg_attr(not(unix), allow(dead_code))]
    pub extra_writable: Vec<PathBuf>,
    #[cfg_attr(not(unix), allow(dead_code))]
    pub extra_rwx: Vec<PathBuf>,
    /// Extra var names to pass through from host (allowlist additions).
    pub extra_env_passthrough: Vec<String>,
    /// Explicit overrides: KEY → VALUE (highest precedence).
    pub env_overrides: Vec<(String, String)>,
    #[cfg_attr(not(unix), allow(dead_code))]
    pub sandbox_run_path: PathBuf,
}
