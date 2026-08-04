use std::path::{Path, PathBuf};

use crate::allowlist::home_or_ancestor;
#[cfg(unix)]
use crate::{detect_project_root, RootKind};

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
    let mut rules = Vec::new();
    add_home_dev_rules(&mut rules, env);
    add_cargo_dev_rules(&mut rules, env);
    add_go_dev_rules(&mut rules, env);
    add_homebrew_rule(&mut rules, env);
    rules
}

fn add_home_dev_rules(rules: &mut Vec<AllowRule>, env: &AllowEnv) {
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

    let Some(home) = &env.home else {
        return;
    };
    rules.extend(
        HOME_READ
            .iter()
            .map(|path| (home.join(path), Permission::Read)),
    );
    for path in HOME_EXEC {
        push_guarded(rules, home.join(path), Permission::ReadExec, env);
    }
    for path in HOME_WRITE {
        push_guarded(rules, home.join(path), Permission::ReadWrite, env);
    }
    push_guarded(
        rules,
        home.join(".config/go"),
        Permission::ReadWriteExec,
        env,
    );
}

fn add_cargo_dev_rules(rules: &mut Vec<AllowRule>, env: &AllowEnv) {
    let Some(cargo_home) = &env.cargo_home else {
        return;
    };
    rules.push((cargo_home.clone(), Permission::Read));
    for (relative, permission) in [
        ("bin", Permission::ReadExec),
        ("registry", Permission::ReadWrite),
        ("git", Permission::ReadWrite),
    ] {
        push_guarded(rules, cargo_home.join(relative), permission, env);
    }
}

fn add_go_dev_rules(rules: &mut Vec<AllowRule>, env: &AllowEnv) {
    if let Some(goroot) = &env.goroot {
        push_guarded(rules, goroot.clone(), Permission::ReadExec, env);
    }
    if let Some(gopath) = &env.gopath {
        push_guarded(rules, gopath.join("bin"), Permission::ReadExec, env);
        push_guarded(rules, gopath.join("pkg"), Permission::ReadWrite, env);
    }
    for cache in [&env.gomodcache, &env.gocache].into_iter().flatten() {
        push_guarded(rules, cache.clone(), Permission::ReadWrite, env);
    }
    if let Some(gobin) = &env.gobin {
        push_guarded(rules, gobin.clone(), Permission::ReadExec, env);
    }
}

fn add_homebrew_rule(rules: &mut Vec<AllowRule>, env: &AllowEnv) {
    if let Some(prefix) = env.homebrew_prefix.clone().or_else(default_homebrew_prefix) {
        push_guarded(rules, prefix, Permission::ReadExec, env);
    }
}

pub fn repo_work(cwd: &Path, env: &AllowEnv) -> Vec<AllowRule> {
    let mut rules = Vec::new();
    for (root, permission) in detected_repo_roots(cwd) {
        push_guarded(&mut rules, root, permission, env);
    }
    push_guarded(
        &mut rules,
        absolute_from(cwd),
        Permission::ReadWriteExec,
        env,
    );
    rules
}

#[cfg(unix)]
fn detected_repo_roots(cwd: &Path) -> Vec<AllowRule> {
    [
        (RootKind::GitRoot, Permission::ReadWriteExec),
        (RootKind::GitCommonDir, Permission::ReadWrite),
        (RootKind::CargoRoot, Permission::ReadWriteExec),
        (RootKind::NodeProjectRoot, Permission::ReadWriteExec),
        (RootKind::GoRoot, Permission::ReadWriteExec),
    ]
    .into_iter()
    .filter_map(|(kind, permission)| detect_project_root(kind, cwd).map(|root| (root, permission)))
    .collect()
}

#[cfg(not(unix))]
fn detected_repo_roots(_cwd: &Path) -> Vec<AllowRule> {
    Vec::new()
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

    #[cfg(unix)]
    #[test]
    fn repo_work_grants_cwd_and_git_common_dir_without_exec() {
        let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let env = AllowEnv {
            home: Some(PathBuf::from("/home/tester")),
            ..Default::default()
        };
        let rules = repo_work(&manifest, &env);
        let common = detect_project_root(RootKind::GitCommonDir, &manifest)
            .expect("workspace git common dir");
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
        let dir = temp.path().canonicalize().unwrap();
        let env = AllowEnv {
            home: Some(dir.clone()),
            ..Default::default()
        };
        let rules = repo_work(&dir, &env);
        assert_eq!(rules, vec![(dir, Permission::Read)]);
    }

    #[cfg(unix)]
    #[test]
    fn repo_work_does_not_select_marker_above_home() {
        use crate::test_support::{env_lock, EnvGuard};

        let _lock = env_lock();
        let _env = EnvGuard::new();
        let temp = tempfile::tempdir().expect("tempdir");
        let base = temp.path().canonicalize().expect("canonical tempdir");
        let home = base.join("home");
        let cwd = home.join("project/src");
        std::fs::create_dir_all(&cwd).expect("create cwd");
        std::fs::write(base.join("Cargo.toml"), "[workspace]\n").expect("write marker above home");
        unsafe { std::env::set_var("HOME", &home) };
        let env = AllowEnv {
            home: Some(home),
            ..Default::default()
        };

        let rules = repo_work(&cwd, &env);

        assert!(!rules.iter().any(|(path, _)| path == &base));
        assert!(has(&rules, &cwd, Permission::ReadWriteExec));
    }

    #[test]
    fn allow_all_is_rwx_root_rule() {
        let rules = all(&AllowEnv::default());
        assert_eq!(rules, vec![(PathBuf::from("/"), Permission::ReadWriteExec)]);
    }
}
