use std::collections::HashSet;
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
        // Walk the directory tree downward from root, collecting any git repo
        // worktrees found within it. gix::discover walks UP (finds the enclosing
        // repo), so we use a manual descent: try each subdirectory recursively.
        collect_repos_in_dir(root, &mut seen, &mut repos);
    }

    repos
}

fn collect_repos_in_dir(dir: &Path, seen: &mut HashSet<PathBuf>, repos: &mut Vec<PathBuf>) {
    // Check if this directory is itself a repo root (has a .git entry)
    if dir.join(".git").exists() {
        if let Ok(canonical) = dir.canonicalize() {
            if seen.insert(canonical.clone()) {
                repos.push(canonical);
            }
        }
        // Don't descend into subdirectories of a repo root — nested repos
        // (submodules) would need to be found via their own root entry.
        return;
    }

    // Descend into immediate subdirectories
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            let name = entry.file_name();
            // Skip hidden dirs (including .git itself) and common non-repo dirs
            if name.to_string_lossy().starts_with('.') {
                continue;
            }
            collect_repos_in_dir(&path, seen, repos);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let dir = tempfile::tempdir().expect("create temp dir");
        // On some systems the temp dir root may itself be inside a checkout — skip rather than fail
        if find_repo_for_path(dir.path().parent().unwrap_or(dir.path())).is_some() {
            return;
        }
        let repo = find_repo_for_path(dir.path());
        assert!(repo.is_none(), "expected no repo for fresh temp dir");
    }
}
