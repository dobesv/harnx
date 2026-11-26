use std::collections::HashMap;
use std::fs;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result};
use gix::object::tree::EntryKind;

use crate::history::capture_tree_blocking;

/// A rollback destination rejected by the write-boundary preflight.
///
/// Returned before any working-tree contents or git refs/index state change,
/// so tool handlers can surface it as an ordinary recoverable tool error
/// (e.g. `Ok(CallToolResult::error(...))`) instead of an internal failure.
/// Handlers classify with `err.downcast_ref::<RollbackPreflightError>()`,
/// so the typed value must reach them as itself inside the anyhow error.
/// anyhow's `downcast_ref` still matches through `.context()` layers
/// (`context_chain_downcast` recurses down the chain), but flattening to a
/// message (e.g. `bail!("...: {e}")`) drops the identity the handlers
/// classify by.
/// Expected rejections: absolute or parent-traversal paths, paths into the
/// repository's git metadata, symlink destinations or symlink ancestors, and
/// file/directory type conflicts.
#[derive(Debug)]
pub struct RollbackPreflightError {
    /// Snapshot-relative destination path that was rejected.
    pub rel_path: PathBuf,
    /// Why the destination was rejected; stable for test assertions.
    pub reason: &'static str,
}

// qual:allow(dry, boilerplate) reason: "Typed preflight rejections need Display to ride the anyhow boundary; a derive dependency or macro for one two-argument write is more boilerplate than the impl."
impl std::fmt::Display for RollbackPreflightError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "refusing to restore {}: {}",
            self.rel_path.display(),
            self.reason
        )
    }
}

impl std::error::Error for RollbackPreflightError {}

fn preflight_rejection(rel: &Path, reason: &'static str) -> anyhow::Error {
    anyhow::Error::new(RollbackPreflightError {
        rel_path: rel.to_path_buf(),
        reason,
    })
}

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

/// Preflight every destination a rollback would write to, before anything is
/// captured or written, so a rejected snapshot leaves contents and git state
/// completely unchanged.
fn preflight_destinations(
    workdir: &Path,
    git_dir: &Path,
    targets: &HashMap<PathBuf, gix::ObjectId>,
) -> Result<()> {
    for rel in targets.keys() {
        preflight_destination(workdir, git_dir, rel)?;
    }
    Ok(())
}

/// Check one snapshot-relative destination against the write boundary.
///
/// All checks use non-following metadata (`symlink_metadata`): a symlink
/// anywhere along the path below the workdir is rejected rather than followed
/// into an unrelated location. This is a bounded guard against unsafe
/// destinations, not a guarantee against adversarial concurrent filesystem
/// swaps between preflight and write.
fn preflight_destination(workdir: &Path, git_dir: &Path, rel: &Path) -> Result<()> {
    // Producers only record plain relative paths, but a crafted or hand-written
    // snapshot tree can name anything, so every component is checked instead of
    // trusting the producer.
    reject_unsafe_components(rel)?;

    let dest = workdir.join(rel);
    reject_git_metadata(&dest, git_dir, rel)?;
    reject_fs_conflicts(workdir, rel)
}

fn reject_unsafe_components(rel: &Path) -> Result<()> {
    // A crafted tree entry with an empty name flattens to an empty path;
    // the component loop below would skip it and the write would fail
    // halfway through instead of being rejected up front.
    if rel.as_os_str().is_empty() {
        return Err(preflight_rejection(rel, "empty paths cannot be restored"));
    }
    if rel.is_absolute() {
        return Err(preflight_rejection(
            rel,
            "absolute paths cannot be restored",
        ));
    }
    for component in rel.components() {
        match component {
            Component::Normal(_) => {}
            Component::ParentDir => {
                return Err(preflight_rejection(rel, "parent traversal is not allowed"));
            }
            Component::CurDir => {
                return Err(preflight_rejection(
                    rel,
                    "'.' path components cannot be restored",
                ));
            }
            _ => {
                return Err(preflight_rejection(
                    rel,
                    "absolute paths cannot be restored",
                ))
            }
        }
    }
    Ok(())
}

/// The repository's own metadata is off limits. The component check covers
/// the `.git` entry in the workdir; the git-dir containment check also covers
/// linked worktrees, where the real git dir sits elsewhere.
fn reject_git_metadata(dest: &Path, git_dir: &Path, rel: &Path) -> Result<()> {
    if dest == git_dir || dest.starts_with(git_dir) {
        return Err(preflight_rejection(
            rel,
            "path points into repository git metadata",
        ));
    }
    if rel
        .components()
        .any(|c| matches!(c, Component::Normal(name) if name == ".git"))
    {
        return Err(preflight_rejection(
            rel,
            "path points into repository git metadata",
        ));
    }
    Ok(())
}

