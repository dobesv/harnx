//! How a write tool reports a change: which level of the snapshot pair it
//! prints, and the two result shapes (content blocks for a model, fields for
//! a script).

use super::FsServer;
use crate::script_result::{script_result, ResultStyle};
use harnx_git_history::DiffLevel;
use rmcp::model::{CallToolResult, ContentBlock};
use rmcp::schemars::{json_schema, Schema};
use serde_json::{json, Value};
use std::path::Path;

/// The description of the `diff` parameter on every write tool.
pub(crate) const DIFF_DESCRIPTION: &str = "How much to report of the change: none (take no snapshot), summary (one line with the size of the change and the git diff command that shows it), files (that line and the changed file) or full (the diff). Default: full for a direct call, summary on stderr inside a bash script.";

/// The schema of the `diff` parameter: one of the four names, or null.
pub(crate) fn diff_schema() -> Schema {
    json_schema!({
        "type": ["string", "null"],
        "enum": ["none", "summary", "files", "full", null],
    })
}

/// The confirmation line of a write and the text describing its change.
pub(crate) struct Mutation {
    pub message: String,
    pub diff: Option<String>,
}

/// Model style: the message block, then the diff block when there is one.
/// Script style: structuredContent `{"message": "<line>\n", "diff": "<text>"}`,
/// where `diff` is left out when there is nothing to show.
pub(crate) fn mutation_result(mutation: Mutation, script: bool) -> CallToolResult {
    let Mutation { message, diff } = mutation;
    let diff = diff.filter(|text| !text.is_empty());
    if !script {
        let mut contents = vec![ContentBlock::text(message)];
        contents.extend(diff.map(ContentBlock::text));
        return CallToolResult::success(contents);
    }
    let mut fields = json!({ "message": format!("{message}\n") });
    if let Some(diff) = diff {
        fields["diff"] = Value::String(diff);
    }
    script_result(fields, message)
}

/// The level a call reports at: its `diff` argument, else the whole diff for
/// a direct call and a summary for a script.
pub(crate) fn diff_level(requested: Option<DiffLevel>, script: bool) -> DiffLevel {
    requested.unwrap_or(if script {
        DiffLevel::Summary
    } else {
        DiffLevel::Full
    })
}

/// The level and result shape of one write call.
#[derive(Clone, Copy)]
pub(crate) struct Report {
    pub level: DiffLevel,
    pub script: bool,
}

impl Report {
    pub(crate) fn new(requested: Option<DiffLevel>, style: ResultStyle) -> Self {
        let script = style == ResultStyle::Script;
        Self {
            level: diff_level(requested, script),
            script,
        }
    }
}

/// A write in progress: the file, its before-snapshot and how to report it.
pub(crate) struct PendingMutation<'a> {
    path: &'a Path,
    op: &'static str,
    before: Option<gix::ObjectId>,
    report: Report,
}

impl FsServer {
    /// Takes the before-snapshot, unless the call reports at `none`.
    pub(crate) async fn begin_mutation<'a>(
        &self,
        path: &'a Path,
        op: &'static str,
        report: Report,
    ) -> PendingMutation<'a> {
        let before = match report.level.snapshots() {
            true => self.snapshot_before(path, &format!("before {op}")).await,
            false => None,
        };
        PendingMutation {
            path,
            op,
            before,
            report,
        }
    }

    /// Takes the after-snapshot and builds the result. `binary` is a write of
    /// bytes git treats as binary, which `full` reports without a diff body.
    pub(crate) async fn finish_mutation(
        &self,
        pending: PendingMutation<'_>,
        message: String,
        binary: bool,
    ) -> CallToolResult {
        let diff = self.change_text(&pending, binary).await;
        mutation_result(Mutation { message, diff }, pending.report.script)
    }

    async fn change_text(&self, pending: &PendingMutation<'_>, binary: bool) -> Option<String> {
        let level = pending.report.level;
        if binary && level == DiffLevel::Full {
            return self
                .snapshot_after_binary_write(pending.path, pending.before)
                .await;
        }
        let label = format!("after {}", pending.op);
        self.snapshot_after_change(pending.path, pending.before, &label, level)
            .await
    }

    /// The change between `before` and a new snapshot of `path`, as `level`
    /// prints it. `None` when there was no before-snapshot or no after.
    pub(crate) async fn snapshot_after_change(
        &self,
        path: &Path,
        before: Option<gix::ObjectId>,
        label: &str,
        level: DiffLevel,
    ) -> Option<String> {
        let before = before?;
        let after = match self.history.snapshot_file(path, label).await {
            Ok(after) => after,
            Err(e) => {
                log::warn!("history after-snapshot failed: {e}");
                return None;
            }
        };
        let Some(repo_dir) = harnx_git_history::discover::find_repo_for_path(path) else {
            return Some(String::new());
        };
        Some(self.change_between(&repo_dir, before, after, level).await)
    }

    /// The whole diff keeps its dedicated entry point; the other levels share
    /// `render_change`.
    async fn change_between(
        &self,
        repo_dir: &Path,
        before: gix::ObjectId,
        after: gix::ObjectId,
        level: DiffLevel,
    ) -> String {
        let text = match level {
            DiffLevel::Full => self.history.diff_commits(repo_dir, before, after).await,
            _ => {
                self.history
                    .render_change(repo_dir, before, after, level)
                    .await
            }
        };
        text.unwrap_or_default()
    }
}
