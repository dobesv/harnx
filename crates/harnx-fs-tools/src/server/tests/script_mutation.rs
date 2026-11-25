//! The `diff` option of the write tools and their script-style results.

use super::*;
use crate::script_result::ResultStyle;
use harnx_git_history::DiffLevel;

const LEVELS: [Option<DiffLevel>; 4] = [
    Some(DiffLevel::None),
    Some(DiffLevel::Summary),
    Some(DiffLevel::Files),
    Some(DiffLevel::Full),
];

/// A repository holding `tracked.txt` with `contents`, committed.
fn committed_repo(contents: &str) -> TestDir {
    let dir = TestDir::new();
    init_git_repo(dir.path());
    git(dir.path(), &["config", "commit.gpgsign", "false"]);
    write_fixture(&dir, "tracked.txt", contents);
    git(dir.path(), &["add", "tracked.txt"]);
    git(dir.path(), &["commit", "-q", "-m", "init"]);
    dir
}

fn rev_parse_head(dir: &Path) -> String {
    let output = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(dir)
        .output()
        .unwrap();
    String::from_utf8(output.stdout).unwrap()
}

/// The summary line a script-style `write` of `new value` reports.
async fn summary_line_of_a_write(server: &FsServer, tracked: &Path) -> String {
    let result = call_write_tool(
        server,
        "write",
        &path_string(tracked),
        Some(DiffLevel::Summary),
        ResultStyle::Script,
    )
    .await;
    let fields = fields(&result);
    diff_of(&fields).unwrap().trim_end().to_owned()
}

/// The undo the docs teach, run from `cwd`: the summary line's command with
/// `--binary` after `diff`, piped into `git -C <repo> apply -R`.
fn run_undo(line: &str, repo: &Path, cwd: &Path, binary: bool) -> std::process::Output {
    let command = line.split_once(": ").expect("colon").1;
    let command = if binary {
        command.replacen(" diff ", " diff --binary ", 1)
    } else {
        command.to_owned()
    };
    std::process::Command::new("sh")
        .arg("-c")
        .arg(format!(
            "{command} | git -C '{}' apply -R",
            path_string(repo)
        ))
        .current_dir(cwd)
        .output()
        .unwrap()
}

/// A text file's contents with CRLF folded to LF, since a git apply may
/// rewrite line endings on Windows.
fn read_lf(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap().replace("\r\n", "\n")
}

fn fields(result: &CallToolResult) -> Value {
    result
        .structured_content
        .clone()
        .expect("script-style result has structured content")
}

fn diff_of(fields: &Value) -> Option<&str> {
    fields.get("diff").and_then(Value::as_str)
}

fn level_name(level: Option<DiffLevel>) -> Value {
    level.map_or(Value::Null, |level| json!(level.as_str()))
}

/// One call of each write tool against `tracked.txt`, rewriting `old` to `new`.
async fn call_write_tool(
    server: &FsServer,
    tool: &str,
    path: &str,
    level: Option<DiffLevel>,
    style: ResultStyle,
) -> CallToolResult {
    let diff = level_name(level);
    let args = match tool {
        "write" => json!({"path": path, "content": "new value\n", "diff": diff}),
        "edit" => {
            json!({"path": path, "old_text": "old value", "new_text": "new value", "diff": diff})
        }
        "insert" => json!({"path": path, "insert_line": 0, "insert_text": "top\n", "diff": diff}),
        "re_replace" => json!({"path": path, "pattern": "old", "replacement": "new", "diff": diff}),
        other => panic!("not a write tool: {other}"),
    };
    server.invoke_tool_value(tool, args, style).await.unwrap()
}

#[tokio::test]
async fn a_script_write_prints_one_line_and_a_summary() {
    let dir = committed_repo("old value\n");
    let path = path_string(&dir.path().join("tracked.txt"));
    let result = make_server(dir.path())
        .invoke_tool_value(
            "write",
            json!({"path": path, "content": "abcde\n"}),
            ResultStyle::Script,
        )
        .await
        .unwrap();

    let fields = fields(&result);
    assert_eq!(
        fields["message"],
        format!("Wrote {} (1 lines) to {path}\n", format_size(6))
    );
    let diff = diff_of(&fields).expect("a summary diff");
    assert_eq!(diff.lines().count(), 1, "{diff}");
    assert!(diff.starts_with("1 file changed"), "{diff}");
    assert!(diff.contains("git -C "), "{diff}");
}

