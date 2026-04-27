use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use uuid::Uuid;

use crate::diff::diff_commits_blocking;
use crate::discover::{find_repos_under_roots, history_dir};
use crate::rollback::rollback_to_commit_blocking;

/// Maximum number of files per snapshot. Override with HARNX_HISTORY_MAX_FILES.
const MAX_SNAPSHOT_FILES: usize = 10_000;
/// Maximum bytes for a single file in a snapshot. Override with HARNX_HISTORY_MAX_FILE_BYTES.
const MAX_FILE_BYTES: u64 = 10 * 1024 * 1024;
/// Maximum total bytes across all files in a snapshot. Override with HARNX_HISTORY_MAX_TOTAL_BYTES.
const MAX_TOTAL_BYTES: u64 = 100 * 1024 * 1024;

pub struct HistoryManager {
    inner: Arc<HistoryManagerInner>,
}

struct HistoryManagerInner {
    repos: tokio::sync::Mutex<HashMap<PathBuf, RepoSession>>,
    session_id: String,
    fallback_history_dir: PathBuf,
}

struct RepoSession {
    repo: gix::ThreadSafeRepository,
    last_commit_id: Option<gix::ObjectId>,
    session_ref: String,
}

impl HistoryManager {
    pub fn new(roots: &[PathBuf]) -> Self {
        let session_id = Uuid::new_v4().to_string();
        let session_ref = format!("refs/harnx-history/{session_id}");
        let mut repos = HashMap::new();

        for repo_root in find_repos_under_roots(roots) {
            if let Ok(repo) = gix::open(&repo_root) {
                repos.insert(
                    repo_root,
                    RepoSession {
                        repo: repo.into_sync(),
                        last_commit_id: None,
                        session_ref: session_ref.clone(),
                    },
                );
            }
        }

        Self {
            inner: Arc::new(HistoryManagerInner {
                repos: tokio::sync::Mutex::new(repos),
                session_id,
                fallback_history_dir: history_dir(),
            }),
        }
    }

    pub async fn snapshot(&self, paths: &[PathBuf], label: &str) -> Result<gix::ObjectId> {
        let path = paths
            .first()
            .context("snapshot requires at least one path")?;
        let (repo_workdir, ts_repo, parent, session_ref) = {
            let repos = self.inner.repos.lock().await;
            let (workdir, session) = repos
                .iter()
                .find(|(workdir, _)| path.starts_with(workdir))
                .context("no tracked repo found for snapshot path")?;
            (
                workdir.clone(),
                session.repo.clone(),
                session.last_commit_id,
                session.session_ref.clone(),
            )
        };

        let label = label.to_owned();
        let repo_workdir_for_task = repo_workdir.clone();
        let commit_id = tokio::task::spawn_blocking(move || {
            let repo = ts_repo.to_thread_local();
            capture_tree_blocking(&repo, &repo_workdir_for_task, parent, &session_ref, &label)
        })
        .await
        .context("snapshot task join failed")??;

        let mut repos = self.inner.repos.lock().await;
        if let Some(session) = repos.get_mut(&repo_workdir) {
            session.last_commit_id = Some(commit_id);
        }

        Ok(commit_id)
    }

    pub async fn snapshot_repos_for_dir(
        &self,
        working_dir: &Path,
        label: &str,
    ) -> Result<Vec<(PathBuf, gix::ObjectId)>> {
        let candidates = {
            let repos = self.inner.repos.lock().await;
            repos
                .iter()
                .filter(|(repo_root, _)| {
                    working_dir.starts_with(repo_root) || repo_root.starts_with(working_dir)
                })
                .map(|(repo_root, session)| {
                    (
                        repo_root.clone(),
                        session.repo.clone(),
                        session.last_commit_id,
                        session.session_ref.clone(),
                    )
                })
                .collect::<Vec<_>>()
        };

        let mut results = Vec::new();
        for (repo_root, ts_repo, parent, session_ref) in candidates {
            let label = label.to_owned();
            let repo_root_for_task = repo_root.clone();
            let commit_id = tokio::task::spawn_blocking(move || {
                let repo = ts_repo.to_thread_local();
                capture_tree_blocking(&repo, &repo_root_for_task, parent, &session_ref, &label)
            })
            .await
            .context("snapshot_repos_for_dir task join failed")??;
            let mut repos = self.inner.repos.lock().await;
            if let Some(session) = repos.get_mut(&repo_root) {
                session.last_commit_id = Some(commit_id);
            }
            results.push((repo_root, commit_id));
        }

        Ok(results)
    }

