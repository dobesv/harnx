use super::*;
use crate::mcp::StreamableHttpMcpCaller;
use crate::policy::FailureKind;
use rmcp::transport::streamable_http_server::{
    session::local::LocalSessionManager, StreamableHttpServerConfig, StreamableHttpService,
};

// Exercise the real fs MCP contract, including the typed error source retained by
// StreamableHttpMcpCaller, before feeding its response to the recording gateway.
async fn unsupported_pod_error(tool: &str) -> Result<McpCallError> {
    let shutdown = CancellationToken::new();
    let service: StreamableHttpService<harnx_fs_tools::FsServer, LocalSessionManager> =
        StreamableHttpService::new(
            || Ok(harnx_fs_tools::FsServer::new(Default::default())),
            Default::default(),
            StreamableHttpServerConfig::default()
                .with_json_response(true)
                .with_cancellation_token(shutdown.child_token())
                .disable_allowed_hosts(),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = format!("http://{}/mcp", listener.local_addr()?);
    let server_shutdown = shutdown.clone();
    let server = tokio::spawn(async move {
        axum::serve(listener, axum::Router::new().nest_service("/mcp", service))
            .with_graceful_shutdown(async move { server_shutdown.cancelled().await })
            .await
    });
    let caller = StreamableHttpMcpCaller::new()?;
    let error = caller
        .call(
            CLAIM,
            &endpoint,
            tool,
            Map::new(),
            BTreeSet::new(),
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    caller.disconnect(CLAIM).await;
    shutdown.cancel();
    server.await??;
    Ok(error)
}

#[tokio::test(flavor = "multi_thread")]
async fn attachment_write_old_pod_returns_rollout_error_without_retry() -> Result<()> {
    harnx_core::require_nextest();
    let Some(fixture) = ToolsetFixture::start(SESSION, Some(CLAIM)).await? else {
        return Ok(());
    };
    let url = store_bytes(&fixture, b"old image").await?;
    let pod_error = unsupported_pod_error("write_bytes").await?;
    fixture.caller.responses.lock().push_back(Err(pod_error));
    let error = fixture
        .fs
        .invoke_with_context(invocation(json!({"path": "/workspace/file", "url": url})))
        .await
        .unwrap_err();
    assert!(matches!(error, ToolInvokeError::Recoverable(_)));
    assert_eq!(error.to_string(), "sandbox fs server does not support write_bytes; enable --sandbox-transport (HARNX_FS_SANDBOX_TRANSPORT=1) for harnx-fs-tools in the sandbox image");
    let calls = fixture.caller.calls.lock();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].tool, "write_bytes");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn attachment_write_other_remote_errors_are_not_misclassified_or_retried() -> Result<()> {
    harnx_core::require_nextest();
    let Some(fixture) = ToolsetFixture::start(SESSION, Some(CLAIM)).await? else {
        return Ok(());
    };
    let url = store_bytes(&fixture, b"remote failure").await?;
    let errors = [
        unsupported_pod_error("another_tool").await?,
        McpCallError::call(
            FailureKind::Transport,
            "response",
            1,
            "transport failed with text: unknown tool: write_bytes",
        ),
    ];
    for error in errors {
        let expected = error.to_string();
        fixture.caller.responses.lock().push_back(Err(error));
        let error = fixture
            .fs
            .invoke_with_context(invocation(json!({"path": "/workspace/file", "url": url})))
            .await
            .unwrap_err();
        assert!(matches!(error, ToolInvokeError::Recoverable(_)));
        assert_eq!(error.to_string(), expected);
    }
    assert_eq!(fixture.caller.calls.lock().len(), 2);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn attachment_write_preserves_pod_domain_error_without_retry() -> Result<()> {
    harnx_core::require_nextest();
    let Some(fixture) = ToolsetFixture::start(SESSION, Some(CLAIM)).await? else {
        return Ok(());
    };
    let url = store_bytes(&fixture, b"denied by pod").await?;
    let response = json!({"isError": true, "content": [{"type": "text", "text": "path is outside allowed write paths"}]});
    fixture
        .caller
        .responses
        .lock()
        .push_back(Ok(response.clone()));
    let result = fixture
        .fs
        .invoke_with_context(invocation(json!({"path": "/outside/file", "url": url})))
        .await?;
    assert_eq!(result, response);
    assert_eq!(fixture.caller.calls.lock().len(), 1);
    Ok(())
}
