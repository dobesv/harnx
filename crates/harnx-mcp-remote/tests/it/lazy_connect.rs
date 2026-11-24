use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::{Context, Result};
use rmcp::model::{CallToolRequestParams, ProtocolVersion};
use rmcp::service::{ClientLifecycleMode, RoleClient};
use rmcp::Peer;
use serde_json::json;
use tokio::time::timeout;

use crate::common::{
    spawn_http_test_server, spawn_http_test_server_on_listener, spawn_proxy_client_with_lifecycle,
    text_content, tool_args, RecordedMethods,
};

fn auto_lifecycle() -> ClientLifecycleMode {
    ClientLifecycleMode::Auto {
        preferred_versions: vec![ProtocolVersion::V_2026_07_28],
        legacy_version: Some(ProtocolVersion::V_2025_11_25),
    }
}

fn assert_no_initialize(methods: &RecordedMethods) {
    let methods = methods.lock().expect("recorded downstream methods");
    assert_eq!(methods.first().map(String::as_str), Some("server/discover"));
    assert!(!methods.iter().any(|method| method == "initialize"));
    assert!(!methods
        .iter()
        .any(|method| method == "notifications/initialized"));
}

async fn assert_echo_call(peer: &Peer<RoleClient>) -> Result<()> {
    let result = peer
        .call_tool(
            CallToolRequestParams::new("echo")
                .with_arguments(tool_args(json!({ "text": "lazy upstream" }))),
        )
        .await?;
    assert_ne!(result.is_error, Some(true));
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&text_content(&result))?["text"],
        "lazy upstream"
    );
    Ok(())
}