    pub async fn diff_commits(
        &self,
        repo_workdir: &Path,
        before_id: gix::ObjectId,
        after_id: gix::ObjectId,
    ) -> Result<String> {
        let ts_repo = {
            let repos = self.inner.repos.lock().await;
            repos
                .get(repo_workdir)
                .map(|session| session.repo.clone())
                .context("repo not tracked for diff")?
        };

        tokio::task::spawn_blocking(move || {
            let repo = ts_repo.to_thread_local();
            diff_commits_blocking(&repo, before_id, after_id)
        })
        .await
        .context("diff task join failed")?
    }

    pub async fn rollback(
        &self,
        repo_workdir: &Path,
        commit_id: gix::ObjectId,
    ) -> Result<gix::ObjectId> {
        let (ts_repo, workdir, parent, session_ref) = {
            let repos = self.inner.repos.lock().await;
            let session = repos
                .get(repo_workdir)
                .context("repo not tracked for rollback")?;
            (
                session.repo.clone(),
                repo_workdir.to_path_buf(),
                session.last_commit_id,
                session.session_ref.clone(),
            )
        };

        let result = tokio::task::spawn_blocking(move || {
            let repo = ts_repo.to_thread_local();
            rollback_to_commit_blocking(
                &repo,
                commit_id,
                &workdir,
                parent,
                &session_ref,
                "rollback",
            )
        })
        .await
        .context("rollback task join failed")??;

        let after_id = result.1;
        {
            let mut repos = self.inner.repos.lock().await;
            if let Some(session) = repos.get_mut(repo_workdir) {
                session.last_commit_id = Some(after_id);
            }
        }
        Ok(after_id)
    }

    #[allow(dead_code)]
    pub fn session_id(&self) -> &str {
        &self.inner.session_id
    }

    #[allow(dead_code)]
    pub fn fallback_history_dir(&self) -> &Path {
        &self.inner.fallback_history_dir
    }
}

pub(crate) fn capture_tree_blocking(
    repo: &gix::Repository,
    workdir: &Path,
    parent: Option<gix::ObjectId>,
    session_ref: &str,
    label: &str,
) -> Result<gix::ObjectId> {
    let mut editor = repo
        .edit_tree(repo.empty_tree().id)
        .context("create tree editor")?;

    let files = collect_files(workdir)?;
    let mut total_bytes: u64 = 0;
    let mut files_processed: usize = 0;

    for file_path in files {
        // Guard 3: Per-file size check (skip oversized files with a warning)
        let file_size = std::fs::metadata(&file_path).map(|m| m.len()).unwrap_or(0);
        let max_file = std::env::var("HARNX_HISTORY_MAX_FILE_BYTES")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(MAX_FILE_BYTES);
        if file_size > max_file {
            log::warn!(
                "harnx-history: skipping {} ({} bytes) — exceeds per-file limit of {} bytes",
                file_path.display(),
                file_size,
                max_file,
            );
            continue;
        }

        // Guard 3: Cumulative total bytes abort
        let max_total = std::env::var("HARNX_HISTORY_MAX_TOTAL_BYTES")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(MAX_TOTAL_BYTES);
        total_bytes += file_size;
        if total_bytes > max_total {
            log::warn!(
                "harnx-history: snapshot of {} aborted after {} bytes — \
                 exceeds total limit of {} bytes (set HARNX_HISTORY_MAX_TOTAL_BYTES to override). \
                 {} files processed before cutoff.",
                workdir.display(),
                total_bytes,
                max_total,
                files_processed,
            );
            break;
        }

        let relative_path = file_path
            .strip_prefix(workdir)
            .context("strip workdir prefix")?;
        let relative_path = relative_path.to_string_lossy().replace('\\', "/");
        let data =
            fs::read(&file_path).with_context(|| format!("read file {}", file_path.display()))?;
        let blob_id = repo
            .write_blob(&data)
            .with_context(|| format!("write blob for {}", file_path.display()))?
            .detach();
        editor
            .upsert(
                relative_path.clone(),
                gix::object::tree::EntryKind::Blob,
                blob_id,
            )
            .with_context(|| format!("add tree entry {relative_path}"))?;

        files_processed += 1;
    }

    let tree_id = editor.write().context("write tree")?.detach();
    let now = gix::date::Time::now_local_or_utc().to_string();
    let signature = gix::actor::SignatureRef {
        name: "harnx-history".into(),
        email: "harnx-history@localhost".into(),
        time: now.as_str(),
    };

    let mut parents = Vec::new();
    if let Some(parent) = parent {
        parents.push(parent);
    } else if let Ok(reference) = repo.find_reference(session_ref) {
        if let Some(id) = reference.target().try_id() {
            parents.push(id.to_owned());
        }
    }

    let commit_id = repo
        .commit_as(signature, signature, session_ref, label, tree_id, parents)
        .context("write snapshot commit")?
        .detach();
    Ok(commit_id)
}

