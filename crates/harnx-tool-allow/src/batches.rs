use std::path::{Path, PathBuf};

use crate::allowlist::home_or_ancestor;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Permission {
    Read,
    ReadWrite,
    ReadExec,
    ReadWriteExec,
}

pub type AllowRule = (PathBuf, Permission);

/// Environment-derived paths needed while resolving batches.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AllowEnv {
    pub home: Option<PathBuf>,
    pub tmpdir: Option<PathBuf>,
    pub cargo_home: Option<PathBuf>,
    pub goroot: Option<PathBuf>,
    pub gopath: Option<PathBuf>,
    pub gobin: Option<PathBuf>,
    pub gomodcache: Option<PathBuf>,
    pub gocache: Option<PathBuf>,
    pub homebrew_prefix: Option<PathBuf>,
}

impl AllowEnv {
    pub fn from_current_process() -> Self {
        Self {
            home: env_path("HOME"),
            tmpdir: env_path("TMPDIR"),
            cargo_home: env_path("CARGO_HOME"),
            goroot: env_path("GOROOT"),
            gopath: env_path("GOPATH"),
            gobin: env_path("GOBIN"),
            gomodcache: env_path("GOMODCACHE"),
            gocache: env_path("GOCACHE"),
            homebrew_prefix: env_path("HOMEBREW_PREFIX"),
        }
    }
}

fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

pub fn common_default(env: &AllowEnv) -> Vec<AllowRule> {
    let mut rules = Vec::new();
    for path in system_exec_paths() {
        push_guarded(&mut rules, PathBuf::from(path), Permission::ReadExec, env);
    }
    for path in system_read_paths() {
        rules.push((PathBuf::from(path), Permission::Read));
    }
    for path in system_writable_paths(env) {
        push_guarded(&mut rules, path, Permission::ReadWriteExec, env);
    }
    rules
}

pub fn dev_tools(env: &AllowEnv) -> Vec<AllowRule> {
    const HOME_READ: &[&str] = &[
        ".gitconfig",
        ".gitignore",
        ".gitignore_global",
        ".tool-versions",
    ];
    const HOME_EXEC: &[&str] = &[
        ".local/bin",
        ".local/lib",
        ".bun",
        ".asdf",
        "go/bin",
        ".cargo",
        ".nvm",
        ".cargo/bin",
        ".mono",
        ".pyenv",
        ".rye",
        ".local/share/claude",
        ".local/share/opencode",
        ".local/share/pipx",
        ".rustup",
    ];
    const HOME_WRITE: &[&str] = &[
        ".cache",
        "go/pkg",
        ".npm",
        ".yarn",
        ".cargo/registry",
        ".cargo/git",
        ".bun/install/cache",
        ".local/share/pnpm",
        ".local/share/uv",
    ];

    let mut rules = Vec::new();
    if let Some(home) = &env.home {
        rules.extend(
            HOME_READ
                .iter()
                .map(|path| (home.join(path), Permission::Read)),
        );
        for path in HOME_EXEC {
            push_guarded(&mut rules, home.join(path), Permission::ReadExec, env);
        }
        for path in HOME_WRITE {
            push_guarded(&mut rules, home.join(path), Permission::ReadWrite, env);
        }
        push_guarded(
            &mut rules,
            home.join(".config/go"),
            Permission::ReadWriteExec,
            env,
        );
    }

    if let Some(cargo_home) = &env.cargo_home {
        rules.push((cargo_home.clone(), Permission::Read));
        push_guarded(
            &mut rules,
            cargo_home.join("bin"),
            Permission::ReadExec,
            env,
        );
        push_guarded(
            &mut rules,
            cargo_home.join("registry"),
            Permission::ReadWrite,
            env,
        );
        push_guarded(
            &mut rules,
            cargo_home.join("git"),
            Permission::ReadWrite,
            env,
        );
    }
    if let Some(goroot) = &env.goroot {
        push_guarded(&mut rules, goroot.clone(), Permission::ReadExec, env);
    }
    if let Some(gopath) = &env.gopath {
        push_guarded(&mut rules, gopath.join("bin"), Permission::ReadExec, env);
        push_guarded(&mut rules, gopath.join("pkg"), Permission::ReadWrite, env);
    }
    if let Some(gobin) = &env.gobin {
        push_guarded(&mut rules, gobin.clone(), Permission::ReadExec, env);
    }
    if let Some(cache) = &env.gomodcache {
        push_guarded(&mut rules, cache.clone(), Permission::ReadWrite, env);
    }
    if let Some(cache) = &env.gocache {
        push_guarded(&mut rules, cache.clone(), Permission::ReadWrite, env);
    }

    let homebrew = env.homebrew_prefix.clone().or_else(default_homebrew_prefix);
    if let Some(prefix) = homebrew {
        push_guarded(&mut rules, prefix, Permission::ReadExec, env);
    }
    rules
}

