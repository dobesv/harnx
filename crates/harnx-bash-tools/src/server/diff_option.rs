//! The `diff` option of `exec`, `spawn`, `wait` and `status`: which level a
//! call reports at, how the snapshots behind it are paired and rendered, and
//! how a script-style result bounds the text.

use super::{BashServer, ExecPipelineParams, ObjectId, SnapshotDecision};
use harnx_git_history::DiffLevel;
use rmcp::schemars::{json_schema, Schema};
use std::path::{Path, PathBuf};

/// The description of the `diff` parameter, the same on every tool that takes it.
pub(crate) const DIFF_DESCRIPTION: &str = "How much to report of what the command changed: none (take no snapshot), summary (one line with the size of the change and the git diff command that shows it), files (that line and the changed files) or full (the diff). Default: full for a direct call, summary on stderr inside a bash script.";

/// The same for `wait` and `status`: the process was snapshotted, or not, when
/// it was spawned, so the level only chooses what this call prints.
pub(crate) const DIFF_REPORT_DESCRIPTION: &str = "How much to report of what the command changed: none (report nothing), summary (one line with the size of the change and the git diff command that shows it), files (that line and the changed files) or full (the diff). The level only chooses what this call prints; whether the process was snapshotted was decided when it was spawned. Default: full for a direct call, summary on stderr inside a bash script.";

/// Whole per-repository entries are kept while their total stays within this.
const DIFF_BOUND_BYTES: usize = 50_000;

/// A repository whose snapshots differ: its directory, the snapshot before
/// the command and the snapshot after it.
pub(crate) type ChangedRepo = (PathBuf, ObjectId, ObjectId);

/// The level this call reports at: its `diff` argument, else the whole diff
/// for a direct call and a summary for a script.
pub(crate) fn diff_level(requested: Option<DiffLevel>) -> DiffLevel {
    requested.unwrap_or_else(|| default_level(crate::toolset::invocation_wants_structured_result()))
}

fn default_level(script: bool) -> DiffLevel {
    if script {
        DiffLevel::Summary
    } else {
        DiffLevel::Full
    }
}

/// Where a call takes its snapshots and how much of the change it reports.
#[derive(Clone, Copy)]
pub(crate) struct SnapshotPlan<'a> {
    pub label: &'a str,
    pub level: DiffLevel,
}

/// The level an exec pipeline reports at. A protocol template's result is
/// its stderr plus the whole diff, whatever the caller asked for.
pub(crate) fn pipeline_level(params: &ExecPipelineParams<'_>) -> DiffLevel {
    if params.protocol.is_some() {
        DiffLevel::Full
    } else {
        diff_level(params.diff)
    }
}

/// `none` snapshots nothing, so nothing is classified for it.
pub(crate) fn decision_at(level: DiffLevel, classified: SnapshotDecision) -> SnapshotDecision {
    if level.snapshots() {
        classified
    } else {
        SnapshotDecision::ReadOnly
    }
}

/// The schema of the `diff` parameter: one of the four names, or null.
pub(crate) fn diff_schema() -> Schema {
    json_schema!({
        "type": ["string", "null"],
        "enum": ["none", "summary", "files", "full", null],
    })
}

/// Whole entries, each ending in a newline, up to the byte bound in total
/// (the first is always kept), and a note counting the entries left out.
pub(crate) fn bounded_diff(parts: Vec<String>) -> (Option<String>, Option<String>) {
    let mut text = String::new();
    let mut dropped = 0;
    for part in parts {
        let entry = if part.ends_with('\n') {
            part
        } else {
            format!("{part}\n")
        };
        if dropped == 0 && (text.is_empty() || text.len() + entry.len() <= DIFF_BOUND_BYTES) {
            text.push_str(&entry);
        } else {
            dropped += 1;
        }
    }
    let noun = if dropped == 1 {
        "repository"
    } else {
        "repositories"
    };
    let note = (dropped > 0).then(|| format!("{dropped} more {noun} changed; diff not shown"));
    ((!text.is_empty()).then_some(text), note)
}

/// The repositories whose snapshot changed between `before` and `after`.
fn changed_repos(
    before: &[(PathBuf, ObjectId)],
    after: &[(PathBuf, ObjectId)],
) -> Vec<ChangedRepo> {
    before
        .iter()
        .filter_map(|(dir, before_id)| {
            let (_, after_id) = after.iter().find(|(after_dir, _)| after_dir == dir)?;
            (before_id != after_id).then(|| (dir.clone(), *before_id, *after_id))
        })
        .collect()
}

impl BashServer {
    /// Takes the after-snapshot and pairs it with `before`. Empty when nothing
    /// was snapshotted before, which is how `none` takes no snapshot at all.
    pub(crate) async fn snapshot_changes(
        &self,
        before: &[(PathBuf, ObjectId)],
        decision: &SnapshotDecision,
        working_dir: &Path,
        label: &str,
    ) -> Vec<ChangedRepo> {
        if before.is_empty() {
            return vec![];
        }
        let after = self.take_snapshots(decision, working_dir, label).await;
        changed_repos(before, &after)
    }

    /// One text per changed repository, as `level` prints it.
    pub(crate) async fn render_changes(
        &self,
        changes: &[ChangedRepo],
        level: DiffLevel,
    ) -> Vec<String> {
        let mut parts = Vec::new();
        for (dir, before_id, after_id) in changes {
            match self
                .inner
                .history
                .render_change(dir, *before_id, *after_id, level)
                .await
            {
                Ok(text) if !text.is_empty() => parts.push(text),
                Ok(_) => {}
                Err(e) => log::warn!("history diff failed: {e}"),
            }
        }
        parts
    }

    /// The `diff` field of a script-style result and the note for what it
    /// leaves out.
    pub(crate) async fn script_diff(
        &self,
        changes: &[ChangedRepo],
        level: DiffLevel,
    ) -> (Option<String>, Option<String>) {
        bounded_diff(self.render_changes(changes, level).await)
    }
}
