//! The `rollback_file` entry point: a summary short id restores only the
//! snapshot's own paths, and a preflight rejection is a recoverable tool
//! error that changes nothing.

use super::*;
use crate::script_result::ResultStyle;

/// A repository with two committed files plus an untracked file and a staged
/// new one, so a rollback has unrelated content to preserve.
fn committed_two_file_repo() -> TestDir {
    let dir = TestDir::new();
    init_git_repo(dir.path());
    git(dir.path(), &["config", "commit.gpgsign", "false"]);
    git(dir.path(), &["config", "core.autocrlf", "false"]);
    write_fixture(&dir, "tracked.txt", "tracked\n");
    write_fixture(&dir, "other.txt", "other\n");
    git(dir.path(), &["add", "."]);
    git(dir.path(), &["commit", "-q", "-m", "two files"]);
    write_fixture(&dir, "untracked.txt", "untracked\n");
    write_fixture(&dir, "staged.txt", "staged\n");
    git(dir.path(), &["add", "staged.txt"]);
    dir
}

fn git_out(dir: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

fn read(dir: &TestDir, name: &str) -> String {
    std::fs::read_to_string(dir.path().join(name)).unwrap()
}

/// The 12-digit before and after ids of the summary line a script-style
/// `write` reports.
async fn summary_ids_of_a_write(server: &FsServer, path: &Path) -> (String, String) {
    let result = server
        .invoke_tool_value(
            "write",
            json!({
                "path": path_string(path),
                "content": "restored\n",
                "diff": "summary",
            }),
            ResultStyle::Script,
        )
        .await
        .unwrap();
    let fields = result
        .structured_content
        .as_ref()
        .expect("script-style result");
    let line = fields
        .get("diff")
        .and_then(Value::as_str)
        .expect("summary diff")
        .trim_end();
    let mut words = line.split_whitespace().rev();
    let after = words.next().expect("after id").to_string();
    let before = words.next().expect("before id").to_string();
    assert_eq!((before.len(), after.len()), (12, 12), "{line}");
    (before, after)
}

/// The git state a non-destructive rollback must leave alone: HEAD and the
/// staged entries. Working-tree edits are the rollback's business, the rest
/// is not.
struct GitState {
    head: String,
    staged: String,
}

fn git_state(dir: &TestDir) -> GitState {
    GitState {
        head: git_out(dir.path(), &["rev-parse", "HEAD"]),
        staged: git_out(dir.path(), &["diff", "--cached", "--name-only"]),
    }
}

/// The entry point under test: `rollback_file_impl` against `dir`, whose
/// panics read as the test's own words.
async fn rollback_file(server: &FsServer, dir: &TestDir, commit_id: String) -> CallToolResult {
    server
        .rollback_file_impl(RollbackParams {
            commit_id,
            repo_path: path_string(dir.path()),
        })
        .await
        .expect("rollback_file_impl runs")
}

/// HEAD and the staged entries are exactly what they were before.
fn assert_git_state_unchanged(dir: &TestDir, before: &GitState) {
    let after = git_state(dir);
    assert_eq!(after.head, before.head);
    assert_eq!(after.staged, before.staged);
}

/// A summary line's short after id restores the written file and nothing else.
///
/// This is the entry-point shape of issue #2929: under the old semantics the
/// file-precise snapshot made the rollback delete `other.txt` and
/// `untracked.txt`, drop the staged entry and hard-reset the branch.
#[tokio::test]
async fn the_short_id_on_a_summary_line_restores_only_the_snapshots_paths() {
    let dir = committed_two_file_repo();
    let server = make_server(dir.path());
    let tracked = dir.path().join("tracked.txt");
    let (_, after) = summary_ids_of_a_write(&server, &tracked).await;
    assert_eq!(read(&dir, "tracked.txt"), "restored\n");

    // Later edits the snapshot never saw, so a restoration is visible.
    std::fs::write(&tracked, "later\n").unwrap();
    std::fs::write(dir.path().join("other.txt"), "edited later\n").unwrap();
    let before_state = git_state(&dir);

    let result = rollback_file(&server, &dir, after).await;

    assert_eq!(result.is_error, Some(false));
    let text = text_content(&result);
    assert!(text.contains("harnx snapshot"), "{text}");
    assert!(text.contains("after-snapshot"), "{text}");
    assert!(!text.contains("new commit"), "{text}");

    // The snapshot's own path is really restored, not left as it was.
    assert_eq!(read(&dir, "tracked.txt"), "restored\n");
    // Everything the snapshot did not record survives.
    assert_eq!(read(&dir, "other.txt"), "edited later\n");
    assert_eq!(read(&dir, "untracked.txt"), "untracked\n");
    assert_eq!(read(&dir, "staged.txt"), "staged\n");
    assert_git_state_unchanged(&dir, &before_state);
}

/// A destination the preflight refuses is a recoverable tool error, not a
/// protocol error, and neither the conflicting path nor the git state moves.
#[tokio::test]
async fn a_conflicting_destination_is_refused_without_touching_anything() {
    let dir = committed_two_file_repo();
    let server = make_server(dir.path());
    // The tool only reports a summary for a file it can snapshot before the
    // write, so the victim exists first.
    let victim = dir.path().join("victim.txt");
    write_fixture(&dir, "victim.txt", "before the snapshot\n");
    let (_, after) = summary_ids_of_a_write(&server, &victim).await;

    // Replace the snapshot's destination with a directory holding a file.
    std::fs::remove_file(&victim).unwrap();
    std::fs::create_dir(&victim).unwrap();
    std::fs::write(victim.join("keep.txt"), "keep\n").unwrap();
    std::fs::write(dir.path().join("other.txt"), "edited later\n").unwrap();
    let before_state = git_state(&dir);

    let result = rollback_file(&server, &dir, after).await;

    assert_eq!(result.is_error, Some(true));
    let text = text_content(&result);
    assert!(
        text.contains("refusing to restore victim.txt: destination is a directory"),
        "{text}"
    );

    // The conflicting directory keeps its contents.
    assert!(victim.is_dir());
    assert_eq!(
        std::fs::read_to_string(victim.join("keep.txt")).unwrap(),
        "keep\n"
    );
    assert_eq!(read(&dir, "tracked.txt"), "tracked\n");
    assert_eq!(read(&dir, "other.txt"), "edited later\n");
    assert_eq!(read(&dir, "untracked.txt"), "untracked\n");
    assert_git_state_unchanged(&dir, &before_state);
}