fn collect_files(root: &Path) -> Result<Vec<PathBuf>> {
    // Use git ls-files to get all tracked + untracked non-ignored files
    // This correctly respects .gitignore
    let output = std::process::Command::new("git")
        .args([
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ])
        .current_dir(root)
        .output()
        .context("run git ls-files")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // If git ls-files fails (e.g. not a git repo), fall back to empty list
        // The snapshot will still create a valid (empty) tree
        log::warn!("git ls-files failed in {}: {stderr}", root.display());
        return Ok(Vec::new());
    }

    // Use NUL-delimited output (-z) to correctly handle filenames containing newlines
    let mut files: Vec<PathBuf> = output
        .stdout
        .split(|&b| b == 0)
        .filter(|s| !s.is_empty())
        .filter_map(|s| std::str::from_utf8(s).ok())
        .map(|line| root.join(line))
        .filter(|p| p.is_file())
        .collect();
    files.sort();

    // Guard 1: Exclude the harnx history directory to prevent the cache from snapshotting itself
    let history = history_dir();
    let files: Vec<PathBuf> = files
        .into_iter()
        .filter(|p| !p.starts_with(&history))
        .collect();

    // Guard 2: File count cap
    let max_files = std::env::var("HARNX_HISTORY_MAX_FILES")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(MAX_SNAPSHOT_FILES);
    if files.len() > max_files {
        log::warn!(
            "harnx-history: snapshot of {} skipped — {} files exceeds limit of {} \
             (set HARNX_HISTORY_MAX_FILES to override)",
            root.display(),
            files.len(),
            max_files,
        );
        return Ok(Vec::new());
    }

    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, OnceLock};

    fn env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    fn init_git_repo(dir: &Path) {
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(dir)
            .output()
            .expect("git init");
        std::process::Command::new("git")
            .args(["config", "user.email", "test@example.com"])
            .current_dir(dir)
            .output()
            .expect("git config user.email");
        std::process::Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(dir)
            .output()
            .expect("git config user.name");
    }

    fn add_file_to_git(dir: &Path, name: &str, content: &[u8]) -> PathBuf {
        let file_path = dir.join(name);
        if let Some(parent) = file_path.parent() {
            std::fs::create_dir_all(parent).expect("create parent dir");
        }
        std::fs::write(&file_path, content).expect("write file");
        std::process::Command::new("git")
            .args(["add", name])
            .current_dir(dir)
            .output()
            .expect("git add");
        file_path
    }

    fn commit_git(dir: &Path) {
        std::process::Command::new("git")
            .args(["commit", "-m", "test commit"])
            .current_dir(dir)
            .output()
            .expect("git commit");
    }

    #[test]
    fn test_collect_files_excludes_history_dir() {
        let guard = env_lock().lock().expect("env lock");

        // Keep temp_dir alive for the duration of the test
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let repo_dir = temp_dir.path().join("repo");
        std::fs::create_dir_all(&repo_dir).expect("create repo dir");

        init_git_repo(&repo_dir);

        // Create a regular file in the repo
        let _regular_file = add_file_to_git(&repo_dir, "regular.txt", b"regular content");

        // Create a file inside a history dir that's inside the repo
        let history_subdir = repo_dir.join("history-subdir");
        std::fs::create_dir_all(&history_subdir).expect("create history subdir");
        let _history_file =
            add_file_to_git(&repo_dir, "history-subdir/history.txt", b"history content");

        // Commit to make git ls-files work correctly
        commit_git(&repo_dir);

        // Set HARNX_LOCAL_HISTORY_DIR to point to the history subdir
        unsafe { std::env::set_var("HARNX_LOCAL_HISTORY_DIR", &history_subdir) };

        let result = collect_files(&repo_dir).expect("collect_files should succeed");

        unsafe { std::env::remove_var("HARNX_LOCAL_HISTORY_DIR") };

        // Regular file should be included, history file should be excluded
        let has_regular = result.iter().any(|p| p.ends_with("regular.txt"));
        let has_history = result.iter().any(|p| p.ends_with("history.txt"));

        drop(guard);
        drop(temp_dir); // Keep temp_dir alive until after the check

        assert!(has_regular, "regular file should be included in result");
        assert!(!has_history, "history file should be excluded from result");
    }

    #[test]
    fn test_collect_files_file_count_cap() {
        let guard = env_lock().lock().expect("env lock");

        let temp_dir = tempfile::tempdir().expect("temp dir");
        let repo_dir = temp_dir.path().join("repo");
        std::fs::create_dir_all(&repo_dir).expect("create repo dir");

        init_git_repo(&repo_dir);

        // Create 3 files
        add_file_to_git(&repo_dir, "file1.txt", b"content 1");
        add_file_to_git(&repo_dir, "file2.txt", b"content 2");
        add_file_to_git(&repo_dir, "file3.txt", b"content 3");
        commit_git(&repo_dir);

        // Set a low file count limit
        unsafe { std::env::set_var("HARNX_HISTORY_MAX_FILES", "2") };

        let result = collect_files(&repo_dir).expect("collect_files should succeed");

        unsafe { std::env::remove_var("HARNX_HISTORY_MAX_FILES") };
        drop(guard);

        // Result should be empty because cap was triggered
        assert!(
            result.is_empty(),
            "result should be empty when file count exceeds cap"
        );
    }

    #[test]
    fn test_capture_tree_skips_large_file() {
        let guard = env_lock().lock().expect("env lock");

        let temp_dir = tempfile::tempdir().expect("temp dir");
        let repo_dir = temp_dir.path().join("repo");
        std::fs::create_dir_all(&repo_dir).expect("create repo dir");

        init_git_repo(&repo_dir);

        // Create a small file
        add_file_to_git(&repo_dir, "small.txt", b"tiny");
        // Create a "large" file (larger than our test limit of 10 bytes)
        add_file_to_git(&repo_dir, "large.txt", b"this is larger than ten bytes");
        commit_git(&repo_dir);

        // Set a very low per-file size limit (10 bytes)
        unsafe { std::env::set_var("HARNX_HISTORY_MAX_FILE_BYTES", "10") };

        // Create a git repo for the history snapshots
        let history_repo_dir = temp_dir.path().join("history-repo");
        std::fs::create_dir_all(&history_repo_dir).expect("create history repo dir");
        init_git_repo(&history_repo_dir);
        let history_repo = gix::open(&history_repo_dir).expect("open history repo");

        let result = capture_tree_blocking(
            &history_repo,
            &repo_dir,
            None,
            "refs/heads/main",
            "test snapshot",
        );

        unsafe { std::env::remove_var("HARNX_HISTORY_MAX_FILE_BYTES") };
        drop(guard);

        // Should succeed (not error)
        assert!(result.is_ok(), "capture_tree_blocking should succeed");

        // Verify the commit was created
        let commit_id = result.expect("commit id");
        let commit = history_repo.find_object(commit_id).expect("find commit");
        assert_eq!(commit.kind, gix::object::Kind::Commit, "should be a commit");
    }
}
