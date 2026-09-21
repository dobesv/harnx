//! HTTP-mode integration tests for harnx-plans-tools.
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

struct ServerGuard {
    _temp_dir: tempfile::TempDir,
    child: std::process::Child,
}

impl Drop for ServerGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
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

/// Spawn harnx-plans-tools with --mcp-http on the given port.
fn spawn_plans_server(port: u16) -> ServerGuard {
    let bin = std::env::var("CARGO_BIN_EXE_harnx-plans-tools")
        .expect("CARGO_BIN_EXE_harnx-plans-tools must be set");
    let temp_dir = tempfile::tempdir().expect("create temp plans dir");
    let dir_path = temp_dir.path().to_string_lossy().to_string();

    let child = std::process::Command::new(bin)
        .arg("--mcp-http")
        .arg("--host")
        .arg("127.0.0.1")
        .arg("--port")
        .arg(port.to_string())
        .arg("--dir")
        .arg(&dir_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn plans-tools");
    ServerGuard {
        _temp_dir: temp_dir,
        child,
    }
}

#[tokio::test]
async fn plans_tools_http_initialize_list_call_unknown() {
    let port = pick_port().await;
    let _server = spawn_plans_server(port);
    wait_for_port(port).await;

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

    // 1. Initialize succeeds - Plans uses bespoke path with serverInfo.name "harnx-plans-tools"
    let info = peer.peer_info().expect("peer info after handshake");
    assert!(
        info.capabilities.tools.is_some(),
        "server should advertise tools capability"
    );
    // Plans has instructions
    assert!(
        info.instructions.is_some(),
        "plans server should have instructions"
    );

    // 2. list_tools returns expected tools
    let tools = peer.list_tools(None).await.expect("list_tools");
    let tool_names: Vec<_> = tools.tools.iter().map(|t| t.name.as_ref()).collect();
    assert!(tool_names.contains(&"list_plans"), "missing list_plans");
    assert!(tool_names.contains(&"add_plan"), "missing add_plan");

    // 3. Initialize + list_tools + unknown-tool is sufficient (plans needs --dir for tools)

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
