use std::ffi::OsString;
use std::path::Path;

/// Serialises tests that mutate process-global Harnx directory overrides.
pub(crate) static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Isolates config and managed NATS runtime data for a TUI test.
///
/// Callers must hold [`ENV_LOCK`] while this guard is alive. Giving each
/// nextest process a separate data directory prevents one test from stopping
/// a shared broker while peer tests are still using it.
pub(crate) struct TestEnvironment {
    prior: [Option<OsString>; 2],
}

impl TestEnvironment {
    const KEYS: [&str; 2] = ["HARNX_CONFIG_DIR", "HARNX_DATA_DIR"];

    pub(crate) fn set(root: &Path) -> Self {
        let prior = Self::KEYS.map(std::env::var_os);
        for key in Self::KEYS {
            std::env::set_var(key, root);
        }
        Self { prior }
    }
}

impl Drop for TestEnvironment {
    fn drop(&mut self) {
        for (key, prior) in Self::KEYS.into_iter().zip(&self.prior) {
            match prior {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
    }
}
