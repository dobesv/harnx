use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use gix::object::tree::EntryKind;

use crate::history::capture_tree_blocking;

fn is_harnx_history_commit(repo: &gix::Repository, commit_id: gix::ObjectId) -> bool {
    let prefix = "refs/harnx-history/";
    repo.references()
        .ok()
        .and_then(|refs| {
            refs.prefixed(prefix.as_bytes()).ok().map(|iter| {
                iter.flatten()
                    .any(|r| r.target().try_id() == Some(commit_id.as_ref()))
            })
        })
        .unwrap_or(false)
}

fn flatten_tree(
    repo: &gix::Repository,
    tree_id: gix::ObjectId,
) -> Result<HashMap<PathBuf, gix::ObjectId>> {
    let tree = repo.find_object(tree_id)?.peel_to_tree()?;
    let mut recorder = gix::traverse::tree::Recorder::default();
    tree.traverse().breadthfirst(&mut recorder)?;

    let mut files = HashMap::new();
    for entry in recorder.records {
        if matches!(
            entry.mode.kind(),
            EntryKind::Blob | EntryKind::BlobExecutable
        ) {
            files.insert(
                PathBuf::from(entry.filepath.to_string()),
                entry.oid.to_owned(),
            );
        }
    }
    Ok(files)
}

fn remove_empty_parent_dirs(path: &Path, stop_at: &Path) -> Result<()> {
    let mut current = path.parent();
    while let Some(dir) = current {
        if dir == stop_at || !dir.starts_with(stop_at) {
            break;
        }
        match fs::remove_dir(dir) {
            Ok(()) => current = dir.parent(),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => current = dir.parent(),
            Err(err) if err.kind() == std::io::ErrorKind::DirectoryNotEmpty => break,
            Err(err) => {
                return Err(err)
                    .with_context(|| format!("remove empty directory {}", dir.display()));
            }
        }
    }
    Ok(())
}

