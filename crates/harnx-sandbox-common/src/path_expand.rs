use std::path::{Path, PathBuf};

pub fn expand_tilde(raw: &str) -> String {
    if !raw.starts_with('~') {
        return raw.to_string();
    }

    let home = match std::env::var("HOME") {
        Ok(home) => home,
        Err(_) => return raw.to_string(),
    };

    if raw == "~" {
        home
    } else if let Some(suffix) = raw.strip_prefix("~/") {
        format!("{home}/{suffix}")
    } else {
        raw.to_string()
    }
}

#[cfg(unix)]
/// Expand raw path string for sandbox flags.
/// 1. If it begins with known pseudo-var (`$GIT_ROOT`, `$GIT_COMMON_DIR`,
///    `$NODE_PROJECT_ROOT`, `$CARGO_ROOT`, `$GO_ROOT`) at a prefix boundary
///    (`$VAR` or `$VAR/...`), run project-root detection against `cwd`.
///    - detection `Some(root)` joins any relative remainder onto `root`.
///    - detection `None` returns `None`, so caller silently skips path.
/// 2. Else apply `expand_tilde` and return resulting `PathBuf`.
pub fn expand_path_var(raw: &str, cwd: &Path) -> Option<PathBuf> {
    let pseudo_var = [
        ("$GIT_ROOT", crate::RootKind::GitRoot),
        ("$GIT_COMMON_DIR", crate::RootKind::GitCommonDir),
        ("$NODE_PROJECT_ROOT", crate::RootKind::NodeProjectRoot),
        ("$CARGO_ROOT", crate::RootKind::CargoRoot),
        ("$GO_ROOT", crate::RootKind::GoRoot),
    ]
    .into_iter()
    .find(|(prefix, _)| {
        raw == *prefix
            || raw
                .strip_prefix(prefix)
                .is_some_and(|rest| rest.starts_with('/'))
    });

    if let Some((prefix, kind)) = pseudo_var {
        let root = crate::detect_project_root(kind, cwd)?;
        let remainder = raw.strip_prefix(prefix).expect("matched prefix");
        return Some(match remainder.strip_prefix('/') {
            Some(relative) => root.join(relative),
            None => root,
        });
    }

    Some(PathBuf::from(expand_tilde(raw)))
}

#[cfg(not(unix))]
pub fn expand_path_var(raw: &str, _cwd: &Path) -> Option<PathBuf> {
    Some(PathBuf::from(raw))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::detect_project_root;
    use crate::test_support::{env_lock, EnvGuard};
    use crate::RootKind;
    use std::path::PathBuf;

    fn touch(path: &Path) {
        std::fs::write(path, "marker").expect("write marker");
    }

    #[test]
    fn git_root_pseudo_var_expands_to_repo_root() {
        let manifest_dir =
            PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
        let expected = detect_project_root(RootKind::GitRoot, &manifest_dir).expect("git root");

        assert_eq!(expand_path_var("$GIT_ROOT", &manifest_dir), Some(expected));
    }

    #[test]
    fn git_root_pseudo_var_joins_suffix() {
        let manifest_dir =
            PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
        let expected = detect_project_root(RootKind::GitRoot, &manifest_dir)
            .expect("git root")
            .join("sub");

        assert_eq!(
            expand_path_var("$GIT_ROOT/sub", &manifest_dir),
            Some(expected)
        );
    }

    #[test]
    fn git_root_pseudo_var_returns_none_outside_repo() {
        let temp = tempfile::tempdir().expect("tempdir");
        if detect_project_root(
            RootKind::GitRoot,
            temp.path().parent().unwrap_or(temp.path()),
        )
        .is_some()
        {
            return;
        }

        assert_eq!(expand_path_var("$GIT_ROOT", temp.path()), None);
    }

    #[test]
    fn node_project_root_pseudo_var_joins_suffix() {
        let _lock = env_lock();
        let _env = EnvGuard::new();
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path().join("workspace");
        let nested = root.join("apps/web/src");
        std::fs::create_dir_all(&nested).expect("create nested");
        touch(&root.join("package.json"));

        assert_eq!(
            expand_path_var("$NODE_PROJECT_ROOT/foo", &nested),
            Some(root.join("foo"))
        );
    }

    #[test]
    fn pseudo_var_prefix_collisions_are_literal() {
        let cwd = Path::new("/");

        assert_eq!(
            expand_path_var("$GIT_ROOTX", cwd),
            Some(PathBuf::from("$GIT_ROOTX"))
        );
        assert_eq!(
            expand_path_var("pre$GIT_ROOT", cwd),
            Some(PathBuf::from("pre$GIT_ROOT"))
        );
    }

    #[test]
    fn tilde_and_absolute_paths_expand_without_pseudo_var_detection() {
        let _lock = env_lock();
        let _env = EnvGuard::new();
        unsafe { std::env::set_var("HOME", "/tmp/test-home") };

        assert_eq!(
            expand_path_var("~/x", Path::new("/")),
            Some(PathBuf::from("/tmp/test-home/x"))
        );
        assert_eq!(
            expand_path_var("/abs/path", Path::new("/")),
            Some(PathBuf::from("/abs/path"))
        );
    }

    #[test]
    fn pseudo_var_resolving_to_home_is_rejected() {
        let _lock = env_lock();
        let _env = EnvGuard::new();
        let temp = tempfile::tempdir().expect("tempdir");
        let home = temp.path().join("home");
        let nested = home.join("project/src");
        std::fs::create_dir_all(&nested).expect("create nested");
        touch(&home.join("package.json"));
        unsafe { std::env::set_var("HOME", &home) };

        assert_eq!(expand_path_var("$NODE_PROJECT_ROOT", &nested), None);
    }

    #[test]
    fn expand_tilde_matches_existing_behavior() {
        let _lock = env_lock();
        let _env = EnvGuard::new();
        unsafe { std::env::set_var("HOME", "/tmp/test-home") };

        assert_eq!(expand_tilde("~/foo"), "/tmp/test-home/foo");
        assert_eq!(expand_tilde("~"), "/tmp/test-home");
        assert_eq!(expand_tilde("~user/foo"), "~user/foo");
        assert_eq!(expand_tilde("/abs/path"), "/abs/path");
    }
}
