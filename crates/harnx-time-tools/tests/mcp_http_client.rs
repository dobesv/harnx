//! HTTP-mode integration tests for harnx-time-tools.
use std::process::Stdio;

use rmcp::handler::client::ClientHandler;
use rmcp::model::{
    CallToolRequestParams, ClientCapabilities, ErrorCode, Implementation, InitializeRequestParams,
};
use rmcp::service::ServiceError;
use rmcp::transport::streamable_http_client::{
    StreamableHttpClientTransport, StreamableHttpClientTransportConfig,
};
use serde_json::Map;

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

/// Spawn harnx-time-tools with --mcp-http on the given port.
fn spawn_time_server(port: u16) -> ChildGuard {
    let bin = std::env::var("CARGO_BIN_EXE_harnx-time-tools")
        .expect("CARGO_BIN_EXE_harnx-time-tools must be set");
    let child = std::process::Command::new(bin)
        .arg("--mcp-http")
        .arg("--host")
        .arg("127.0.0.1")
        .arg("--port")
        .arg(port.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn time-tools");
    ChildGuard(child)
}

#[tokio::test]
async fn time_tools_http_initialize_list_call_unknown() {
    let port = pick_port().await;
    let _child = spawn_time_server(port);
    wait_for_port(port).await;

    // Give the server time to bind and start listening
    let url = format!("http://127.0.0.1:{port}/mcp");
    let client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("build reqwest client");
    let config = StreamableHttpClientTransportConfig::with_uri(url.as_str());
    let transport = StreamableHttpClientTransport::with_client(client, config);

    let service = rmcp::service::serve_client(TestClient, transport)
        .await
        .expect("connect rmcp client");
    let peer = service.peer().clone();

    // 1. Initialize succeeds (peer_info already populated by serve_client)
    let info = peer.peer_info().expect("peer info after handshake");
    assert!(
        info.capabilities.tools.is_some(),
        "server should advertise tools capability"
    );

    // 2. list_tools returns expected tools
    let tools = peer.list_tools(None).await.expect("list_tools");
    let tool_names: Vec<_> = tools.tools.iter().map(|t| t.name.as_ref()).collect();
    assert!(
        tool_names.contains(&"get_current_time"),
        "missing get_current_time"
    );
    assert!(tool_names.contains(&"convert_time"), "missing convert_time");

    // 3. Representative call_tool succeeds (deterministic/offline)
    let mut args = Map::new();
    args.insert("timezone".to_string(), serde_json::json!("UTC"));
    let result = peer
        .call_tool(CallToolRequestParams::new("get_current_time").with_arguments(args))
        .await
        .expect("call_tool get_current_time");
    assert_ne!(
        result.is_error,
        Some(true),
        "get_current_time should succeed"
    );
    assert!(!result.content.is_empty(), "result should have content");

    // 4. Unknown tool returns JSON-RPC -32602 invalid_params
    let err = peer
        .call_tool(CallToolRequestParams::new("nonexistent_tool"))
        .await
        .expect_err("unknown tool should fail");
    match err {
        ServiceError::McpError(error) => {
            assert_eq!(error.code, ErrorCode::INVALID_PARAMS);
        }
        other => panic!("expected MCP invalid_params error, got {other:?}"),
    }

    service.cancel().await.expect("cancel client");
}
