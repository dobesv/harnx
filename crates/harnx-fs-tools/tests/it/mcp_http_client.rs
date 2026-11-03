//! HTTP-mode integration tests for harnx-fs-tools.
use std::process::Stdio;

use rmcp::handler::client::ClientHandler;
use rmcp::model::{
    CallToolRequestParams, ClientCapabilities, ErrorCode, Implementation, InitializeRequestParams,
};
use rmcp::service::ServiceError;
use rmcp::transport::streamable_http_client::{
    StreamableHttpClientTransport, StreamableHttpClientTransportConfig,
};

struct TestClient;

impl ClientHandler for TestClient {
    fn get_info(&self) -> InitializeRequestParams {
        InitializeRequestParams::new(
            ClientCapabilities::default(),
            Implementation::new("test-client", env!("CARGO_PKG_VERSION")),
        )
    }
}

struct ChildGuard(std::process::Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Bind an ephemeral port, drop the listener, and return the port number.
async fn pick_port() -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    listener.local_addr().expect("get local addr").port()
}

async fn wait_for_port(port: u16) {
    let addr = format!("127.0.0.1:{port}");
    let start = std::time::Instant::now();
    while start.elapsed() < std::time::Duration::from_secs(5) {
        if tokio::net::TcpStream::connect(&addr).await.is_ok() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("timed out waiting for 127.0.0.1:{port} to accept connections");
}

/// Spawn harnx-fs-tools with --mcp-http on the given port.
fn spawn_fs_server(port: u16, args: &[&str], sandbox_env: bool) -> ChildGuard {
    let bin = std::env::var("CARGO_BIN_EXE_harnx-fs-tools")
        .expect("CARGO_BIN_EXE_harnx-fs-tools must be set");
    let child = std::process::Command::new(bin)
        .arg("--mcp-http")
        .arg("--host")
        .arg("127.0.0.1")
        .arg("--port")
        .arg(port.to_string())
        .args(args)
        .env(
            "HARNX_FS_SANDBOX_TRANSPORT",
            if sandbox_env { "1" } else { "0" },
        )
        .env("HARNX_NATS_URL", "invalid NATS URL")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn fs-tools");
    ChildGuard(child)
}

async fn connect_client(port: u16) -> rmcp::service::RunningService<rmcp::RoleClient, TestClient> {
    wait_for_port(port).await;
    let url = format!("http://127.0.0.1:{port}/mcp");
    let client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("build reqwest client");
    let config = StreamableHttpClientTransportConfig::with_uri(url.as_str());
    let transport = StreamableHttpClientTransport::with_client(client, config);

    rmcp::service::serve_client(TestClient, transport)
        .await
        .expect("connect rmcp client")
}

#[tokio::test]
async fn fs_tools_http_initialize_list_call_unknown() {
    let port = pick_port().await;
    let _child = spawn_fs_server(port, &[], false);
    let service = connect_client(port).await;
    let peer = service.peer().clone();

    // 1. Initialize succeeds
    let info = peer.peer_info().expect("peer info after handshake");
    assert!(
        info.capabilities.tools.is_some(),
        "server should advertise tools capability"
    );

    // 2. list_tools returns expected tools
    let tools = peer.list_tools(None).await.expect("list_tools");
    let tool_names: Vec<_> = tools.tools.iter().map(|t| t.name.as_ref()).collect();
    assert!(tool_names.contains(&"read"), "missing read");
    assert!(tool_names.contains(&"write"), "missing write");

    assert!(
        !tool_names.contains(&"write_bytes"),
        "private tool leaked by default"
    );

    for tool in ["nonexistent_tool", "write_bytes"] {
        let err = peer
            .call_tool(CallToolRequestParams::new(tool))
            .await
            .expect_err("unknown tool should fail");
        match err {
            ServiceError::McpError(error) => {
                assert_eq!(error.code, ErrorCode::INVALID_PARAMS);
            }
            other => panic!("expected MCP invalid_params error, got {other:?}"),
        }
    }

    service.cancel().await.expect("cancel client");
}

async fn write_bytes_round_trip_over_http(size: usize, args: &[&str], sandbox_env: bool) {
    use base64::{engine::general_purpose::STANDARD, Engine};
    use serde_json::json;

    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("nested/payload.bin");
    let root_arg = root.path().to_str().unwrap();
    let mut server_args = vec!["--allow-write", root_arg];
    server_args.extend_from_slice(args);
    let port = pick_port().await;
    let _child = spawn_fs_server(port, &server_args, sandbox_env);
    let service = connect_client(port).await;
    let tools = service.peer().list_tools(None).await.expect("list_tools");
    assert!(tools.tools.iter().any(|tool| tool.name == "write_bytes"));

    let bytes = vec![0xff; size];
    let arguments = json!({"path": path, "data_base64": STANDARD.encode(&bytes)});
    let result = service
        .peer()
        .call_tool(
            CallToolRequestParams::new("write_bytes")
                .with_arguments(serde_json::from_value(arguments).unwrap()),
        )
        .await
        .expect("write_bytes HTTP call");
    assert_ne!(result.is_error, Some(true), "{result:?}");
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    assert!(serde_json::to_string(&result).unwrap().len() < 8192);

    let invalid = json!({"path": path, "data_base64": "invalid base64!"});
    let result = service
        .peer()
        .call_tool(
            CallToolRequestParams::new("write_bytes")
                .with_arguments(serde_json::from_value(invalid).unwrap()),
        )
        .await
        .expect("bad base64 must be recoverable over HTTP");
    assert_eq!(result.is_error, Some(true));
    assert_eq!(std::fs::read(path).unwrap(), bytes);
    service.cancel().await.expect("cancel client");
}

#[tokio::test]
async fn fs_tools_http_write_bytes_exceeds_default_body_limit() {
    write_bytes_round_trip_over_http(8 * 1024 * 1024, &["--sandbox-transport"], false).await;
}

#[tokio::test]
async fn fs_tools_http_write_bytes_sandbox_env_enables_transport_and_body_limit() {
    write_bytes_round_trip_over_http(8 * 1024 * 1024, &[], true).await;
}

#[tokio::test]
async fn fs_tools_http_default_mode_preserves_request_body_limit() {
    let port = pick_port().await;
    let _child = spawn_fs_server(port, &[], false);
    let service = connect_client(port).await;
    let response = reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .post(format!("http://127.0.0.1:{port}/mcp"))
        .header("Accept", "application/json, text/event-stream")
        .header("MCP-Protocol-Version", "2025-06-18")
        .json(&serde_json::json!({
            "jsonrpc": "2.0", "id": 2, "method": "tools/call",
            "params": {"name": "write_bytes", "arguments": {
                "path": "unused", "data_base64": "A".repeat(4 * 1024 * 1024),
            }},
        }))
        .send()
        .await
        .expect("send body exceeding default RMCP limit");
    assert_eq!(response.status(), reqwest::StatusCode::PAYLOAD_TOO_LARGE);
    service.cancel().await.expect("cancel client");
}

#[tokio::test]
#[ignore = "full 100 MiB payload; run explicitly to check the transport boundary"]
async fn fs_tools_http_write_bytes_full_limit() {
    write_bytes_round_trip_over_http(
        harnx_core::safety::WRITE_MAX_BYTES,
        &["--sandbox-transport"],
        false,
    )
    .await;
}
