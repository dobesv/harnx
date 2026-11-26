//! The `rollback_file` entry point through the bash toolset: a targeted
//! snapshot's summary short id restores only the snapshot's own paths, and a
//! preflight rejection is a recoverable tool error that changes nothing.

use crate::script_results::{call, diff_field, summary_ids, toolset_in};
use harnx_bash_tools::BashToolset;
use serde_json::json;

/// A git repository with two committed files plus an untracked file and a
/// staged new one, so a rollback has unrelated content to preserve.
struct CommittedRepo {
    directory: tempfile::TempDir,
    toolset: BashToolset,
}

impl CommittedRepo {
    async fn new() -> Self {
        let directory = tempfile::tempdir().expect("temp dir");
        let dir = directory.path();
        run_git(dir, &["init"]);
        run_git(dir, &["config", "user.name", "harnx test"]);
        run_git(dir, &["config", "user.email", "harnx@example.com"]);
        run_git(dir, &["config", "core.autocrlf", "false"]);
        std::fs::write(dir.join("tracked.txt"), "tracked\n").expect("write tracked");
        std::fs::write(dir.join("other.txt"), "other\n").expect("write other");
        run_git(dir, &["add", "."]);
        run_git(dir, &["commit", "-q", "-m", "two files"]);
        std::fs::write(dir.join("untracked.txt"), "untracked\n").expect("write untracked");
        std::fs::write(dir.join("staged.txt"), "staged\n").expect("write staged");
        run_git(dir, &["add", "staged.txt"]);
        let toolset = toolset_in(dir).await;
        Self { directory, toolset }
    }

    fn path(&self) -> &std::path::Path {
        self.directory.path()
    }

    /// A targeted snapshot of one file, reported on a summary line: returns
    /// the line's 12-digit after id, the state with `text` written.
    async fn targeted_snapshot_of(&self, file: &str, text: &str) -> String {
        let args = json!({
            "command": format!("printf '{text}\\n' > {file}"),
            "working_dir": self.path(),
        });
        let script = call(&self.toolset, "exec", args, true).await;
        let summary = diff_field(&script);
        let (_, after) = summary_ids(summary.trim_end());
        after
    }
}

fn run_git(dir: &std::path::Path, args: &[&str]) {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("git");
    assert!(
        output.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
}

fn git_out(dir: &std::path::Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("git");
    assert!(
        output.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("git output")
}

fn read(dir: &std::path::Path, name: &str) -> String {
    std::fs::read_to_string(dir.join(name)).expect("read")
}

/// The git state a non-destructive rollback must leave alone: HEAD and the
/// staged entries. Working-tree edits are the rollback's business, the rest
/// is not.
struct GitState {
    head: String,
    staged: String,
}

fn git_state(dir: &std::path::Path) -> GitState {
    GitState {
        head: git_out(dir, &["rev-parse", "HEAD"]),
        staged: git_out(dir, &["diff", "--cached", "--name-only"]),
    }
}

/// HEAD and the staged entries are exactly what they were before.
fn assert_git_state_unchanged(dir: &std::path::Path, before: &GitState) {
    let after = git_state(dir);
    assert_eq!(after.head, before.head);
    assert_eq!(after.staged, before.staged);
}

/// The summary line's short after id restores the file the targeted snapshot
/// recorded and nothing else: unrelated tracked and untracked contents, the
/// staged entry, HEAD and the index stay as they were (issue #2929).
#[tokio::test]
async fn rollback_takes_the_short_id_from_a_summary_line() {
    let repo = CommittedRepo::new().await;
    let dir = repo.path();
    let after = repo.targeted_snapshot_of("tracked.txt", "restored").await;

    // Later edits the snapshot never saw, so a restoration is visible.
    std::fs::write(dir.join("tracked.txt"), "later\n").expect("later edit");
    std::fs::write(dir.join("other.txt"), "edited later\n").expect("unrelated edit");
    let before_state = git_state(dir);

    let args = json!({ "commit_id": after, "repo_path": dir });
    let result = call(&repo.toolset, "rollback_file", args, false).await;
    assert!(result["isError"] != json!(true), "{result}");
    let text = result["content"][0]["text"].as_str().expect("text block");
    assert!(text.contains("after-snapshot"), "{text}");
    assert!(!text.contains("new commit"), "{text}");

    assert_eq!(read(dir, "tracked.txt"), "restored\n");
    assert_eq!(read(dir, "other.txt"), "edited later\n");
    assert_eq!(read(dir, "untracked.txt"), "untracked\n");
    assert_eq!(read(dir, "staged.txt"), "staged\n");
    assert_git_state_unchanged(dir, &before_state);
}

/// A destination the preflight refuses is a recoverable tool error, not a
/// protocol error, and neither the conflicting path nor the git state moves.
#[tokio::test]
async fn rollback_refuses_a_conflicting_destination_without_touching_anything() {
    let repo = CommittedRepo::new().await;
    let dir = repo.path();
    let after = repo.targeted_snapshot_of("victim.txt", "victim").await;

    // Replace the snapshot's destination with a directory holding a file.
    let victim = dir.join("victim.txt");
    std::fs::remove_file(&victim).expect("remove victim");
    std::fs::create_dir(&victim).expect("mkdir victim");
    std::fs::write(victim.join("keep.txt"), "keep\n").expect("write keep");
    let before_state = git_state(dir);

    let args = json!({ "commit_id": after, "repo_path": dir });
    let result = call(&repo.toolset, "rollback_file", args, false).await;
    assert_eq!(result["isError"], json!(true), "{result}");
    let text = result["content"][0]["text"].as_str().expect("text block");
    assert!(
        text.contains("refusing to restore victim.txt: destination is a directory"),
        "{text}"
    );

    assert!(victim.is_dir());
    assert_eq!(read(&victim, "keep.txt"), "keep\n");
    assert_eq!(read(dir, "tracked.txt"), "tracked\n");
    assert_eq!(read(dir, "other.txt"), "other\n");
    assert_git_state_unchanged(dir, &before_state);
}
