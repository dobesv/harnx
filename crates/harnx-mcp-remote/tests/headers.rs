mod common;

use anyhow::Result;
use common::{spawn_http_test_server, spawn_proxy_client, text_content, tool_args};
use rmcp::model::CallToolRequestParams;
use serde_json::json;

#[tokio::test(flavor = "multi_thread")]
async fn custom_headers_are_forwarded_to_remote_server() -> Result<()> {
    let server = spawn_http_test_server(None, true).await?;
    let url = format!("http://127.0.0.1:{}/mcp", server.port);
    let proxy = spawn_proxy_client(&["--url", &url, "--header", "X-Custom:myvalue"]).await?;
    let peer = proxy.peer().clone();

    let result = peer
        .call_tool(
            CallToolRequestParams::new("echo")
                .with_arguments(tool_args(json!({ "text": "header check" }))),
        )
        .await?;
    let payload: serde_json::Value = serde_json::from_str(&text_content(&result))?;
    assert_eq!(payload["headers"]["x-custom"], "myvalue");

    proxy.cancel().await?;
    server.shutdown().await;
    Ok(())
}
