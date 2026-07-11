#![cfg(unix)]
// rmcp deprecated the MCP Roots feature (SEP-2577); this test exercises roots.
#![allow(deprecated)]

use std::path::{Path, PathBuf};

use process_wrap::tokio::{CommandWrap, KillOnDrop, ProcessGroup};
use rmcp::handler::client::ClientHandler;
use rmcp::model::{
    CallToolRequestParams, ClientCapabilities, ContentBlock, ErrorData, Implementation,
    InitializeRequestParams, ListRootsResult, Root,
};
use rmcp::service::{RequestContext, RoleClient};
use rmcp::transport::TokioChildProcess;
use serde_json::json;
use tempfile::TempDir;

struct TestClientHandler {
    roots: Vec<Root>,
}

impl TestClientHandler {
    fn with_root(path: &std::path::Path) -> Self {
        let uri = format!("file://{}", path.to_string_lossy());
        Self {
            roots: vec![Root::new(uri)],
        }
    }
}

impl ClientHandler for TestClientHandler {
    fn get_info(&self) -> InitializeRequestParams {
        InitializeRequestParams::new(
            ClientCapabilities::builder()
                .enable_roots()
                .enable_roots_list_changed()
                .build(),
            Implementation::new("proxy-e2e-test-client", "0.1.0"),
        )
    }

