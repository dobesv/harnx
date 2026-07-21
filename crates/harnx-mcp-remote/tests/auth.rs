mod common;

use anyhow::Result;
use common::{spawn_auth_guard_server, spawn_proxy_client, text_content, tool_args};
use rmcp::model::CallToolRequestParams;
use serde_json::json;

#[tokio::test(flavor = "multi_thread")]
async fn missing_bearer_token_surfaces_remote_401() -> Result<()> {
    let server = spawn_auth_guard_server("Bearer test-token").await?;
    let url = format!("http://127.0.0.1:{}/mcp", server.port);
    let err = spawn_proxy_client(&["--url", &url])
        .await
        .expect_err("expected unauthorized remote initialize to fail");
    let message = err.to_string();
    assert!(
        message.contains("401") || message.contains("unauthorized"),
        "expected 401/unauthorized in error, got: {message}"
    );

    server.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn bearer_token_allows_proxy_calls() -> Result<()> {
    let server = spawn_auth_guard_server("Bearer test-token").await?;
    let url = format!("http://127.0.0.1:{}/mcp", server.port);
    let proxy = spawn_proxy_client(&["--url", &url, "--bearer-token", "test-token"]).await?;
    let peer = proxy.peer().clone();

    let tools = peer.list_tools(None).await?;
    assert!(tools.tools.iter().any(|tool| tool.name.as_ref() == "echo"));

    let result = peer
        .call_tool(
            CallToolRequestParams::new("echo")
                .with_arguments(tool_args(json!({ "text": "auth ok" }))),
        )
        .await?;
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&text_content(&result))?["text"],
        "auth ok"
    );

    proxy.cancel().await?;
    server.shutdown().await;
    Ok(())
}
