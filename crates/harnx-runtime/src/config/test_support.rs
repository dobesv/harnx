//! Shared test-only helpers for the config module's test submodules.
#![cfg(test)]

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
