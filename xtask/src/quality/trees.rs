//! The two trees `cargo xtask quality` analyses. Each is a scratch copy of
//! just the `.rs` files: the base has no checkout to analyse, and rustqual
//! walks every directory it is given, so pointing it at the checkout would
//! also read files git ignores.

use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{id, Command, Stdio};

use anyhow::{anyhow, Context, Result};

use crate::{command_failure, git};

/// A directory under the system temp dir, removed on drop.
pub struct ScratchDir {
    path: PathBuf,
}

impl ScratchDir {
    pub fn create(label: &str) -> Result<Self> {
        let path = env::temp_dir().join(format!("harnx-{label}-{}", id()));
        // An earlier run that was killed may have left one under the same pid.
        if path.exists() {
            fs::remove_dir_all(&path)
                .with_context(|| format!("failed to remove {}", path.display()))?;
        }
        fs::create_dir_all(&path)
            .with_context(|| format!("failed to create {}", path.display()))?;
        Ok(Self { path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        if let Err(err) = fs::remove_dir_all(&self.path) {
            eprintln!("warning: failed to remove {}: {err}", self.path.display());
        }
    }
}

/// The base and head trees, written under `scratch`. The head is `head` when
/// given, otherwise the working tree with uncommitted and untracked files.
pub struct Trees {
    pub base: PathBuf,
    pub head: PathBuf,
}

impl Trees {
    pub fn export(root: &Path, base: &str, head: Option<&str>, scratch: &Path) -> Result<Self> {
        let trees = Self {
            base: scratch.join("base"),
            head: scratch.join("head"),
        };
        export_revision(root, base, &trees.base, &scratch.join("base.index"))?;
        match head {
            Some(head) => export_revision(root, head, &trees.head, &scratch.join("head.index"))?,
            None => copy_worktree(root, &trees.head)?,
        }
        Ok(trees)
    }
}

/// Writes `rev`'s `.rs` files under `dest` through the throwaway index
/// `index`, leaving the repository's own index and worktree list alone.
fn export_revision(root: &Path, rev: &str, dest: &Path, index: &Path) -> Result<()> {
    let git_with_index = || {
        let mut command = Command::new("git");
        command.current_dir(root).env("GIT_INDEX_FILE", index);
        command
    };
    let read_tree = git_with_index().args(["read-tree", rev]).output();
    check(read_tree, "git read-tree")?;

    let mut list = git_with_index()
        .args(["ls-files", "-z", "--", "*.rs"])
        .stdout(Stdio::piped())
        .spawn()
        .context("failed to run git ls-files")?;
    let listed = list
        .stdout
        .take()
        .ok_or_else(|| anyhow!("git ls-files has no stdout"))?;
    let checkout = git_with_index()
        .args(["checkout-index", "-z", "--stdin"])
        .arg(format!("--prefix={}/", dest.display()))
        .stdin(listed)
        .output();
    let listing = list.wait().context("failed to wait for git ls-files")?;
    check(checkout, "git checkout-index")?;
    if !listing.success() {
        return Err(command_failure("git ls-files", listing, &[]));
    }
    Ok(())
}

/// Copies the `.rs` files git tracks, or would track, from the checkout.
fn copy_worktree(root: &Path, dest: &Path) -> Result<()> {
    let listed = git::output(
        root,
        &[
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
            "--",
            "*.rs",
        ],
    )?;
    for file in listed.split('\0').filter(|file| !file.is_empty()) {
        let source = root.join(file);
        // Deleted in the working tree but not yet staged.
        if !source.is_file() {
            continue;
        }
        let target = dest.join(file);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        fs::copy(&source, &target)
            .with_context(|| format!("failed to copy {}", source.display()))?;
    }
    Ok(())
}

fn check(output: std::io::Result<std::process::Output>, label: &str) -> Result<()> {
    let output = output.with_context(|| format!("failed to run {label}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(command_failure(label, output.status, &output.stderr))
    }
}

/// The base paths the change renamed, mapped to their new paths. Without
/// `head` the change is the working tree.
pub fn renames(root: &Path, base: &str, head: Option<&str>) -> Result<BTreeMap<String, String>> {
    let mut args = vec!["diff", "-z", "--name-status", "-M", base];
    args.extend(head);
    Ok(parse_renames(&git::output(root, &args)?))
}

/// Reads `git diff -z --name-status` output, where a rename or copy is
/// followed by two paths and every other status by one.
fn parse_renames(name_status: &str) -> BTreeMap<String, String> {
    let mut fields = name_status.split('\0');
    let mut renames = BTreeMap::new();
    while let Some(status) = fields.next() {
        match status.chars().next() {
            Some('R') => {
                if let (Some(from), Some(to)) = (fields.next(), fields.next()) {
                    renames.insert(from.to_owned(), to.to_owned());
                }
            }
            Some('C') => {
                fields.nth(1);
            }
            Some(_) => {
                fields.next();
            }
            None => {}
        }
    }
    renames
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renames_are_read_from_name_status_output() {
        let output = "M\0src/kept.rs\0R087\0src/old.rs\0src/new.rs\0C100\0src/a.rs\0src/b.rs\0A\0src/added.rs\0";

        let renames = parse_renames(output);

        assert_eq!(
            renames,
            BTreeMap::from([("src/old.rs".to_owned(), "src/new.rs".to_owned())])
        );
    }

    #[test]
    fn a_revision_exports_only_its_rust_files() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let scratch = ScratchDir::create("quality-test").unwrap();
        let dest = scratch.path().join("tree");

        export_revision(&root, "HEAD", &dest, &scratch.path().join("index")).unwrap();

        assert!(dest.join("xtask/src/main.rs").is_file());
        assert!(!dest.join("xtask/Cargo.toml").exists());
    }
}