fn reject_fs_conflicts(workdir: &Path, rel: &Path) -> Result<()> {
    let components: Vec<Component<'_>> = rel.components().collect();
    let mut cur = workdir.to_path_buf();
    for (idx, component) in components.iter().enumerate() {
        cur = cur.join(component.as_os_str());
        let metadata = match fs::symlink_metadata(&cur) {
            Ok(metadata) => metadata,
            // Nothing at this level: nothing deeper can exist either, so the
            // rest of the path will be created at write time.
            Err(_) => break,
        };
        reject_entry_conflict(rel, &metadata, idx + 1 == components.len())?;
    }
    Ok(())
}

/// Check one existing path along the destination route. `last` distinguishes
/// the destination itself from an ancestor that must stay a directory.
fn reject_entry_conflict(rel: &Path, metadata: &fs::Metadata, last: bool) -> Result<()> {
    if metadata.file_type().is_symlink() {
        return Err(preflight_rejection(
            rel,
            if last {
                "destination is a symlink"
            } else {
                "parent directory is a symlink"
            },
        ));
    }
    if last {
        if metadata.is_dir() {
            return Err(preflight_rejection(
                rel,
                "destination is a directory, refusing to replace it with a file",
            ));
        }
        if !metadata.is_file() {
            return Err(preflight_rejection(
                rel,
                "destination is not a regular file",
            ));
        }
    } else if !metadata.is_dir() {
        return Err(preflight_rejection(
            rel,
            "parent path exists where a directory is required",
        ));
    }
    Ok(())
}

/// Restore the files recorded in `target_commit`'s snapshot tree into
/// `workdir` as uncommitted working-tree changes, returning the `(before,
/// after)` snapshot ids taken around the restore and chained from `parent`.
///
/// Only paths present in the snapshot tree are restored: a path missing from
/// the tree is left exactly as-is, because every capture builds its tree
/// from empty and can be truncated by size limits, so absence carries no
/// deletion evidence. HEAD, refs and the index are never moved. All
/// destinations are preflighted before the `before` snapshot is captured, so
/// a rejected target changes no contents, refs or index at all; preflight
/// rejections surface as the typed [`RollbackPreflightError`] inside the
/// returned error.
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

    let target_tree_id = repo
        .find_object(target_commit)
        .context("find target commit")?
        .peel_to_commit()
        .context("peel target commit")?
        .tree_id()?
        .detach();

    // Only paths recorded in the snapshot tree are restored. A path missing
    // from the tree never means "delete": every capture builds its tree from
    // empty and can be truncated by size limits, so absence carries no
    // information about the working tree. Unrelated tracked, untracked and
    // staged files are left untouched.
    let mut target_paths = HashMap::new();
    flatten_tree(repo, target_tree_id, Path::new(""), &mut target_paths)?;

    // Validate the whole destination set before touching anything, so an
    // unsafe destination changes no contents, refs or index at all.
    preflight_destinations(workdir, repo.git_dir(), &target_paths)?;

    let before_snap = capture_tree_blocking(repo, workdir, parent, "before rollback")?;

    for (rel, blob_id) in &target_paths {
        let abs = workdir.join(rel);
        let data = repo
            .find_object(*blob_id)
            .context("find target blob")?
            .data
            .to_vec();
        if let Some(parent_dir) = abs.parent() {
            fs::create_dir_all(parent_dir)
                .with_context(|| format!("create parent directories for {}", abs.display()))?;
        }
        fs::write(&abs, data).with_context(|| format!("write restored file {}", abs.display()))?;
    }

    // The after snapshot records the resulting working tree as a history
    // object only: HEAD, refs and the index are never moved, and the restored
    // contents stay ordinary working-tree changes.
    let after_parent = Some(before_snap);
    let after_snap = capture_tree_blocking(repo, workdir, after_parent, label)?;

    Ok((before_snap, after_snap))
}

#[cfg(test)]
mod tests {
    use super::*;
    use gix::bstr::ByteSlice;
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