async fn assert_first_request(peer: &Peer<RoleClient>, method: &str) -> Result<()> {
    match method {
        "tools/list" => assert_eq!(peer.list_tools(None).await?.tools[0].name, "echo"),
        "tools/call" => assert_echo_call(peer).await?,
        "prompts/list" => assert_eq!(
            peer.list_prompts(None).await?.prompts[0].name,
            "upstream-prompt"
        ),
        "resources/list" => assert_eq!(
            peer.list_resources(None).await?.resources[0].uri,
            "test://upstream"
        ),
        _ => unreachable!("unknown test request"),
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn discover_flow_connects_on_first_proxied_request() -> Result<()> {
    let server = spawn_http_test_server(None, false).await?;
    let url = server.url();
    // Each handler must open the connection itself, without relying on tools/list first.
    for (index, method) in ["tools/list", "tools/call", "prompts/list", "resources/list"]
        .into_iter()
        .enumerate()
    {
        let (proxy, methods) =
            spawn_proxy_client_with_lifecycle(&["--url", &url], auto_lifecycle()).await?;
        assert_no_initialize(&methods);
        assert_eq!(server.initialize_count.load(Ordering::SeqCst), index);

        timeout(
            Duration::from_secs(5),
            assert_first_request(proxy.peer(), method),
        )
        .await??;
        assert_echo_call(proxy.peer()).await?;
        assert_eq!(proxy.peer().list_tools(None).await?.tools[0].name, "echo");
        assert_eq!(server.initialize_count.load(Ordering::SeqCst), index + 1);
        assert_no_initialize(&methods);
        proxy.cancel().await?;
    }
    server.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_first_requests_share_upstream_connection() -> Result<()> {
    let server = spawn_http_test_server(None, false).await?;
    let url = server.url();
    let (proxy, methods) =
        spawn_proxy_client_with_lifecycle(&["--url", &url], auto_lifecycle()).await?;
    assert_eq!(server.initialize_count.load(Ordering::SeqCst), 0);

    let mut requests = tokio::task::JoinSet::new();
    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(8));
    for _ in 0..8 {
        let peer = proxy.peer().clone();
        let barrier = barrier.clone();
        requests.spawn(async move {
            barrier.wait().await;
            peer.list_tools(None).await
        });
    }
    timeout(Duration::from_secs(5), async {
        while let Some(result) = requests.join_next().await {
            assert_eq!(result??.tools[0].name, "echo");
        }
        anyhow::Ok(())
    })
    .await??;
    assert_eq!(server.initialize_count.load(Ordering::SeqCst), 1);
    assert_no_initialize(&methods);

    proxy.cancel().await?;
    server.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn failed_lazy_connect_retries_on_same_downstream_connection() -> Result<()> {
    // A bound, non-listening socket silently drops SYNs on macOS rather than
    // refusing connections. Return an explicit HTTP failure instead, retaining
    // the same listener for recovery so no process can claim its port.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await?
        .into_std()?;
    let url = format!("http://{}/mcp", listener.local_addr()?);
    let failing_listener = tokio::net::TcpListener::from_std(listener.try_clone()?)?;
    let (stop_failing, stopped) = tokio::sync::oneshot::channel::<()>();
    let failing_server = tokio::spawn(async move {
        axum::serve(
            failing_listener,
            axum::Router::new().fallback(|| async { axum::http::StatusCode::SERVICE_UNAVAILABLE }),
        )
        .with_graceful_shutdown(async {
            let _ = stopped.await;
        })
        .await
    });
    let (proxy, methods) =
        spawn_proxy_client_with_lifecycle(&["--url", &url], auto_lifecycle()).await?;
    let err = timeout(Duration::from_secs(5), proxy.peer().list_tools(None))
        .await
        .context("failed upstream handshake should return a JSON-RPC error")?
        .expect_err("upstream rejected the handshake");
    let rmcp::service::ServiceError::McpError(err) = err else {
        panic!("expected JSON-RPC error, got {err:?}");
    };
    assert!(
        err.message
            .contains("failed to connect to remote MCP server"),
        "{err}"
    );
    assert!(
        err.message.contains("HTTP 503"),
        "missing underlying connect error: {err}"
    );
    assert!(!err.message.contains("remote MCP peer not initialized"));

    stop_failing.send(()).expect("stop failing upstream");
    timeout(Duration::from_secs(5), failing_server)
        .await
        .context("failing upstream should shut down")???;
    let server = spawn_http_test_server_on_listener(
        tokio::net::TcpListener::from_std(listener)?,
        None,
        false,
    )
    .await?;
    let tools = timeout(Duration::from_secs(5), proxy.peer().list_tools(None)).await??;
    assert_eq!(tools.tools[0].name, "echo");
    assert_eq!(server.initialize_count.load(Ordering::SeqCst), 1);
    assert_no_initialize(&methods);

    proxy.cancel().await?;
    server.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn legacy_initialize_preserves_upstream_fields_and_proxy_identity() -> Result<()> {
    let server = spawn_http_test_server(None, false).await?;
    let url = server.url();
    let upstream = rmcp::service::serve_client(
        crate::common::TestClientHandler,
        rmcp::transport::StreamableHttpClientTransport::from_uri(url.clone()),
    )
    .await?;
    let (proxy, methods) =
        spawn_proxy_client_with_lifecycle(&["--url", &url], ClientLifecycleMode::Initialize)
            .await?;
    let expected = upstream.peer_info().expect("upstream initialize response");
    let actual = proxy.peer_info().expect("proxy initialize response");
    assert_eq!(actual.protocol_version, expected.protocol_version);
    assert_eq!(actual.capabilities, expected.capabilities);
    assert_eq!(actual.instructions, expected.instructions);
    assert_eq!(actual.meta, expected.meta);
    assert_eq!(
        actual.server_info.as_ref().unwrap().name,
        "harnx-mcp-remote"
    );
    assert_ne!(actual.server_info, expected.server_info);
    assert_eq!(methods.lock().unwrap()[0], "initialize");
    assert_eq!(server.initialize_count.load(Ordering::SeqCst), 2);
    assert_echo_call(proxy.peer()).await?;

    upstream.cancel().await?;
    proxy.cancel().await?;
    server.shutdown().await;
    Ok(())
}
