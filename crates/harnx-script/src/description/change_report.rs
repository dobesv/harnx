use crate::{commands::ToolSpec, description::examples};

const REPORT: &str = "- A tool that changes files reports the change on stderr in one line: its size and the git diff command that shows it (pipe that command into git apply -R to undo the change). --diff full prints the diff, --diff files the changed files, --diff none skips the snapshot (nothing to roll back to).";

/// The bullet about change reports, empty unless a listed tool with a shell
/// form takes a `diff` option. The snapshot recipe needs the agent's own
/// name for `bash_exec`, so it is left out when no such tool takes `diff`.
pub(super) fn bullet(tools: &[&ToolSpec]) -> String {
    let with_diff: Vec<_> = tools.iter().filter(|tool| takes_diff(tool)).collect();
    if with_diff.is_empty() {
        return String::new();
    }
    let recipe = with_diff
        .iter()
        .find(|tool| tool.name.ends_with("bash_exec"))
        .map(|tool| snapshot_recipe(&examples::shell_word(&tool.name)))
        .unwrap_or_default();
    format!("{REPORT}{recipe}\n")
}

fn takes_diff(tool: &ToolSpec) -> bool {
    tool.output.is_some()
        && tool
            .schema
            .get("properties")
            .is_some_and(|properties| properties.get("diff").is_some())
}

fn snapshot_recipe(exec: &str) -> String {
    format!(" For one diff across several changes, snapshot first: base=$({exec} --command 'git stash create'), make the changes, then {exec} --command \"git status --short; git diff ${{base:-HEAD}}\" (git status lists new files, which git diff leaves out).")
}
