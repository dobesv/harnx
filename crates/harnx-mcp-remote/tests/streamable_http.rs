mod common;

use anyhow::Result;
use common::{spawn_http_test_server, spawn_proxy_client, text_content, tool_args};
use rmcp::model::CallToolRequestParams;
use serde_json::json;

#[tokio::test(flavor = "multi_thread")]
async fn proxies_streamable_http_tools_and_calls() -> Result<()> {
    let server = spawn_http_test_server(None, false).await?;
    let url = format!("http://127.0.0.1:{}/mcp", server.port);
    let proxy = spawn_proxy_client(&["--url", &url]).await?;
    let peer = proxy.peer().clone();

    let proxy_info = peer
        .peer_info()
        .expect("proxy initialize result should be cached after handshake");
    assert!(proxy_info.capabilities.tools.is_some());

    let tools = peer.list_tools(None).await?;
    assert!(
        tools.tools.iter().any(|tool| tool.name.as_ref() == "echo"),
        "expected echo tool in proxied list, got {:?}",
        tools
            .tools
            .iter()
            .map(|tool| tool.name.as_ref())
            .collect::<Vec<_>>()
    );

    let result = peer
        .call_tool(
            CallToolRequestParams::new("echo")
                .with_arguments(tool_args(json!({ "text": "hello through proxy" }))),
        )
        .await?;
    let text = text_content(&result);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&text)?["text"],
        "hello through proxy"
    );

    proxy.cancel().await?;
    server.shutdown().await;
    Ok(())
}
