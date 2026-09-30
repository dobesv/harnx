//! HTTP-mode integration tests for harnx-exa-tools.
use std::process::Stdio;

use rmcp::handler::client::ClientHandler;
use rmcp::model::{
    CallToolRequestParams, ClientCapabilities, ErrorCode, Implementation, InitializeRequestParams,
};
use rmcp::service::ServiceError;
use rmcp::transport::streamable_http_client::{
    StreamableHttpClientTransport, StreamableHttpClientTransportConfig,
};
use serde_json::json;

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

async fn pick_port() -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    listener.local_addr().expect("get local addr").port()
}

async fn wait_for_port(port: u16) {
    let address = format!("127.0.0.1:{port}");
    let start = std::time::Instant::now();
    while start.elapsed() < std::time::Duration::from_secs(5) {
        if tokio::net::TcpStream::connect(&address).await.is_ok() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("timed out waiting for 127.0.0.1:{port} to accept connections");
}

fn spawn_exa_server(port: u16) -> ChildGuard {
    let bin = std::env::var("CARGO_BIN_EXE_harnx-exa-tools")
        .expect("CARGO_BIN_EXE_harnx-exa-tools must be set");
    let child = std::process::Command::new(bin)
        .arg("--mcp-http")
        .arg("--host")
        .arg("127.0.0.1")
        .arg("--port")
        .arg(port.to_string())
        .env_remove("EXA_API_KEY")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn exa-tools");
    ChildGuard(child)
}

#[tokio::test]
async fn exa_tools_http_initialize_list_and_reject_unknown_tool() {
    let port = pick_port().await;
    let _child = spawn_exa_server(port);
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

    let info = peer.peer_info().expect("peer info after handshake");
    assert!(info.capabilities.tools.is_some());

    let tools = peer.list_tools(None).await.expect("list_tools");
    let tool_names = tools
        .tools
        .iter()
        .map(|tool| tool.name.as_ref())
        .collect::<Vec<_>>();
    assert!(tool_names.contains(&"web_search_exa"), "{tool_names:?}");
    assert!(tool_names.contains(&"web_fetch_exa"), "{tool_names:?}");
    assert_eq!(tool_names.len(), 2);

    let missing_key = peer
        .call_tool(
            CallToolRequestParams::new("web_search_exa").with_arguments(
                json!({"query": "rust"})
                    .as_object()
                    .expect("arguments are an object")
                    .clone(),
            ),
        )
        .await
        .expect("known-tool failures are MCP tool results");
    assert_eq!(missing_key.is_error, Some(true));
    let error_text = missing_key
        .content
        .iter()
        .find_map(|content| content.as_text().map(|text| text.text.as_str()))
        .expect("error result contains text");
    assert_eq!(
        error_text,
        "❌ Error: EXA_API_KEY is not set. Get a key at https://exa.ai and set it in ~/.local/share/harnx/.env"
    );

    let error = peer
        .call_tool(CallToolRequestParams::new("nonexistent_tool"))
        .await
        .expect_err("unknown tool should fail");
    match error {
        ServiceError::McpError(error) => assert_eq!(error.code, ErrorCode::INVALID_PARAMS),
        other => panic!("expected MCP invalid_params error, got {other:?}"),
    }

    service.cancel().await.expect("cancel client");
}
