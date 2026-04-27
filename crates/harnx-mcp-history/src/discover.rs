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
        let Some(repo) = discover_repo(root) else {
            continue;
        };
        let Some(workdir) = repo.workdir() else {
            continue;
        };
        let workdir = workdir.to_path_buf();
        if seen.insert(workdir.clone()) {
            repos.push(workdir);
        }
    }

    repos
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
