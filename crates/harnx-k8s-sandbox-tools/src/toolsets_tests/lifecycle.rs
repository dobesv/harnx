use super::*;

#[tokio::test(flavor = "multi_thread")]
async fn release_uses_and_clears_the_ambient_binding() -> Result<()> {
    let Some(fixture) = ToolsetFixture::start("session-1", Some("claim-ambient")).await? else {
        return Ok(());
    };
    fixture
        .sandbox
        .invoke_with_context(ToolInvocation {
            tool: "release".to_string(),
            args: json!({}),
            context: ToolInvocationContext {
                operation: None,
                call_id: "call-2".to_string(),
                invoking_session_id: Some("session-1".to_string()),
                capabilities: BTreeSet::new(),
            },
            cancel: CancellationToken::new(),
        })
        .await
        .unwrap();
    assert_eq!(
        fixture.caller.disconnected.lock().as_slice(),
        ["claim-ambient"]
    );
    assert_eq!(
        fixture.api.replica_updates.lock().as_slice(),
        [("claim-ambient".to_string(), 0)]
    );

    fixture
        .sandbox
        .invoke_with_context(ToolInvocation {
            tool: "release".to_string(),
            args: json!({"destroy": true}),
            context: ToolInvocationContext {
                operation: None,
                call_id: "call-3".to_string(),
                invoking_session_id: Some("session-1".to_string()),
                capabilities: BTreeSet::new(),
            },
            cancel: CancellationToken::new(),
        })
        .await
        .unwrap();
    assert_eq!(fixture.api.deletes.lock().as_slice(), ["claim-ambient"]);
    assert!(!fixture
        .metadata
        .get_tool_context("session-1")
        .await?
        .unwrap()
        .values
        .contains_key(SANDBOX_CONTEXT_KEY));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn connect_clones_after_a_retry_and_binds_the_session() -> Result<()> {
    harnx_core::require_nextest();
    let Some(fixture) = ToolsetFixture::start("session-clone", None).await? else {
        return Ok(());
    };
    fixture.caller.responses.lock().extend([
            Ok(json!({
                "isError": true,
                "content": [{"type": "text", "text": "execution_id: first\nexit_code: 128\nfatal: Repository not found."}]
            })),
            Ok(json!({
                "content": [{"type": "text", "text": "execution_id: second\nexit_code: 0\n<!-- start stdout -->\n```\nmain\n```\n<!-- end stdout -->"}]
            })),
        ]);
    let result = fixture
        .sandbox
        .invoke_with_context(ToolInvocation {
            tool: "connect".to_string(),
            args: json!({
                "sandbox_id": "claim-clone",
                "repos": [{"repo_url": "https://github.com/acme/widgets.git"}]
            }),
            context: ToolInvocationContext {
                operation: None,
                call_id: "call-clone".to_string(),
                invoking_session_id: Some("session-clone".to_string()),
                capabilities: BTreeSet::new(),
            },
            cancel: CancellationToken::new(),
        })
        .await
        .unwrap();

    assert_eq!(result["sandbox_id"], "claim-clone");
    assert_eq!(result["repos"][0]["clone_path"], "/workspace/widgets");
    assert_eq!(result["repos"][0]["branch"], "main");
    assert!(result["repos"][0].get("error").is_none());
    {
        let calls = fixture.caller.calls.lock();
        assert_eq!(calls.len(), 2);
        assert!(calls
            .iter()
            .all(|call| call.tool == "bash_exec" && call.sandbox_id == "claim-clone"));
    }
    let binding = bound_sandbox(&fixture.metadata, "session-clone").await?;
    assert_eq!(binding.sandbox_id, "claim-clone");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn connect_binds_before_a_cancelled_clone_returns() -> Result<()> {
    harnx_core::require_nextest();
    let Some(fixture) = ToolsetFixture::start("session-cancelled-clone", None).await? else {
        return Ok(());
    };
    fixture
        .caller
        .responses
        .lock()
        .push_back(Err(McpCallError::cancelled("response", 1)));

    let error = fixture
        .sandbox
        .invoke_with_context(ToolInvocation {
            tool: "connect".to_string(),
            args: json!({
                "sandbox_id": "claim-cancelled-clone",
                "repos": [{"repo_url": "https://github.com/acme/widgets.git"}]
            }),
            context: ToolInvocationContext {
                operation: None,
                call_id: "call-cancelled-clone".to_string(),
                invoking_session_id: Some("session-cancelled-clone".to_string()),
                capabilities: BTreeSet::new(),
            },
            cancel: CancellationToken::new(),
        })
        .await
        .unwrap_err();

    assert!(matches!(error, ToolInvokeError::Fatal(_)));
    let binding = bound_sandbox(&fixture.metadata, "session-cancelled-clone").await?;
    assert_eq!(binding.sandbox_id, "claim-cancelled-clone");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn ambiguous_post_dispatch_failure_is_not_replayed() -> Result<()> {
    harnx_core::require_nextest();
    let Some(fixture) = ToolsetFixture::start("session-no-replay", Some("claim-no-replay")).await?
    else {
        return Ok(());
    };
    fixture
        .caller
        .responses
        .lock()
        .push_back(Err(McpCallError::call(
            crate::policy::FailureKind::Transport,
            "response",
            1,
            "connection lost after dispatch",
        )));

    let error = fixture
        .bash
        .invoke_with_context(ToolInvocation {
            tool: "exec".to_string(),
            args: json!({"command": "touch /workspace/once"}),
            context: ToolInvocationContext {
                operation: None,
                call_id: "call-no-replay".to_string(),
                invoking_session_id: Some("session-no-replay".to_string()),
                capabilities: BTreeSet::new(),
            },
            cancel: CancellationToken::new(),
        })
        .await
        .unwrap_err();

    assert!(matches!(error, ToolInvokeError::Recoverable(_)));
    assert_eq!(fixture.caller.calls.lock().len(), 1);
    Ok(())
}
