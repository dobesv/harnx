use super::*;

fn connect_invocation(call_id: &str, session_id: &str, args: Value) -> ToolInvocation {
    ToolInvocation {
        tool: "connect".to_string(),
        args,
        context: ToolInvocationContext {
            run_context: None,
            call_id: call_id.to_string(),
            invoking_session_id: Some(harnx_core::session_identity::session_key(
                Some("coder"),
                session_id,
            )),
            invoking_session: None,
            capabilities: BTreeSet::new(),
            checkpoint: None,
            checkpoint_store: None,
            partial_result_store: None,
            progress: Default::default(),
        },
        cancel: CancellationToken::new(),
    }
}
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
                run_context: None,
                call_id: "call-2".to_string(),
                invoking_session_id: Some(harnx_core::session_identity::session_key(
                    Some("coder"),
                    "session-1",
                )),
                invoking_session: None,
                capabilities: BTreeSet::new(),
                checkpoint: None,
                checkpoint_store: None,
                partial_result_store: None,
                progress: Default::default(),
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
                run_context: None,
                call_id: "call-3".to_string(),
                invoking_session_id: Some(harnx_core::session_identity::session_key(
                    Some("coder"),
                    "session-1",
                )),
                invoking_session: None,
                capabilities: BTreeSet::new(),
                checkpoint: None,
                checkpoint_store: None,
                partial_result_store: None,
                progress: Default::default(),
            },
            cancel: CancellationToken::new(),
        })
        .await
        .unwrap();
    assert_eq!(fixture.api.deletes.lock().as_slice(), ["claim-ambient"]);
    assert!(!fixture
        .metadata
        .get_tool_context(&harnx_core::session_identity::session_key(
            Some("coder"),
            "session-1"
        ))
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
                run_context: None,
                call_id: "call-clone".to_string(),
                invoking_session_id: Some(harnx_core::session_identity::session_key(
                    Some("coder"),
                    "session-clone",
                )),
                invoking_session: None,
                capabilities: BTreeSet::new(),
                checkpoint: None,
                checkpoint_store: None,
                partial_result_store: None,
                progress: Default::default(),
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
        assert!(calls.iter().all(|call| {
            call.tool == "exec"
                && call.sandbox_id == "claim-clone"
                && call.endpoint == "http://10.0.0.8:3002/mcp"
        }));
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
                run_context: None,
                call_id: "call-cancelled-clone".to_string(),
                invoking_session_id: Some(harnx_core::session_identity::session_key(
                    Some("coder"),
                    "session-cancelled-clone",
                )),
                invoking_session: None,
                capabilities: BTreeSet::new(),
                checkpoint: None,
                checkpoint_store: None,
                partial_result_store: None,
                progress: Default::default(),
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
                run_context: None,
                call_id: "call-no-replay".to_string(),
                invoking_session_id: Some(harnx_core::session_identity::session_key(
                    Some("coder"),
                    "session-no-replay",
                )),
                invoking_session: None,
                capabilities: BTreeSet::new(),
                checkpoint: None,
                checkpoint_store: None,
                partial_result_store: None,
                progress: Default::default(),
            },
            cancel: CancellationToken::new(),
        })
        .await
        .unwrap_err();

    assert!(matches!(error, ToolInvokeError::Recoverable(_)));
    assert_eq!(fixture.caller.calls.lock().len(), 1);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn connect_without_id_reuses_the_ambient_binding() -> Result<()> {
    let Some(fixture) = ToolsetFixture::start("session-reuse", Some("claim-ambient")).await? else {
        return Ok(());
    };

    let result = fixture
        .sandbox
        .invoke_with_context(connect_invocation("call-reuse", "session-reuse", json!({})))
        .await
        .unwrap();

    assert_eq!(result["sandbox_id"], "claim-ambient");
    assert!(fixture
        .api
        .seen_ids
        .lock()
        .contains(&"claim-ambient".to_string()));
    let binding = bound_sandbox(&fixture.metadata, "session-reuse").await?;
    assert_eq!(binding.sandbox_id, "claim-ambient");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn connect_with_id_resolves_and_updates_the_ambient_binding() -> Result<()> {
    for (session_id, requested_id, expected_id) in [
        ("session-blank", "   ", "claim-ambient"),
        ("session-override", "claim-other", "claim-other"),
    ] {
        let Some(fixture) = ToolsetFixture::start(session_id, Some("claim-ambient")).await? else {
            return Ok(());
        };

        let result = fixture
            .sandbox
            .invoke_with_context(connect_invocation(
                "call-connect",
                session_id,
                json!({"sandbox_id": requested_id}),
            ))
            .await
            .unwrap();

        assert_eq!(result["sandbox_id"], expected_id);
        let binding = bound_sandbox(&fixture.metadata, session_id).await?;
        assert_eq!(binding.sandbox_id, expected_id);
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn connect_without_id_and_no_binding_creates_a_claim() -> Result<()> {
    let Some(fixture) = ToolsetFixture::start("session-create", None).await? else {
        return Ok(());
    };

    let result = fixture
        .sandbox
        .invoke_with_context(connect_invocation(
            "call-create",
            "session-create",
            json!({}),
        ))
        .await
        .unwrap();

    let sandbox_id = result["sandbox_id"].as_str().unwrap();
    assert!(
        sandbox_id.starts_with("sandbox-"),
        "expected a fresh claim name, got {sandbox_id}"
    );
    let binding = bound_sandbox(&fixture.metadata, "session-create").await?;
    assert_eq!(binding.version, 1);
    assert_eq!(binding.sandbox_id, sandbox_id);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn connect_without_id_replaces_an_unusable_binding() -> Result<()> {
    let Some(fixture) = ToolsetFixture::start("session-stale", None).await? else {
        return Ok(());
    };
    for unusable in [
        json!({"version": 2, "sandbox_id": "claim-stale"}),
        json!({"version": 1, "sandbox_id": "  "}),
    ] {
        fixture
            .metadata
            .replace_tool_context_value(
                ToolContextEntry {
                    session_id: &harnx_core::session_identity::session_key(
                        Some("coder"),
                        "session-stale",
                    ),
                    key: SANDBOX_CONTEXT_KEY,
                },
                unusable,
            )
            .await?;

        let result = fixture
            .sandbox
            .invoke_with_context(connect_invocation("call-stale", "session-stale", json!({})))
            .await
            .unwrap();

        let sandbox_id = result["sandbox_id"].as_str().unwrap();
        assert_ne!(sandbox_id, "claim-stale");
        assert!(
            sandbox_id.starts_with("sandbox-"),
            "expected a fresh claim name, got {sandbox_id}"
        );
        let binding = bound_sandbox(&fixture.metadata, "session-stale").await?;
        assert_eq!(binding.version, 1);
        assert_eq!(binding.sandbox_id, sandbox_id);
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn connect_without_id_fails_loudly_on_a_malformed_binding() -> Result<()> {
    let Some(fixture) = ToolsetFixture::start("session-corrupt", None).await? else {
        return Ok(());
    };
    fixture
        .metadata
        .replace_tool_context_value(
            ToolContextEntry {
                session_id: &harnx_core::session_identity::session_key(
                    Some("coder"),
                    "session-corrupt",
                ),
                key: SANDBOX_CONTEXT_KEY,
            },
            json!({"version": 1, "sandbox_id": 42}),
        )
        .await?;

    let error = fixture
        .sandbox
        .invoke_with_context(connect_invocation(
            "call-corrupt",
            "session-corrupt",
            json!({}),
        ))
        .await
        .unwrap_err();

    assert!(matches!(error, ToolInvokeError::Recoverable(_)));
    Ok(())
}
