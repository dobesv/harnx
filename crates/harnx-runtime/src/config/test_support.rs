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
