//! Turns the CLI's `--allow-*` paths into `harnx-sandbox-exec` arguments.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use harnx_sandbox_common::{expand_path_var, is_home_or_ancestor, resolve_path};

use crate::cli::Cli;

/// Appends `harnx-sandbox-exec` arguments for the CLI's `--allow-*` grants.
pub(crate) fn push_cli_grants(args: &mut Vec<OsString>, cli: &Cli, cwd: &Path) {
    let grants: [(&str, &[&str], &Vec<PathBuf>); 4] = [
        ("--allow-read", &["--read"], &cli.allow_read),
        ("--allow-write", &["--write"], &cli.allow_write),
        ("--allow-exec", &["--exec"], &cli.allow_exec),
        (
            "--allow-rwx",
            &["--read", "--write", "--exec"],
            &cli.allow_rwx,
        ),
    ];
    for (cli_flag, exec_flags, paths) in grants {
        for path in paths {
            let granted = granted_paths(path, cli_flag, cwd);
            push_exec_flags(args, exec_flags, &granted);
        }
    }
}

/// The paths to grant for one `--allow-*` argument, or none when it would
/// expose the home directory. The resolved path is always granted. When the
/// path as given differs, as it does through a symlink such as `/lib64` on
/// merged-/usr systems, it is granted too: given that, the sandbox recreates
/// the symlink, and binaries still find their ELF interpreter.
fn granted_paths(path: &Path, cli_flag: &str, cwd: &Path) -> Vec<PathBuf> {
    let Some(expanded) = expand_path_var(&path.to_string_lossy(), cwd) else {
        return Vec::new();
    };
    let resolved = resolve_path(&expanded);
    if is_home_or_ancestor(&resolved) {
        eprintln!(
            "harnx-sandbox-run: warning: ignoring {cli_flag} {}: would expose home directory",
            path.display()
        );
        return Vec::new();
    }
    let given = std::path::absolute(&expanded).unwrap_or_else(|_| resolved.clone());
    let mut granted = vec![resolved];
    if given != granted[0] {
        granted.push(given);
    }
    granted
}

/// Appends each of `exec_flags` for each of `paths`.
fn push_exec_flags(args: &mut Vec<OsString>, exec_flags: &[&str], paths: &[PathBuf]) {
    for flag in exec_flags {
        for path in paths {
            args.push((*flag).into());
            args.push(path.clone().into_os_string());
        }
    }
}
