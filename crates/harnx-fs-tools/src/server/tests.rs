use super::*;

use harnx_mcp::content::audience;
use rmcp::handler::client::ClientHandler;
use rmcp::model::{CallToolRequestParams, ClientCapabilities, InitializeRequestParams};
use rmcp::service::{serve_client, serve_server, RoleClient, RoleServer, RunningService};
use std::path::Path;
use tokio::io::duplex;
use uuid::Uuid;

struct TestDir {
    path: PathBuf,
}

impl TestDir {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("harnx-fs-tools-test-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&path).unwrap();
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

#[derive(Clone, Default)]
struct TestClientHandler;

impl ClientHandler for TestClientHandler {
    fn get_info(&self) -> InitializeRequestParams {
        InitializeRequestParams::new(
            ClientCapabilities::builder().build(),
            Implementation::new("test", "0.1"),
        )
    }
}

struct TestConnection {
    _server_service: RunningService<RoleServer, FsServer>,
    client_service: RunningService<RoleClient, TestClientHandler>,
}

async fn connect_server(server: FsServer, _roots: Vec<PathBuf>) -> TestConnection {
    let (client_transport, server_transport) = duplex(65_536);
    let server_fut = serve_server(server, server_transport);
    let client_fut = serve_client(TestClientHandler, client_transport);
    let (server_res, client_res) = tokio::join!(server_fut, client_fut);
    TestConnection {
        _server_service: server_res.unwrap(),
        client_service: client_res.unwrap(),
    }
}

fn text_content(result: &CallToolResult) -> String {
    result
        .content
        .iter()
        .find_map(|content| content.as_text().map(|text| text.text.clone()))
        .unwrap()
}

fn rwx_allowlist(paths: impl IntoIterator<Item = PathBuf>) -> ResolvedAllowlist {
    let mut allowlist = ResolvedAllowlist::new();
    for path in paths {
        allowlist.insert_rwx(path);
    }
    allowlist
}

fn make_server(dir: &Path) -> FsServer {
    FsServer::new(rwx_allowlist([dir.to_path_buf()]))
}

fn write_fixture(dir: &TestDir, name: &str, contents: impl AsRef<[u8]>) -> PathBuf {
    let path = dir.path().join(name);
    std::fs::write(&path, contents).unwrap();
    path
}

fn fixture_path(dir: &TestDir, name: &str) -> PathBuf {
    dir.path().join(name)
}

pub(crate) fn path_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn git(dir: &Path, args: &[&str]) {
    let status = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .status()
        .unwrap();
    assert!(status.success(), "git {:?} failed with {status}", args);
}

fn init_git_repo(dir: &Path) {
    git(dir, &["init"]);
    git(dir, &["config", "user.name", "harnx test"]);
    git(dir, &["config", "user.email", "harnx@example.com"]);
}

fn user_summary(result: &CallToolResult) -> String {
    result
        .content
        .iter()
        .filter(|content| {
            audience(content)
                .map(|a| a.contains(&Role::User))
                .unwrap_or(false)
        })
        .find_map(|content| content.as_text().map(|text| text.text.clone()))
        .unwrap_or_default()
}

fn tool_args(value: Value) -> Map<String, Value> {
    value.as_object().unwrap().clone()
}

#[tokio::test]
async fn fs_tools_advertise_call_template_only() {
    // Each tool ships a `_meta.call_template` for the TUI's call header.
    // We deliberately omit `result_template` so the MCP client falls
    // back to its audience-aware generic renderer — that's what surfaces
    // the history diff content blocks (issue #398).
    let temp_dir = TestDir::new();
    let TestConnection {
        _server_service,
        client_service,
    } = connect_server(
        make_server(temp_dir.path()),
        vec![temp_dir.path().to_path_buf()],
    )
    .await;
    let peer = client_service.peer().clone();
    let _client_task = tokio::spawn(async move {
        let _ = client_service.waiting().await;
    });

    let tools = peer.list_tools(Default::default()).await.unwrap().tools;
    assert!(!tools.is_empty(), "server should expose at least one tool");
    for tool in &tools {
        let meta = tool
            .meta
            .as_ref()
            .unwrap_or_else(|| panic!("tool '{}' has no _meta", tool.name));
        assert!(
            meta.0.contains_key("call_template"),
            "tool '{}' missing call_template in _meta",
            tool.name
        );
        assert!(
            !meta.0.contains_key("result_template"),
            "tool '{}' must not pin result_template — let the client fall back to its generic audience-aware renderer",
            tool.name
        );
    }
}

#[tokio::test]
async fn test_fs_server_list_tools() {
    let temp_dir = TestDir::new();
    let TestConnection {
        _server_service,
        client_service,
    } = connect_server(
        make_server(temp_dir.path()),
        vec![temp_dir.path().to_path_buf()],
    )
    .await;
    let peer = client_service.peer().clone();
    let _client_task = tokio::spawn(async move {
        let _ = client_service.waiting().await;
    });

    let tools = peer.list_tools(Default::default()).await.unwrap();
    let names = tools
        .tools
        .iter()
        .map(|tool| tool.name.to_string())
        .collect::<Vec<_>>();

    assert_eq!(
        names,
        vec![
            "read",
            "write",
            "edit",
            "insert",
            "re_replace",
            "ls",
            "grep",
            "find",
            "rollback_file"
        ]
    );

    // Verify read tool advertises image support in its description
    let read_tool = tools
        .tools
        .iter()
        .find(|t| t.name == "read")
        .expect("read tool should exist");
    let desc = read_tool
        .description
        .as_ref()
        .expect("read tool should have description");
    assert!(
        desc.to_lowercase().contains("image"),
        "read tool description should mention image support"
    );
}

#[tokio::test]
async fn test_fs_server_read_file() {
    let temp_dir = TestDir::new();
    let file_path = write_fixture(&temp_dir, "notes.txt", "alpha\nbeta\n");

    let TestConnection {
        _server_service,
        client_service,
    } = connect_server(
        make_server(temp_dir.path()),
        vec![temp_dir.path().to_path_buf()],
    )
    .await;
    let peer = client_service.peer().clone();
    let _client_task = tokio::spawn(async move {
        let _ = client_service.waiting().await;
    });

    let result = peer
        .call_tool(CallToolRequestParams::new("read").with_arguments(tool_args(
            serde_json::json!({
                "path": path_string(&file_path)
            }),
        )))
        .await
        .unwrap();

    let text = text_content(&result);
    assert_eq!(result.is_error, Some(false));
    assert!(text.contains("1: alpha"));
    assert!(text.contains("2: beta"));
}

#[tokio::test]
async fn test_fs_server_write_and_read() {
    let temp_dir = TestDir::new();
    let file_path = fixture_path(&temp_dir, "written.txt");

    let TestConnection {
        _server_service,
        client_service,
    } = connect_server(
        make_server(temp_dir.path()),
        vec![temp_dir.path().to_path_buf()],
    )
    .await;
    let peer = client_service.peer().clone();
    let _client_task = tokio::spawn(async move {
        let _ = client_service.waiting().await;
    });

    let write_result = peer
        .call_tool(
            CallToolRequestParams::new("write").with_arguments(tool_args(serde_json::json!({
                "path": path_string(&file_path),
                "content": "hello\nworld\n"
            }))),
        )
        .await
        .unwrap();
    assert_eq!(write_result.is_error, Some(false));

    let read_result = peer
        .call_tool(CallToolRequestParams::new("read").with_arguments(tool_args(
            serde_json::json!({
                "path": path_string(&file_path)
            }),
        )))
        .await
        .unwrap();

    let text = text_content(&read_result);
    assert!(text.contains("1: hello"));
    assert!(text.contains("2: world"));
}

#[tokio::test]
async fn test_fs_server_edit_file() {
    let temp_dir = TestDir::new();
    let file_path = write_fixture(&temp_dir, "edit.txt", "old value\n");

    let TestConnection {
        _server_service,
        client_service,
    } = connect_server(
        make_server(temp_dir.path()),
        vec![temp_dir.path().to_path_buf()],
    )
    .await;
    let peer = client_service.peer().clone();
    let _client_task = tokio::spawn(async move {
        let _ = client_service.waiting().await;
    });

    let edit_result = peer
        .call_tool(CallToolRequestParams::new("edit").with_arguments(tool_args(
            serde_json::json!({
                "path": path_string(&file_path),
                "old_text": "old value",
                "new_text": "new value"
            }),
        )))
        .await
        .unwrap();
    assert_eq!(edit_result.is_error, Some(false));
    assert_eq!(std::fs::read_to_string(&file_path).unwrap(), "new value\n");
}

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

#[cfg(unix)]
#[tokio::test]
async fn read_only_allowlist_reads_but_denies_writes() {
    let dir = TestDir::new();
    let file = dir.path().join("read-only.txt");
    std::fs::write(&file, "original").unwrap();
    let mut allowlist = ResolvedAllowlist::new();
    allowlist.insert_read(dir.path());
    let server = FsServer::new(allowlist);

    server
        .read_file_impl(ReadFileParams {
            path: path_string(&file),
            offset: None,
            limit: None,
            tail: None,
            grep: None,
            head_lines: None,
            tail_lines: None,
            max_output_bytes: None,
        })
        .await
        .expect("read grant permits reads");

    let denied = server
        .write_file_impl(WriteFileParams {
            path: path_string(&file),
            content: "changed".into(),
        })
        .await
        .unwrap_err();
    assert!(denied.message.contains("filesystem writes are denied"));
}

#[tokio::test]
async fn rollback_requires_write_allowlist() {
    let dir = TestDir::new();
    let mut allowlist = ResolvedAllowlist::new();
    allowlist.insert_read(dir.path());
    let server = FsServer::new(allowlist);

    let denied = server
        .rollback_file_impl(RollbackParams {
            commit_id: "not-reached".into(),
            repo_path: path_string(dir.path()),
        })
        .await
        .unwrap_err();
    assert!(denied.message.contains("filesystem writes are denied"));
}

#[tokio::test]
async fn rwx_allowlist_reads_and_writes() {
    let dir = TestDir::new();
    let file = dir.path().join("writable.txt");
    std::fs::write(&file, "original").unwrap();
    let server = FsServer::new(rwx_allowlist([dir.path().to_path_buf()]));

    server
        .write_file_impl(WriteFileParams {
            path: path_string(&file),
            content: "changed".into(),
        })
        .await
        .expect("rwx grant permits writes");
    let result = server
        .read_file_impl(ReadFileParams {
            path: path_string(&file),
            offset: None,
            limit: None,
            tail: None,
            grep: None,
            head_lines: None,
            tail_lines: None,
            max_output_bytes: None,
        })
        .await
        .expect("rwx grant permits reads");
    assert!(text_content(&result).contains("changed"));
}

#[tokio::test]
async fn empty_allowlist_denies_default_search_path() {
    let server = FsServer::new(ResolvedAllowlist::new());
    let denied = server
        .find_files_impl(FindFilesParams {
            pattern: "**/*".into(),
            path: None,
            max_results: None,
        })
        .await
        .unwrap_err();
    assert!(denied
        .message
        .contains("No readable directories configured"));
}

#[tokio::test]
async fn test_read_file_with_offset_limit() {
    let temp_dir = TestDir::new();
    let file_path = temp_dir.path().join("offset.txt");
    std::fs::write(&file_path, "one\ntwo\nthree\nfour\n").unwrap();
    let server = make_server(temp_dir.path());

    let result = server
        .read_file_impl(ReadFileParams {
            path: path_string(&file_path),
            offset: Some(2),
            limit: Some(2),
            tail: None,
            grep: None,
            head_lines: None,
            tail_lines: None,
            max_output_bytes: None,
        })
        .await
        .unwrap();

    let text = text_content(&result);
    assert!(text.contains("2: two"));
    assert!(text.contains("3: three"));
    assert!(text.contains("Use offset=4 to continue"));
}

#[tokio::test]
async fn test_read_file_with_grep() {
    let temp_dir = TestDir::new();
    let file_path = temp_dir.path().join("grep.txt");
    std::fs::write(&file_path, "alpha\nmatch-one\nbeta\nmatch-two\n").unwrap();
    let server = make_server(temp_dir.path());

    let result = server
        .read_file_impl(ReadFileParams {
            path: path_string(&file_path),
            offset: None,
            limit: None,
            tail: None,
            grep: Some("match".to_string()),
            head_lines: None,
            tail_lines: None,
            max_output_bytes: None,
        })
        .await
        .unwrap();

    let text = text_content(&result);
    assert!(text.contains("2: match-one"));
    assert!(text.contains("4: match-two"));
    assert!(!text.contains("1: alpha"));
}

#[tokio::test]
async fn test_read_file_with_tail() {
    let temp_dir = TestDir::new();
    let file_path = temp_dir.path().join("tail.txt");
    std::fs::write(&file_path, "one\ntwo\nthree\nfour\n").unwrap();
    let server = make_server(temp_dir.path());

    let result = server
        .read_file_impl(ReadFileParams {
            path: path_string(&file_path),
            offset: None,
            limit: None,
            tail: Some(2),
            grep: None,
            head_lines: None,
            tail_lines: None,
            max_output_bytes: None,
        })
        .await
        .unwrap();

    let text = text_content(&result);
    assert!(text.contains("3: three"));
    assert!(text.contains("4: four"));
    assert!(text.contains("showing last 2 of 4 matching lines"));
}

/// Read `content` with combined `offset`+`tail` and return the rendered text.
async fn read_offset_tail(content: &str, offset: usize, tail: usize) -> String {
    let temp_dir = TestDir::new();
    let file_path = temp_dir.path().join("offset_tail.txt");
    std::fs::write(&file_path, content).unwrap();
    let server = make_server(temp_dir.path());

    let result = server
        .read_file_impl(ReadFileParams {
            path: path_string(&file_path),
            offset: Some(offset),
            limit: None,
            tail: Some(tail),
            grep: None,
            head_lines: None,
            tail_lines: None,
            max_output_bytes: None,
        })
        .await
        .unwrap();

    text_content(&result)
}

#[tokio::test]
async fn test_read_file_offset_and_tail_combinations() {
    let six = "one\ntwo\nthree\nfour\nfive\nsix\n";

    // Skip to line 3, then tail the last 2 lines of the remaining window
    // (lines 3..6) → lines 5 and 6. Tail is anchored to the end, so no
    // forward "more matching lines" notice.
    let text = read_offset_tail(six, 3, 2).await;
    for expect in ["5: five", "6: six", "showing last 2 of 4 matching lines"] {
        assert!(text.contains(expect), "expected {expect:?} in: {text}");
    }
    for absent in ["4: four", "more matching lines"] {
        assert!(!text.contains(absent), "unexpected {absent:?} in: {text}");
    }

    // tail == window_len (offset=3 leaves a 4-line window): whole window, no
    // "showing last" notice.
    let text = read_offset_tail(six, 3, 4).await;
    assert_window_without_notice(&text);

    // tail > window_len (offset=3 leaves a 2-line window on a 4-line file):
    // whole window, no notice.
    let text = read_offset_tail("one\ntwo\nthree\nfour\n", 3, 5).await;
    assert_window_without_notice(&text);
}

/// Asserts a post-offset window starting at line 3 was returned in full with
/// no truncation notice.
fn assert_window_without_notice(text: &str) {
    assert!(text.contains("3: three"), "got: {text}");
    assert!(!text.contains("2: two"), "got: {text}");
    assert!(!text.contains("showing last"), "got: {text}");
}

#[tokio::test]
async fn test_read_file_offset_beyond_eof_with_tail_errors() {
    let temp_dir = TestDir::new();
    let file_path = temp_dir.path().join("offset_tail_eof.txt");
    std::fs::write(&file_path, "one\ntwo\nthree\n").unwrap();
    let server = make_server(temp_dir.path());

    // offset one past EOF (total=3, offset=4) with tail must error, matching
    // the non-tail path.
    let result = server
        .read_file_impl(ReadFileParams {
            path: path_string(&file_path),
            offset: Some(4),
            limit: None,
            tail: Some(2),
            grep: None,
            head_lines: None,
            tail_lines: None,
            max_output_bytes: None,
        })
        .await;

    let message = result.expect_err("expected error").message;
    assert!(
        message.contains("beyond end of result set"),
        "got: {message}"
    );
}

#[tokio::test]
async fn test_read_file_offset_zero_rejected() {
    let temp_dir = TestDir::new();
    let file_path = temp_dir.path().join("offset_zero.txt");
    std::fs::write(&file_path, "one\ntwo\n").unwrap();
    let server = make_server(temp_dir.path());

    // Explicit offset=0 is invalid (offset is 1-indexed), matching
    // read_exec_log's contract rather than silently coercing to 1.
    let result = server
        .read_file_impl(ReadFileParams {
            path: path_string(&file_path),
            offset: Some(0),
            limit: None,
            tail: None,
            grep: None,
            head_lines: None,
            tail_lines: None,
            max_output_bytes: None,
        })
        .await;

    let message = result.expect_err("expected error").message;
    assert!(message.contains("offset must be >= 1"), "got: {message}");
}

#[tokio::test]
async fn test_read_file_binary_detection() {
    let temp_dir = TestDir::new();
    let file_path = temp_dir.path().join("binary.bin");
    std::fs::write(&file_path, b"hello\0world").unwrap();
    let server = make_server(temp_dir.path());

    let result = server
        .read_file_impl(ReadFileParams {
            path: path_string(&file_path),
            offset: None,
            limit: None,
            tail: None,
            grep: None,
            head_lines: None,
            tail_lines: None,
            max_output_bytes: None,
        })
        .await
        .unwrap();

    assert_eq!(result.is_error, Some(true));
    assert!(text_content(&result).contains("appears to be a binary file"));
}

#[tokio::test]
async fn test_read_file_image_png() {
    let temp_dir = TestDir::new();
    let file_path = temp_dir.path().join("test.png");
    std::fs::write(&file_path, b"\x89PNG\r\n\x1a\n...fake...").unwrap();
    let server = make_server(temp_dir.path());

    let result = server
        .read_file_impl(ReadFileParams {
            path: path_string(&file_path),
            offset: None,
            limit: None,
            tail: None,
            grep: None,
            head_lines: None,
            tail_lines: None,
            max_output_bytes: None,
        })
        .await
        .unwrap();

    assert_eq!(result.is_error, Some(false));
    let mut found_image = false;
    for content in result.content {
        let v = serde_json::to_value(&content).unwrap();
        if v["type"] == "image" {
            found_image = true;
            assert_eq!(v["mimeType"], "image/png");
            assert!(!v["data"].as_str().unwrap().is_empty());
        }
    }
    assert!(found_image, "expected to find an Image content block");
}

#[tokio::test]
async fn test_read_file_image_oversized() {
    let temp_dir = TestDir::new();
    let file_path = temp_dir.path().join("big.jpg");
    // > 5MB
    let big_data = vec![0xFF, 0xD8, 0xFF, 0x00];
    let mut file_data = big_data.clone();
    file_data.resize(5 * 1024 * 1024 + 10, 0);
    std::fs::write(&file_path, &file_data).unwrap();
    let server = make_server(temp_dir.path());

    let result = server
        .read_file_impl(ReadFileParams {
            path: path_string(&file_path),
            offset: None,
            limit: None,
            tail: None,
            grep: None,
            head_lines: None,
            tail_lines: None,
            max_output_bytes: None,
        })
        .await
        .unwrap();

    assert_eq!(result.is_error, Some(true));
    assert!(text_content(&result).contains("image too large"));
}

#[test]
fn test_detect_image_mime_logic() {
    let p = Path::new("test.txt");
    assert_eq!(
        FsServer::detect_image_mime(p, b"\x89PNG\r\n\x1a\n123"),
        Some("image/png")
    );
    assert_eq!(
        FsServer::detect_image_mime(p, b"\xFF\xD8\xFF123"),
        Some("image/jpeg")
    );
    assert_eq!(
        FsServer::detect_image_mime(p, b"GIF87a123"),
        Some("image/gif")
    );
    assert_eq!(
        FsServer::detect_image_mime(p, b"GIF89a123"),
        Some("image/gif")
    );
    assert_eq!(
        FsServer::detect_image_mime(p, b"RIFF1234WEBP123"),
        Some("image/webp")
    );

    // Extension fallback
    assert_eq!(
        FsServer::detect_image_mime(Path::new("test.png"), b"random"),
        Some("image/png")
    );
    assert_eq!(
        FsServer::detect_image_mime(Path::new("TEST.JPG"), b"random"),
        Some("image/jpeg")
    );
    assert_eq!(
        FsServer::detect_image_mime(Path::new("file.webp"), b"random"),
        Some("image/webp")
    );

    // Neither
    assert_eq!(
        FsServer::detect_image_mime(Path::new("test.txt"), b"random"),
        None
    );
}
#[tokio::test]
async fn test_edit_file_unique_match() {
    let temp_dir = TestDir::new();
    let file_path = temp_dir.path().join("unique.txt");
    std::fs::write(&file_path, "alpha\nbeta\n").unwrap();
    let server = FsServer::new(rwx_allowlist([temp_dir.path().to_path_buf()]));

    let result = server
        .edit_file_impl(EditFileParams {
            path: path_string(&file_path),
            old_text: "beta".to_string(),
            new_text: "gamma".to_string(),
            replace_all: Some(false),
        })
        .await
        .unwrap();

    assert_eq!(result.is_error, Some(false));
    assert_eq!(
        std::fs::read_to_string(&file_path).unwrap(),
        "alpha\ngamma\n"
    );
}

#[tokio::test]
async fn test_edit_file_multiple_matches() {
    let temp_dir = TestDir::new();
    let file_path = temp_dir.path().join("multiple.txt");
    std::fs::write(&file_path, "value\nvalue\n").unwrap();
    let server = FsServer::new(rwx_allowlist([temp_dir.path().to_path_buf()]));

    let result = server
        .edit_file_impl(EditFileParams {
            path: path_string(&file_path),
            old_text: "value".to_string(),
            new_text: "updated".to_string(),
            replace_all: Some(false),
        })
        .await
        .unwrap();

    assert_eq!(result.is_error, Some(true));
    assert!(text_content(&result).contains("Found 2 matches"));
}

#[tokio::test]
async fn test_list_directory_flat() {
    let temp_dir = TestDir::new();
    std::fs::create_dir_all(temp_dir.path().join("nested")).unwrap();
    std::fs::write(temp_dir.path().join("root.txt"), "root").unwrap();
    std::fs::write(temp_dir.path().join("nested").join("child.txt"), "child").unwrap();
    let server = FsServer::new(rwx_allowlist([temp_dir.path().to_path_buf()]));

    let result = server
        .list_directory_impl(ListDirectoryParams {
            path: temp_dir.path().to_string_lossy().to_string(),
            recursive: Some(false),
        })
        .await
        .unwrap();

    let text = text_content(&result);
    assert!(text.contains("nested/"));
    assert!(text.contains("root.txt"));
    assert!(!text.contains("child.txt"));
}

#[tokio::test]
async fn test_search_files_basic() {
    let temp_dir = TestDir::new();
    std::fs::write(temp_dir.path().join("one.txt"), "alpha\nneedle\nomega\n").unwrap();
    std::fs::write(temp_dir.path().join("two.txt"), "nothing here\n").unwrap();
    let server = FsServer::new(rwx_allowlist([temp_dir.path().to_path_buf()]));

    let result = server
        .search_files_impl(SearchFilesParams {
            pattern: "needle".to_string(),
            path: Some(temp_dir.path().to_string_lossy().to_string()),
            include: Some("*.txt".to_string()),
            context_lines: Some(0),
            ignore_case: Some(false),
            max_results: Some(10),
        })
        .await
        .unwrap();

    let text = text_content(&result);
    assert!(text.contains("one.txt:2: needle"));
    assert!(!text.contains("two.txt"));
}

// ── truncation-in-user-summary tests (issue #144) ──────────────────────

#[tokio::test]
async fn test_read_file_summary_limited_on_pagination() {
    // offset=1 limit=2 on a 4-line file → shows lines 1–2, more remain.
    // Summary must show the slice range and byte counts.
    let temp_dir = TestDir::new();
    let file_path = temp_dir.path().join("paginated.txt");
    std::fs::write(&file_path, "one\ntwo\nthree\nfour\n").unwrap();
    let server = FsServer::new(rwx_allowlist([temp_dir.path().to_path_buf()]));

    let result = server
        .read_file_impl(ReadFileParams {
            path: path_string(&file_path),
            offset: Some(1),
            limit: Some(2),
            tail: None,
            grep: None,
            head_lines: None,
            tail_lines: None,
            max_output_bytes: None,
        })
        .await
        .unwrap();

    let summary = user_summary(&result);
    assert!(
        summary.contains("lines 1\u{2013}2 of 4"),
        "expected exact paginated range 'lines 1\u{2013}2 of 4' in summary, got: {summary:?}"
    );
}

#[tokio::test]
async fn test_list_directory_summary_not_limited_when_small() {
    let temp_dir = TestDir::new();
    for i in 0..3 {
        std::fs::write(temp_dir.path().join(format!("f{i}.txt")), "x").unwrap();
    }
    let server = FsServer::new(rwx_allowlist([temp_dir.path().to_path_buf()]));

    let result = server
        .list_directory_impl(ListDirectoryParams {
            path: temp_dir.path().to_string_lossy().to_string(),
            recursive: Some(false),
        })
        .await
        .unwrap();

    let summary = user_summary(&result);
    assert!(
        !summary.contains("limited"),
        "expected no 'limited' for small listing, got: {summary:?}"
    );
    assert!(
        summary.contains("Listed 3 entries"),
        "expected count in summary, got: {summary:?}"
    );
}

#[tokio::test]
async fn test_list_directory_summary_limited_when_over_default_limit() {
    // Create DEFAULT_LS_LIMIT + 1 files to trigger limit_reached.
    // Summary should show "Listed 500 of 501 entries in …".
    let temp_dir = TestDir::new();
    for i in 0..=DEFAULT_LS_LIMIT {
        std::fs::write(temp_dir.path().join(format!("f{i:04}.txt")), "x").unwrap();
    }
    let server = FsServer::new(rwx_allowlist([temp_dir.path().to_path_buf()]));

    let result = server
        .list_directory_impl(ListDirectoryParams {
            path: temp_dir.path().to_string_lossy().to_string(),
            recursive: Some(false),
        })
        .await
        .unwrap();

    let summary = user_summary(&result);
    // Should show "Listed 500 of 501 entries" — capped count + true total.
    assert!(
        summary.contains(&format!(
            "Listed {} of {} entries",
            DEFAULT_LS_LIMIT,
            DEFAULT_LS_LIMIT + 1
        )),
        "expected 'Listed N of M entries' in summary, got: {summary:?}"
    );
}

#[tokio::test]
async fn test_search_files_summary_variants() {
    struct Case {
        files: &'static [(&'static str, &'static str)],
        max_results: usize,
        check: fn(&str),
    }

    let cases: &[Case] = &[
        Case {
            files: &[
                ("match0.txt", "needle\n"),
                ("match1.txt", "needle\n"),
                ("match2.txt", "needle\n"),
            ],
            max_results: 1,
            check: |summary| {
                assert!(
                    summary.contains("1+"),
                    "expected '1+' in summary when max_results hit, got: {summary:?}"
                );
                assert!(
                    summary.contains("showing 1"),
                    "expected 'showing 1' in summary, got: {summary:?}"
                );
            },
        },
        Case {
            files: &[("one.txt", "needle\n")],
            max_results: 10,
            check: |summary| {
                assert!(
                    !summary.contains("limited"),
                    "expected no 'limited' when all results returned, got: {summary:?}"
                );
            },
        },
    ];

    for case in cases {
        let temp_dir = TestDir::new();
        for (name, content) in case.files {
            std::fs::write(temp_dir.path().join(name), content).unwrap();
        }
        let server = FsServer::new(rwx_allowlist([temp_dir.path().to_path_buf()]));

        let result = server
            .search_files_impl(SearchFilesParams {
                pattern: "needle".to_string(),
                path: Some(temp_dir.path().to_string_lossy().to_string()),
                include: None,
                context_lines: Some(0),
                ignore_case: Some(false),
                max_results: Some(case.max_results),
            })
            .await
            .unwrap();

        (case.check)(user_summary(&result).as_str());
    }
}

#[tokio::test]
async fn test_find_files_basic() {
    // Regression: glob pattern must use '/' not MAIN_SEPARATOR —
    // the glob crate expects Unix separators on all platforms.
    let temp_dir = TestDir::new();
    std::fs::write(temp_dir.path().join("hello.txt"), "").unwrap();
    let server = FsServer::new(rwx_allowlist([temp_dir.path().to_path_buf()]));

    let result = server
        .find_files_impl(FindFilesParams {
            pattern: "*.txt".to_string(),
            path: Some(temp_dir.path().to_string_lossy().to_string()),
            max_results: Some(10),
        })
        .await
        .unwrap();

    let text = text_content(&result);
    assert!(
        text.contains("hello.txt"),
        "find_files should locate files on any platform, got: {text:?}"
    );
}

#[tokio::test]
async fn test_find_files_summary_variants() {
    struct Case {
        files: &'static [&'static str],
        max_results: usize,
        check: fn(&str),
    }

    let cases: &[Case] = &[
        Case {
            files: &["file0.txt", "file1.txt", "file2.txt"],
            max_results: 1,
            check: |summary| {
                assert!(
                    summary.contains("1+"),
                    "expected '1+' in find_files summary when max_results hit, got: {summary:?}"
                );
                assert!(
                    summary.contains("showing 1"),
                    "expected 'showing 1' in find_files summary, got: {summary:?}"
                );
            },
        },
        Case {
            files: &["only.txt"],
            max_results: 10,
            check: |summary| {
                assert!(
                    !summary.contains("limited"),
                    "expected no 'limited' when all files returned, got: {summary:?}"
                );
            },
        },
    ];

    for case in cases {
        let temp_dir = TestDir::new();
        for name in case.files {
            std::fs::write(temp_dir.path().join(name), "").unwrap();
        }
        let server = FsServer::new(rwx_allowlist([temp_dir.path().to_path_buf()]));

        let result = server
            .find_files_impl(FindFilesParams {
                pattern: "*.txt".to_string(),
                path: Some(temp_dir.path().to_string_lossy().to_string()),
                max_results: Some(case.max_results),
            })
            .await
            .unwrap();

        (case.check)(user_summary(&result).as_str());
    }
}

#[tokio::test]
async fn test_insert_prepend() {
    let temp_dir = TestDir::new();
    let file_path = temp_dir.path().join("prepend.txt");
    std::fs::write(
        &file_path,
        "beta
gamma
",
    )
    .unwrap();
    let server = FsServer::new(rwx_allowlist([temp_dir.path().to_path_buf()]));

    let result = server
        .insert_impl(InsertParams {
            path: path_string(&file_path),
            insert_line: Some(0),
            insert_text: "alpha
"
            .to_string(),
            column: None,
        })
        .await
        .unwrap();

    assert_eq!(result.is_error, Some(false));
    assert_eq!(
        std::fs::read_to_string(&file_path).unwrap(),
        "alpha
beta
gamma
"
    );
}

#[tokio::test]
async fn test_insert_append() {
    let temp_dir = TestDir::new();
    let file_path = temp_dir.path().join("append.txt");
    std::fs::write(
        &file_path,
        "alpha
beta
",
    )
    .unwrap();
    let server = FsServer::new(rwx_allowlist([temp_dir.path().to_path_buf()]));

    let result = server
        .insert_impl(InsertParams {
            path: path_string(&file_path),
            insert_line: Some(2),
            insert_text: "gamma
"
            .to_string(),
            column: None,
        })
        .await
        .unwrap();

    assert_eq!(result.is_error, Some(false));
    assert_eq!(
        std::fs::read_to_string(&file_path).unwrap(),
        "alpha
beta
gamma
"
    );
}

#[tokio::test]
async fn test_insert_append_omit_line() {
    // Omitting insert_line entirely should append to end of file
    let temp_dir = TestDir::new();
    let file_path = temp_dir.path().join("append_omit.txt");
    std::fs::write(&file_path, "alpha\nbeta\n").unwrap();
    let server = FsServer::new(rwx_allowlist([temp_dir.path().to_path_buf()]));

    let result = server
        .insert_impl(InsertParams {
            path: path_string(&file_path),
            insert_line: None,
            insert_text: "gamma\n".to_string(),
            column: None,
        })
        .await
        .unwrap();

    assert_eq!(result.is_error, Some(false));
    assert_eq!(
        std::fs::read_to_string(&file_path).unwrap(),
        "alpha\nbeta\ngamma\n"
    );
}

#[tokio::test]
async fn test_insert_append_omit_line_ignores_column() {
    // When insert_line is omitted, a supplied column must NOT trigger
    // mid-line insertion — it must still append at EOF.
    let temp_dir = TestDir::new();
    let file_path = temp_dir.path().join("append_col.txt");
    std::fs::write(&file_path, "alpha\nbeta\n").unwrap();
    let server = FsServer::new(rwx_allowlist([temp_dir.path().to_path_buf()]));

    let result = server
        .insert_impl(InsertParams {
            path: path_string(&file_path),
            insert_line: None,
            insert_text: "gamma\n".to_string(),
            column: Some(3), // would have inserted into last line without the fix
        })
        .await
        .unwrap();

    assert_eq!(result.is_error, Some(false));
    assert_eq!(
        std::fs::read_to_string(&file_path).unwrap(),
        "alpha\nbeta\ngamma\n"
    );
}

#[tokio::test]
async fn test_insert_middle() {
    let temp_dir = TestDir::new();
    let file_path = temp_dir.path().join("middle.txt");
    std::fs::write(
        &file_path,
        "one
two
three
four
",
    )
    .unwrap();
    let server = FsServer::new(rwx_allowlist([temp_dir.path().to_path_buf()]));

    let result = server
        .insert_impl(InsertParams {
            path: path_string(&file_path),
            insert_line: Some(2),
            insert_text: "between
"
            .to_string(),
            column: None,
        })
        .await
        .unwrap();

    assert_eq!(result.is_error, Some(false));
    assert_eq!(
        std::fs::read_to_string(&file_path).unwrap(),
        "one
two
between
three
four
"
    );
}

#[tokio::test]
async fn test_insert_column() {
    let temp_dir = TestDir::new();
    let file_path = temp_dir.path().join("column.txt");
    std::fs::write(
        &file_path,
        "abcd
xyz
",
    )
    .unwrap();
    let server = FsServer::new(rwx_allowlist([temp_dir.path().to_path_buf()]));

    let result = server
        .insert_impl(InsertParams {
            path: path_string(&file_path),
            insert_line: Some(1),
            insert_text: "-MID-".to_string(),
            column: Some(5),
        })
        .await
        .unwrap();

    assert_eq!(result.is_error, Some(false));
    assert_eq!(
        std::fs::read_to_string(&file_path).unwrap(),
        "abcd-MID-
xyz
"
    );
}

#[tokio::test]
async fn test_insert_column_utf8_boundary() {
    let temp_dir = TestDir::new();
    let file_path = temp_dir.path().join("utf8_boundary.txt");
    std::fs::write(
        &file_path, "🦀abc
",
    )
    .unwrap();
    let server = FsServer::new(rwx_allowlist([temp_dir.path().to_path_buf()]));

    let ok_result = server
        .insert_impl(InsertParams {
            path: path_string(&file_path),
            insert_line: Some(1),
            insert_text: "X".to_string(),
            column: Some(5),
        })
        .await
        .unwrap();

    assert_eq!(ok_result.is_error, Some(false));
    assert_eq!(
        std::fs::read_to_string(&file_path).unwrap(),
        "🦀Xabc
"
    );

    std::fs::write(
        &file_path, "🦀abc
",
    )
    .unwrap();

    let bad_result = server
        .insert_impl(InsertParams {
            path: path_string(&file_path),
            insert_line: Some(1),
            insert_text: "X".to_string(),
            column: Some(2),
        })
        .await
        .unwrap();

    assert_eq!(bad_result.is_error, Some(true));
    assert!(text_content(&bad_result).contains("UTF-8 character boundary"));
}

#[tokio::test]
async fn test_insert_line_out_of_range() {
    let temp_dir = TestDir::new();
    let file_path = temp_dir.path().join("line_oob.txt");
    std::fs::write(
        &file_path,
        "alpha
beta
",
    )
    .unwrap();
    let server = FsServer::new(rwx_allowlist([temp_dir.path().to_path_buf()]));

    let result = server
        .insert_impl(InsertParams {
            path: path_string(&file_path),
            insert_line: Some(3),
            insert_text: "gamma
"
            .to_string(),
            column: None,
        })
        .await
        .unwrap();

    assert_eq!(result.is_error, Some(true));
}

#[tokio::test]
async fn test_insert_column_out_of_range() {
    let temp_dir = TestDir::new();
    let file_path = write_fixture(
        &temp_dir,
        "column_oob.txt",
        "abcd
",
    );
    let server = FsServer::new(rwx_allowlist([temp_dir.path().to_path_buf()]));

    let result = server
        .insert_impl(InsertParams {
            path: path_string(&file_path),
            insert_line: Some(1),
            insert_text: "X".to_string(),
            column: Some(6),
        })
        .await
        .unwrap();

    assert_eq!(result.is_error, Some(true));
}

#[tokio::test]
async fn test_re_replace_basic() {
    let temp_dir = TestDir::new();
    let file_path = write_fixture(
        &temp_dir,
        "re_basic.txt",
        "foo123
",
    );
    let server = FsServer::new(rwx_allowlist([temp_dir.path().to_path_buf()]));

    let result = server
        .re_replace_impl(ReReplaceParams {
            path: path_string(&file_path),
            pattern: r"foo(\d+)".to_string(),
            replacement: "bar$1".to_string(),
            replace_all: None,
        })
        .await
        .unwrap();

    assert_eq!(result.is_error, Some(false));
    assert_eq!(
        std::fs::read_to_string(&file_path).unwrap(),
        "bar123
"
    );
}

#[tokio::test]
async fn test_re_replace_all() {
    let temp_dir = TestDir::new();
    let file_path = write_fixture(
        &temp_dir,
        "re_all.txt",
        "foo1 foo2 foo3
",
    );
    let server = FsServer::new(rwx_allowlist([temp_dir.path().to_path_buf()]));

    let result = server
        .re_replace_impl(ReReplaceParams {
            path: path_string(&file_path),
            pattern: r"foo(\d+)".to_string(),
            replacement: "bar$1".to_string(),
            replace_all: Some(true),
        })
        .await
        .unwrap();

    assert_eq!(result.is_error, Some(false));
    assert_eq!(
        std::fs::read_to_string(&file_path).unwrap(),
        "bar1 bar2 bar3
"
    );
}

#[tokio::test]
async fn test_re_replace_no_match() {
    let temp_dir = TestDir::new();
    let file_path = write_fixture(
        &temp_dir,
        "re_no_match.txt",
        "alpha
beta
",
    );
    let server = FsServer::new(rwx_allowlist([temp_dir.path().to_path_buf()]));

    let result = server
        .re_replace_impl(ReReplaceParams {
            path: path_string(&file_path),
            pattern: "foo".to_string(),
            replacement: "bar".to_string(),
            replace_all: None,
        })
        .await
        .unwrap();

    assert_eq!(result.is_error, Some(true));
    assert!(text_content(&result).contains("did not match"));
}

#[tokio::test]
async fn test_re_replace_multiple_no_flag() {
    let temp_dir = TestDir::new();
    let file_path = write_fixture(
        &temp_dir,
        "re_multiple.txt",
        "foo1 foo2
",
    );
    let server = FsServer::new(rwx_allowlist([temp_dir.path().to_path_buf()]));

    let result = server
        .re_replace_impl(ReReplaceParams {
            path: path_string(&file_path),
            pattern: r"foo(\d+)".to_string(),
            replacement: "bar$1".to_string(),
            replace_all: None,
        })
        .await
        .unwrap();

    assert_eq!(result.is_error, Some(true));
    assert!(text_content(&result).contains("replace_all"));
}

#[tokio::test]
async fn test_re_replace_invalid_pattern() {
    let temp_dir = TestDir::new();
    let file_path = write_fixture(
        &temp_dir,
        "re_invalid.txt",
        "foo1
",
    );
    let server = FsServer::new(rwx_allowlist([temp_dir.path().to_path_buf()]));

    let err = server
        .re_replace_impl(ReReplaceParams {
            path: path_string(&file_path),
            pattern: "(".to_string(),
            replacement: "bar".to_string(),
            replace_all: None,
        })
        .await
        .unwrap_err();

    assert!(err.message.contains("invalid regex pattern"));
}

#[tokio::test]
async fn test_insert_impl_serializes_concurrent_same_file_updates() {
    let temp_dir = TestDir::new();
    let file_path = write_fixture(&temp_dir, "concurrent_insert.txt", "start\n");
    let server = FsServer::new(rwx_allowlist([temp_dir.path().to_path_buf()]));

    let task_count = 32usize;
    let mut tasks = tokio::task::JoinSet::new();
    for index in 0..task_count {
        let server = server.clone();
        let path = path_string(&file_path);
        tasks.spawn(async move {
            server
                .insert_impl(InsertParams {
                    path,
                    insert_line: None,
                    insert_text: format!("task-{index}\n"),
                    column: None,
                })
                .await
        });
    }

    while let Some(result) = tasks.join_next().await {
        let result = result.unwrap().unwrap();
        assert_eq!(result.is_error, Some(false));
    }

    let content = std::fs::read_to_string(&file_path).unwrap();
    let lines = content.lines().collect::<Vec<_>>();
    assert_eq!(lines.len(), task_count + 1);
    assert_eq!(lines[0], "start");

    for index in 0..task_count {
        assert!(
            lines.contains(&format!("task-{index}").as_str()),
            "missing task-{index} in final file: {content:?}"
        );
    }
}

#[tokio::test]
async fn test_repo_lock_for_paths_in_same_repo_share_lock() {
    let temp_dir = TestDir::new();
    init_git_repo(temp_dir.path());
    let repo_root = temp_dir.path().canonicalize().unwrap();
    let file_a = repo_root.join("a.txt");
    std::fs::write(&file_a, "a\n").unwrap();
    let nested_dir = repo_root.join("nested");
    std::fs::create_dir_all(&nested_dir).unwrap();
    let file_b = repo_root.join("nested/b.txt");
    std::fs::write(&file_b, "b\n").unwrap();
    let server = FsServer::new(rwx_allowlist([repo_root.clone()]));

    let repo_guard = server.repo_write_guard_for_path(&file_a).await;
    let same_repo_try = tokio::time::timeout(
        std::time::Duration::from_millis(50),
        server.repo_read_guard_for_path(&file_b),
    )
    .await;

    assert!(
        same_repo_try.is_err(),
        "same repo lock should block readers while write held"
    );
    drop(repo_guard);

    server.repo_read_guard_for_path(&file_b).await;
}

/// Gated to Unix because `std::env::temp_dir()` on Windows runners
/// yields an 8.3 short-name path (`C:\\Users\\RUNNER~1\\...`); the
/// canonicalize-then-`gix::open` flow inside `HistoryManager::new`
/// then fails to register the repo, leaving the production code
/// without a base to diff against. That's a pre-existing Windows
/// limitation in `harnx-mcp-history`, not something this regression
/// test introduces.
#[cfg(unix)]
#[tokio::test]
async fn test_rollback_excludes_concurrent_edits_in_same_repo() {
    let temp_dir = TestDir::new();
    init_git_repo(temp_dir.path());
    let repo_root = temp_dir.path().canonicalize().unwrap();

    let tracked_path = repo_root.join("tracked.txt");
    std::fs::write(&tracked_path, "tracked-v1\n").unwrap();
    let server = FsServer::new(rwx_allowlist([repo_root.clone()]));

    let tracked_before = server
        .snapshot_before(&tracked_path, "before tracked change")
        .await
        .unwrap();
    std::fs::write(&tracked_path, "tracked-v2\n").unwrap();
    let rollback_diff = server
        .snapshot_after_diff(&tracked_path, Some(tracked_before), "after tracked change")
        .await
        .unwrap();
    let rollback_commit = rollback_diff
        .lines()
        .next()
        .unwrap()
        .strip_prefix("commit ")
        .unwrap()
        .to_string();

    let repo_guard = server.repo_write_guard_for_path(&tracked_path).await;
    let edit_server = server.clone();
    let edit_path = path_string(&repo_root.join("other.txt"));
    let blocked_edit = tokio::spawn(async move {
        tokio::time::timeout(
            std::time::Duration::from_millis(50),
            edit_server.write_file_impl(WriteFileParams {
                path: edit_path,
                content: "blocked\n".to_string(),
            }),
        )
        .await
    });

    assert!(blocked_edit.await.unwrap().is_err());
    drop(repo_guard);

    let rollback_server = server.clone();
    let rollback_path = path_string(&repo_root);
    let rollback_task = tokio::spawn(async move {
        rollback_server
            .rollback_file_impl(RollbackParams {
                commit_id: rollback_commit,
                repo_path: rollback_path,
            })
            .await
    });

    let rollback_result = rollback_task.await.unwrap().unwrap();
    assert_eq!(rollback_result.is_error, Some(false));
}