    fn init_git_repo(dir: &std::path::Path) {
        run_git(dir, &["init"]);
        run_git(dir, &["config", "user.name", "Test User"]);
        run_git(dir, &["config", "user.email", "test@example.com"]);
        // Disable autocrlf so line endings in snapshotted files are preserved
        // exactly on Windows; without this git rewrites LF→CRLF on checkout
        // and the restored file content no longer matches what we snapshotted.
        run_git(dir, &["config", "core.autocrlf", "false"]);
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

    fn head_oid(dir: &std::path::Path) -> String {
        output_git(dir, &["rev-parse", "HEAD"]).trim().to_owned()
    }

    /// Index state as comparable text: staged entries, including the staged
    /// blob for a modified file, so an index reset cannot pass unnoticed.
    fn index_state(dir: &std::path::Path) -> String {
        output_git(dir, &["ls-files", "--stage"]).trim().to_owned()
    }

    fn head_is_unborn(dir: &std::path::Path) -> bool {
        Command::new("git")
            .args(["rev-parse", "--verify", "HEAD"])
            .current_dir(dir)
            .output()
            .map(|out| !out.status.success())
            .expect("git rev-parse runs")
    }

    fn head_is_detached(dir: &std::path::Path) -> bool {
        let out = Command::new("git")
            .args(["symbolic-ref", "-q", "HEAD"])
            .current_dir(dir)
            .output()
            .expect("git symbolic-ref runs");
        !out.status.success()
    }

    fn write_file(dir: &std::path::Path, rel: &str, contents: &[u8]) {
        let path = dir.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create parent dirs");
        }
        fs::write(&path, contents).expect("write file");
    }

    fn read_file(dir: &std::path::Path, rel: &str) -> Vec<u8> {
        fs::read(dir.join(rel)).expect("read file")
    }

    /// Commit the given files so the repo has real HEAD/index state that a
    /// destructive rollback would clobber.
    fn commit_files(dir: &std::path::Path, files: &[(&str, &[u8])]) -> String {
        for (rel, contents) in files {
            write_file(dir, rel, contents);
            run_git(dir, &["add", rel]);
        }
        run_git(dir, &["commit", "-m", "committed state"]);
        head_oid(dir)
    }

    /// Commit the given files, snapshot the index and open the repo handle:
    /// the shared setup of the rollback tests that need real HEAD/index state
    /// a destructive rollback would clobber.
    fn committed_repo(
        dir: &std::path::Path,
        files: &[(&str, &[u8])],
    ) -> (String, String, gix::Repository) {
        let head = commit_files(dir, files);
        let index = index_state(dir);
        let repo = gix::open(dir).expect("open repo");
        (head, index, repo)
    }

    /// Record a single-file snapshot of `rel`'s current state in an
    /// already-open repo, the shared target of the single-file rollback
    /// tests.
    fn file_snapshot(repo: &gix::Repository, dir: &std::path::Path, rel: &str) -> gix::ObjectId {
        crate::history::capture_file_blocking(repo, dir, &dir.join(rel), None, "target snapshot")
            .expect("target snapshot")
    }

    /// The hex id of `oid`'s parent commit, for asserting snapshot chains.
    fn parent_commit_oid(dir: &std::path::Path, oid: gix::ObjectId) -> String {
        output_git(dir, &["rev-parse", &format!("{oid}^")])
            .trim()
            .to_owned()
    }

    /// On preflight rejection nothing may have changed: the victim behind the
    /// symlink keeps its contents, the symlink itself survives, and HEAD
    /// stays where it was.
    #[cfg(unix)]
    fn assert_symlink_rejection_untouched(
        dir: &std::path::Path,
        link_rel: &str,
        victim_rel: &str,
        head_before: &str,
    ) {
        assert_eq!(
            read_file(dir, victim_rel),
            b"outside\n",
            "symlinked-to contents must be untouched"
        );
        assert!(
            fs::symlink_metadata(dir.join(link_rel))
                .expect("symlink still present")
                .file_type()
                .is_symlink(),
            "symlink must be left in place"
        );
        assert_eq!(head_oid(dir), head_before, "HEAD must not move");
    }

    fn expect_preflight_error(
        result: Result<(gix::ObjectId, gix::ObjectId)>,
        expected_reason: &'static str,
    ) -> RollbackPreflightError {
        let err = result.expect_err("rollback should be rejected by the preflight");
        let pre = err
            .downcast_ref::<RollbackPreflightError>()
            .expect("rejection should carry a RollbackPreflightError");
        assert_eq!(pre.reason, expected_reason, "unexpected rejection: {pre}");
        RollbackPreflightError {
            rel_path: pre.rel_path.clone(),
            reason: pre.reason,
        }
    }

