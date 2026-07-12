//! Shared test-only helpers for the config module's test submodules.
#![cfg(test)]

use super::*;

/// RAII guard that sets an env var for a test and restores the prior value on
/// drop. Test-only; callers must hold the global test lock while it is alive to
/// prevent concurrent env mutation.
pub(super) struct EnvGuard {
    key: &'static str,
    prev: Option<std::ffi::OsString>,
}
impl EnvGuard {
    pub(super) fn new(key: &'static str, value: &std::path::Path) -> Self {
        let prev = std::env::var_os(key);
        // SAFETY: test-only; concurrent env mutation is prevented by the
        // global test lock held by the caller while the guard is alive.
        unsafe { std::env::set_var(key, value) };
        Self { key, prev }
    }

    pub(super) fn new_file(key: &'static str, value: &std::path::Path) -> Self {
        let prev = std::env::var_os(key);
        // SAFETY: test-only; concurrent env mutation is prevented by the
        // global test lock held by the caller while the guard is alive.
        unsafe { std::env::set_var(key, value) };
        Self { key, prev }
    }
}
impl Drop for EnvGuard {
    fn drop(&mut self) {
        match &self.prev {
            Some(v) => unsafe { std::env::set_var(self.key, v) },
            None => unsafe { std::env::remove_var(self.key) },
        }
    }
}

/// Serialize env-mutating tests to prevent HOME/HARNX_CONFIG_DIR from racing.
/// Cross-platform: used by both the unix-only HOME tests and the
/// platform-agnostic remote-agent/use_tools tests, so it must compile on all
/// targets (Windows CI builds these tests too).
pub(super) fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    match LOCK.get_or_init(|| std::sync::Mutex::new(())).lock() {
        Ok(g) => g,
        Err(e) => e.into_inner(),
    }
}

/// Build a `Config` wired with a dummy `test:model` client and an isolated
/// sessions dir. Shared by the message-edit and session-ops test modules.
pub(super) fn editor_test_config(sessions_dir: std::path::PathBuf) -> Config {
    let mut config = Config {
        sessions_dir_override: Some(sessions_dir),
        working_mode: WorkingMode::Cmd,
        ..Config::default()
    };
    config
        .clients
        .push(harnx_client::ClientConfig::OpenAICompatibleConfig(
            harnx_core::provider_config::openai_compatible::OpenAICompatibleConfig {
                name: "test".to_string(),
                api_base: None,
                api_key: None,
                models: vec![],
                patches: None,
                extra: None,
                system_prompt_prefix: None,
                package: None,
            },
        ));
    config.model = harnx_client::Model::new("test", "model");
    config.model_id = "test:model".to_string();
    config
}

/// A package-loaded server identity: the bare yaml stem plus the package it
/// belongs to. Mirrors what `load_package_servers` records on disk.
pub(super) struct PackageServer<'a> {
    pub(super) stem: &'a str,
    pub(super) package: &'a str,
}

impl<'a> PackageServer<'a> {
    pub(super) fn new(stem: &'a str, package: &'a str) -> Self {
        Self { stem, package }
    }

    /// Build an ACP server config for this package server.
    pub(super) fn into_acp(self) -> AcpServerConfig {
        AcpServerConfig {
            name: self.stem.to_string(),
            command: "echo".to_string(),
            args: vec![],
            env: HashMap::new(),
            enabled: true,
            description: None,
            idle_timeout_secs: 300,
            operation_timeout_secs: 3600,
            package: Some(self.package.to_string()),
        }
    }

    /// Build an MCP server config for this package server.
    ///
    /// Built inline (rather than via the `#[cfg(unix)]` `make_test_mcp_server`
    /// helper) so these regression tests compile on all platforms.
    pub(super) fn into_mcp(self) -> McpServerConfig {
        McpServerConfig {
            name: self.stem.to_string(),
            command: "echo".to_string(),
            args: vec![],
            env: HashMap::new(),
            roots: vec![],
            enabled: true,
            description: None,
            rename_tools: HashMap::new(),
            tool_templates: HashMap::new(),
            hooks: None,
            package: Some(self.package.to_string()),
        }
    }
}