pub fn repo_work(cwd: &Path, env: &AllowEnv) -> Vec<AllowRule> {
    let mut rules = Vec::new();
    if let Some(root) = git_root(cwd) {
        push_guarded(&mut rules, root, Permission::ReadWriteExec, env);
    }
    if let Some(common) = git_common_dir(cwd) {
        push_guarded(&mut rules, common, Permission::ReadWrite, env);
    }
    for (marker, highest) in [
        ("Cargo.toml", true),
        ("package.json", true),
        ("go.mod", false),
    ] {
        if let Some(root) = marker_root(cwd, marker, highest) {
            push_guarded(&mut rules, root, Permission::ReadWriteExec, env);
        }
    }
    push_guarded(
        &mut rules,
        absolute_from(cwd),
        Permission::ReadWriteExec,
        env,
    );
    rules
}

pub fn all(_env: &AllowEnv) -> Vec<AllowRule> {
    vec![(PathBuf::from("/"), Permission::ReadWriteExec)]
}

fn push_guarded(rules: &mut Vec<AllowRule>, path: PathBuf, permission: Permission, env: &AllowEnv) {
    let privileged = permission != Permission::Read;
    if privileged
        && env
            .home
            .as_deref()
            .is_some_and(|home| home_or_ancestor(&path, home))
    {
        rules.push((path, Permission::Read));
    } else {
        rules.push((path, permission));
    }
}

fn absolute_from(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| {
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()
                .map(|current| current.join(path))
                .unwrap_or_else(|_| path.to_path_buf())
        }
    })
}

#[cfg(unix)]
fn git_root(cwd: &Path) -> Option<PathBuf> {
    gix::discover(cwd).ok()?.workdir().map(Path::to_path_buf)
}

#[cfg(not(unix))]
fn git_root(_cwd: &Path) -> Option<PathBuf> {
    None
}

#[cfg(unix)]
fn git_common_dir(cwd: &Path) -> Option<PathBuf> {
    Some(gix::discover(cwd).ok()?.common_dir().to_path_buf())
}

#[cfg(not(unix))]
fn git_common_dir(_cwd: &Path) -> Option<PathBuf> {
    None
}

fn marker_root(cwd: &Path, marker: &str, highest: bool) -> Option<PathBuf> {
    let mut current = if cwd.is_dir() {
        cwd.to_path_buf()
    } else {
        cwd.parent()?.to_path_buf()
    };
    let mut found = None;
    loop {
        if current.join(marker).exists() {
            if !highest {
                return Some(current);
            }
            found = Some(current.clone());
        }
        let Some(parent) = current.parent() else {
            return found;
        };
        current = parent.to_path_buf();
    }
}

#[cfg(target_os = "linux")]
fn system_exec_paths() -> &'static [&'static str] {
    &[
        "/usr/bin",
        "/bin",
        "/usr/local/bin",
        "/usr/local/lib",
        "/usr/sbin",
        "/sbin",
        "/usr/lib",
        "/usr/lib64",
        "/lib",
        "/lib64",
        "/usr/lib/x86_64-linux-gnu",
        "/usr/libexec",
        "/usr/share",
        "/proc",
        "/dev",
        "/sys",
        "/etc",
        "/tmp",
        "/run/systemd/resolve",
        "/run/resolvconf",
        "/run/NetworkManager",
        "/run/current-system",
        "/run/opengl-driver",
        "/run/opengl-driver-32",
        "/run/udev",
    ]
}

