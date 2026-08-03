// Auto-split from server.rs for cohesion. See server/mod.rs.
use super::*;

// ---------------------------------------------------------------------------
// Home boundary guard
// ---------------------------------------------------------------------------

/// Returns `true` if `path` is `$HOME` itself or an ancestor of `$HOME`
/// (e.g. `/home` or `/`). Returns `false` when `$HOME` is unset or when
/// `path` is a child of `$HOME` (e.g. `$HOME/projects`).
///
/// Used to prevent over-broad roots from granting sandbox write/exec access.
#[cfg(unix)]
pub(crate) fn is_home_or_ancestor(path: &Path) -> bool {
    harnx_sandbox_common::is_home_or_ancestor(path)
}

// ---------------------------------------------------------------------------
// Sandbox arg push helpers (used by build_sandbox_args)
// ---------------------------------------------------------------------------

/// Push `--write` and `--exec` args for `root`, unless it is `$HOME` or an ancestor.
#[cfg(unix)]
pub(crate) fn push_root_write_exec(
    root: &Path,
    args: &mut Vec<OsString>,
    writable: &mut Vec<PathBuf>,
) {
    if is_home_or_ancestor(root) {
        return;
    }
    args.push(OsString::from("--write"));
    args.push(root.as_os_str().to_os_string());
    args.push(OsString::from("--exec"));
    args.push(root.as_os_str().to_os_string());
    writable.push(root.to_path_buf());
}
