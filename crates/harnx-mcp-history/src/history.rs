use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use gix::bstr::ByteSlice;

use crate::diff::diff_commits_blocking;
use crate::discover::find_repos_under_roots;
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
    last_gc: std::sync::Mutex<HashMap<PathBuf, std::time::Instant>>,
}

struct RepoSession {
    repo: gix::ThreadSafeRepository,
    last_commit_id: Option<gix::ObjectId>,
}

fn maybe_trigger_gc(
    workdir: &Path,
    last_gc: &std::sync::Mutex<HashMap<PathBuf, std::time::Instant>>,
) {
    const GC_INTERVAL: std::time::Duration = std::time::Duration::from_secs(3600);
    let now = std::time::Instant::now();
    let mut map = last_gc.lock().unwrap_or_else(|e| e.into_inner());
    let should_run = map
        .get(workdir)
        .map(|t| now.duration_since(*t) >= GC_INTERVAL)
        .unwrap_or(true);
    if should_run {
        map.insert(workdir.to_path_buf(), now);
        drop(map);
        let _ = std::process::Command::new("git")
            .args(["gc", "--auto", "--quiet"])
            .current_dir(workdir)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
    }
}

impl HistoryManager {
    pub fn new(roots: &[PathBuf]) -> Self {
        let mut repos = HashMap::new();

        for repo_root in find_repos_under_roots(roots) {
            if let Ok(repo) = gix::open(&repo_root) {
                repos.insert(
                    repo_root,
                    RepoSession {
                        repo: repo.into_sync(),
                        last_commit_id: None,
                    },
                );
            }
        }

        Self {
            inner: Arc::new(HistoryManagerInner {
                repos: tokio::sync::Mutex::new(repos),
                last_gc: std::sync::Mutex::new(HashMap::new()),
            }),
        }
    }