#[tokio::test]
async fn a_script_write_reports_each_level() {
    for tool in ["write", "edit", "insert", "re_replace"] {
        for level in LEVELS {
            let dir = committed_repo("old value\n");
            let path = path_string(&dir.path().join("tracked.txt"));
            let server = make_server(dir.path());
            let result = call_write_tool(&server, tool, &path, level, ResultStyle::Script).await;
            let fields = fields(&result);
            let label = format!("{tool} at {level:?}");

            assert!(
                fields["message"].as_str().unwrap().ends_with('\n'),
                "{label}"
            );
            let diff = diff_of(&fields);
            match level {
                Some(DiffLevel::None) => assert_eq!(diff, None, "{label}"),
                Some(DiffLevel::Files) => {
                    let diff = diff.expect(&label);
                    assert!(diff.contains("tracked.txt"), "{label}: {diff}");
                    assert!(diff.lines().count() >= 2, "{label}: {diff}");
                }
                Some(DiffLevel::Full) => {
                    let diff = diff.expect(&label);
                    assert!(diff.starts_with("commit "), "{label}: {diff}");
                    assert!(diff.contains("```diff"), "{label}: {diff}");
                }
                _ => assert_eq!(diff.expect(&label).lines().count(), 1, "{label}"),
            }
        }
    }
}

#[tokio::test]
async fn the_short_id_on_a_summary_line_rolls_the_file_back() {
    let dir = committed_repo("old value\n");
    let tracked = dir.path().join("tracked.txt");
    let server = make_server(dir.path());
    let line = summary_line_of_a_write(&server, &tracked).await;
    let line = line.as_str();
    let after = line.rsplit(' ').next().unwrap();
    let before = line.rsplit(' ').nth(1).unwrap();
    assert_eq!((before.len(), after.len()), (12, 12), "{line}");

    server
        .rollback_file_impl(RollbackParams {
            commit_id: before.to_string(),
            repo_path: path_string(dir.path()),
        })
        .await
        .expect("a short id rolls back");
    assert_eq!(std::fs::read_to_string(&tracked).unwrap(), "old value\n");
}

#[tokio::test]
async fn reverse_applying_a_summary_lines_diff_undoes_the_write() {
    let dir = committed_repo("old value\n");
    write_fixture(&dir, "other.txt", "other\n");
    git(dir.path(), &["add", "other.txt"]);
    git(dir.path(), &["commit", "-q", "-m", "second file"]);
    let head = rev_parse_head(dir.path());
    let tracked = dir.path().join("tracked.txt");
    let server = make_server(dir.path());
    let line = summary_line_of_a_write(&server, &tracked).await;
    let line = line.as_str();
    assert_eq!(
        std::fs::read_to_string(&tracked).unwrap(),
        "new value\n",
        "{line}"
    );

    let outside = TestDir::new();
    let undo = run_undo(line, dir.path(), outside.path(), true);
    assert!(
        undo.status.success(),
        "{}",
        String::from_utf8_lossy(&undo.stderr)
    );

    assert_eq!(read_lf(&tracked), "old value\n");
    assert_eq!(read_lf(&dir.path().join("other.txt")), "other\n");
    assert_eq!(rev_parse_head(dir.path()), head);
}

#[tokio::test]
async fn a_script_binary_write_is_summarized_and_at_full_says_the_diff_is_omitted() {
    let dir = committed_repo("old value\n");
    let tracked = dir.path().join("tracked.txt");
    let server = make_server(dir.path());
    let bytes = b"\x89PNG\x00\xff";

    let summary = server
        .write_bytes_at(
            &path_string(&tracked),
            bytes,
            Some(DiffLevel::Summary),
            ResultStyle::Script,
        )
        .await
        .unwrap();
    let fields = fields(&summary);
    assert_eq!(diff_of(&fields).unwrap().lines().count(), 1);
    assert!(!diff_of(&fields).unwrap().contains("Binary"));

    std::fs::write(&tracked, "old value\n").unwrap();
    let full = server
        .write_bytes_at(
            &path_string(&tracked),
            bytes,
            Some(DiffLevel::Full),
            ResultStyle::Script,
        )
        .await
        .unwrap();
    let fields = self::fields(&full);
    let diff = diff_of(&fields).unwrap();
    assert!(diff.starts_with("commit "), "{diff}");
    assert!(
        diff.contains("Binary file changed (diff omitted)."),
        "{diff}"
    );
}

#[tokio::test]
async fn a_script_write_outside_a_repository_has_no_diff_field() {
    for tool in ["write", "edit", "insert", "re_replace"] {
        for level in LEVELS {
            let dir = TestDir::new();
            write_fixture(&dir, "tracked.txt", "old value\n");
            let path = path_string(&dir.path().join("tracked.txt"));
            let server = make_server(dir.path());
            let result = call_write_tool(&server, tool, &path, level, ResultStyle::Script).await;
            let fields = fields(&result);
            assert!(
                fields.get("diff").is_none(),
                "{tool} at {level:?}: {fields}"
            );
        }
    }
}

