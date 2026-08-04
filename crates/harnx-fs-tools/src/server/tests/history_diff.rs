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
/// limitation in `harnx-mcp-history`, not something this PR
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
