use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};

/// Canonical read, write, and execute grants.
///
/// Write and execute grants are also read grants. Use insertion methods to
/// preserve that closure and enforce the `$HOME` ancestor guard.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResolvedAllowlist {
    read: BTreeSet<PathBuf>,
    write: BTreeSet<PathBuf>,
    exec: BTreeSet<PathBuf>,
}

impl ResolvedAllowlist {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn read_paths(&self) -> &BTreeSet<PathBuf> {
        &self.read
    }

    pub fn write_paths(&self) -> &BTreeSet<PathBuf> {
        &self.write
    }

    pub fn exec_paths(&self) -> &BTreeSet<PathBuf> {
        &self.exec
    }

    pub fn is_empty(&self) -> bool {
        self.read.is_empty() && self.write.is_empty() && self.exec.is_empty()
    }

    pub fn insert_read(&mut self, path: impl AsRef<Path>) {
        self.read
            .insert(canonicalize_for_containment(path.as_ref()));
    }

    /// Inserts a write grant unless it would expose `$HOME` or an ancestor.
    pub fn insert_write(&mut self, path: impl AsRef<Path>) -> bool {
        let home = std::env::var_os("HOME").map(PathBuf::from);
        self.insert_write_with_home(path.as_ref(), home.as_deref())
    }

    /// Inserts an execute grant unless it would expose `$HOME` or an ancestor.
    pub fn insert_exec(&mut self, path: impl AsRef<Path>) -> bool {
        let home = std::env::var_os("HOME").map(PathBuf::from);
        self.insert_exec_with_home(path.as_ref(), home.as_deref())
    }

    pub fn insert_rwx(&mut self, path: impl AsRef<Path>) -> bool {
        let home = std::env::var_os("HOME").map(PathBuf::from);
        self.insert_rwx_with_home(path.as_ref(), home.as_deref())
    }

    pub fn contains_read(&self, path: impl AsRef<Path>) -> bool {
        contains(&self.read, path.as_ref())
    }

    pub fn contains_write(&self, path: impl AsRef<Path>) -> bool {
        contains(&self.write, path.as_ref())
    }

    pub fn contains_exec(&self, path: impl AsRef<Path>) -> bool {
        contains(&self.exec, path.as_ref())
    }

    pub(crate) fn insert_write_with_home(&mut self, path: &Path, home: Option<&Path>) -> bool {
        let path = canonicalize_for_containment(path);
        self.read.insert(path.clone());
        if home.is_some_and(|home| home_or_ancestor(&path, home)) {
            return false;
        }
        self.write.insert(path);
        true
    }

    pub(crate) fn insert_exec_with_home(&mut self, path: &Path, home: Option<&Path>) -> bool {
        let path = canonicalize_for_containment(path);
        self.read.insert(path.clone());
        if home.is_some_and(|home| home_or_ancestor(&path, home)) {
            return false;
        }
        self.exec.insert(path);
        true
    }

    pub(crate) fn insert_rwx_with_home(&mut self, path: &Path, home: Option<&Path>) -> bool {
        let path = canonicalize_for_containment(path);
        self.read.insert(path.clone());
        if home.is_some_and(|home| home_or_ancestor(&path, home)) {
            return false;
        }
        self.write.insert(path.clone());
        self.exec.insert(path);
        true
    }
}

fn contains(paths: &BTreeSet<PathBuf>, path: &Path) -> bool {
    let candidate = canonicalize_for_containment(path);
    paths.iter().any(|root| candidate.starts_with(root))
}

pub(crate) fn home_or_ancestor(path: &Path, home: &Path) -> bool {
    let path = canonicalize_for_containment(path);
    let home = canonicalize_for_containment(home);
    home.starts_with(path)
}

pub(crate) fn canonicalize_for_containment(path: &Path) -> PathBuf {
    if let Ok(canonical) = std::fs::canonicalize(path) {
        return canonical;
    }

    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    };
    normalize(&absolute)
}

fn normalize(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_and_exec_imply_read() {
        let temp = tempfile::tempdir().unwrap();
        let write = temp.path().join("write");
        let exec = temp.path().join("exec");
        let mut allow = ResolvedAllowlist::new();
        assert!(allow.insert_write_with_home(&write, None));
        assert!(allow.insert_exec_with_home(&exec, None));
        assert!(allow.contains_read(write));
        assert!(allow.contains_read(exec));
    }

    #[test]
    fn containment_is_component_aware() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("root");
        let sibling = temp.path().join("root-other");
        let mut allow = ResolvedAllowlist::new();
        allow.insert_read(&root);
        assert!(allow.contains_read(root.join("nested/file")));
        assert!(!allow.contains_read(sibling));
    }

    #[test]
    fn home_and_ancestors_never_receive_write_or_exec() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        std::fs::create_dir(&home).unwrap();
        let mut allow = ResolvedAllowlist::new();
        assert!(!allow.insert_write_with_home(&home, Some(&home)));
        assert!(!allow.insert_exec_with_home(temp.path(), Some(&home)));
        assert!(allow.write_paths().is_empty());
        assert!(allow.exec_paths().is_empty());
    }
}
