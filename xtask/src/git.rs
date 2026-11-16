//! The git commands the xtask subcommands share.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};

use crate::command_failure;

/// Where a change starts when no `--base` is given: its merge base with the
/// remote's default branch.
const DEFAULT_BASE_REFS: [&str; 2] = ["origin/HEAD", "origin/main"];

/// The repository's top-level directory, found from the current directory.
pub fn toplevel() -> Result<PathBuf> {
    let root = output(Path::new("."), &["rev-parse", "--show-toplevel"])?;
    Ok(PathBuf::from(root.trim()))
}

/// The merge base of `head` with the first of [`DEFAULT_BASE_REFS`] that
/// exists.
pub fn default_base(root: &Path, head: &str) -> Result<String> {
    for candidate in DEFAULT_BASE_REFS {
        if let Ok(base) = output(root, &["merge-base", head, candidate]) {
            return Ok(base.trim().to_owned());
        }
    }
    bail!("no merge base with {DEFAULT_BASE_REFS:?}; pass --base <rev>")
}

/// Runs git in `dir` and returns its standard output, failing with its
/// standard error when it exits unsuccessfully.
pub fn output(dir: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .context("failed to run git")?;
    if !output.status.success() {
        let label = format!("git {}", args.join(" "));
        return Err(command_failure(&label, output.status, &output.stderr));
    }
    String::from_utf8(output.stdout).context("git output is not UTF-8")
}
