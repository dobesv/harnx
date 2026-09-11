use super::*;

#[tokio::test]
async fn sandbox_cancel_requires_remote_ack_and_preserves_shared_session() -> Result<()> {
    harnx_core::require_nextest();
    let mcp = TestMcp::start().await?;
    assert_eq!(response_text(&mcp.increment("shared").await?), "1");
    {
        let cancel = CancellationToken::new();
        let call = mcp.call("shared", "blocking", cancel.clone());
        tokio::pin!(call);
        assert!(tokio::time::timeout(Duration::from_millis(100), &mut call)
            .await
            .is_err());
        cancel.cancel();
        assert!(tokio::time::timeout(Duration::from_millis(100), &mut call)
            .await
            .is_err());
        // A counter reset would expose invalidation/reconnection of the shared
        // MCP session while its unrelated call is still entitled to use it.
        assert_eq!(response_text(&mcp.increment("shared").await?), "2");
        assert!(tokio::time::timeout(Duration::from_millis(100), &mut call)
            .await
            .is_err());
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
async fn response_deadline_uses_waiter_preserving_stop_path() -> Result<()> {
    harnx_core::require_nextest();
    let mcp = TestMcp::start_with_config(McpCallerConfig {
        response_timeout: Some(Duration::from_millis(50)),
        ..McpCallerConfig::default()
    })
    .await?;
    assert_eq!(response_text(&mcp.increment("deadline").await?), "1");

    {
        let call = mcp.call("deadline", "blocking", CancellationToken::new());
        tokio::pin!(call);
        assert!(tokio::time::timeout(Duration::from_millis(150), &mut call)
            .await
            .is_err());
        assert_eq!(response_text(&mcp.increment("deadline").await?), "2");
        assert!(tokio::time::timeout(Duration::from_millis(100), &mut call)
            .await
            .is_err());
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
