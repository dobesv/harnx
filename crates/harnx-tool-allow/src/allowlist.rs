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

    /// Picks a readable directory, preferring `preferred` when it is allowed.
    /// File grants are skipped because callers use this as a search or process cwd.
    pub fn default_read_directory(&self, preferred: Option<&Path>) -> Option<PathBuf> {
        preferred
            .filter(|path| path.is_dir() && self.contains_read(path))
            .map(Path::to_path_buf)
            .or_else(|| self.read.iter().find(|path| path.is_dir()).cloned())
    }

    pub fn insert_read(&mut self, path: impl AsRef<Path>) {
        self.read.insert(granted_path(path.as_ref()));
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
        self.insert_privileged_with_home(path, home, PrivilegedPermission::Write)
    }

    pub(crate) fn insert_exec_with_home(&mut self, path: &Path, home: Option<&Path>) -> bool {
        self.insert_privileged_with_home(path, home, PrivilegedPermission::Exec)
    }

    pub(crate) fn insert_rwx_with_home(&mut self, path: &Path, home: Option<&Path>) -> bool {
        self.insert_privileged_with_home(path, home, PrivilegedPermission::WriteExec)
    }

    fn insert_privileged_with_home(
        &mut self,
        path: &Path,
        home: Option<&Path>,
        permission: PrivilegedPermission,
    ) -> bool {
        let path = granted_path(path);
        self.read.insert(path.clone());
        if home.is_some_and(|home| home_or_ancestor(&path, home)) {
            return false;
        }
        if permission.grants_write() {
            self.write.insert(path.clone());
        }
        if permission.grants_exec() {
            self.exec.insert(path);
        }
        true
    }
}

#[cfg(test)]
#[cfg(unix)]
mod symlink_alias_tests {
    use super::*;
    use std::os::unix::fs::symlink;

    /// A sandbox mounts what the allowlist lists, and executables name their
    /// loader by absolute path, so a symlinked grant must survive as itself.
    /// Its target must NOT be granted: resolving here would widen the grant to
    /// wherever the link happens to point.
    #[test]
    fn symlinked_directory_is_granted_as_written_and_target_is_not() {
        let temp = tempfile::tempdir().expect("temp dir");
        let real = temp.path().join("usr-lib64");
        std::fs::create_dir(&real).expect("create target");
        let link = temp.path().join("lib64");
        symlink(&real, &link).expect("create symlink");

        let mut allowlist = ResolvedAllowlist::new();
        allowlist.insert_exec_with_home(&link, None);

        let canonical = std::fs::canonicalize(&real).expect("canonical target");
        assert!(
            allowlist.exec_paths().contains(&link),
            "the granted path itself is missing: {:?}",
            allowlist.exec_paths()
        );
        assert!(
            !allowlist.exec_paths().contains(&canonical),
            "granting a symlink must not grant its target: {:?}",
            allowlist.exec_paths()
        );
        // Checking still resolves symlinks, so a path under the grant matches
        // even when the caller names it canonically. The file has to exist:
        // a path that does not cannot be resolved, and matching then falls back
        // to comparing it literally.
        let under_grant = real.join("libc.so");
        std::fs::write(&under_grant, b"").expect("create file under the grant");
        assert!(allowlist.contains_exec(&under_grant));
    }

    #[test]
    fn a_plain_directory_is_not_duplicated() {
        let temp = tempfile::tempdir().expect("temp dir");
        let dir = temp.path().join("plain");
        std::fs::create_dir(&dir).expect("create dir");

        let mut allowlist = ResolvedAllowlist::new();
        allowlist.insert_read(&dir);

        let canonical = std::fs::canonicalize(&dir).expect("canonical");
        let matching = allowlist
            .read_paths()
            .iter()
            .filter(|path| path.ends_with("plain"))
            .count();
        assert_eq!(matching, 1, "expected only {canonical:?}");
    }
}

#[derive(Clone, Copy)]
enum PrivilegedPermission {
    Write,
    Exec,
    WriteExec,
}

impl PrivilegedPermission {
    fn grants_write(self) -> bool {
        matches!(self, Self::Write | Self::WriteExec)
    }

    fn grants_exec(self) -> bool {
        matches!(self, Self::Exec | Self::WriteExec)
    }
}

fn contains(paths: &BTreeSet<PathBuf>, path: &Path) -> bool {
    let candidate = canonicalize_for_containment(path);
    paths.iter().any(|root| {
        // Literal first: grants are stored as written, and most are already
        // canonical, so this answers without touching the filesystem.
        candidate.starts_with(root)
            // Then resolved, so a grant written through a symlink still matches
            // the canonical form of a path underneath it. Resolving here rather
            // than at insertion keeps the grant itself exactly as narrow as it
            // was written.
            || candidate.starts_with(canonicalize_for_containment(root))
    })
}

pub(crate) fn home_or_ancestor(path: &Path, home: &Path) -> bool {
    let path = canonicalize_for_containment(path);
    let home = canonicalize_for_containment(home);
    home.starts_with(path)
}

/// A grant is stored exactly as asked for, only made absolute.
///
/// Resolving symlinks here would silently widen the grant to the target, so
/// allowing a link inside your own directory could hand over whatever it points
/// at. It also loses the path callers actually use: on merged-`/usr` systems
/// `/lib64` canonicalises to `/usr/lib64`, so the sandbox never mounted
/// `/lib64`, and every dynamically linked binary failed to exec because its
/// loader is named absolutely as `/lib64/ld-linux-x86-64.so.2`.
///
/// Symlinks are still resolved when *checking* a path against these grants —
/// see [`contains`] — which is what stops a link escaping an allowed directory.
fn granted_path(path: &Path) -> PathBuf {
    if path.is_absolute() {
        return path.to_path_buf();
    }
    std::env::current_dir()
        .map(|cwd| cwd.join(path))
        .unwrap_or_else(|_| path.to_path_buf())
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
