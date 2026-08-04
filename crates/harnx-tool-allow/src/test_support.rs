#[cfg(all(test, unix))]
use std::path::PathBuf;
#[cfg(all(test, unix))]
use std::sync::{Mutex, MutexGuard, OnceLock};

#[cfg(all(test, unix))]
pub(crate) fn env_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    match LOCK.get_or_init(|| Mutex::new(())).lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

#[cfg(all(test, unix))]
pub(crate) struct EnvGuard {
    saved_home: Option<std::ffi::OsString>,
    saved_cwd: PathBuf,
}

#[cfg(all(test, unix))]
impl EnvGuard {
    pub(crate) fn new() -> Self {
        Self {
            saved_home: std::env::var_os("HOME"),
            saved_cwd: std::env::current_dir().expect("current_dir"),
        }
    }
}

#[cfg(all(test, unix))]
impl Drop for EnvGuard {
    fn drop(&mut self) {
        // Non-panicking: log restoration failures rather than silencing them.
        if let Err(e) = std::env::set_current_dir(&self.saved_cwd) {
            eprintln!(
                "test_support: restoring working directory to {} failed: {e}",
                self.saved_cwd.display()
            );
        }
        // SAFETY: mutates process-global env (HOME) to restore the prior value.
        // EnvGuard is only constructed in tests holding the process-global
        // `env_lock()` mutex, so environment access is serialized.
        match &self.saved_home {
            Some(home) => unsafe { std::env::set_var("HOME", home) },
            None => unsafe { std::env::remove_var("HOME") },
        }
    }
}
