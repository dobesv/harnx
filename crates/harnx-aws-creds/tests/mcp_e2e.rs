#![cfg(unix)]

use std::path::PathBuf;
use std::process::Stdio;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::time::{timeout, Duration};

#[tokio::test(flavor = "multi_thread")]
async fn mcp_initialize_returns_server_info() {
    let Some(binary) = aws_creds_binary_path() else {
        eprintln!("SKIP: harnx-aws-creds binary not found");
        return;
    };

    let mut child = spawn_aws_creds(&binary);
    let (mut stdin, mut stdout) = stdio(&mut child);

    let response = send_and_read_response(
        &mut stdin,
        &mut stdout,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "test", "version": "0.1.0"}
            }
        })
        .to_string(),
        1,
    )
    .await;

    assert_eq!(response["result"]["serverInfo"]["name"], "harnx-aws-creds");
    assert!(response["result"]["capabilities"].get("tools").is_some());

    cleanup_child(child).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn mcp_list_tools_returns_aws_auth_setup() {
    let Some(binary) = aws_creds_binary_path() else {
        eprintln!("SKIP: harnx-aws-creds binary not found");
        return;
    };

    let mut child = spawn_aws_creds(&binary);
    let (mut stdin, mut stdout) = stdio(&mut child);

    initialize_session(&mut stdin, &mut stdout).await;
    let response = send_and_read_response(
        &mut stdin,
        &mut stdout,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/list",
            "params": {}
        })
        .to_string(),
        2,
    )
    .await;

    let tools = response["result"]["tools"]
        .as_array()
        .expect("tools/list should return array");
    assert!(tools.iter().any(|tool| tool["name"] == "aws_auth_setup"));

    cleanup_child(child).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn mcp_call_aws_auth_setup_returns_env_vars() {
    let Some(binary) = aws_creds_binary_path() else {
        eprintln!("SKIP: harnx-aws-creds binary not found");
        return;
    };

    let mut child = spawn_aws_creds(&binary);
    let (mut stdin, mut stdout) = stdio(&mut child);

    initialize_session(&mut stdin, &mut stdout).await;
    let response = send_and_read_response(
        &mut stdin,
        &mut stdout,
        &json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {
                "name": "aws_auth_setup",
                "arguments": {}
            }
        })
        .to_string(),
        3,
    )
    .await;

    let text = response["result"]["content"][0]["text"]
        .as_str()
        .expect("tools/call should return text content");
    assert!(text.contains("AWS_CONTAINER_CREDENTIALS_FULL_URI=http://127.0.0.1:"));
    assert!(text.contains("AWS_CONTAINER_AUTHORIZATION_TOKEN="));
    assert!(text.contains("AWS_REGION="));

    cleanup_child(child).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn mcp_call_aws_auth_setup_idempotent() {
    let Some(binary) = aws_creds_binary_path() else {
        eprintln!("SKIP: harnx-aws-creds binary not found");
        return;
    };

    let mut child = spawn_aws_creds(&binary);
    let (mut stdin, mut stdout) = stdio(&mut child);

    initialize_session(&mut stdin, &mut stdout).await;

    let response1 = send_and_read_response(
        &mut stdin,
        &mut stdout,
        &json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {
                "name": "aws_auth_setup",
                "arguments": {}
            }
        })
        .to_string(),
        3,
    )
    .await;
    let response2 = send_and_read_response(
        &mut stdin,
        &mut stdout,
        &json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "tools/call",
            "params": {
                "name": "aws_auth_setup",
                "arguments": {}
            }
        })
        .to_string(),
        4,
    )
    .await;

    let uri1 = extract_env_value(
        response1["result"]["content"][0]["text"]
            .as_str()
            .expect("first response text"),
        "AWS_CONTAINER_CREDENTIALS_FULL_URI",
    )
    .expect("first response should contain full uri");
    let uri2 = extract_env_value(
        response2["result"]["content"][0]["text"]
            .as_str()
            .expect("second response text"),
        "AWS_CONTAINER_CREDENTIALS_FULL_URI",
    )
    .expect("second response should contain full uri");

    assert_eq!(uri1, uri2, "both calls should return same credential uri");

    cleanup_child(child).await;
}

fn spawn_aws_creds(binary: &PathBuf) -> Child {
    Command::new(binary)
        .arg("--mcp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn harnx-aws-creds --mcp")
}

fn stdio(child: &mut Child) -> (ChildStdin, BufReader<ChildStdout>) {
    let stdin = child.stdin.take().expect("child stdin");
    let stdout = child.stdout.take().expect("child stdout");
    (stdin, BufReader::new(stdout))
}

async fn initialize_session(stdin: &mut ChildStdin, stdout: &mut BufReader<ChildStdout>) {
    let _ = send_and_read_response(
        stdin,
        stdout,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "test", "version": "0.1.0"}
            }
        })
        .to_string(),
        1,
    )
    .await;

    send_notification(
        stdin,
        &json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized",
            "params": {}
        })
        .to_string(),
    )
    .await;
}

async fn send_notification(stdin: &mut ChildStdin, msg: &str) {
    stdin
        .write_all(msg.as_bytes())
        .await
        .expect("write notification");
    stdin
        .write_all(b"\n")
        .await
        .expect("write notification newline");
    stdin.flush().await.expect("flush notification");
}

async fn send_and_read_response(
    stdin: &mut ChildStdin,
    stdout: &mut BufReader<ChildStdout>,
    msg: &str,
    id: i64,
) -> Value {
    stdin
        .write_all(msg.as_bytes())
        .await
        .expect("write request");
    stdin.write_all(b"\n").await.expect("write request newline");
    stdin.flush().await.expect("flush request");

    let mut line = String::new();
    loop {
        line.clear();
        let read = timeout(Duration::from_secs(10), stdout.read_line(&mut line))
            .await
            .expect("timed out waiting for MCP response")
            .expect("read MCP response line");
        assert!(read > 0, "child stdout closed before response for id {id}");

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let value: Value = serde_json::from_str(trimmed)
            .unwrap_or_else(|err| panic!("invalid JSON line from child: {trimmed}; error: {err}"));
        if value.get("id").and_then(Value::as_i64) == Some(id) {
            return value;
        }
    }
}

async fn cleanup_child(mut child: Child) {
    let _ = child.start_kill();
    let _ = timeout(Duration::from_secs(5), child.wait()).await;
}

fn extract_env_value<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    text.split_whitespace()
        .find_map(|entry| entry.strip_prefix(&format!("{key}=")))
}

fn aws_creds_binary_path() -> Option<PathBuf> {
    if let Some(path) = std::option_env!("CARGO_BIN_EXE_harnx-aws-creds") {
        let p = PathBuf::from(path);
        if p.is_file() {
            return Some(p);
        }
    }
    let candidate = target_dir().join(binary_name("harnx-aws-creds"));
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
