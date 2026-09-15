use super::*;

#[tokio::test]
async fn sandbox_cancel_drops_waiter_and_preserves_shared_session() -> Result<()> {
    harnx_core::require_nextest();
    let mcp = TestMcp::start().await?;
    assert_eq!(response_text(&mcp.increment("shared").await?), "1");
    {
        let cancel = CancellationToken::new();
        let cleanup = harnx_toolset::cleanup::InvocationCleanup::default();
        let call = harnx_toolset::cleanup::INVOCATION_CLEANUP.scope(
            cleanup.clone(),
            mcp.call("shared", "blocking", cancel.clone()),
        );
        tokio::pin!(call);
        tokio::select! {
            _ = mcp.started.notified() => {},
            result = &mut call => anyhow::bail!("call ended before start barrier: {result:?}"),
        }
        cancel.cancel();
        let error = tokio::time::timeout(Duration::from_secs(2), &mut call)
            .await?
            .unwrap_err();
        assert_eq!(error.kind, McpCallErrorKind::Cancelled);
        assert!(cleanup
            .last_error()
            .unwrap()
            .contains("remote shutdown unconfirmed"));
        // A counter reset would expose invalidation/reconnection of the shared
        // MCP session while its unrelated call is still entitled to use it.
        assert_eq!(response_text(&mcp.increment("shared").await?), "2");
    }
    // Tear down the fixture only after proving that cancellation itself kept
    // the shared session intact. The blocked handler has no shutdown ack.
    mcp.caller.disconnect("shared").await;
    mcp.shutdown.cancel();
    mcp.server.abort();
    let _ = mcp.server.await;
    Ok(())
}

#[tokio::test]
async fn response_deadline_drops_waiter_with_unconfirmed_cleanup() -> Result<()> {
    harnx_core::require_nextest();
    let mcp = TestMcp::start_with_config(McpCallerConfig {
        response_timeout: Some(Duration::from_millis(50)),
        ..McpCallerConfig::default()
    })
    .await?;
    assert_eq!(response_text(&mcp.increment("deadline").await?), "1");

    {
        let cleanup = harnx_toolset::cleanup::InvocationCleanup::default();
        let call = harnx_toolset::cleanup::INVOCATION_CLEANUP.scope(
            cleanup.clone(),
            mcp.call("deadline", "blocking", CancellationToken::new()),
        );
        let error = tokio::time::timeout(Duration::from_secs(2), call)
            .await?
            .unwrap_err();
        assert_eq!(error.end_reason, EndReason::DeadlineExceeded);
        assert!(cleanup.last_error().is_some());
        assert_eq!(response_text(&mcp.increment("deadline").await?), "2");
    }

    mcp.caller.disconnect("deadline").await;
    mcp.shutdown.cancel();
    mcp.server.abort();
    let _ = mcp.server.await;
    Ok(())
}

#[tokio::test]
async fn response_can_outlive_short_pre_dispatch_budget() -> Result<()> {
    harnx_core::require_nextest();
    let mcp = TestMcp::start_with_config(McpCallerConfig {
        pre_dispatch_timeout: Duration::from_millis(50),
        response_timeout: Some(Duration::from_secs(1)),
        ..McpCallerConfig::default()
    })
    .await?;
    assert_eq!(response_text(&mcp.increment("long-call").await?), "1");

    let result = mcp
        .call("long-call", "slow_success", CancellationToken::new())
        .await?;
    assert_eq!(response_text(&result), "2");

    mcp.stop().await
}