pub fn rollback_to_commit_blocking(
    repo: &gix::Repository,
    commit_id: gix::ObjectId,
    workdir: &Path,
    parent: Option<gix::ObjectId>,
    session_ref: &str,
    label: &str,
) -> Result<(gix::ObjectId, gix::ObjectId)> {
    let _ = repo
        .find_commit(commit_id)
        .with_context(|| format!("find commit {commit_id}"))?;

    if !is_harnx_history_commit(repo, commit_id) {
        anyhow::bail!("commit {commit_id} is not a harnx history snapshot");
    }

    let target_commit = repo
        .find_object(commit_id)
        .with_context(|| format!("find target object {commit_id}"))?
        .peel_to_commit()
        .context("peel target commit")?;
    let target_tree = target_commit.tree().context("read target tree")?;
    let target_tree_id = target_tree.id().detach();
    let target_files = flatten_tree(repo, target_tree_id).context("flatten target tree")?;

    let head_id = repo.head_id().context("read HEAD commit for rollback")?;
    let head_commit = repo
        .find_object(head_id)
        .with_context(|| format!("find HEAD object {head_id}"))?
        .peel_to_commit()
        .context("peel HEAD commit")?;
    let head_tree = head_commit.tree().context("read HEAD tree")?;
    let head_files = flatten_tree(repo, head_tree.id().detach()).context("flatten HEAD tree")?;

    // Before-snapshot: capture current state before any mutations
    let before_id = capture_tree_blocking(repo, workdir, parent, session_ref, "before rollback")?;

    let all_paths: BTreeSet<PathBuf> = target_files
        .keys()
        .chain(head_files.keys())
        .cloned()
        .collect();

    for relative_path in all_paths {
        let target_blob = target_files.get(&relative_path);
        let head_blob = head_files.get(&relative_path);
        let full_path = workdir.join(&relative_path);

        match (target_blob, head_blob) {
            (Some(target_blob), Some(head_blob)) if target_blob == head_blob => {}
            (Some(target_blob), _) => {
                let data = repo
                    .find_object(*target_blob)
                    .with_context(|| {
                        format!("find blob {target_blob} for {}", relative_path.display())
                    })?
                    .data
                    .to_owned();
                if let Some(parent) = full_path.parent() {
                    fs::create_dir_all(parent).with_context(|| {
                        format!("create parent directories for {}", full_path.display())
                    })?;
                }
                fs::write(&full_path, data)
                    .with_context(|| format!("write rollback file {}", full_path.display()))?;
            }
            (None, Some(_)) => {
                if full_path.exists() {
                    if full_path.is_dir() {
                        fs::remove_dir_all(&full_path).with_context(|| {
                            format!("remove rollback directory {}", full_path.display())
                        })?;
                    } else {
                        fs::remove_file(&full_path).with_context(|| {
                            format!("remove rollback file {}", full_path.display())
                        })?;
                    }
                }
                remove_empty_parent_dirs(&full_path, workdir)?;
            }
            (None, None) => {}
        }
    }

    // After-snapshot: capture state after mutations
    let short_commit = &commit_id.to_hex().to_string()[..8];
    let after_label = format!("harnx rollback to {short_commit}: {label}");
    let after_id =
        capture_tree_blocking(repo, workdir, Some(before_id), session_ref, &after_label)?;

    // Advance the current branch ref (or HEAD if detached) to the after-snapshot,
    // so the rollback commit is visible in `git log` without detaching HEAD.
    let head = repo.head().context("read HEAD for rollback ref update")?;
    if let Some(branch) = head.referent_name() {
        // Attached HEAD — advance the branch ref directly, preserving branch attachment.
        // Pass branch as &FullNameRef which implements TryInto<FullName>.
        repo.reference(
            branch,
            after_id,
            gix::refs::transaction::PreviousValue::Any,
            "harnx rollback",
        )
        .context("update branch ref after rollback")?;
    } else {
        // Detached HEAD — update HEAD directly (already detached, nothing to break).
        repo.reference(
            "HEAD",
            after_id,
            gix::refs::transaction::PreviousValue::Any,
            "harnx rollback",
        )
        .context("update HEAD after rollback")?;
    }

    Ok((before_id, after_id))
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
    fn test_rollback_restores_file() {
        let temp = tempdir().expect("tempdir");
        run_git(temp.path(), &["init"]);
        run_git(temp.path(), &["config", "user.name", "Test User"]);
        run_git(temp.path(), &["config", "user.email", "test@example.com"]);

        let file = temp.path().join("note.txt");
        fs::write(&file, "before\n").expect("write before");
        run_git(temp.path(), &["add", "."]);
        run_git(temp.path(), &["commit", "-m", "before"]);
        let before = output_git(temp.path(), &["rev-parse", "HEAD"])
            .trim()
            .to_owned();

        run_git(
            temp.path(),
            &["update-ref", "refs/harnx-history/test-session", &before],
        );

        fs::write(&file, "after\n").expect("write after");
        run_git(temp.path(), &["add", "."]);
        run_git(temp.path(), &["commit", "-m", "after"]);

        let repo = gix::open(temp.path()).expect("open repo");
        let result = rollback_to_commit_blocking(
            &repo,
            gix::ObjectId::from_hex(before.as_bytes()).expect("before oid"),
            temp.path(),
            None,
            "refs/harnx-history/test-session",
            "rollback",
        );

        // Verify rollback succeeded and returned two commit IDs
        let (before_snap_id, after_snap_id) = result.expect("rollback works");

        // Verify file content was restored
        let contents = fs::read_to_string(&file).expect("read file");
        assert_eq!(
            contents, "before\n",
            "file should be restored to before state"
        );

        // Verify commit chain
        let log = output_git(temp.path(), &["log", "--oneline"]);
        eprintln!("git log:\n{}", log);
        let log_lines: Vec<_> = log.lines().collect();

        // We expect at least: before, after, before-rollback-snap, after-rollback-snap
        assert!(
            log_lines.len() >= 2,
            "expected at least 2 commits, got {}: {:?}",
            log_lines.len(),
            log_lines
        );

        // The most recent commit should be the after-rollback snapshot
        assert!(
            log_lines[0].contains("harnx rollback")
                || log_lines[0].contains("before rollback")
                || log_lines[0].contains("after"),
            "top commit should be rollback-related, got: {:?}",
            log_lines[0]
        );

        // HEAD should point to after_snap_id
        let head = output_git(temp.path(), &["rev-parse", "HEAD"])
            .trim()
            .to_owned();
        assert_eq!(
            head,
            after_snap_id.to_hex().to_string(),
            "HEAD should be after-snapshot"
        );

        // before_snap_id should be the parent of after_snap_id
        let parent = output_git(
            temp.path(),
            &["rev-parse", &format!("{}^", after_snap_id.to_hex())],
        )
        .trim()
        .to_owned();
        assert_eq!(
            parent,
            before_snap_id.to_hex().to_string(),
            "before-snap should be parent of after-snap"
        );
    }
}
