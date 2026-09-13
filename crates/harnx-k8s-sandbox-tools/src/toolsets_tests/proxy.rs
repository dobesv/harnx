use super::*;

#[tokio::test(flavor = "multi_thread")]
async fn proxy_requires_or_resolves_an_ambient_session_binding() -> Result<()> {
    harnx_core::require_nextest();
    let Some(fixture) = ToolsetFixture::start("session-1", Some("claim-ambient")).await? else {
        return Ok(());
    };

    let missing = fixture
        .bash
        .invoke_with_context(ToolInvocation {
            tool: "exec".to_string(),
            args: json!({"command": "pwd"}),
            context: ToolInvocationContext::default(),
            cancel: CancellationToken::new(),
        })
        .await
        .unwrap_err();
    assert!(matches!(missing, ToolInvokeError::Recoverable(_)));
    assert!(missing.to_string().contains("no sandbox is bound"));

    let result = fixture
        .bash
        .invoke_with_context(ToolInvocation {
            tool: "exec".to_string(),
            args: json!({"command": "pwd"}),
            context: ToolInvocationContext {
                operation: None,
                call_id: "call-1".to_string(),
                invoking_session_id: Some("session-1".to_string()),
                capabilities: BTreeSet::from([
                    harnx_core::execution_context::EXECUTION_CONTEXT_NAMESPACE.to_string(),
                ]),
            },
            cancel: CancellationToken::new(),
        })
        .await
        .unwrap();

    assert_eq!(result["content"][0]["text"], "proxied");
    assert_eq!(fixture.api.seen_ids.lock().as_slice(), ["claim-ambient"]);
    assert_eq!(fixture.api.activity.lock().as_slice(), ["claim-ambient"]);
    {
        let calls = fixture.caller.calls.lock();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].sandbox_id, "claim-ambient");
        assert_eq!(calls[0].endpoint, "http://10.0.0.8:8080/mcp");
        assert_eq!(calls[0].tool, "bash_exec");
        assert_eq!(
            calls[0].args,
            Map::from_iter([("command".to_string(), json!("pwd"))])
        );
        assert!(calls[0]
            .capabilities
            .contains(harnx_core::execution_context::EXECUTION_CONTEXT_NAMESPACE));
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn proxy_explicit_override_is_one_call_and_not_forwarded() -> Result<()> {
    let Some(fixture) = ToolsetFixture::start("session-1", Some("claim-ambient")).await? else {
        return Ok(());
    };
    fixture
        .bash
        .invoke_with_context(ToolInvocation {
            tool: "exec".to_string(),
            args: json!({"command": "pwd", "sandbox_id": "claim-explicit"}),
            context: ToolInvocationContext {
                operation: None,
                call_id: "call-explicit".to_string(),
                invoking_session_id: Some("session-1".to_string()),
                capabilities: BTreeSet::new(),
            },
            cancel: CancellationToken::new(),
        })
        .await
        .unwrap();

    let calls = fixture.caller.calls.lock();
    assert_eq!(calls[0].sandbox_id, "claim-explicit");
    assert!(!calls[0].args.contains_key("sandbox_id"));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn proxy_forwards_cancellation_after_the_mcp_call_starts() -> Result<()> {
    harnx_core::require_nextest();
    let Some(fixture) = ToolsetFixture::start("session-cancel", Some("claim-cancel")).await? else {
        return Ok(());
    };
    fixture
        .caller
        .wait_for_cancellation
        .store(true, Ordering::SeqCst);
    let call_started = fixture.caller.call_started.notified();
    let cancel = CancellationToken::new();
    let invocation_cancel = cancel.clone();
    let bash = fixture.bash.clone();
    let call = tokio::spawn(async move {
        bash.invoke_with_context(ToolInvocation {
            tool: "exec".to_string(),
            args: json!({"command": "sleep 30"}),
            context: ToolInvocationContext {
                operation: None,
                call_id: "call-cancel".to_string(),
                invoking_session_id: Some("session-cancel".to_string()),
                capabilities: BTreeSet::new(),
            },
            cancel: invocation_cancel,
        })
        .await
    });

    call_started.await;
    cancel.cancel();
    let error = tokio::time::timeout(Duration::from_secs(1), call)
        .await??
        .unwrap_err();

    assert!(matches!(error, ToolInvokeError::Fatal(_)));
    assert!(fixture.caller.cancellation_observed.load(Ordering::SeqCst));
    Ok(())
}

#[tokio::test]
async fn heartbeat_cancellation_does_not_drop_mcp_stop_waiter() -> Result<()> {
    harnx_core::require_nextest();
    let Some(fixture) = ToolsetFixture::start("session-heartbeat", None).await? else {
        return Ok(());
    };
    // The broker uses real time. Pause only after its I/O has completed so
    // auto-advance cannot expire setup requests before NATS answers them.
    tokio::time::pause();
    fixture.api.hold_activity.store(true, Ordering::SeqCst);
    fixture
        .caller
        .wait_for_cancellation
        .store(true, Ordering::SeqCst);
    fixture
        .caller
        .hold_after_cancellation
        .store(true, Ordering::SeqCst);
    let activity_started = fixture.api.activity_started.notified();
    let gateway = Gateway {
        manager: SandboxManager::new(fixture.api.clone(), SandboxManagerConfig::default()),
        caller: fixture.caller.clone(),
        metadata: fixture.metadata.clone(),
    };
    let cancel = CancellationToken::new();
    let call_cancel = cancel.clone();
    let call = tokio::spawn(async move {
        gateway
            .call_with_activity_heartbeat(
                "claim-heartbeat",
                "http://10.0.0.8:8080/mcp",
                "bash_exec",
                Map::new(),
                BTreeSet::new(),
                call_cancel,
            )
            .await
    });

    activity_started.await;
    cancel.cancel();
    while !fixture.caller.cancellation_observed.load(Ordering::SeqCst) {
        tokio::task::yield_now().await;
    }
    tokio::task::yield_now().await;
    assert!(
        !call.is_finished(),
        "heartbeat dropped MCP cancellation waiter"
    );

    fixture.caller.finish_cancelled_call.notify_one();
    let error = call.await?.unwrap_err();
    assert_eq!(error.kind, McpCallErrorKind::Cancelled);
    Ok(())
}
