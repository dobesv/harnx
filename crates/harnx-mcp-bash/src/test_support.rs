#[cfg(test)]
use std::ffi::{OsStr, OsString};
#[cfg(all(test, unix))]
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::sync::{Mutex, MutexGuard, OnceLock};

#[cfg(test)]
pub(crate) fn env_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    match LOCK.get_or_init(|| Mutex::new(())).lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

#[cfg(test)]
pub(crate) struct EnvVar {
    key: String,
    prev: Option<OsString>,
}

#[cfg(test)]
impl EnvVar {
    pub(crate) fn set(key: &str, value: impl AsRef<OsStr>) -> Self {
        let prev = std::env::var_os(key);
        // SAFETY: mutates process-global env. Tests that mutate env hold the
        // process-global `env_lock()` mutex, so access is serialized and no
        // other thread reads/writes the environment concurrently.
        unsafe { std::env::set_var(key, value.as_ref()) };
        Self {
            key: key.to_string(),
            prev,
        }
    }

    // Only used by Unix-gated tests; avoids dead-code warnings on Windows.
    #[cfg(unix)]
    pub(crate) fn unset(key: &str) -> Self {
        let prev = std::env::var_os(key);
        // SAFETY: see `set` — serialized by the process-global `env_lock()`.
        unsafe { std::env::remove_var(key) };
        Self {
            key: key.to_string(),
            prev,
        }
    }
}

#[cfg(test)]
impl Drop for EnvVar {
    fn drop(&mut self) {
        // SAFETY: mutates process-global env to restore the prior value. The
        // guard is created and dropped while the owning test holds the
        // process-global `env_lock()`, so env access stays serialized.
        unsafe {
            match self.prev.take() {
                Some(value) => std::env::set_var(&self.key, value),
                None => std::env::remove_var(&self.key),
            }
        }
    }
}

// Only used by Unix-gated tests; gating avoids dead-code warnings on Windows.
#[cfg(all(test, unix))]
pub(crate) struct CwdGuard {
    prev: PathBuf,
}

#[cfg(all(test, unix))]
impl CwdGuard {
    pub(crate) fn set(path: &Path) -> Self {
        let prev = std::env::current_dir().expect("current_dir");
        std::env::set_current_dir(path).expect("set_current_dir");
        Self { prev }
    }
}

#[cfg(all(test, unix))]
impl Drop for CwdGuard {
    fn drop(&mut self) {
        // Non-panicking: log restoration failures rather than silencing them.
        if let Err(e) = std::env::set_current_dir(&self.prev) {
            eprintln!(
                "test_support: restoring working directory to {} failed: {e}",
                self.prev.display()
            );
        }
    }
}
