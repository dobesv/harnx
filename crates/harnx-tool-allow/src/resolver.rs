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
    apply_explicit_inputs(&mut resolved, inputs, cwd, env);
    apply_enabled_batches(&mut resolved, inputs, cwd, env);
    resolved
}

fn apply_explicit_inputs(
    resolved: &mut ResolvedAllowlist,
    inputs: &AllowInputs,
    cwd: &Path,
    env: &AllowEnv,
) {
    for path in &inputs.read {
        resolved.insert_read(absolute(path, cwd, env.home.as_deref()));
    }
    for path in &inputs.write {
        resolved.insert_write_with_home(
            &absolute(path, cwd, env.home.as_deref()),
            env.home.as_deref(),
        );
    }
    for path in &inputs.exec {
        resolved.insert_exec_with_home(
            &absolute(path, cwd, env.home.as_deref()),
            env.home.as_deref(),
        );
    }
    for path in &inputs.rwx {
        resolved.insert_rwx_with_home(
            &absolute(path, cwd, env.home.as_deref()),
            env.home.as_deref(),
        );
    }
}

fn apply_enabled_batches(
    resolved: &mut ResolvedAllowlist,
    inputs: &AllowInputs,
    cwd: &Path,
    env: &AllowEnv,
) {
    if inputs.common_default {
        apply_rules(resolved, common_default(env), env);
    }
    if inputs.dev_tools {
        apply_rules(resolved, dev_tools(env), env);
    }
    if inputs.repo_work {
        apply_rules(resolved, repo_work(cwd, env), env);
    }
    if inputs.all {
        apply_rules(resolved, all(env), env);
    }
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

fn absolute(path: &Path, cwd: &Path, home: Option<&Path>) -> PathBuf {
    let path = expand_home(path, home);
    if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    }
}

/// Expand a leading `~`, which is not a relative path.
///
/// Allowlist entries come from config files and command lines that no shell
/// has touched, so a written `~/.cache` arrives literally. Treating it as
/// relative silently produced `<cwd>/~/.cache`; the sandbox then failed to set
/// up that nonexistent directory and reported a bare "No such file or
/// directory" naming neither the path nor the flag it came from.
///
/// `~user` is left alone: resolving another account's home needs a passwd
/// lookup, and guessing would grant access to the wrong directory.
fn expand_home(path: &Path, home: Option<&Path>) -> PathBuf {
    let Some(home) = home else {
        return path.to_path_buf();
    };
    // Matching on components rather than on a lossy string: a path whose bytes
    // are not valid UTF-8 would come back with replacement characters, and
    // joining that onto the home directory names a different file. Comparing
    // components also leaves `~user` alone for free, since `~user` is one
    // component and does not have `~` as a prefix.
    match path.strip_prefix("~") {
        Ok(rest) if rest.as_os_str().is_empty() => home.to_path_buf(),
        Ok(rest) => home.join(rest),
        Err(_) => path.to_path_buf(),
    }
}

#[cfg(test)]
#[cfg(unix)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn leading_tilde_resolves_against_home_not_the_working_directory() {
        let home = PathBuf::from("/home/example");
        let cwd = PathBuf::from("/mnt/projects/repo");

        assert_eq!(
            absolute(Path::new("~/.cache"), &cwd, Some(&home)),
            PathBuf::from("/home/example/.cache")
        );
        assert_eq!(absolute(Path::new("~"), &cwd, Some(&home)), home);
        // Absolute and genuinely relative paths keep their existing behaviour.
        assert_eq!(
            absolute(Path::new("/etc"), &cwd, Some(&home)),
            PathBuf::from("/etc")
        );
        assert_eq!(
            absolute(Path::new("sub/dir"), &cwd, Some(&home)),
            cwd.join("sub/dir")
        );
    }

    /// A filename does not have to be valid UTF-8. Converting one to a string
    /// substitutes replacement characters, which would grant a path that is not
    /// the one asked for.
    #[cfg(unix)]
    #[test]
    fn non_utf8_bytes_under_the_home_directory_survive_expansion() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let home = PathBuf::from("/home/example");
        let cwd = PathBuf::from("/mnt/projects/repo");
        let written = PathBuf::from(OsStr::from_bytes(b"~/\xff\xfe-cache"));

        let resolved = absolute(&written, &cwd, Some(&home));

        // Byte-exact: expanding through a lossy string yields the U+FFFD
        // encoding here instead of the original 0xff 0xfe.
        assert_eq!(
            resolved,
            PathBuf::from(OsStr::from_bytes(b"/home/example/\xff\xfe-cache"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn other_users_home_and_unknown_home_are_left_alone() {
        let cwd = PathBuf::from("/mnt/projects/repo");
        // Resolving ~someone needs a passwd lookup; guessing could grant access
        // to the wrong directory, so it stays relative as before.
        assert_eq!(
            absolute(
                Path::new("~other/data"),
                &cwd,
                Some(Path::new("/home/example"))
            ),
            cwd.join("~other/data")
        );
        assert_eq!(
            absolute(Path::new("~/.cache"), &cwd, None),
            cwd.join("~/.cache")
        );
    }

    #[cfg(unix)]
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

    #[cfg(unix)]
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

    #[cfg(unix)]
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