    /// A harnx history snapshot wrapping a hand-built tree, for crafted
    /// fixtures whose paths the capture functions cannot produce (traversal,
    /// git metadata, absolute names).
    fn crafted_history_snapshot(
        repo: &gix::Repository,
        entries: Vec<gix::objs::tree::Entry>,
    ) -> gix::ObjectId {
        let mut entries = entries;
        entries.sort();
        let tree_id = repo
            .write_object(gix::objs::Tree { entries })
            .expect("write crafted tree")
            .detach();

        let time = gix::date::Time::now_utc();
        let mut time_buf = Vec::with_capacity(time.size());
        time.write_to(&mut time_buf).expect("serialize time");
        let time_str = std::str::from_utf8(&time_buf).expect("time not utf-8");
        let signature = gix::actor::SignatureRef {
            name: "harnx-history".as_bytes().as_bstr(),
            email: "harnx-history@localhost".as_bytes().as_bstr(),
            time: time_str,
        }
        .to_owned()
        .expect("build snapshot signature");
        let commit = gix::objs::Commit {
            tree: tree_id,
            parents: Vec::new().into_iter().collect(),
            author: signature.clone(),
            committer: signature,
            encoding: None,
            message: "crafted fixture".into(),
            extra_headers: vec![],
        };
        repo.write_object(commit)
            .expect("write crafted snapshot commit")
            .detach()
    }

    fn blob_entry(repo: &gix::Repository, filename: &str) -> gix::objs::tree::Entry {
        let blob_id: gix::ObjectId = repo.write_blob(b"evil\n").expect("write blob").into();
        gix::objs::tree::Entry {
            mode: gix::object::tree::EntryKind::Blob.into(),
            filename: filename.into(),
            oid: blob_id,
        }
    }

    #[test]
    fn test_rollback_rejects_non_history_commit() {
        let temp = tempdir().expect("tempdir");
        init_git_repo(temp.path());

        let file = temp.path().join("file.txt");
        fs::write(&file, "hello\n").expect("write file");
        run_git(temp.path(), &["add", "."]);
        run_git(temp.path(), &["commit", "-m", "regular commit"]);

        let head_before = head_oid(temp.path());
        let index_before = index_state(temp.path());
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
        assert_eq!(read_file(temp.path(), "file.txt"), b"hello\n");
        assert_eq!(head_oid(temp.path()), head_before);
        assert_eq!(index_state(temp.path()), index_before);
    }

    #[test]
    fn test_rollback_restores_file() {
        let temp = tempdir().expect("tempdir");
        init_git_repo(temp.path());

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
        let head_commit_oid = gix::ObjectId::from_hex(
            output_git(temp.path(), &["rev-parse", "HEAD"])
                .trim()
                .as_bytes(),
        )
        .expect("head oid");
        let index_before = index_state(temp.path());

        let result = rollback_to_commit_blocking(
            &repo,
            before_snap_id,
            temp.path(),
            Some(head_commit_oid),
            "rollback",
        );

        let (before_rollback_id, after_snap_id) = result.expect("rollback works");

        let contents = fs::read_to_string(&file).expect("read file");
        assert_eq!(
            contents, "before\n",
            "file should be restored to before state"
        );

        // HEAD, the branch ref and the index are untouched: the restore is a
        // working-tree change, not a commit or reset.
        assert_eq!(
            head_oid(temp.path()),
            head_commit_oid.to_hex().to_string(),
            "HEAD must stay on the user's commit"
        );
        assert_eq!(
            index_state(temp.path()),
            index_before,
            "index must stay unchanged"
        );

        assert_eq!(
            parent_commit_oid(temp.path(), after_snap_id),
            before_rollback_id.to_hex().to_string(),
            "before-snap should be parent of after-snap"
        );

        assert_eq!(
            parent_commit_oid(temp.path(), before_rollback_id),
            head_commit_oid.to_hex().to_string(),
            "before rollback snapshot should chain from original head"
        );
    }

    /// The issue #2929 reproduction: a single-file target in a repo with other
    /// committed files must restore only the target. The old implementation
    /// unioned the target with HEAD, deleted the difference and reset --hard,
    /// destroying `other.txt`, the untracked file and the staged change.
    #[test]
    fn test_rollback_restores_only_target_paths() {
        let temp = tempdir().expect("tempdir");
        init_git_repo(temp.path());

        let (head_before, _index, repo) = committed_repo(
            temp.path(),
            &[("tracked.txt", b"old\n"), ("other.txt", b"other\n")],
        );

        // An unrelated untracked file and an unrelated staged change must both
        // survive the rollback; the staged change also proves the index is
        // not reset.
        write_file(temp.path(), "untracked.txt", b"unrelated\n");
        write_file(temp.path(), "other.txt", b"other staged\n");
        run_git(temp.path(), &["add", "other.txt"]);
        let index_before = index_state(temp.path());

        let target = file_snapshot(&repo, temp.path(), "tracked.txt");

        fs::write(temp.path().join("tracked.txt"), "new\n").expect("write new");

        let (_before, _after) = rollback_to_commit_blocking(
            &repo,
            target,
            temp.path(),
            Some(gix::ObjectId::from_hex(head_before.as_bytes()).expect("head oid")),
            "rollback",
        )
        .expect("rollback works");

        assert_eq!(
            read_file(temp.path(), "tracked.txt"),
            b"old\n",
            "target path should be restored"
        );
        assert_eq!(
            read_file(temp.path(), "other.txt"),
            b"other staged\n",
            "unrelated tracked file must not be restored or deleted"
        );
        assert_eq!(
            read_file(temp.path(), "untracked.txt"),
            b"unrelated\n",
            "untracked file must not be inferred as deleted"
        );
        assert_eq!(head_oid(temp.path()), head_before, "HEAD must not move");
        assert_eq!(
            index_state(temp.path()),
            index_before,
            "staged change must stay staged"
        );
    }

