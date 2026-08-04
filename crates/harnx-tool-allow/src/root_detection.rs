use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RootKind {
    GitRoot,
    GitCommonDir,
    NodeProjectRoot,
    CargoRoot,
    GoRoot,
}

/// Detect project root of `kind` starting from `cwd`.
///
/// Returns `None` if no matching root is found or if detected path resolves to
/// `$HOME` or an ancestor of `$HOME`.
///
/// `NodeProjectRoot` returns highest ancestor containing `package.json` so
/// monorepos resolve to workspace root instead of nested package root.
/// `CargoRoot` returns highest ancestor containing `Cargo.toml` so monorepo
/// crates resolve to workspace root that owns shared `target/` and `Cargo.lock`.
/// `GoRoot` returns nearest ancestor containing `go.mod` because nested modules
/// are independent roots.
pub fn detect_project_root(kind: RootKind, cwd: &Path) -> Option<PathBuf> {
    let root = match kind {
        RootKind::GitRoot => gix::discover(cwd).ok()?.workdir()?.to_path_buf(),
        RootKind::GitCommonDir => gix::discover(cwd).ok()?.common_dir().to_path_buf(),
        RootKind::NodeProjectRoot => {
            detect_marker_root(cwd, "package.json", WalkStrategy::Highest)?
        }
        RootKind::CargoRoot => detect_marker_root(cwd, "Cargo.toml", WalkStrategy::Highest)?,
        RootKind::GoRoot => detect_marker_root(cwd, "go.mod", WalkStrategy::Nearest)?,
    };

    if crate::is_home_or_ancestor(&root) {
        return None;
    }

    Some(root)
}

#[derive(Clone, Copy)]
enum WalkStrategy {
    Highest,
    Nearest,
}

fn detect_marker_root(cwd: &Path, marker: &str, strategy: WalkStrategy) -> Option<PathBuf> {
    let mut current = if cwd.is_dir() {
        cwd.to_path_buf()
    } else {
        cwd.parent()?.to_path_buf()
    };
    let mut found = None;

    loop {
        if current.join(marker).exists() {
            match strategy {
                WalkStrategy::Nearest => return guard_candidate(current),
                WalkStrategy::Highest => found = Some(current.clone()),
            }
        }

        // Stops at the home boundary: is_home_or_ancestor canonicalizes and
        // returns true when `current` is $HOME itself or an ancestor of it, so
        // a separate raw-equality check against $HOME would be redundant.
        if crate::is_home_or_ancestor(&current) {
            break;
        }

        let Some(parent) = current.parent() else {
            break;
        };
        current = parent.to_path_buf();
    }

    found.and_then(guard_candidate)
}