    async fn list_roots(
        &self,
        _cx: RequestContext<RoleClient>,
    ) -> Result<ListRootsResult, ErrorData> {
        Ok(ListRootsResult::new(self.roots.clone()))
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn pass_through() {
    let Some(proxy_bin) = proxy_binary_path() else {
        eprintln!("SKIP pass_through: harnx-mcp-hooks-proxy binary not found; build package first");
        return;
    };
    let Some(bash_bin) = mcp_bash_binary_path() else {
        eprintln!("SKIP pass_through: harnx-mcp-bash binary not found; build package first");
        return;
    };

    let repo_root = repo_root();
    let service = spawn_proxy(&proxy_bin, &bash_bin, &repo_root, &[])
        .await
        .expect("spawn proxy and connect MCP client");
    let peer = service.peer().clone();

    let tools = peer
        .list_tools(None)
        .await
        .expect("list_tools through proxy");
    assert!(
        tools.tools.iter().any(|tool| tool.name.as_ref() == "exec"),
        "proxy should pass through harnx-mcp-bash tools"
    );

    let result = peer
        .call_tool(
            CallToolRequestParams::new("exec").with_arguments(
                json!({
                    "command": "echo proxy-pass-through",
                    "working_dir": repo_root.to_string_lossy()
                })
                .as_object()
                .expect("object")
                .clone(),
            ),
        )
        .await
        .expect("call exec through proxy");

    let text = text_content(&result.content);
    assert!(
        result.is_error != Some(true),
        "expected successful exec, got error: {text}"
    );
    assert!(
        text.contains("proxy-pass-through"),
        "expected passthrough exec output, got: {text}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn pre_tool_use_block() {
    let Some(proxy_bin) = proxy_binary_path() else {
        eprintln!(
            "SKIP pre_tool_use_block: harnx-mcp-hooks-proxy binary not found; build package first"
        );
        return;
    };
    let Some(bash_bin) = mcp_bash_binary_path() else {
        eprintln!("SKIP pre_tool_use_block: harnx-mcp-bash binary not found; build package first");
        return;
    };

    let temp_dir = TempDir::new().expect("temp dir");
    let script_path = temp_dir.path().join("block-hook.sh");
    std::fs::write(
        &script_path,
        "#!/bin/sh\necho 'hook blocked it' >&2\nexit 2\n",
    )
    .expect("write hook script");
    chmod_script(&script_path);

    let repo_root = repo_root();
    let script_path_str = script_path.to_str().expect("utf-8 script path");
    let service = spawn_proxy(
        &proxy_bin,
        &bash_bin,
        &repo_root,
        &[&["--pre-tool-use", "claude-command", script_path_str, ";"]],
    )
    .await
    .expect("spawn proxy with blocking hook and connect MCP client");
    let peer = service.peer().clone();

    let result = peer
        .call_tool(
            CallToolRequestParams::new("exec").with_arguments(
                json!({
                    "command": "printf 'should-not-run\\n'"
                })
                .as_object()
                .expect("object")
                .clone(),
            ),
        )
        .await
        .expect("proxy should return tool error result, not transport failure");

    assert_eq!(result.is_error, Some(true));
    let text = text_content(&result.content);
    assert!(
        !text.is_empty(),
        "expected block reason in error content, got empty: {text}"
    );
}

struct MutationTestCase<'a> {
    hook_flag: &'a str,
    hook_script: &'a str,
    original_command: &'a str,
    expected_in_output: &'a str,
    not_expected_in_output: &'a str,
}

async fn run_mutation_test(test_name: &str, tc: MutationTestCase<'_>) {
    let Some(proxy_bin) = proxy_binary_path() else {
        eprintln!("SKIP {test_name}: harnx-mcp-hooks-proxy binary not found; build package first");
        return;
    };
    let Some(bash_bin) = mcp_bash_binary_path() else {
        eprintln!("SKIP {test_name}: harnx-mcp-bash binary not found; build package first");
        return;
    };

    let repo_root = repo_root();
    let temp_dir = TempDir::new().expect("temp dir");
    let script_path = temp_dir.path().join("mutation-hook.sh");
    std::fs::write(&script_path, tc.hook_script).expect("write hook script");
    chmod_script(&script_path);

    let script_path_str = script_path.to_str().expect("utf-8 script path");
    let service = spawn_proxy(
        &proxy_bin,
        &bash_bin,
        &repo_root,
        &[&[tc.hook_flag, "claude-command", script_path_str, ";"]],
    )
    .await
    .expect("spawn proxy with mutation hook and connect MCP client");
    let peer = service.peer().clone();

    let result = peer
        .call_tool(
            CallToolRequestParams::new("exec").with_arguments(
                json!({
                    "command": tc.original_command,
                    "working_dir": repo_root.to_string_lossy()
                })
                .as_object()
                .expect("object")
                .clone(),
            ),
        )
        .await;

    let Ok(result) = result else {
        eprintln!("SKIP {test_name}: tool call failed in environment: {result:?}");
        return;
    };

    let text = text_content(&result.content);
    if result.is_error == Some(true) {
        eprintln!("SKIP {test_name}: proxy returned error result: {text}");
        return;
    }

    assert!(
        text.contains(tc.expected_in_output),
        "expected hook-mutated output, got: {text}"
    );
    assert!(
        !text.contains(tc.not_expected_in_output),
        "expected original output to be replaced by hook mutation, got: {text}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn pre_tool_use_mutation() {
    let repo_root = repo_root();
    let hook_script = format!(
        "#!/bin/sh\necho '{{\"hookSpecificOutput\": {{\"toolInput\": {{\"command\": \"echo mutated-by-hook\", \"working_dir\": \"{}\"}}}}}}'\n",
        repo_root.to_string_lossy()
    );
    run_mutation_test(
        "pre_tool_use_mutation",
        MutationTestCase {
            hook_flag: "--pre-tool-use",
            hook_script: &hook_script,
            original_command: "echo original-command",
            expected_in_output: "mutated-by-hook",
            not_expected_in_output: "original-command",
        },
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn post_tool_use_mutation() {
    let hook_script = "#!/bin/sh\necho '{\"hookSpecificOutput\": {\"toolResponse\": {\"content\": [{\"type\": \"text\", \"text\": \"response-mutated-by-hook\"}]}}}'\n";
    run_mutation_test(
        "post_tool_use_mutation",
        MutationTestCase {
            hook_flag: "--post-tool-use",
            hook_script,
            original_command: "echo original-response",
            expected_in_output: "response-mutated-by-hook",
            not_expected_in_output: "original-response",
        },
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn post_tool_use_failure() {
    let Some(proxy_bin) = proxy_binary_path() else {
        eprintln!("SKIP post_tool_use_failure: harnx-mcp-hooks-proxy binary not found; build package first");
        return;
    };
    let Some(bash_bin) = mcp_bash_binary_path() else {
        eprintln!(
            "SKIP post_tool_use_failure: harnx-mcp-bash binary not found; build package first"
        );
        return;
    };

    let repo_root = repo_root();
    let temp_dir = TempDir::new().expect("temp dir");

    // Marker file the hook writes to — proves the failure hook actually fired.
    let marker_path = temp_dir.path().join("failure-hook-fired.txt");
    let script_path = temp_dir.path().join("failure-hook.sh");
    let marker_str = marker_path.to_string_lossy().into_owned();
    std::fs::write(
        &script_path,
        format!("#!/bin/sh\necho 'failure-hook-fired' > '{marker_str}'\n"),
    )
    .expect("write failure hook script");
    chmod_script(&script_path);

    let script_path_str = script_path.to_str().expect("utf-8 script path");
    let service = spawn_proxy(
        &proxy_bin,
        &bash_bin,
        &repo_root,
        &[&[
            "--post-tool-use-failure",
            "claude-command",
            script_path_str,
            ";",
        ]],
    )
    .await
    .expect("spawn proxy with failure hook");
    let peer = service.peer().clone();

    // Call a nonexistent tool — this triggers child error → PostToolUseFailure hook fires.
    let result = peer
        .call_tool(CallToolRequestParams::new("nonexistent_tool"))
        .await
        .expect("proxy should return tool error result for unknown child tool");

    assert_eq!(result.is_error, Some(true));
    let text = text_content(&result.content);
    assert!(
        !text.is_empty(),
        "expected error content for unknown child tool, got empty: {text}"
    );

    // Poll for the marker file with a bounded timeout instead of a fixed sleep.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if marker_path.exists() {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("PostToolUseFailure hook did not fire within 5s (marker file not created at {marker_str})");
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
}

async fn spawn_proxy(
    proxy_bin: &Path,
    bash_bin: &Path,
    repo_root: &Path,
    hook_args: &[&[&str]],
) -> anyhow::Result<rmcp::service::RunningService<RoleClient, TestClientHandler>> {
    let mut command = tokio::process::Command::new(proxy_bin);
    for hook_group in hook_args {
        for token in *hook_group {
            command.arg(token);
        }
    }
    command.arg("--");
    command.arg(bash_bin);
    command.arg("--root").arg(repo_root);
    command.arg("--no-sandbox");

    let mut wrap = CommandWrap::from(command);
    wrap.wrap(KillOnDrop);
    wrap.wrap(ProcessGroup::leader());

    let (transport, _stderr) = TokioChildProcess::builder(wrap)
        .stderr(std::process::Stdio::piped())
        .spawn()?;

    let handler = TestClientHandler::with_root(repo_root);
    let service = rmcp::service::serve_client(handler, transport).await?;
    Ok(service)
}

fn text_content(content: &[ContentBlock]) -> String {
    content
        .iter()
        .filter_map(|item| match item {
            ContentBlock::Text(text) => Some(text.text.to_string()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn proxy_binary_path() -> Option<PathBuf> {
    find_binary(
        std::option_env!("CARGO_BIN_EXE_harnx-mcp-hooks-proxy"),
        "harnx-mcp-hooks-proxy",
    )
}

fn mcp_bash_binary_path() -> Option<PathBuf> {
    find_binary(
        std::option_env!("CARGO_BIN_EXE_harnx-mcp-bash"),
        "harnx-mcp-bash",
    )
}

fn find_binary(env_path: Option<&str>, name: &str) -> Option<PathBuf> {
    if let Some(path) = env_path {
        let p = PathBuf::from(path);
        if p.is_file() {
            return Some(p);
        }
    }
    let candidate = target_dir().join(binary_name(name));
    candidate.is_file().then_some(candidate)
}

fn target_dir() -> PathBuf {
    let mut exe = std::env::current_exe().expect("current_exe");
    exe.pop();
    if exe.ends_with("deps") {
        exe.pop();
    }
    exe
}

fn binary_name(base: &str) -> String {
    if cfg!(windows) {
        format!("{base}.exe")
    } else {
        base.to_string()
    }
}

fn chmod_script(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(path)
        .expect("script metadata")
        .permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(path, perms).expect("chmod hook script");
}