    /// A targeted multi-file snapshot restores exactly its own paths,
    /// including a nested path whose directory was removed meanwhile.
    #[test]
    fn test_rollback_targeted_multi_file_snapshot_restores_only_targets() {
        let temp = tempdir().expect("tempdir");
        init_git_repo(temp.path());

        let (head_before, index_before, repo) = committed_repo(
            temp.path(),
            &[
                ("a.txt", b"first\n"),
                ("b.txt", b"second\n"),
                ("nested/deep.txt", b"deep\n"),
                ("untouched.txt", b"keep me\n"),
            ],
        );

        let target = crate::history::capture_files_blocking(
            &repo,
            temp.path(),
            &[
                temp.path().join("a.txt"),
                temp.path().join("b.txt"),
                temp.path().join("nested/deep.txt"),
            ],
            None,
            "targeted snapshot",
        )
        .expect("targeted snapshot");

        write_file(temp.path(), "a.txt", b"changed\n");
        write_file(temp.path(), "b.txt", b"changed\n");
        fs::remove_dir_all(temp.path().join("nested")).expect("remove nested dir");
        write_file(temp.path(), "created-later.txt", b"later\n");

        rollback_to_commit_blocking(
            &repo,
            target,
            temp.path(),
            Some(gix::ObjectId::from_hex(head_before.as_bytes()).expect("head oid")),
            "rollback",
        )
        .expect("rollback works");

        assert_eq!(read_file(temp.path(), "a.txt"), b"first\n");
        assert_eq!(read_file(temp.path(), "b.txt"), b"second\n");
        assert_eq!(
            read_file(temp.path(), "nested/deep.txt"),
            b"deep\n",
            "missing target path with missing parent must be recreated"
        );
        assert_eq!(
            read_file(temp.path(), "untouched.txt"),
            b"keep me\n",
            "path absent from the target must not be deleted"
        );
        assert_eq!(
            read_file(temp.path(), "created-later.txt"),
            b"later\n",
            "path created after the snapshot must not be inferred as deleted"
        );
        assert_eq!(head_oid(temp.path()), head_before, "HEAD must not move");
        assert_eq!(index_state(temp.path()), index_before);
    }

    /// Even a whole-tree target restores only the paths it recorded; paths
    /// absent from it are left alone (trees do not encode completeness and
    /// whole-tree capture can be truncated by size limits).
    #[test]
    fn test_rollback_whole_tree_target_preserves_absent_paths() {
        let temp = tempdir().expect("tempdir");
        init_git_repo(temp.path());

        let (head_before, _index, repo) = committed_repo(temp.path(), &[("file.txt", b"v1\n")]);

        let target = capture_tree_blocking(&repo, temp.path(), None, "whole tree").expect("tree");

        write_file(temp.path(), "file.txt", b"v2\n");
        write_file(temp.path(), "new.txt", b"new file\n");

        rollback_to_commit_blocking(
            &repo,
            target,
            temp.path(),
            Some(gix::ObjectId::from_hex(head_before.as_bytes()).expect("head oid")),
            "rollback",
        )
        .expect("rollback works");

        assert_eq!(read_file(temp.path(), "file.txt"), b"v1\n");
        assert_eq!(
            read_file(temp.path(), "new.txt"),
            b"new file\n",
            "absent path must be preserved, not deleted"
        );
        assert_eq!(head_oid(temp.path()), head_before);
    }