#[tokio::test]
async fn a_direct_write_reports_the_level_it_was_asked_for() {
    let table = [
        (Some(DiffLevel::None), 1, None),
        (Some(DiffLevel::Summary), 2, Some("1 file changed")),
        (Some(DiffLevel::Files), 2, Some("tracked.txt")),
        (Some(DiffLevel::Full), 2, Some("```diff")),
    ];
    for (level, blocks, expect) in table {
        let dir = committed_repo("old value\n");
        let path = path_string(&dir.path().join("tracked.txt"));
        let server = make_server(dir.path());
        let result = call_write_tool(&server, "edit", &path, level, ResultStyle::Model).await;
        assert_eq!(result.content.len(), blocks, "{level:?}");
        assert!(result.structured_content.is_none());
        if let Some(expect) = expect {
            let diff = &result.content[1].as_text().unwrap().text;
            assert!(diff.contains(expect), "{level:?}: {diff}");
        }
    }
}

#[tokio::test]
async fn an_invalid_diff_value_names_the_four_levels() {
    let dir = committed_repo("old value\n");
    let path = path_string(&dir.path().join("tracked.txt"));
    let error = make_server(dir.path())
        .invoke_tool_value(
            "write",
            json!({"path": path, "content": "x", "diff": "loud"}),
            ResultStyle::Model,
        )
        .await
        .unwrap_err();
    for name in DiffLevel::NAMES {
        assert!(error.message.contains(name), "{}", error.message);
    }
}

#[tokio::test]
async fn a_write_at_none_takes_no_snapshot() {
    let dir = committed_repo("old value\n");
    let tracked = dir.path().join("tracked.txt");
    let path = path_string(&tracked);
    let server = make_server(dir.path());
    let objects_before = object_count(dir.path());

    call_write_tool(
        &server,
        "write",
        &path,
        Some(DiffLevel::None),
        ResultStyle::Script,
    )
    .await;
    assert_eq!(
        object_count(dir.path()),
        objects_before,
        "none wrote objects"
    );

    // The next write at full shows only its own change.
    let result = server
        .invoke_tool_value(
            "edit",
            json!({"path": path, "old_text": "new value", "new_text": "newer value", "diff": "full"}),
            ResultStyle::Model,
        )
        .await
        .unwrap();
    let diff = &result.content[1].as_text().unwrap().text;
    assert!(
        diff.contains("-new value") && diff.contains("+newer value"),
        "{diff}"
    );
    assert!(!diff.contains("old value"), "{diff}");
}

/// Objects in the repository, counting loose and packed ones.
fn object_count(dir: &Path) -> usize {
    let out = std::process::Command::new("git")
        .args(["count-objects", "-v"])
        .current_dir(dir)
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|line| line.starts_with("count:") || line.starts_with("in-pack:"))
        .filter_map(|line| line.split_whitespace().nth(1)?.parse::<usize>().ok())
        .sum()
}

#[tokio::test]
async fn a_binary_change_is_undone_with_a_binary_patch() {
    let dir = TestDir::new();
    init_git_repo(dir.path());
    git(dir.path(), &["config", "commit.gpgsign", "false"]);
    let original: &[u8] = b"\x00\x01original\xff\x00";
    let binary = dir.path().join("blob.bin");
    std::fs::write(&binary, original).unwrap();
    git(dir.path(), &["add", "blob.bin"]);
    git(dir.path(), &["commit", "-q", "-m", "init"]);
    let server = make_server(dir.path());
    let written = server
        .write_bytes_at(
            &path_string(&binary),
            b"\x89PNG\x00changed\xfe",
            Some(DiffLevel::Summary),
            ResultStyle::Script,
        )
        .await
        .unwrap();
    let line = diff_of(&fields(&written)).unwrap().trim_end().to_owned();
    let outside = TestDir::new();

    let plain = run_undo(&line, dir.path(), outside.path(), false);
    assert!(
        !plain.status.success(),
        "a diff without --binary applied: {line}"
    );
    assert_ne!(std::fs::read(&binary).unwrap(), original);

    let undo = run_undo(&line, dir.path(), outside.path(), true);
    assert!(
        undo.status.success(),
        "{}",
        String::from_utf8_lossy(&undo.stderr)
    );
    assert_eq!(std::fs::read(&binary).unwrap(), original);
}