#[cfg(target_os = "macos")]
fn system_exec_paths() -> &'static [&'static str] {
    &[
        "/usr/bin",
        "/bin",
        "/usr/local/bin",
        "/usr/sbin",
        "/sbin",
        "/usr/lib",
        "/usr/local/lib",
        "/Library",
        "/System",
        "/private/tmp",
        "/private/var",
        "/dev",
    ]
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn system_exec_paths() -> &'static [&'static str] {
    &[
        "/usr/bin",
        "/bin",
        "/usr/lib",
        "/lib",
        "/usr/local/bin",
        "/usr/local/lib",
        "/tmp",
        "/dev",
    ]
}

#[cfg(target_os = "linux")]
fn system_read_paths() -> &'static [&'static str] {
    &[
        "/usr/local",
        "/usr/include",
        "/usr/include/x86_64-linux-gnu",
    ]
}

#[cfg(target_os = "macos")]
fn system_read_paths() -> &'static [&'static str] {
    &["/usr/local"]
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn system_read_paths() -> &'static [&'static str] {
    &["/usr/include"]
}

fn system_writable_paths(_env: &AllowEnv) -> Vec<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        vec![PathBuf::from("/tmp"), PathBuf::from("/dev/shm")]
    }
    #[cfg(target_os = "macos")]
    {
        let mut paths = vec![PathBuf::from("/private/tmp")];
        if let Some(tmpdir) = &_env.tmpdir {
            if tmpdir != Path::new("/private/tmp") {
                paths.push(tmpdir.clone());
            }
        }
        paths
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = env;
        vec![PathBuf::from("/tmp")]
    }
}

fn default_homebrew_prefix() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        Some(PathBuf::from("/opt/homebrew"))
    }
    #[cfg(target_os = "linux")]
    {
        Some(PathBuf::from("/home/linuxbrew/.linuxbrew"))
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn has(rules: &[AllowRule], path: impl AsRef<Path>, permission: Permission) -> bool {
        rules
            .iter()
            .any(|rule| rule == &(path.as_ref().to_path_buf(), permission))
    }

    #[test]
    fn common_default_has_system_and_temp_members() {
        let rules = common_default(&AllowEnv::default());
        assert!(has(&rules, "/usr/bin", Permission::ReadExec));
        #[cfg(target_os = "linux")]
        assert!(has(&rules, "/dev/shm", Permission::ReadWriteExec));
    }

    #[test]
    fn dev_tools_has_user_red_line_members() {
        let env = AllowEnv {
            home: Some(PathBuf::from("/home/tester")),
            ..Default::default()
        };
        let rules = dev_tools(&env);
        assert!(has(&rules, "/home/tester/.rustup", Permission::ReadExec));
        assert!(has(
            &rules,
            "/home/tester/.config/go",
            Permission::ReadWriteExec
        ));
    }

    #[test]
    fn repo_work_grants_cwd_and_git_common_dir_without_exec() {
        let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let env = AllowEnv {
            home: Some(PathBuf::from("/home/tester")),
            ..Default::default()
        };
        let rules = repo_work(&manifest, &env);
        let common = git_common_dir(&manifest).expect("workspace git common dir");
        assert!(has(&rules, &common, Permission::ReadWrite));
        assert!(!has(&rules, &common, Permission::ReadWriteExec));
        assert!(has(
            &rules,
            absolute_from(&manifest),
            Permission::ReadWriteExec
        ));
    }

    #[test]
    fn repo_work_guards_home_cwd() {
        let temp = tempfile::tempdir().unwrap();
        let env = AllowEnv {
            home: Some(temp.path().to_path_buf()),
            ..Default::default()
        };
        let rules = repo_work(temp.path(), &env);
        assert_eq!(rules, vec![(temp.path().to_path_buf(), Permission::Read)]);
    }

    #[test]
    fn allow_all_is_rwx_root_rule() {
        let rules = all(&AllowEnv::default());
        assert_eq!(rules, vec![(PathBuf::from("/"), Permission::ReadWriteExec)]);
    }
}