    /// An empty snapshot (e.g. captured before a file existed) restores zero
    /// paths: it is not a creation undo, so the newly created file stays and
    /// nothing else changes.
    #[test]
    fn test_rollback_empty_target_snapshot_restores_nothing() {
        let temp = tempdir().expect("tempdir");
        init_git_repo(temp.path());

        let (head_before, index_before, repo) =
            committed_repo(temp.path(), &[("file.txt", b"kept\n")]);

        // capture_file_blocking of a missing file records an empty tree.
        let target = file_snapshot(&repo, temp.path(), "created.txt");

        write_file(temp.path(), "created.txt", b"just created\n");

        let (before, after) = rollback_to_commit_blocking(
            &repo,
            target,
            temp.path(),
            Some(gix::ObjectId::from_hex(head_before.as_bytes()).expect("head oid")),
            "rollback",
        )
        .expect("rollback of empty snapshot works");

        assert_eq!(
            read_file(temp.path(), "created.txt"),
            b"just created\n",
            "empty target must not undo a file creation"
        );
        assert_eq!(read_file(temp.path(), "file.txt"), b"kept\n");
        assert_eq!(head_oid(temp.path()), head_before, "HEAD must not move");
        assert_eq!(index_state(temp.path()), index_before);

        // Even with an empty target the snapshot chain is still recorded.
        assert_eq!(
            parent_commit_oid(temp.path(), after),
            before.to_hex().to_string()
        );
    }

    /// Detached HEAD must not acquire or move a branch as a side effect of a
    /// rollback.
    #[test]
    fn test_rollback_on_detached_head_leaves_detached_head_unchanged() {
        let temp = tempdir().expect("tempdir");
        init_git_repo(temp.path());

        commit_files(temp.path(), &[("file.txt", b"one\n")]);
        let branch = output_git(temp.path(), &["rev-parse", "--symbolic-full-name", "HEAD"])
            .trim()
            .to_owned();
        run_git(temp.path(), &["commit", "--allow-empty", "-m", "second"]);
        let branch_tip = head_oid(temp.path());
        run_git(temp.path(), &["checkout", "--detach"]);

        let detached_oid = head_oid(temp.path());
        assert!(head_is_detached(temp.path()));

        let repo = gix::open(temp.path()).expect("open repo");
        let target = capture_tree_blocking(&repo, temp.path(), None, "snapshot").expect("tree");

        write_file(temp.path(), "file.txt", b"two\n");

        rollback_to_commit_blocking(
            &repo,
            target,
            temp.path(),
            Some(gix::ObjectId::from_hex(detached_oid.as_bytes()).expect("oid")),
            "rollback",
        )
        .expect("rollback works");

        assert_eq!(read_file(temp.path(), "file.txt"), b"one\n");
        assert_eq!(head_oid(temp.path()), detached_oid, "detached HEAD stays");
        assert!(
            head_is_detached(temp.path()),
            "rollback must not attach a branch"
        );
        assert_eq!(
            branch_tip,
            output_git(temp.path(), &["rev-parse", &branch])
                .trim()
                .to_owned(),
            "the original branch must not move"
        );
    }

