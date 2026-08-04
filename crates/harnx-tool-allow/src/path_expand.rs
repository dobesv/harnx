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
/// 2. Else if it begins with `$NAME` at that same prefix boundary, resolve
///    `NAME` from environment and join any relative remainder onto that value.
///    Pseudo-vars take precedence; unset env vars stay literal; no mid-path
///    expansion and no `${...}` syntax.
/// 3. Else apply `expand_tilde` and return resulting `PathBuf`.
pub fn expand_path_var(raw: &str, cwd: &Path) -> Option<PathBuf> {
    if let Some(pseudo) = pseudo_var(raw) {
        return expand_pseudo_var(raw, pseudo, cwd);
    }
    expand_env_var(raw).or_else(|| Some(PathBuf::from(expand_tilde(raw))))
}

#[cfg(unix)]
#[derive(Clone, Copy)]
struct PseudoVar {
    prefix: &'static str,
    kind: crate::RootKind,
}

#[cfg(unix)]
fn pseudo_var(raw: &str) -> Option<PseudoVar> {
    [
        PseudoVar {
            prefix: "$GIT_ROOT",
            kind: crate::RootKind::GitRoot,
        },
        PseudoVar {
            prefix: "$GIT_COMMON_DIR",
            kind: crate::RootKind::GitCommonDir,
        },
        PseudoVar {
            prefix: "$NODE_PROJECT_ROOT",
            kind: crate::RootKind::NodeProjectRoot,
        },
        PseudoVar {
            prefix: "$CARGO_ROOT",
            kind: crate::RootKind::CargoRoot,
        },
        PseudoVar {
            prefix: "$GO_ROOT",
            kind: crate::RootKind::GoRoot,
        },
    ]
    .into_iter()
    .find(|pseudo| path_var_matches(raw, pseudo.prefix))
}

#[cfg(unix)]
fn path_var_matches(raw: &str, prefix: &str) -> bool {
    raw == prefix
        || raw
            .strip_prefix(prefix)
            .is_some_and(|rest| rest.starts_with('/'))
}

#[cfg(unix)]
fn expand_pseudo_var(raw: &str, pseudo: PseudoVar, cwd: &Path) -> Option<PathBuf> {
    let root = crate::detect_project_root(pseudo.kind, cwd)?;
    Some(join_remainder(
        root,
        raw.strip_prefix(pseudo.prefix).expect("matched prefix"),
    ))
}

#[cfg(unix)]
fn expand_env_var(raw: &str) -> Option<PathBuf> {
    let stripped = raw.strip_prefix('$')?;
    let name_end = env_name_end(stripped)?;
    let name = &stripped[..name_end];
    let remainder = &stripped[name_end..];
    if !remainder.is_empty() && !remainder.starts_with('/') {
        return None;
    }
    let value = std::env::var_os(name)?;
    Some(join_remainder(PathBuf::from(value), remainder))
}

#[cfg(unix)]
fn env_name_end(stripped: &str) -> Option<usize> {
    let mut chars = stripped.char_indices();
    let (_, first) = chars.next()?;
    if !first.is_ascii_alphabetic() && first != '_' {
        return None;
    }
    Some(
        chars
            .find_map(|(idx, ch)| (!(ch.is_ascii_alphanumeric() || ch == '_')).then_some(idx))
            .unwrap_or(stripped.len()),
    )
}

#[cfg(unix)]
fn join_remainder(base: PathBuf, remainder: &str) -> PathBuf {
    remainder
        .strip_prefix('/')
        .map_or(base.clone(), |relative| base.join(relative))
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
    fn env_var_expands_to_value_and_joined_suffix() {
        let _lock = env_lock();
        let _env = EnvGuard::new();
        unsafe { std::env::set_var("HARNX_TEST_VAR_A", "/tmp/harnx-var-a") };

        assert_eq!(
            expand_path_var("$HARNX_TEST_VAR_A", Path::new("/")),
            Some(PathBuf::from("/tmp/harnx-var-a"))
        );
        assert_eq!(
            expand_path_var("$HARNX_TEST_VAR_A/sub", Path::new("/")),
            Some(PathBuf::from("/tmp/harnx-var-a").join("sub"))
        );

        unsafe { std::env::remove_var("HARNX_TEST_VAR_A") };
    }

    #[test]
    fn unset_env_var_stays_literal() {
        let _lock = env_lock();
        let _env = EnvGuard::new();
        unsafe { std::env::remove_var("HARNX_TEST_NONEXISTENT_XYZ") };

        assert_eq!(
            expand_path_var("$HARNX_TEST_NONEXISTENT_XYZ", Path::new("/")),
            Some(PathBuf::from("$HARNX_TEST_NONEXISTENT_XYZ"))
        );
    }

    #[test]
    fn env_var_prefix_boundary_negatives_stay_literal() {
        let _lock = env_lock();
        let _env = EnvGuard::new();
        unsafe { std::env::set_var("HARNX_TEST_VAR_A", "/tmp/harnx-var-a") };
        let cwd = Path::new("/");

        assert_eq!(
            expand_path_var("$HARNX_TEST_VAR_AX", cwd),
            Some(PathBuf::from("$HARNX_TEST_VAR_AX"))
        );
        assert_eq!(
            expand_path_var("pre$HARNX_TEST_VAR_A", cwd),
            Some(PathBuf::from("pre$HARNX_TEST_VAR_A"))
        );
        assert_eq!(
            expand_path_var("$HARNX_TEST_VAR_A-bar", cwd),
            Some(PathBuf::from("$HARNX_TEST_VAR_A-bar"))
        );

        unsafe { std::env::remove_var("HARNX_TEST_VAR_A") };
    }

    #[test]
    fn tilde_is_not_reexpanded_after_env_var_expansion() {
        let _lock = env_lock();
        let _env = EnvGuard::new();
        unsafe { std::env::set_var("HARNX_TEST_TILDE_VAR", "~/foo") };

        assert_eq!(
            expand_path_var("$HARNX_TEST_TILDE_VAR", Path::new("/")),
            Some(PathBuf::from("~/foo"))
        );

        unsafe { std::env::remove_var("HARNX_TEST_TILDE_VAR") };
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
