#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::Stdio;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::time::{timeout, Duration};

#[tokio::test(flavor = "multi_thread")]
async fn mcp_initialize_returns_server_info() {
    let Some(binary) = proxy_binary_path() else {
        eprintln!("SKIP: harnx-proxy-auth binary not found");
        return;
    };

    let mut session = McpSession::spawn(&binary, None).await;
    let response = session
        .send_request(
            "initialize",
            json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "test", "version": "0.1.0"}
            }),
        )
        .await;

    assert_eq!(response["result"]["serverInfo"]["name"], "harnx-proxy-auth");
    assert!(response["result"]["capabilities"].get("tools").is_some());
}

#[tokio::test(flavor = "multi_thread")]
async fn mcp_list_tools_without_services() {
    let description = list_tools_description(None).await;
    assert!(description.contains("auth proxy"));
    assert!(!description.contains("GitHub"));
}

#[tokio::test(flavor = "multi_thread")]
async fn mcp_list_tools_with_services() {
    let description = list_tools_description(Some("GitHub")).await;
    assert!(description.contains("GitHub"));
}

#[tokio::test(flavor = "multi_thread")]
async fn mcp_call_proxy_auth_setup_returns_env_vars() {
    let text = proxy_auth_setup_text().await;
    assert!(text.contains("HTTP_PROXY=http://127.0.0.1:"));
    assert!(text.contains("HTTPS_PROXY=http://127.0.0.1:"));
    assert!(text.contains("SSL_CERT_FILE="));
    assert!(text.contains("REQUESTS_CA_BUNDLE="));
    assert!(text.contains("NODE_EXTRA_CA_CERTS="));
}

#[tokio::test(flavor = "multi_thread")]
async fn mcp_call_proxy_auth_setup_idempotent() {
    let Some(binary) = proxy_binary_path() else {
        eprintln!("SKIP: harnx-proxy-auth binary not found");
        return;
    };

    let mut session = McpSession::spawn_initialized(&binary, None).await;
    let response1 = session
        .send_request(
            "tools/call",
            json!({
                "name": "proxy_auth_setup",
                "arguments": {}
            }),
        )
        .await;
    let response2 = session
        .send_request(
            "tools/call",
            json!({
                "name": "proxy_auth_setup",
                "arguments": {}
            }),
        )
        .await;

    let proxy1 = extract_env_value(
        response1["result"]["content"][0]["text"]
            .as_str()
            .expect("first response text"),
        "HTTP_PROXY",
    )
    .expect("first response should contain HTTP_PROXY");
    let proxy2 = extract_env_value(
        response2["result"]["content"][0]["text"]
            .as_str()
            .expect("second response text"),
        "HTTP_PROXY",
    )
    .expect("second response should contain HTTP_PROXY");

    assert_eq!(proxy1, proxy2, "both calls should return same proxy port");
}

#[tokio::test(flavor = "multi_thread")]
async fn mcp_call_proxy_auth_setup_ca_cert_file_exists() {
    let text = proxy_auth_setup_text().await;
    let ca_cert =
        extract_env_value(&text, "SSL_CERT_FILE").expect("response should include SSL_CERT_FILE");
    assert!(
        Path::new(ca_cert).is_file(),
        "CA cert file should exist: {ca_cert}"
    );
}

struct McpSession {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: i64,
}

impl McpSession {
    async fn spawn(binary: &PathBuf, services: Option<&str>) -> Self {
        let mut child = Command::new(binary)
            .arg("--mcp")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .args(
                services
                    .into_iter()
                    .flat_map(|services| ["--services", services]),
            )
            .spawn()
            .expect("spawn harnx-proxy-auth --mcp");
        let stdin = child.stdin.take().expect("child stdin");
        let stdout = child.stdout.take().expect("child stdout");
        Self {
            child,
            stdin,
            stdout: BufReader::new(stdout),
            next_id: 1,
        }
    }

    async fn spawn_initialized(binary: &PathBuf, services: Option<&str>) -> Self {
        let mut session = Self::spawn(binary, services).await;
        let _ = session
            .send_request(
                "initialize",
                json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                    "clientInfo": {"name": "test", "version": "0.1.0"}
                }),
            )
            .await;
        session
            .send_notification("notifications/initialized", json!({}))
            .await;
        session
    }

    async fn send_request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        self.write_message(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }))
        .await;
        self.read_response(id).await
    }

    async fn send_notification(&mut self, method: &str, params: Value) {
        self.write_message(&json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        }))
        .await;
    }

    async fn write_message(&mut self, message: &Value) {
        let message = message.to_string();
        self.stdin
            .write_all(message.as_bytes())
            .await
            .expect("write MCP message");
        self.stdin
            .write_all(b"\n")
            .await
            .expect("write MCP newline");
        self.stdin.flush().await.expect("flush MCP message");
    }

    async fn read_response(&mut self, id: i64) -> Value {
        let mut line = String::new();
        loop {
            line.clear();
            let read = timeout(Duration::from_secs(10), self.stdout.read_line(&mut line))
                .await
                .expect("timed out waiting for MCP response")
                .expect("read MCP response line");
            assert!(read > 0, "child stdout closed before response for id {id}");

            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }

            let value: Value = serde_json::from_str(trimmed).unwrap_or_else(|err| {
                panic!("invalid JSON line from child: {trimmed}; error: {err}")
            });
            if value.get("id").and_then(Value::as_i64) == Some(id) {
                return value;
            }
        }
    }
}

async fn list_tools_description(services: Option<&str>) -> String {
    let Some(binary) = proxy_binary_path() else {
        eprintln!("SKIP: harnx-proxy-auth binary not found");
        return String::new();
    };

    let mut session = McpSession::spawn_initialized(&binary, services).await;
    let response = session.send_request("tools/list", json!({})).await;
    response["result"]["tools"]
        .as_array()
        .expect("tools/list should return array")
        .iter()
        .find(|tool| tool["name"] == "proxy_auth_setup")
        .and_then(|tool| tool["description"].as_str())
        .expect("tools/list should include proxy_auth_setup description")
        .to_string()
}

async fn proxy_auth_setup_text() -> String {
    let Some(binary) = proxy_binary_path() else {
        eprintln!("SKIP: harnx-proxy-auth binary not found");
        return String::new();
    };

    let mut session = McpSession::spawn_initialized(&binary, None).await;
    let response = session
        .send_request(
            "tools/call",
            json!({
                "name": "proxy_auth_setup",
                "arguments": {}
            }),
        )
        .await;
    response["result"]["content"][0]["text"]
        .as_str()
        .expect("tools/call should return text content")
        .to_string()
}

impl Drop for McpSession {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}

fn extract_env_value<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    text.lines()
        .find_map(|line| line.strip_prefix(&format!("{key}=")))
}

fn proxy_binary_path() -> Option<PathBuf> {
    if let Some(path) = std::option_env!("CARGO_BIN_EXE_harnx-proxy-auth") {
        let p = PathBuf::from(path);
        if p.is_file() {
            return Some(p);
        }
    }
    let candidate = target_dir().join(binary_name("harnx-proxy-auth"));
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
