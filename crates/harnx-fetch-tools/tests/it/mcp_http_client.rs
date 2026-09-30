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
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    listener.local_addr().unwrap().port()
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
    panic!("timed out waiting for {address}");
}

fn spawn_server(port: u16) -> ChildGuard {
    let bin = std::env::var("CARGO_BIN_EXE_harnx-fetch-tools")
        .expect("CARGO_BIN_EXE_harnx-fetch-tools must be set");
    ChildGuard(
        std::process::Command::new(bin)
            .args([
                "--mcp-http",
                "--host",
                "127.0.0.1",
                "--port",
                &port.to_string(),
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn fetch-tools"),
    )
}

#[tokio::test]
async fn http_mode_lists_tools_and_returns_ssrf_as_tool_error() {
    let port = pick_port().await;
    let _child = spawn_server(port);
    wait_for_port(port).await;

    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let config =
        StreamableHttpClientTransportConfig::with_uri(format!("http://127.0.0.1:{port}/mcp"));
    let transport = StreamableHttpClientTransport::with_client(client, config);
    let service = rmcp::service::serve_client(TestClient, transport)
        .await
        .expect("connect MCP client");
    let peer = service.peer().clone();

    let names = peer
        .list_tools(None)
        .await
        .unwrap()
        .tools
        .into_iter()
        .map(|tool| tool.name.into_owned())
        .collect::<Vec<_>>();
    assert_eq!(
        names,
        [
            "fetch_html",
            "fetch_markdown",
            "fetch_txt",
            "fetch_json",
            "fetch_readable",
            "fetch_youtube_transcript"
        ]
    );

    let blocked = peer
        .call_tool(
            CallToolRequestParams::new("fetch_html").with_arguments(
                json!({"url":"http://127.0.0.1:9/private"})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        )
        .await
        .expect("domain failures are tool results");
    assert_eq!(blocked.is_error, Some(true));
    let message = blocked
        .content
        .iter()
        .find_map(|content| content.as_text().map(|text| text.text.as_str()))
        .unwrap();
    assert!(
        message.contains("blocked by private-IP policy"),
        "{message}"
    );

    let error = peer
        .call_tool(CallToolRequestParams::new("missing"))
        .await
        .expect_err("unknown tool is protocol invalid_params");
    match error {
        ServiceError::McpError(error) => assert_eq!(error.code, ErrorCode::INVALID_PARAMS),
        other => panic!("expected invalid_params, got {other:?}"),
    }

    service.cancel().await.unwrap();
}
