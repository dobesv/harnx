use std::collections::HashSet;
use std::env;
use std::path::{Path, PathBuf};

fn discover_repo(path: &Path) -> Option<gix::Repository> {
    let search_path = if path.is_file() {
        path.parent().unwrap_or(path)
    } else {
        path
    };
    gix::discover(search_path).ok()
}

pub fn find_repo_for_path(path: &Path) -> Option<PathBuf> {
    let repo = discover_repo(path)?;
    repo.workdir().map(Path::to_path_buf)
}

pub fn find_repos_under_roots(roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut seen = HashSet::new();
    let mut repos = Vec::new();

    for root in roots {
        let Some(repo) = discover_repo(root) else {
            continue;
        };
        let Some(workdir) = repo.workdir() else {
            continue;
        };
        let canonical = workdir
            .canonicalize()
            .unwrap_or_else(|_| workdir.to_path_buf());
        if seen.insert(canonical.clone()) {
            repos.push(canonical);
        }
    }

    repos
}

pub fn history_dir() -> PathBuf {
    if let Some(dir) = env::var_os("HARNX_LOCAL_HISTORY_DIR") {
        return PathBuf::from(dir);
    }

    let config_root = env::var_os("HARNX_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(dirs::config_dir)
        .or_else(|| {
            env::var_os("HOME")
                .map(PathBuf::from)
                .map(|home| home.join(".config"))
        })
        .unwrap_or_else(|| PathBuf::from(".config"));

    config_root.join("harnx").join("history")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, OnceLock};

    fn env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    #[test]
    fn test_find_repo_for_path_inside_repo() {
        // CARGO_MANIFEST_DIR is always set by cargo/nextest and points to this crate's directory,
        // which lives inside the harnx git repo — so discovery must succeed regardless of where
        // the source tree is checked out.
        let manifest_dir =
            std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR must be set by cargo");
        let repo = find_repo_for_path(Path::new(&manifest_dir));
        assert!(
            repo.is_some(),
            "expected a git repo to be discovered from CARGO_MANIFEST_DIR={manifest_dir}"
        );
    }

    #[test]
    fn test_find_repo_for_path_outside_repo() {
        let repo = find_repo_for_path(Path::new("/tmp"));
        assert!(repo.is_none());
    }

    #[test]
    fn test_history_dir_env_override() {
        let guard = env_lock().lock().expect("env lock");
        let expected = PathBuf::from("/tmp/harnx-history-override");
        unsafe { env::set_var("HARNX_LOCAL_HISTORY_DIR", &expected) };
        let actual = history_dir();
        unsafe { env::remove_var("HARNX_LOCAL_HISTORY_DIR") };
        drop(guard);
        assert_eq!(actual, expected);
    }
}
