use std::path::{Path, PathBuf};

use crate::{
    all, common_default, dev_tools, repo_work, AllowEnv, AllowRule, Permission, ResolvedAllowlist,
};

/// Parsed allowlist inputs. CLI and environment parsing stay in caller crates.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AllowInputs {
    pub read: Vec<PathBuf>,
    pub write: Vec<PathBuf>,
    pub exec: Vec<PathBuf>,
    pub rwx: Vec<PathBuf>,
    pub common_default: bool,
    pub dev_tools: bool,
    pub repo_work: bool,
    pub all: bool,
}

/// Resolve explicit grants and enabled batches without async work or global state.
pub fn resolve_allowlist(inputs: &AllowInputs, cwd: &Path, env: &AllowEnv) -> ResolvedAllowlist {
    let mut resolved = ResolvedAllowlist::new();
    for path in &inputs.read {
        resolved.insert_read(absolute(path, cwd));
    }
    for path in &inputs.write {
        resolved.insert_write_with_home(&absolute(path, cwd), env.home.as_deref());
    }
    for path in &inputs.exec {
        resolved.insert_exec_with_home(&absolute(path, cwd), env.home.as_deref());
    }
    for path in &inputs.rwx {
        resolved.insert_rwx_with_home(&absolute(path, cwd), env.home.as_deref());
    }

    if inputs.common_default {
        apply_rules(&mut resolved, common_default(env), env);
    }
    if inputs.dev_tools {
        apply_rules(&mut resolved, dev_tools(env), env);
    }
    if inputs.repo_work {
        apply_rules(&mut resolved, repo_work(cwd, env), env);
    }
    if inputs.all {
        apply_rules(&mut resolved, all(env), env);
    }
    resolved
}

fn apply_rules(resolved: &mut ResolvedAllowlist, rules: Vec<AllowRule>, env: &AllowEnv) {
    for (path, permission) in rules {
        match permission {
            Permission::Read => resolved.insert_read(path),
            Permission::ReadWrite => {
                resolved.insert_write_with_home(&path, env.home.as_deref());
            }
            Permission::ReadExec => {
                resolved.insert_exec_with_home(&path, env.home.as_deref());
            }
            Permission::ReadWriteExec => {
                resolved.insert_rwx_with_home(&path, env.home.as_deref());
            }
        }
    }
}

fn absolute(path: &Path, cwd: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_inputs_hard_deny_without_fallback() {
        let resolved = resolve_allowlist(
            &AllowInputs::default(),
            Path::new("/work/project"),
            &AllowEnv {
                home: Some(PathBuf::from("/home/tester")),
                ..Default::default()
            },
        );
        assert!(resolved.is_empty());
        assert!(!resolved.contains_read("/work/project"));
    }

    #[test]
    fn explicit_write_and_exec_are_home_guarded() {
        let inputs = AllowInputs {
            write: vec![PathBuf::from("/")],
            exec: vec![PathBuf::from("/home/tester")],
            read: vec![PathBuf::from("/home/tester")],
            ..Default::default()
        };
        let env = AllowEnv {
            home: Some(PathBuf::from("/home/tester")),
            ..Default::default()
        };
        let resolved = resolve_allowlist(&inputs, Path::new("/work"), &env);
        assert!(resolved.contains_read("/home/tester"));
        assert!(!resolved.contains_write("/home/tester"));
        assert!(!resolved.contains_exec("/home/tester"));
    }

    #[test]
    fn allow_all_is_downgraded_to_read_by_home_guard() {
        let inputs = AllowInputs {
            all: true,
            ..Default::default()
        };
        let env = AllowEnv {
            home: Some(PathBuf::from("/home/tester")),
            ..Default::default()
        };
        let resolved = resolve_allowlist(&inputs, Path::new("/work"), &env);
        assert!(resolved.contains_read("/home/tester"));
        assert!(!resolved.contains_write("/"));
        assert!(!resolved.contains_exec("/"));
    }
}
