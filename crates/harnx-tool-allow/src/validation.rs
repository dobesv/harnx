use std::path::{Path, PathBuf};

use crate::ResolvedAllowlist;

/// Returns `true` if `path` equals `$HOME` or is an ancestor of `$HOME`.
pub fn path_is_home_or_ancestor(path: &Path) -> bool {
    let Some(home) = std::env::var_os("HOME") else {
        return false;
    };
    crate::allowlist::home_or_ancestor(path, Path::new(&home))
}

/// Returns canonical process CWD unless it equals or contains `$HOME`.
pub fn default_root_from_cwd() -> Option<PathBuf> {
    std::env::var_os("HOME")?;
    let cwd = std::env::current_dir().ok()?.canonicalize().ok()?;
    (!path_is_home_or_ancestor(&cwd)).then_some(cwd)
}

pub fn validate_path(path_str: &str, allowlist: &ResolvedAllowlist) -> Result<PathBuf, String> {
    let resolved = resolve_input(path_str);
    let canonical = resolved
        .canonicalize()
        .map_err(|error| format!("Cannot resolve path '{path_str}': {error}"))?;

    if allowlist.read_paths().is_empty() {
        return Err("No paths configured — all filesystem access is denied".to_string());
    }
    if allowlist.contains_read(&canonical) {
        return Ok(canonical);
    }
    Err(format!(
        "Path '{}' is outside allowed read paths: [{}]",
        path_str,
        display_paths(allowlist.read_paths().iter())
    ))
}

pub fn validate_write_path(
    path_str: &str,
    allowlist: &ResolvedAllowlist,
) -> Result<PathBuf, String> {
    let resolved = resolve_input(path_str);
    if allowlist.write_paths().is_empty() {
        return Err("No write paths configured — all filesystem writes are denied".to_string());
    }

    if resolved.exists() {
        let canonical = resolved
            .canonicalize()
            .map_err(|error| format!("Cannot resolve path '{path_str}': {error}"))?;
        return if allowlist.contains_write(&canonical) {
            Ok(canonical)
        } else {
            Err(format!("Path '{path_str}' is outside allowed write paths"))
        };
    }

    let mut ancestor = resolved
        .parent()
        .ok_or_else(|| format!("Cannot determine parent directory for '{path_str}'"))?;
    while !ancestor.exists() {
        ancestor = ancestor
            .parent()
            .ok_or_else(|| format!("No existing ancestor for '{path_str}'"))?;
    }
    let canonical_ancestor = ancestor
        .canonicalize()
        .map_err(|error| format!("Cannot resolve ancestor: {error}"))?;
    if allowlist.contains_write(&canonical_ancestor) {
        Ok(resolved)
    } else {
        Err(format!("Path '{path_str}' is outside allowed write paths"))
    }
}

fn resolve_input(path_str: &str) -> PathBuf {
    let path = Path::new(path_str);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    }
}

fn display_paths<'a>(paths: impl Iterator<Item = &'a PathBuf>) -> String {
    paths
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_path_within_root() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("inside.txt");
        std::fs::write(&file, "ok").unwrap();
        let mut allow = ResolvedAllowlist::new();
        allow.insert_read(temp.path());
        assert_eq!(
            validate_path(&file.to_string_lossy(), &allow).unwrap(),
            file.canonicalize().unwrap()
        );
    }

    #[test]
    fn test_validate_path_outside_root() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let file = outside.path().join("outside.txt");
        std::fs::write(&file, "nope").unwrap();
        let mut allow = ResolvedAllowlist::new();
        allow.insert_read(root.path());
        assert!(validate_path(&file.to_string_lossy(), &allow)
            .unwrap_err()
            .contains("outside allowed read paths"));
    }

    #[test]
    fn test_validate_path_no_roots_denies_access() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("anywhere.txt");
        std::fs::write(&file, "ok").unwrap();
        assert!(
            validate_path(&file.to_string_lossy(), &ResolvedAllowlist::new())
                .unwrap_err()
                .contains("No paths configured")
        );
    }

    #[test]
    fn test_validate_write_path_new_file() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("nested/new.txt");
        let mut allow = ResolvedAllowlist::new();
        assert!(allow.insert_write_with_home(temp.path(), None));
        assert_eq!(
            validate_write_path(&file.to_string_lossy(), &allow).unwrap(),
            file
        );
    }
}
