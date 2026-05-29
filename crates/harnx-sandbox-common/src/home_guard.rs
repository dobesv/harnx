use std::path::{Path, PathBuf};

/// Resolve `path` to canonical absolute form when possible.
///
/// If canonicalization fails (for example, path does not exist yet), fall back to
/// joining relative paths against current working directory and returning an
/// absolute path without resolving symlinks.
#[cfg(unix)]
pub fn resolve_path(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| {
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()
                .map(|cwd| cwd.join(path))
                .unwrap_or_else(|_| path.to_path_buf())
        }
    })
}

/// Returns `true` if `path` is `$HOME` itself or an ancestor of `$HOME`
/// (e.g. `/home` or `/`). Returns `false` when `$HOME` is unset or when
/// `path` is a child of `$HOME` (e.g. `$HOME/projects`).
///
/// Used to prevent over-broad roots from granting sandbox write/exec access.
#[cfg(unix)]
pub fn is_home_or_ancestor(path: &Path) -> bool {
    let home_os = match std::env::var_os("HOME") {
        Some(h) => h,
        None => return false,
    };
    let home = std::fs::canonicalize(&home_os).unwrap_or_else(|_| {
        let home_path = Path::new(&home_os);
        if home_path.is_absolute() {
            home_path.to_path_buf()
        } else {
            std::env::current_dir()
                .map(|cwd| cwd.join(home_path))
                .unwrap_or_else(|_| home_path.to_path_buf())
        }
    });
    let candidate = resolve_path(path);
    home.starts_with(&candidate)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::{Mutex, OnceLock};

    /// Serialise all tests that mutate process-global HOME / cwd.
    fn env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    /// RAII guard: snapshots HOME and cwd on creation, restores both on drop.
    struct EnvGuard {
        saved_home: Option<std::ffi::OsString>,
        saved_cwd: PathBuf,
    }

    impl EnvGuard {
        fn new() -> Self {
            Self {
                saved_home: std::env::var_os("HOME"),
                saved_cwd: std::env::current_dir().expect("current_dir"),
            }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            // Restore cwd first so that any relative-path lookups during HOME
            // restoration happen against the original directory.
            let _ = std::env::set_current_dir(&self.saved_cwd);
            match &self.saved_home {
                Some(h) => unsafe { std::env::set_var("HOME", h) },
                None => unsafe { std::env::remove_var("HOME") },
            }
        }
    }

    #[test]
    fn home_path_matches() {
        let _lock = env_lock().lock().expect("lock poisoned");
        let _env = EnvGuard::new();
        let temp = tempfile::tempdir().expect("tempdir");
        let home = temp.path().join("home");
        std::fs::create_dir(&home).expect("create home");
        unsafe { std::env::set_var("HOME", &home) };

        assert!(is_home_or_ancestor(&home));
    }

    #[test]
    fn ancestor_of_home_matches() {
        let _lock = env_lock().lock().expect("lock poisoned");
        let _env = EnvGuard::new();
        let temp = tempfile::tempdir().expect("tempdir");
        let parent = temp.path().join("parent");
        let home = parent.join("home");
        std::fs::create_dir_all(&home).expect("create home tree");
        unsafe { std::env::set_var("HOME", &home) };

        assert!(is_home_or_ancestor(&parent));
    }

    #[test]
    fn child_of_home_does_not_match() {
        let _lock = env_lock().lock().expect("lock poisoned");
        let _env = EnvGuard::new();
        let temp = tempfile::tempdir().expect("tempdir");
        let home = temp.path().join("home");
        let child = home.join("projects");
        std::fs::create_dir_all(&child).expect("create child");
        unsafe { std::env::set_var("HOME", &home) };

        assert!(!is_home_or_ancestor(&child));
    }

    #[test]
    fn relative_path_resolves_against_cwd() {
        let _lock = env_lock().lock().expect("lock poisoned");
        let _env = EnvGuard::new(); // restores HOME and cwd on drop
        let temp = tempfile::tempdir().expect("tempdir");
        let cwd = temp.path().join("cwd");
        let home = cwd.join("home");
        std::fs::create_dir_all(&home).expect("create cwd/home");
        std::env::set_current_dir(&cwd).expect("set cwd");
        unsafe { std::env::set_var("HOME", &home) };

        assert!(is_home_or_ancestor(Path::new(".")));
    }
}
