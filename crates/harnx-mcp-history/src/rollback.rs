use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use gix::object::tree::EntryKind;

use crate::history::capture_tree_blocking;

fn is_harnx_history_commit(repo: &gix::Repository, commit_id: gix::ObjectId) -> bool {
    repo.find_object(commit_id)
        .ok()
        .and_then(|obj| obj.peel_to_commit().ok())
        .map(|commit| {
            commit
                .author()
                .ok()
                .map(|a| a.email == "harnx-history@localhost")
                .unwrap_or(false)
        })
        .unwrap_or(false)
}

fn flatten_tree(
    repo: &gix::Repository,
    tree_id: gix::ObjectId,
    prefix: &Path,
    out: &mut HashMap<PathBuf, gix::ObjectId>,
) -> Result<()> {
    let tree = repo
        .find_object(tree_id)
        .context("find tree")?
        .peel_to_tree()
        .context("peel tree")?;
    for entry in tree.iter() {
        let entry = entry.context("decode tree entry")?;
        let rel = prefix.join(std::str::from_utf8(entry.filename().as_ref()).unwrap_or_default());
        match entry.mode().kind() {
            EntryKind::Tree => flatten_tree(repo, entry.oid().to_owned(), &rel, out)?,
            EntryKind::Blob | EntryKind::BlobExecutable | EntryKind::Link => {
                out.insert(rel, entry.oid().to_owned());
            }
            EntryKind::Commit => {}
        }
    }
    Ok(())
}

