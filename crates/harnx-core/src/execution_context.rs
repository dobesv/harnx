//! Private, tool-observed execution context shared by tool servers and sessions.
//!
//! The reserved metadata is transported in MCP result `_meta`, removed before
//! any user/model-visible processing, and retained in canonical session
//! metadata. Paths and transport provenance are deliberately private.

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

pub const EXECUTION_CONTEXT_NAMESPACE: &str = "dev.harnx.execution_context";
pub const EXECUTION_CONTEXT_VERSION: u32 = 1;
pub const EXECUTION_CONTEXT_MAX_RETAINED: usize = 16;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitRemoteObservation {
    pub name: String,
    /// Portable, credential-free identity in `host/path` form.
    pub repository: String,
    #[serde(default)]
    pub primary: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitRepositoryObservation {
    pub worktree_root: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remotes: Vec<GitRemoteObservation>,
}

impl GitRepositoryObservation {
    pub fn primary_repository(&self) -> Option<&str> {
        self.remotes
            .iter()
            .find(|remote| remote.primary)
            .or_else(|| self.remotes.first())
            .map(|remote| remote.repository.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolObservationProvenance {
    pub server_scope: String,
    pub server_identity: String,
    pub tool_name: String,
    pub call_id: String,
    pub worker_received_at: DateTime<Utc>,
}

impl ToolObservationProvenance {
    pub fn new(
        server_scope: impl Into<String>,
        server_identity: impl Into<String>,
        tool_name: impl Into<String>,
        call_id: impl Into<String>,
    ) -> Self {
        Self {
            server_scope: server_scope.into(),
            server_identity: server_identity.into(),
            tool_name: tool_name.into(),
            call_id: call_id.into(),
            worker_received_at: Utc::now(),
        }
    }

    pub fn execution_scope(&self) -> (&str, &str) {
        (&self.server_scope, &self.server_identity)
    }

    fn validate(&self) -> Result<()> {
        for value in [
            self.server_scope.as_str(),
            self.server_identity.as_str(),
            self.tool_name.as_str(),
            self.call_id.as_str(),
        ] {
            if value.trim().is_empty() {
                bail!("execution-context provenance fields must not be empty");
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionContextObservation {
    pub version: u32,
    pub observed_at: DateTime<Utc>,
    pub workspace_root: String,
    pub working_directory: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository: Option<GitRepositoryObservation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provenance: Option<ToolObservationProvenance>,
}

impl ExecutionContextObservation {
    /// Observe only `target` and its ancestors. This intentionally does not
    /// scan descendants, so a newly cloned repository is discovered after a
    /// later tool call targets it.
    pub fn observe(workspace_root: &Path, target: &Path) -> Self {
        let workspace_root = canonical_or_absolute(workspace_root);
        let target = canonical_or_absolute(target);
        let working_directory = if target.is_dir() {
            target.clone()
        } else {
            target
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| workspace_root.clone())
        };
        let repository = observe_git_repository(&target);
        Self {
            version: EXECUTION_CONTEXT_VERSION,
            observed_at: Utc::now(),
            workspace_root: workspace_root.to_string_lossy().into_owned(),
            working_directory: working_directory.to_string_lossy().into_owned(),
            repository,
            provenance: None,
        }
    }

    pub fn validate(&self) -> Result<()> {
        if self.version != EXECUTION_CONTEXT_VERSION {
            bail!("unsupported execution-context version {}", self.version);
        }
        if self.workspace_root.trim().is_empty() || self.working_directory.trim().is_empty() {
            bail!("execution-context paths must not be empty");
        }
        if let Some(repository) = &self.repository {
            repository.validate()?;
        }
        self.provenance
            .as_ref()
            .context("execution-context observation is missing transport provenance")?
            .validate()
    }

    pub fn primary_repository(&self) -> Option<&str> {
        self.repository
            .as_ref()
            .and_then(GitRepositoryObservation::primary_repository)
    }

    pub fn branch(&self) -> Option<&str> {
        self.repository
            .as_ref()
            .and_then(|repository| repository.branch.as_deref())
    }

    pub fn execution_scope(&self) -> Option<(&str, &str)> {
        self.provenance
            .as_ref()
            .map(ToolObservationProvenance::execution_scope)
    }

    /// Equality for persistence no-op detection. Per-call timestamps and
    /// provenance details do not make an otherwise unchanged context new.
    pub fn same_observed_state(&self, other: &Self) -> bool {
        self.workspace_root == other.workspace_root
            && self.working_directory == other.working_directory
            && self.repository == other.repository
            && self.execution_scope() == other.execution_scope()
    }
}

impl GitRepositoryObservation {
    fn validate(&self) -> Result<()> {
        if self.worktree_root.trim().is_empty() {
            bail!("execution-context worktree root must not be empty");
        }
        if let Some(branch) = &self.branch {
            validate_branch(branch)?;
        }
        for remote in &self.remotes {
            validate_remote(remote)?;
        }
        Ok(())
    }
}

fn validate_branch(branch: &str) -> Result<()> {
    if branch.is_empty() || branch.chars().any(char::is_control) {
        bail!("execution-context branch is not safe to display");
    }
    Ok(())
}

fn validate_remote(remote: &GitRemoteObservation) -> Result<()> {
    if remote.name.trim().is_empty() || !is_normalized_repository_identity(&remote.repository) {
        bail!("execution-context remotes must have names and normalized portable identities");
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionContextExtension {
    pub version: u32,
    #[serde(default)]
    pub contexts: Vec<ExecutionContextObservation>,
}

impl Default for ExecutionContextExtension {
    fn default() -> Self {
        Self {
            version: EXECUTION_CONTEXT_VERSION,
            contexts: Vec::new(),
        }
    }
}

impl ExecutionContextExtension {
    pub fn from_value(value: Value) -> Result<Self> {
        let extension: Self = serde_json::from_value(value)
            .context("decode dev.harnx.execution_context session extension")?;
        if extension.version != EXECUTION_CONTEXT_VERSION {
            bail!(
                "unsupported execution-context extension version {}",
                extension.version
            );
        }
        for context in &extension.contexts {
            context.validate()?;
        }
        Ok(extension)
    }

    /// Merge one observation. Returns false when the retained state is an
    /// exact semantic repeat and no metadata write is needed.
    pub fn merge(&mut self, observation: ExecutionContextObservation) -> bool {
        if self
            .contexts
            .iter()
            .any(|current| current.same_observed_state(&observation))
        {
            return false;
        }

        if let Some(index) = self
            .contexts
            .iter()
            .position(|current| contexts_replace_each_other(current, &observation))
        {
            self.contexts[index] = observation;
        } else {
            self.contexts.push(observation);
        }
        self.contexts
            .sort_by_key(|context| std::cmp::Reverse(context.observed_at));
        self.contexts.truncate(EXECUTION_CONTEXT_MAX_RETAINED);
        true
    }
}

fn contexts_replace_each_other(
    current: &ExecutionContextObservation,
    incoming: &ExecutionContextObservation,
) -> bool {
    match (current.primary_repository(), incoming.primary_repository()) {
        (Some(left), Some(right)) if left == right => return true,
        _ => {}
    }
    if current.execution_scope() != incoming.execution_scope() {
        return false;
    }
    match (&current.repository, &incoming.repository) {
        (Some(left), Some(right)) => left.worktree_root == right.worktree_root,
        (None, None) => current.workspace_root == incoming.workspace_root,
        _ => false,
    }
}

/// Remove and return the reserved result `_meta` value. Empty `_meta` maps are
/// removed too, keeping downstream rendering byte-for-byte free of private
/// context.
pub fn take_result_execution_context(result: &mut Value) -> Option<Value> {
    let object = result.as_object_mut()?;
    let meta = object.get_mut("_meta")?.as_object_mut()?;
    let context = meta.remove(EXECUTION_CONTEXT_NAMESPACE);
    if meta.is_empty() {
        object.remove("_meta");
    }
    context
}

pub fn put_result_execution_context(result: &mut Value, context: Value) {
    let Some(object) = result.as_object_mut() else {
        return;
    };
    let meta = object
        .entry("_meta")
        .or_insert_with(|| Value::Object(serde_json::Map::new()));
    if let Some(meta) = meta.as_object_mut() {
        meta.insert(EXECUTION_CONTEXT_NAMESPACE.to_string(), context);
    }
}

fn observe_git_repository(target: &Path) -> Option<GitRepositoryObservation> {
    let search = if target.is_dir() {
        target
    } else {
        target.parent()?
    };
    let root = git_output(search, &["rev-parse", "--show-toplevel"])?;
    let root = canonical_or_absolute(Path::new(&root));
    let branch = git_output(&root, &["symbolic-ref", "--quiet", "--short", "HEAD"])
        .filter(|branch| !branch.is_empty());
    let tracking_remote = branch.as_deref().and_then(|branch| {
        git_output(
            &root,
            &["config", "--get", &format!("branch.{branch}.remote")],
        )
    });
    let remote_names = git_output(&root, &["remote"])
        .unwrap_or_default()
        .lines()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .collect::<BTreeSet<_>>();
    let mut remotes = remote_names
        .into_iter()
        .filter_map(|name| {
            let url = git_output(&root, &["remote", "get-url", &name])?;
            let repository = normalize_git_remote(&url)?;
            Some(GitRemoteObservation {
                name,
                repository,
                primary: false,
            })
        })
        .collect::<Vec<_>>();
    remotes.sort_by(|left, right| {
        left.name
            .cmp(&right.name)
            .then_with(|| left.repository.cmp(&right.repository))
    });
    let primary_index = tracking_remote
        .as_deref()
        .and_then(|name| remotes.iter().position(|remote| remote.name == name))
        .or_else(|| remotes.iter().position(|remote| remote.name == "origin"))
        .or_else(|| (!remotes.is_empty()).then_some(0));
    if let Some(index) = primary_index {
        remotes[index].primary = true;
    }
    Some(GitRepositoryObservation {
        worktree_root: root.to_string_lossy().into_owned(),
        branch,
        remotes,
    })
}

fn git_output(directory: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(directory)
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn canonical_or_absolute(path: &Path) -> PathBuf {
    if let Ok(canonical) = path.canonicalize() {
        return canonical;
    }
    if path.is_absolute() {
        return path.to_path_buf();
    }
    std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join(path)
}

/// Convert common Git transports to a credential-free portable identity.
/// Local filesystem and `file://` remotes intentionally return `None`.
pub fn normalize_git_remote(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if is_local_git_remote(raw) {
        return None;
    }

    let (host, path) = if raw.contains("://") {
        split_url_git_remote(raw)?
    } else {
        split_scp_git_remote(raw)?
    };

    normalize_git_remote_parts(host, path)
}

fn is_local_git_remote(raw: &str) -> bool {
    if raw.is_empty() || looks_like_windows_path(raw) {
        return true;
    }
    for prefix in ["/", "./", "../", "file://"] {
        if raw.starts_with(prefix) {
            return true;
        }
    }
    false
}

fn split_url_git_remote(raw: &str) -> Option<(&str, &str)> {
    let scheme_index = raw.find("://")?;
    let scheme = raw[..scheme_index].to_ascii_lowercase();
    if !matches!(scheme.as_str(), "ssh" | "git" | "http" | "https") {
        return None;
    }
    let remainder = &raw[scheme_index + 3..];
    let remainder = remainder.split(['?', '#']).next()?;
    let slash = remainder.find('/')?;
    let authority = &remainder[..slash];
    let host = authority.rsplit('@').next()?.trim();
    Some((host, remainder[slash + 1..].trim()))
}

fn split_scp_git_remote(raw: &str) -> Option<(&str, &str)> {
    let without_suffix = raw.split(['?', '#']).next()?;
    let colon = without_suffix.find(':')?;
    let authority = &without_suffix[..colon];
    if authority.contains('/') || authority.is_empty() {
        return None;
    }
    let host = authority.rsplit('@').next()?.trim();
    Some((host, without_suffix[colon + 1..].trim_start_matches('/')))
}

fn normalize_git_remote_parts(host: &str, path: &str) -> Option<String> {
    let host = host.trim_matches(|character| character == '[' || character == ']');
    let mut path = path.trim_matches('/');
    if let Some(stripped) = path.strip_suffix(".git") {
        path = stripped.trim_end_matches('/');
    }
    if host.is_empty() || path.is_empty() {
        return None;
    }
    Some(format!("{}/{path}", host.to_ascii_lowercase()))
}

fn looks_like_windows_path(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && matches!(bytes[2], b'/' | b'\\')
}

fn is_normalized_repository_identity(value: &str) -> bool {
    if normalized_repository_identity_parts(value).is_none() {
        return false;
    }
    repository_identity_characters_are_safe(value)
}

fn normalized_repository_identity_parts(value: &str) -> Option<(&str, &str)> {
    let (host, path) = value.split_once('/')?;
    if host.is_empty() || host != host.to_ascii_lowercase() {
        return None;
    }
    if path.is_empty() {
        return None;
    }
    if path.ends_with('/') {
        return None;
    }
    if path.ends_with(".git") {
        return None;
    }
    if value.contains("://") {
        return None;
    }
    Some((host, path))
}

fn repository_identity_characters_are_safe(value: &str) -> bool {
    for character in value.chars() {
        if character.is_whitespace() || character.is_control() {
            return false;
        }
        if matches!(character, '@' | '?' | '#' | '\\') {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(directory: &Path, args: &[&str]) {
        let output = Command::new("git")
            .arg("-C")
            .arg(directory)
            .args(args)
            .output()
            .expect("run git");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn repository() -> tempfile::TempDir {
        let directory = tempfile::tempdir().expect("temp repository");
        git(directory.path(), &["init", "-b", "main"]);
        git(directory.path(), &["config", "user.name", "Harnx Test"]);
        git(
            directory.path(),
            &["config", "user.email", "harnx@example.com"],
        );
        std::fs::write(directory.path().join("README.md"), "test\n").expect("write fixture");
        git(directory.path(), &["add", "README.md"]);
        git(directory.path(), &["commit", "-m", "fixture"]);
        directory
    }

    #[test]
    fn normalizes_network_remotes_without_credentials() {
        let cases = [
            ("git@github.com:dobesv/harnx.git", "github.com/dobesv/harnx"),
            (
                "ssh://git@GitHub.com/dobesv/harnx.git",
                "github.com/dobesv/harnx",
            ),
            (
                "https://token:secret@github.com/dobesv/harnx.git?x=1#frag",
                "github.com/dobesv/harnx",
            ),
            ("git://example.com/team/repo/", "example.com/team/repo"),
        ];
        for (input, expected) in cases {
            assert_eq!(normalize_git_remote(input).as_deref(), Some(expected));
        }
    }

    #[test]
    fn rejects_local_remote_paths() {
        for input in ["/srv/repo.git", "../repo", "file:///srv/repo", "C:\\repo"] {
            assert_eq!(normalize_git_remote(input), None, "{input}");
        }
    }

    #[test]
    fn validation_rejects_credentialed_or_unsafe_display_context() {
        let mut observation =
            ExecutionContextObservation::observe(Path::new("/workspace"), Path::new("/workspace"));
        observation.provenance = Some(ToolObservationProvenance::new(
            "scope", "fs", "read", "call",
        ));
        observation.repository = Some(GitRepositoryObservation {
            worktree_root: "/workspace".to_string(),
            branch: Some("feature\nspoofed".to_string()),
            remotes: vec![GitRemoteObservation {
                name: "origin".to_string(),
                repository: "token@github.com/acme/repo".to_string(),
                primary: true,
            }],
        });
        assert!(observation.validate().is_err());
    }

    #[test]
    fn merge_replaces_branch_state_and_caps_retention() {
        let mut extension = ExecutionContextExtension::default();
        for index in 0..=EXECUTION_CONTEXT_MAX_RETAINED {
            let mut observation = ExecutionContextObservation::observe(
                Path::new("/workspace"),
                Path::new(&format!("/workspace-{index}")),
            );
            observation.workspace_root = format!("/workspace-{index}");
            observation.working_directory = format!("/workspace-{index}");
            observation.observed_at += chrono::Duration::seconds(index as i64);
            observation.provenance = Some(ToolObservationProvenance::new(
                "scope",
                "fs",
                "read",
                index.to_string(),
            ));
            assert!(extension.merge(observation));
        }
        assert_eq!(extension.contexts.len(), EXECUTION_CONTEXT_MAX_RETAINED);
        assert!(!extension
            .contexts
            .iter()
            .any(|context| context.workspace_root == "/workspace-0"));
    }

    #[test]
    fn result_metadata_is_removed_without_disturbing_public_meta() {
        let mut result = serde_json::json!({
            "content": [],
            "_meta": {
                EXECUTION_CONTEXT_NAMESPACE: {"version": 1},
                "public": true
            }
        });
        assert!(take_result_execution_context(&mut result).is_some());
        assert_eq!(result["_meta"], serde_json::json!({"public": true}));
    }

    #[test]
    fn observes_branch_worktree_and_tracking_remote() {
        let repository = repository();
        git(
            repository.path(),
            &[
                "remote",
                "add",
                "origin",
                "https://github.com/acme/origin.git",
            ],
        );
        git(
            repository.path(),
            &[
                "remote",
                "add",
                "upstream",
                "git@github.com:acme/upstream.git",
            ],
        );
        git(
            repository.path(),
            &["config", "branch.main.remote", "upstream"],
        );
        let worktree_parent = tempfile::tempdir().expect("worktree parent");
        let worktree = worktree_parent.path().join("feature-worktree");
        git(
            repository.path(),
            &[
                "worktree",
                "add",
                "-b",
                "feature",
                worktree.to_str().expect("utf8 worktree"),
            ],
        );

        let main = observe_git_repository(repository.path()).expect("main repository");
        assert_eq!(main.branch.as_deref(), Some("main"));
        assert_eq!(main.remotes.len(), 2);
        assert_eq!(main.primary_repository(), Some("github.com/acme/upstream"));
        let linked = observe_git_repository(&worktree).expect("linked worktree");
        assert_eq!(linked.branch.as_deref(), Some("feature"));
        assert_eq!(
            Path::new(&linked.worktree_root),
            worktree.canonicalize().expect("canonical worktree")
        );
    }

    #[test]
    fn observes_detached_head_and_non_repository() {
        let repository = repository();
        git(repository.path(), &["checkout", "--detach"]);
        let detached = observe_git_repository(repository.path()).expect("detached repository");
        assert_eq!(detached.branch, None);

        let non_repository = tempfile::tempdir().expect("non repository");
        assert!(observe_git_repository(non_repository.path()).is_none());
    }
}