fn guard_candidate(path: PathBuf) -> Option<PathBuf> {
    if crate::is_home_or_ancestor(&path) {
        None
    } else {
        Some(path)
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::test_support::{env_lock, EnvGuard};

    fn touch(path: &Path) {
        std::fs::write(path, "marker").expect("write marker");
    }

    fn assert_marker_roots(
        cwd: &Path,
        node: Option<PathBuf>,
        cargo: Option<PathBuf>,
        go: Option<PathBuf>,
    ) {
        assert_eq!(detect_project_root(RootKind::NodeProjectRoot, cwd), node);
        assert_eq!(detect_project_root(RootKind::CargoRoot, cwd), cargo);
        assert_eq!(detect_project_root(RootKind::GoRoot, cwd), go);
    }

    #[test]
    fn detects_node_cargo_and_go_roots_from_nested_directory() {
        let _lock = env_lock();
        let _env = EnvGuard::new();
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path().join("workspace");
        let nested = root.join("apps/api/src");
        std::fs::create_dir_all(&nested).expect("create nested");
        touch(&root.join("package.json"));
        touch(&root.join("Cargo.toml"));
        touch(&root.join("go.mod"));

        assert_marker_roots(&nested, Some(root.clone()), Some(root.clone()), Some(root));
    }

    #[test]
    fn returns_none_when_no_marker_present() {
        let _lock = env_lock();
        let _env = EnvGuard::new();
        let temp = tempfile::tempdir().expect("tempdir");
        let nested = temp.path().join("workspace/subdir");
        std::fs::create_dir_all(&nested).expect("create nested");

        assert_marker_roots(&nested, None, None, None);
    }

    #[test]
    fn returns_none_when_detected_root_is_home() {
        let _lock = env_lock();
        let _env = EnvGuard::new();
        let temp = tempfile::tempdir().expect("tempdir");
        let home = temp.path().join("home");
        let nested = home.join("project/src");
        std::fs::create_dir_all(&nested).expect("create nested");
        touch(&home.join("package.json"));
        touch(&home.join("Cargo.toml"));
        touch(&home.join("go.mod"));
        unsafe { std::env::set_var("HOME", &home) };

        assert_marker_roots(&nested, None, None, None);
    }

    #[test]
    fn detects_git_root_from_manifest_dir() {
        let manifest_dir =
            PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));

        assert!(detect_project_root(RootKind::GitRoot, &manifest_dir).is_some());
    }

    #[test]
    fn distinguishes_git_root_from_git_common_dir_in_linked_worktree() {
        let temp = tempfile::tempdir().expect("tempdir");
        let primary = temp.path().join("primary");
        let linked = temp.path().join("linked-worktree");
        std::fs::create_dir_all(&primary).expect("create primary");

        let run_git = |args: &[&str], dir: &Path| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(dir)
                .output()
                .expect("run git command")
        };

        let init = run_git(&["init", "--initial-branch=main"], &primary);
        assert!(
            init.status.success(),
            "git init failed: {}",
            String::from_utf8_lossy(&init.stderr)
        );

        let config_name = run_git(&["config", "user.name", "Test User"], &primary);
        assert!(
            config_name.status.success(),
            "git config user.name failed: {}",
            String::from_utf8_lossy(&config_name.stderr)
        );

        let config_email = run_git(&["config", "user.email", "test@example.com"], &primary);
        assert!(
            config_email.status.success(),
            "git config user.email failed: {}",
            String::from_utf8_lossy(&config_email.stderr)
        );

        std::fs::write(primary.join("README.md"), "hello\n").expect("write readme");
        let add = run_git(&["add", "README.md"], &primary);
        assert!(
            add.status.success(),
            "git add failed: {}",
            String::from_utf8_lossy(&add.stderr)
        );

        let commit = run_git(&["commit", "-m", "initial"], &primary);
        assert!(
            commit.status.success(),
            "git commit failed: {}",
            String::from_utf8_lossy(&commit.stderr)
        );

        let worktree = run_git(
            &[
                "worktree",
                "add",
                linked.to_str().expect("utf8 linked path"),
            ],
            &primary,
        );
        assert!(
            worktree.status.success(),
            "git worktree add failed: {}",
            String::from_utf8_lossy(&worktree.stderr)
        );

        let expected_common_dir = primary.join(".git");
        let detected_git_root = detect_project_root(RootKind::GitRoot, &linked).expect("git root");
        let detected_common_dir =
            detect_project_root(RootKind::GitCommonDir, &linked).expect("git common dir");

        assert_eq!(
            std::fs::canonicalize(detected_git_root).expect("canonical git root"),
            std::fs::canonicalize(&linked).expect("canonical linked worktree")
        );
        assert_eq!(
            std::fs::canonicalize(detected_common_dir).expect("canonical common dir"),
            std::fs::canonicalize(expected_common_dir).expect("canonical expected common dir")
        );
    }

    #[test]
    fn symlink_target_in_home_is_rejected() {
        let _lock = env_lock();
        let _env = EnvGuard::new();
        let temp = tempfile::tempdir().expect("tempdir");
        let home = temp.path().join("home");
        let link_parent = temp.path().join("outside");
        let symlink_root = link_parent.join("link-to-home");
        let nested = symlink_root.join("deep/src");
        std::fs::create_dir_all(&home).expect("create home");
        std::fs::create_dir_all(&link_parent).expect("create link parent");
        touch(&home.join("package.json"));
        touch(&home.join("Cargo.toml"));
        touch(&home.join("go.mod"));
        std::os::unix::fs::symlink(&home, &symlink_root).expect("symlink home");
        std::fs::create_dir_all(&nested).expect("create nested via symlink");
        unsafe { std::env::set_var("HOME", &home) };

        assert_marker_roots(&nested, None, None, None);
    }

    #[test]
    fn highest_vs_nearest_marker_selection() {
        let _lock = env_lock();
        let _env = EnvGuard::new();
        let temp = tempfile::tempdir().expect("tempdir");
        let top = temp.path().join("workspace");
        let middle = top.join("apps/service");
        let nested = middle.join("src/bin");
        std::fs::create_dir_all(&nested).expect("create nested");
        touch(&top.join("package.json"));
        touch(&middle.join("package.json"));
        touch(&top.join("Cargo.toml"));
        touch(&middle.join("Cargo.toml"));
        touch(&top.join("go.mod"));
        touch(&middle.join("go.mod"));

        assert_marker_roots(&nested, Some(top.clone()), Some(top), Some(middle));
    }
}