pub fn rollback_to_commit_blocking(
    repo: &gix::Repository,
    target_commit: gix::ObjectId,
    workdir: &Path,
    parent: Option<gix::ObjectId>,
    label: &str,
) -> Result<(gix::ObjectId, gix::ObjectId)> {
    if !is_harnx_history_commit(repo, target_commit) {
        anyhow::bail!("target commit is not a harnx history snapshot");
    }

    let before_snap = capture_tree_blocking(repo, workdir, parent, "before rollback")?;

    let target_tree_id = repo
        .find_object(target_commit)
        .context("find target commit")?
        .peel_to_commit()
        .context("peel target commit")?
        .tree_id()?
        .detach();

    let mut current_paths = HashMap::new();
    let mut target_paths = HashMap::new();

    if let Ok(head_commit) = repo.head_commit() {
        flatten_tree(
            repo,
            head_commit.tree_id()?.detach(),
            Path::new(""),
            &mut current_paths,
        )?;
    }
    flatten_tree(repo, target_tree_id, Path::new(""), &mut target_paths)?;

    let all_paths: BTreeSet<PathBuf> = current_paths
        .keys()
        .cloned()
        .chain(target_paths.keys().cloned())
        .collect();

    for rel in all_paths {
        let abs = workdir.join(&rel);
        match target_paths.get(&rel) {
            Some(blob_id) => {
                let data = repo
                    .find_object(*blob_id)
                    .context("find target blob")?
                    .data
                    .to_vec();
                if let Some(parent_dir) = abs.parent() {
                    fs::create_dir_all(parent_dir).with_context(|| {
                        format!("create parent directories for {}", abs.display())
                    })?;
                }
                fs::write(&abs, data)
                    .with_context(|| format!("write restored file {}", abs.display()))?;
            }
            None => {
                if abs.exists() {
                    fs::remove_file(&abs)
                        .with_context(|| format!("remove extra file {}", abs.display()))?;
                }
            }
        }
    }

    let after_parent = Some(before_snap);
    let after_snap = capture_tree_blocking(repo, workdir, after_parent, label)?;

    let status = std::process::Command::new("git")
        .arg("reset")
        .arg("--hard")
        .arg(after_snap.to_hex().to_string())
        .current_dir(workdir)
        .status()
        .context("run git reset --hard")?;
    if !status.success() {
        anyhow::bail!("git reset --hard failed");
    }

    Ok((before_snap, after_snap))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use tempfile::tempdir;

    fn run_git(dir: &std::path::Path, args: &[&str]) {
        let status = Command::new("git")
            .args(args)
            .current_dir(dir)
            .status()
            .expect("git command runs");
        assert!(status.success(), "git {:?} failed", args);
    }

    fn output_git(dir: &std::path::Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .expect("git output command runs");
        assert!(output.status.success(), "git {:?} failed", args);
        String::from_utf8(output.stdout).expect("utf8 git output")
    }

    #[test]
    fn test_rollback_rejects_non_history_commit() {
        let temp = tempdir().expect("tempdir");
        run_git(temp.path(), &["init"]);
        run_git(temp.path(), &["config", "user.name", "Test User"]);
        run_git(temp.path(), &["config", "user.email", "test@example.com"]);

        let file = temp.path().join("file.txt");
        fs::write(&file, "hello\n").expect("write file");
        run_git(temp.path(), &["add", "."]);
        run_git(temp.path(), &["commit", "-m", "regular commit"]);

        let commit = output_git(temp.path(), &["rev-parse", "HEAD"])
            .trim()
            .to_owned();
        let repo = gix::open(temp.path()).expect("open repo");

        let err = rollback_to_commit_blocking(
            &repo,
            gix::ObjectId::from_hex(commit.as_bytes()).expect("oid"),
            temp.path(),
            None,
            "rollback",
        )
        .expect_err("non-history commit should be rejected");

        assert!(
            err.to_string().contains("not a harnx history snapshot"),
            "unexpected error: {err:?}"
        );
    }

    #[test]
    fn test_rollback_restores_file() {
        let temp = tempdir().expect("tempdir");
        run_git(temp.path(), &["init"]);
        run_git(temp.path(), &["config", "user.name", "Test User"]);
        run_git(temp.path(), &["config", "user.email", "test@example.com"]);

        let file = temp.path().join("file.txt");
        fs::write(&file, "before\n").expect("write before");
        run_git(temp.path(), &["add", "."]);
        run_git(temp.path(), &["commit", "-m", "before"]);
        let before_regular = output_git(temp.path(), &["rev-parse", "HEAD"])
            .trim()
            .to_owned();

        let repo = gix::open(temp.path()).expect("open repo");
        let before_regular_oid =
            gix::ObjectId::from_hex(before_regular.as_bytes()).expect("before regular oid");
        let before_snap_id = capture_tree_blocking(
            &repo,
            temp.path(),
            Some(before_regular_oid),
            "before snapshot",
        )
        .expect("before snapshot");

        fs::write(&file, "after\n").expect("write after");
        run_git(temp.path(), &["add", "."]);
        run_git(temp.path(), &["commit", "-m", "after"]);
        let head_oid = gix::ObjectId::from_hex(
            output_git(temp.path(), &["rev-parse", "HEAD"])
                .trim()
                .as_bytes(),
        )
        .expect("head oid");

        let result = rollback_to_commit_blocking(
            &repo,
            before_snap_id,
            temp.path(),
            Some(head_oid),
            "rollback",
        );

        let (before_rollback_id, after_snap_id) = result.expect("rollback works");

        let contents = fs::read_to_string(&file).expect("read file");
        assert_eq!(
            contents, "before\n",
            "file should be restored to before state"
        );

        let head = output_git(temp.path(), &["rev-parse", "HEAD"])
            .trim()
            .to_owned();
        assert_eq!(
            head,
            after_snap_id.to_hex().to_string(),
            "HEAD should be after-snapshot"
        );

        let parent = output_git(
            temp.path(),
            &["rev-parse", &format!("{}^", after_snap_id.to_hex())],
        )
        .trim()
        .to_owned();
        assert_eq!(
            parent,
            before_rollback_id.to_hex().to_string(),
            "before-snap should be parent of after-snap"
        );

        let rollback_parent = output_git(
            temp.path(),
            &["rev-parse", &format!("{}^", before_rollback_id.to_hex())],
        )
        .trim()
        .to_owned();
        assert_eq!(
            rollback_parent,
            head_oid.to_hex().to_string(),
            "before rollback snapshot should chain from original head"
        );
    }
}
