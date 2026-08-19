//! Process-wide environment isolation for runtime unit tests.
//!
//! Rust's test harness runs unit tests from every module in one process. Any
//! test that changes a path such as `HARNX_CONFIG_DIR` must therefore use this
//! one lock and restore the previous value when it finishes.

use std::ffi::{OsStr, OsString};
use tokio::sync::{Mutex, MutexGuard};

static LOCK: Mutex<()> = Mutex::const_new(());

pub(crate) fn env_lock() -> MutexGuard<'static, ()> {
    LOCK.blocking_lock()
}

pub(crate) async fn env_lock_async() -> MutexGuard<'static, ()> {
    LOCK.lock().await
}

/// Sets one environment variable and restores its exact previous value on
/// drop. Callers must hold [`env_lock`] for the guard's whole lifetime.
pub(crate) struct EnvGuard {
    key: String,
    previous: Option<OsString>,
}

impl EnvGuard {
    pub(crate) fn new(key: impl Into<String>, value: impl AsRef<OsStr>) -> Self {
        let key = key.into();
        let previous = std::env::var_os(&key);
        // SAFETY: every runtime unit test that mutates process environment uses
        // the shared lock above for the full lifetime of this guard.
        unsafe { std::env::set_var(&key, value) };
        Self { key, previous }
    }

    pub(crate) fn remove(key: impl Into<String>) -> Self {
        let key = key.into();
        let previous = std::env::var_os(&key);
        // SAFETY: every runtime unit test that mutates process environment uses
        // the shared lock for the full lifetime of this guard.
        unsafe { std::env::remove_var(&key) };
        Self { key, previous }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        // SAFETY: see `EnvGuard::new`; the caller still holds the shared lock.
        match &self.previous {
            Some(value) => unsafe { std::env::set_var(&self.key, value) },
            None => unsafe { std::env::remove_var(&self.key) },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nested_guard_restores_existing_config_root() {
        let _lock = env_lock();
        let sentinel = tempfile::tempdir().expect("create sentinel config root");
        let fixture = tempfile::tempdir().expect("create fixture config root");
        let _original = EnvGuard::new("HARNX_CONFIG_DIR", sentinel.path());

        {
            let _fixture = EnvGuard::new("HARNX_CONFIG_DIR", fixture.path());
            assert_eq!(
                std::env::var_os("HARNX_CONFIG_DIR"),
                Some(fixture.path().into())
            );
        }

        assert_eq!(
            std::env::var_os("HARNX_CONFIG_DIR"),
            Some(sentinel.path().into()),
            "dropping a fixture guard must restore the caller's config root"
        );
        assert!(
            !sentinel.path().join("config.yaml").exists(),
            "the sentinel config root must remain untouched"
        );
    }
}
