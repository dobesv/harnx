use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use uuid::Uuid;

use crate::diff::diff_commits_blocking;
use crate::discover::{find_repos_under_roots, history_dir};
use crate::rollback::rollback_to_commit_blocking;

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

    for file_path in collect_files(workdir)? {
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
            "-z",
            "ls-files",
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
    Ok(files)
}
