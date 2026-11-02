use super::*;

/// Initialize `dir` as a git repo with `tracked.txt` committed.
/// Returns `false` if git isn't available so callers can skip — every
/// platform we ship to has git, but local devs may not.
#[cfg(unix)]
fn seed_committed_file(dir: &Path, name: &str, contents: &str) -> bool {
    let run = |args: &[&str]| -> bool {
        std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    };
    if !run(&["init", "-q"]) {
        return false;
    }
    run(&["config", "user.name", "Test"]);
    run(&["config", "user.email", "test@example.com"]);
    run(&["config", "commit.gpgsign", "false"]);
    std::fs::write(dir.join(name), contents).expect("write seed file");
    run(&["add", name]) && run(&["commit", "-q", "-m", "init"])
}

/// In a git-tracked working directory, edit_file should append the
/// snapshot diff as a second content block with no `audience`
/// annotation so the MCP client surfaces it to the user. Regression
/// for issue #398.
///
/// Gated to Unix because `std::env::temp_dir()` on Windows runners
/// yields an 8.3 short-name path (`C:\\Users\\RUNNER~1\\...`); the
/// canonicalize-then-`gix::open` flow inside `HistoryManager::new`
/// then fails to register the repo, leaving the production code
/// without a base to diff against. That's a pre-existing Windows
/// limitation in `harnx-git-history`, not something this PR
/// introduces — the meta-shape regression test
/// (`fs_tools_advertise_call_template_only`) still runs everywhere.
#[cfg(unix)]
#[tokio::test]
async fn edit_file_emits_unaudienced_diff_content() {
    let temp_dir = TestDir::new();
    let dir = temp_dir.path();
    if !seed_committed_file(dir, "tracked.txt", "old value\n") {
        return;
    }

    let result = make_server(dir)
        .edit_file_impl(EditFileParams {
            path: path_string(&dir.join("tracked.txt")),
            old_text: "old value".into(),
            new_text: "new value".into(),
            replace_all: None,
        })
        .await
        .expect("edit succeeds");

    assert_eq!(result.is_error, Some(false));
    let texts: Vec<&str> = result
        .content
        .iter()
        .filter_map(|c| c.as_text().map(|t| t.text.as_str()))
        .collect();
    assert!(
        texts.len() >= 2,
        "expected summary + diff content blocks, got {}: {texts:?}",
        texts.len()
    );
    assert!(texts[0].contains("Edited"), "summary missing: {texts:?}");
    assert!(texts[1].contains("-old value"), "diff missing: {texts:?}");
    // The diff/summary must not be assistant-only — that would hide
    // them from the MCP client's audience-aware generic renderer.
    assert!(audience(&result.content[0]).is_none(), "summary audience");
    assert!(audience(&result.content[1]).is_none(), "diff audience");
}

#[tokio::test]
async fn write_bytes_emits_unchanged_text_diff() {
    let dir = TestDir::new();
    if !seed_committed_file(dir.path(), "tracked.txt", "old value\n") {
        return;
    }
    let path = dir.path().join("tracked.txt");

    let result = make_server(dir.path())
        .write_bytes_impl(&path_string(&path), b"new value\n")
        .await
        .unwrap();

    assert_eq!(result.is_error, Some(false));
    assert_eq!(
        text_content(&result),
        format!("Wrote 10B (1 lines) to {}", path.display())
    );
    assert_eq!(result.content.len(), 2);
    let diff = &result.content[1].as_text().unwrap().text;
    assert!(diff.starts_with("commit "));
    assert!(diff.contains("```diff\n"));
    assert!(diff.contains("-old value"));
    assert!(diff.contains("+new value"));
    assert!(audience(&result.content[1]).is_none());
}

#[tokio::test]
async fn write_bytes_snapshots_binary_without_rendering_payload() {
    let dir = TestDir::new();
    if !seed_committed_file(dir.path(), "tracked.png", "original\n") {
        return;
    }
    let path = dir.path().join("tracked.png");
    let server = make_server(dir.path());

    // Exercise both git's text-like and NUL-detected binary cases.
    for bytes in [b"\x89PNG\r\n\x1a\n\xff\xff".as_slice(), b"\x89PNG\x00\xff"] {
        let previous = std::fs::read(&path).unwrap();
        let result = server
            .write_bytes_impl(&path_string(&path), bytes)
            .await
            .unwrap();

        assert_eq!(result.is_error, Some(false));
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        write_ops::assert_binary_output_is_safe(&result);
        assert_eq!(result.content.len(), 2);
        let summary = &result.content[1].as_text().unwrap().text;
        assert!(summary.contains("Binary file changed (diff omitted)."));
        assert!(audience(&result.content[1]).is_none());
        let commit = summary
            .lines()
            .next()
            .unwrap()
            .strip_prefix("commit ")
            .unwrap();
        assert_eq!(snapshot_bytes(dir.path(), commit, "tracked.png"), bytes);
        assert_eq!(
            snapshot_bytes(dir.path(), &format!("{commit}^"), "tracked.png"),
            previous
        );
    }
}

fn snapshot_bytes(dir: &Path, commit: &str, path: &str) -> Vec<u8> {
    let output = std::process::Command::new("git")
        .args(["show", &format!("{commit}:{path}")])
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(output.status.success(), "snapshot blob should be readable");
    output.stdout
}