    pub async fn snapshot(&self, paths: &[PathBuf], label: &str) -> Result<gix::ObjectId> {
        let path = paths
            .first()
            .context("snapshot requires at least one path")?;
        let (repo_workdir, ts_repo, parent) = {
            let repos = self.inner.repos.lock().await;
            let (workdir, session) = repos
                .iter()
                .find(|(workdir, _)| path.starts_with(workdir))
                .context("no tracked repo found for snapshot path")?;
            (
                workdir.clone(),
                session.repo.clone(),
                session.last_commit_id,
            )
        };

        let label = label.to_owned();
        let repo_workdir_for_task = repo_workdir.clone();
        let commit_id = tokio::task::spawn_blocking(move || {
            let repo = ts_repo.to_thread_local();
            capture_tree_blocking(&repo, &repo_workdir_for_task, parent, &label)
        })
        .await
        .context("snapshot task join failed")??;

        {
            let mut repos = self.inner.repos.lock().await;
            if let Some(session) = repos.get_mut(&repo_workdir) {
                session.last_commit_id = Some(commit_id);
            }
        }
        maybe_trigger_gc(&repo_workdir, &self.inner.last_gc);

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
                    )
                })
                .collect::<Vec<_>>()
        };

        let mut results = Vec::new();
        for (repo_root, ts_repo, parent) in candidates {
            let label = label.to_owned();
            let repo_root_for_task = repo_root.clone();
            let commit_id = tokio::task::spawn_blocking(move || {
                let repo = ts_repo.to_thread_local();
                capture_tree_blocking(&repo, &repo_root_for_task, parent, &label)
            })
            .await
            .context("snapshot_repos_for_dir task join failed")??;
            {
                let mut repos = self.inner.repos.lock().await;
                if let Some(session) = repos.get_mut(&repo_root) {
                    session.last_commit_id = Some(commit_id);
                }
            }
            maybe_trigger_gc(&repo_root, &self.inner.last_gc);
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
        let (ts_repo, workdir, parent) = {
            let repos = self.inner.repos.lock().await;
            let session = repos
                .get(repo_workdir)
                .context("repo not tracked for rollback")?;
            (
                session.repo.clone(),
                repo_workdir.to_path_buf(),
                session.last_commit_id,
            )
        };

        let result = tokio::task::spawn_blocking(move || {
            let repo = ts_repo.to_thread_local();
            rollback_to_commit_blocking(&repo, commit_id, &workdir, parent, "rollback")
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
}

pub(crate) fn capture_tree_blocking(
    repo: &gix::Repository,
    workdir: &Path,
    parent: Option<gix::ObjectId>,
    label: &str,
) -> Result<gix::ObjectId> {
    let files = match collect_files(workdir)? {
        Some(f) => f,
        None => {
            return Err(anyhow::anyhow!(
                "snapshot skipped: file count exceeds limit"
            ))
        }
    };

    let mut editor = repo
        .edit_tree(repo.empty_tree().id)
        .context("create tree editor")?;

    let mut total_bytes: u64 = 0;
    let mut files_processed: usize = 0;

    for file_path in files {
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

        let max_total = std::env::var("HARNX_HISTORY_MAX_TOTAL_BYTES")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(MAX_TOTAL_BYTES);
        if total_bytes + file_size > max_total {
            log::warn!(
                "harnx-history: snapshot of {} truncated after {} files / {} bytes — exceeds total \
                 limit of {} bytes (set HARNX_HISTORY_MAX_TOTAL_BYTES to override)",
                workdir.display(),
                files_processed,
                total_bytes,
                max_total,
            );
            break;
        }

        let rel = match file_path.strip_prefix(workdir) {
            Ok(rel) => rel,
            Err(_) => continue,
        };

        let data = match fs::read(&file_path) {
            Ok(data) => data,
            Err(_) => continue,
        };

        total_bytes += file_size;
        files_processed += 1;

        let blob_id: gix::ObjectId = repo.write_blob(data).context("write blob")?.into();
        let rel = rel.to_string_lossy().replace('\\', "/");
        editor
            .upsert(rel.as_str(), gix::object::tree::EntryKind::Blob, blob_id)
            .with_context(|| format!("insert {} into tree", rel))?;
    }

    let tree_id = editor.write().context("write tree")?.detach();

    let time = gix::date::Time::now_utc();
    let mut time_buf = Vec::with_capacity(time.size());
    time.write_to(&mut time_buf)
        .context("serialize snapshot time")?;
    let time_str = std::str::from_utf8(&time_buf).context("snapshot time not utf-8")?;
    let signature = gix::actor::SignatureRef {
        name: "harnx-history".as_bytes().as_bstr(),
        email: "harnx-history@localhost".as_bytes().as_bstr(),
        time: time_str,
    }
    .to_owned()
    .context("build snapshot signature")?;

    let mut parents = Vec::new();
    if let Some(parent) = parent {
        parents.push(parent);
    }

    let commit = gix::objs::Commit {
        tree: tree_id,
        parents: parents.into_iter().collect(),
        author: signature.clone(),
        committer: signature,
        encoding: None,
        message: label.into(),
        extra_headers: vec![],
    };
    let commit_id = repo
        .write_object(&commit)
        .context("write snapshot commit")?
        .detach();
    Ok(commit_id)
}

fn collect_files(root: &Path) -> Result<Option<Vec<PathBuf>>> {
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
        log::warn!("git ls-files failed in {}: {stderr}", root.display());
        return Ok(None);
    }

    let mut files: Vec<PathBuf> = output
        .stdout
        .split(|&b| b == 0)
        .filter(|s| !s.is_empty())
        .filter_map(|s| std::str::from_utf8(s).ok())
        .map(|line| root.join(line))
        .filter(|p| p.is_file())
        .collect();
    files.sort();

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
        return Ok(None);
    }

    Ok(Some(files))
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
        let status = std::process::Command::new("git")
            .args(["init"])
            .current_dir(dir)
            .status()
            .expect("init repo");
        assert!(status.success(), "git init failed");

        let status = std::process::Command::new("git")
            .args(["config", "user.name", "Test User"])
            .current_dir(dir)
            .status()
            .expect("config user.name");
        assert!(status.success(), "git config user.name failed");

        let status = std::process::Command::new("git")
            .args(["config", "user.email", "test@example.com"])
            .current_dir(dir)
            .status()
            .expect("config user.email");
        assert!(status.success(), "git config user.email failed");
    }

    fn add_file_to_git(dir: &Path, rel: &str, contents: &[u8]) {
        let file_path = dir.join(rel);
        if let Some(parent) = file_path.parent() {
            std::fs::create_dir_all(parent).expect("create parent dirs");
        }
        std::fs::write(&file_path, contents).expect("write file");
        let status = std::process::Command::new("git")
            .args(["add", rel])
            .current_dir(dir)
            .status()
            .expect("git add");
        assert!(status.success(), "git add failed");
    }

    fn commit_git(dir: &Path) {
        let status = std::process::Command::new("git")
            .args(["commit", "-m", "test commit"])
            .current_dir(dir)
            .status()
            .expect("git commit");
        assert!(status.success(), "git commit failed");
    }

    #[test]
    fn test_collect_files_file_count_cap() {
        let guard = env_lock().lock().expect("env lock");

        let temp_dir = tempfile::tempdir().expect("temp dir");
        let repo_dir = temp_dir.path().join("repo");
        std::fs::create_dir_all(&repo_dir).expect("create repo dir");

        init_git_repo(&repo_dir);

        add_file_to_git(&repo_dir, "file1.txt", b"content 1");
        add_file_to_git(&repo_dir, "file2.txt", b"content 2");
        add_file_to_git(&repo_dir, "file3.txt", b"content 3");
        commit_git(&repo_dir);

        unsafe { std::env::set_var("HARNX_HISTORY_MAX_FILES", "2") };

        let result = collect_files(&repo_dir).expect("collect_files should succeed");

        unsafe { std::env::remove_var("HARNX_HISTORY_MAX_FILES") };
        drop(guard);

        assert!(
            result.is_none(),
            "result should be None when file count exceeds cap"
        );
    }

    #[test]
    fn test_capture_tree_skips_large_file() {
        let guard = env_lock().lock().expect("env lock");

        let temp_dir = tempfile::tempdir().expect("temp dir");
        let repo_dir = temp_dir.path().join("repo");
        std::fs::create_dir_all(&repo_dir).expect("create repo dir");

        init_git_repo(&repo_dir);

        add_file_to_git(&repo_dir, "small.txt", b"tiny");
        add_file_to_git(&repo_dir, "large.txt", b"this is larger than ten bytes");
        commit_git(&repo_dir);

        unsafe { std::env::set_var("HARNX_HISTORY_MAX_FILE_BYTES", "10") };

        let history_repo_dir = temp_dir.path().join("history-repo");
        std::fs::create_dir_all(&history_repo_dir).expect("create history repo dir");
        init_git_repo(&history_repo_dir);
        let history_repo = gix::open(&history_repo_dir).expect("open history repo");

        let result = capture_tree_blocking(&history_repo, &repo_dir, None, "test snapshot");

        unsafe { std::env::remove_var("HARNX_HISTORY_MAX_FILE_BYTES") };
        drop(guard);

        assert!(result.is_ok(), "capture_tree_blocking should succeed");

        let commit_id = result.expect("commit id");
        let commit = history_repo.find_object(commit_id).expect("find commit");
        assert_eq!(commit.kind, gix::object::Kind::Commit, "should be a commit");
    }
}