    /// An unborn repo (no commits yet) must stay unborn: the rollback writes
    /// working-tree contents only.
    #[test]
    fn test_rollback_on_unborn_repo_stays_unborn() {
        let temp = tempdir().expect("tempdir");
        init_git_repo(temp.path());
        assert!(head_is_unborn(temp.path()));

        write_file(temp.path(), "file.txt", b"first\n");

        let repo = gix::open(temp.path()).expect("open repo");
        let target = capture_tree_blocking(&repo, temp.path(), None, "snapshot").expect("tree");

        write_file(temp.path(), "file.txt", b"second\n");

        rollback_to_commit_blocking(&repo, target, temp.path(), None, "rollback")
            .expect("rollback works");

        assert_eq!(read_file(temp.path(), "file.txt"), b"first\n");
        assert!(
            head_is_unborn(temp.path()),
            "unborn HEAD must not acquire a commit or branch"
        );
        let symbolic_head = Command::new("git")
            .args(["symbolic-ref", "HEAD"])
            .current_dir(temp.path())
            .output()
            .expect("git symbolic-ref runs");
        assert!(
            symbolic_head.status.success(),
            "HEAD must keep pointing at the unborn branch"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_rollback_preflight_rejects_symlink_destination() {
        let temp = tempdir().expect("tempdir");
        init_git_repo(temp.path());

        let (head_before, index_before, repo) =
            committed_repo(temp.path(), &[("target.txt", b"old\n")]);

        let target = file_snapshot(&repo, temp.path(), "target.txt");

        // Replace the destination with a symlink to a file outside the repo.
        let victim = temp.path().join("outside-victim.txt");
        fs::write(&victim, b"outside\n").expect("write victim");
        fs::remove_file(temp.path().join("target.txt")).expect("remove target");
        std::os::unix::fs::symlink(&victim, temp.path().join("target.txt"))
            .expect("symlink destination");

        let rejection = expect_preflight_error(
            rollback_to_commit_blocking(
                &repo,
                target,
                temp.path(),
                Some(gix::ObjectId::from_hex(head_before.as_bytes()).expect("oid")),
                "rollback",
            ),
            "destination is a symlink",
        );
        assert_eq!(rejection.rel_path, Path::new("target.txt"));

        // Neither the symlinked-to contents, the symlink itself, nor git
        // state were touched.
        assert_symlink_rejection_untouched(
            temp.path(),
            "target.txt",
            "outside-victim.txt",
            &head_before,
        );
        assert_eq!(index_state(temp.path()), index_before);
    }

    #[cfg(unix)]
    #[test]
    fn test_rollback_preflight_rejects_symlink_parent_directory() {
        let temp = tempdir().expect("tempdir");
        init_git_repo(temp.path());

        let (head_before, _index, repo) =
            committed_repo(temp.path(), &[("nested/file.txt", b"old\n")]);

        let target = file_snapshot(&repo, temp.path(), "nested/file.txt");

        // Replace the parent directory with a symlink to a directory outside
        // the repo.
        let victim_dir = temp.path().join("outside-dir");
        fs::create_dir_all(&victim_dir).expect("create victim dir");
        fs::write(victim_dir.join("victim.txt"), b"outside\n").expect("write victim");
        fs::remove_dir_all(temp.path().join("nested")).expect("remove nested");
        std::os::unix::fs::symlink(&victim_dir, temp.path().join("nested"))
            .expect("symlink parent");

        let rejection = expect_preflight_error(
            rollback_to_commit_blocking(
                &repo,
                target,
                temp.path(),
                Some(gix::ObjectId::from_hex(head_before.as_bytes()).expect("oid")),
                "rollback",
            ),
            "parent directory is a symlink",
        );
        assert_eq!(
            rejection.rel_path,
            Path::new("nested/file.txt"),
            "rejection should name the snapshot path, not the ancestor"
        );

        assert_symlink_rejection_untouched(
            temp.path(),
            "nested",
            "outside-dir/victim.txt",
            &head_before,
        );
    }

    #[test]
    fn test_rollback_preflight_rejects_directory_destination() {
        let temp = tempdir().expect("tempdir");
        init_git_repo(temp.path());

        let (head_before, index_before, repo) =
            committed_repo(temp.path(), &[("target.txt", b"old\n")]);

        let target = file_snapshot(&repo, temp.path(), "target.txt");

        // Replace the destination with a directory holding unrelated contents.
        fs::remove_file(temp.path().join("target.txt")).expect("remove target");
        write_file(temp.path(), "target.txt/inside.txt", b"unrelated\n");

        let rejection = expect_preflight_error(
            rollback_to_commit_blocking(
                &repo,
                target,
                temp.path(),
                Some(gix::ObjectId::from_hex(head_before.as_bytes()).expect("oid")),
                "rollback",
            ),
            "destination is a directory, refusing to replace it with a file",
        );
        assert_eq!(rejection.rel_path, Path::new("target.txt"));

        assert_eq!(
            read_file(temp.path(), "target.txt/inside.txt"),
            b"unrelated\n",
            "directory contents must be untouched"
        );
        assert_eq!(head_oid(temp.path()), head_before);
        assert_eq!(index_state(temp.path()), index_before);
    }

    #[test]
    fn test_rollback_preflight_rejects_file_in_place_of_parent_directory() {
        let temp = tempdir().expect("tempdir");
        init_git_repo(temp.path());

        let (head_before, _index, repo) =
            committed_repo(temp.path(), &[("nested/file.txt", b"old\n")]);

        let target = file_snapshot(&repo, temp.path(), "nested/file.txt");

        // Replace the parent directory with a regular file.
        fs::remove_dir_all(temp.path().join("nested")).expect("remove nested");
        fs::write(temp.path().join("nested"), b"now a file\n").expect("write parent file");

        let rejection = expect_preflight_error(
            rollback_to_commit_blocking(
                &repo,
                target,
                temp.path(),
                Some(gix::ObjectId::from_hex(head_before.as_bytes()).expect("oid")),
                "rollback",
            ),
            "parent path exists where a directory is required",
        );
        assert_eq!(rejection.rel_path, Path::new("nested/file.txt"));

        assert_eq!(
            read_file(temp.path(), "nested"),
            b"now a file\n",
            "conflicting parent file must be untouched"
        );
        assert_eq!(head_oid(temp.path()), head_before);
    }

    /// Producers cannot emit traversal, git-metadata or absolute paths, so
    /// these use crafted snapshot trees.
    #[test]
    fn test_rollback_preflight_rejects_unsafe_crafted_paths() {
        let temp = tempdir().expect("tempdir");
        init_git_repo(temp.path());

        let (head_before, index_before, repo) =
            committed_repo(temp.path(), &[("safe.txt", b"committed\n")]);

        // Nested traversal: a subtree named `sub` holding an entry named `..`.
        let mut subtree_entries = vec![blob_entry(&repo, "..")];
        subtree_entries.sort();
        let subtree_id = repo
            .write_object(gix::objs::Tree {
                entries: subtree_entries,
            })
            .expect("write subtree")
            .detach();

        let cases: Vec<(Vec<gix::objs::tree::Entry>, PathBuf, &'static str)> = vec![
            (
                vec![blob_entry(&repo, "..")],
                PathBuf::from(".."),
                "parent traversal is not allowed",
            ),
            (
                vec![gix::objs::tree::Entry {
                    mode: gix::object::tree::EntryKind::Tree.into(),
                    filename: "sub".into(),
                    oid: subtree_id,
                }],
                PathBuf::from("sub/.."),
                "parent traversal is not allowed",
            ),
            (
                vec![blob_entry(&repo, ".git")],
                PathBuf::from(".git"),
                "path points into repository git metadata",
            ),
            (
                vec![blob_entry(&repo, "")],
                PathBuf::new(),
                "empty paths cannot be restored",
            ),
        ];

        for (entries, expected_rel, expected_reason) in cases {
            let target = crafted_history_snapshot(&repo, entries);

            let rejection = expect_preflight_error(
                rollback_to_commit_blocking(
                    &repo,
                    target,
                    temp.path(),
                    Some(gix::ObjectId::from_hex(head_before.as_bytes()).expect("oid")),
                    "rollback",
                ),
                expected_reason,
            );
            assert_eq!(rejection.rel_path, expected_rel);

            assert_eq!(
                read_file(temp.path(), "safe.txt"),
                b"committed\n",
                "no target contents may change on preflight rejection"
            );
            assert_eq!(head_oid(temp.path()), head_before);
            assert_eq!(index_state(temp.path()), index_before);
        }

        // An absolute entry name (crafted raw tree) must be rejected too.
        let target = crafted_history_snapshot(&repo, vec![blob_entry(&repo, "/etc/passwd")]);
        let rejection = expect_preflight_error(
            rollback_to_commit_blocking(
                &repo,
                target,
                temp.path(),
                Some(gix::ObjectId::from_hex(head_before.as_bytes()).expect("oid")),
                "rollback",
            ),
            "absolute paths cannot be restored",
        );
        assert_eq!(rejection.rel_path, Path::new("/etc/passwd"));
    }

    /// The preflight must validate the entire destination set before the
    /// first write: a good destination earlier in the set must stay unchanged
    /// when a later destination is rejected.
    #[test]
    fn test_rollback_preflight_checks_all_destinations_before_any_write() {
        let temp = tempdir().expect("tempdir");
        init_git_repo(temp.path());

        let (head_before, _index, repo) = committed_repo(temp.path(), &[("good.txt", b"good\n")]);

        // Both files exist at capture time so the snapshot records both.
        write_file(temp.path(), "bad.txt", b"bad\n");
        let target = crate::history::capture_files_blocking(
            &repo,
            temp.path(),
            &[temp.path().join("good.txt"), temp.path().join("bad.txt")],
            None,
            "targeted snapshot",
        )
        .expect("targeted snapshot");

        // Make the second destination a directory so the set is unsafe.
        fs::remove_file(temp.path().join("bad.txt")).expect("remove bad.txt");
        write_file(temp.path(), "bad.txt/occupant.txt", b"occupied\n");
        write_file(temp.path(), "good.txt", b"modified before rollback\n");

        expect_preflight_error(
            rollback_to_commit_blocking(
                &repo,
                target,
                temp.path(),
                Some(gix::ObjectId::from_hex(head_before.as_bytes()).expect("oid")),
                "rollback",
            ),
            "destination is a directory, refusing to replace it with a file",
        );

        assert_eq!(
            read_file(temp.path(), "good.txt"),
            b"modified before rollback\n",
            "no partial write may happen before the preflight rejects"
        );
        assert_eq!(
            read_file(temp.path(), "bad.txt/occupant.txt"),
            b"occupied\n"
        );
        assert_eq!(head_oid(temp.path()), head_before);
    }
}
