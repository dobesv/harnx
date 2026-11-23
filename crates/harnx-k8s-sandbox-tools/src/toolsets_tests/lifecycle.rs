use super::*;

pub(super) fn connect_invocation(call_id: &str, session_id: &str, args: Value) -> ToolInvocation {
    ToolInvocation {
        tool: "connect".to_string(),
        args,
        context: ToolInvocationContext {
            call_id: call_id.to_string(),
            invoking_session_id: Some(harnx_core::session_identity::session_key(
                Some("coder"),
                session_id,
            )),
            ..Default::default()
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
                call_id: "call-2".to_string(),
                invoking_session_id: Some(harnx_core::session_identity::session_key(
                    Some("coder"),
                    "session-1",
                )),
                ..Default::default()
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
                call_id: "call-3".to_string(),
                invoking_session_id: Some(harnx_core::session_identity::session_key(
                    Some("coder"),
                    "session-1",
                )),
                ..Default::default()
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
                call_id: "call-clone".to_string(),
                invoking_session_id: Some(harnx_core::session_identity::session_key(
                    Some("coder"),
                    "session-clone",
                )),
                ..Default::default()
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
                call_id: "call-cancelled-clone".to_string(),
                invoking_session_id: Some(harnx_core::session_identity::session_key(
                    Some("coder"),
                    "session-cancelled-clone",
                )),
                ..Default::default()
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
                call_id: "call-no-replay".to_string(),
                invoking_session_id: Some(harnx_core::session_identity::session_key(
                    Some("coder"),
                    "session-no-replay",
                )),
                ..Default::default()
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

/// A `kube::Error::Api` wrapped the same way `KubernetesSandboxApi` wraps
/// failures, so the retry policy classifies it exactly like a real one.
pub(super) fn scripted_api_error(code: u16) -> anyhow::Error {
    let reason = if code == 429 {
        "TooManyRequests"
    } else {
        "Forbidden"
    };
    let mut status = kube::error::Status::failure("scripted Kubernetes error", reason)
        .with_code(code)
        .boxed();
    status.details = Some(kube::core::response::StatusDetails {
        name: String::new(),
        group: String::new(),
        kind: String::new(),
        uid: String::new(),
        causes: Vec::new(),
        retry_after_seconds: 0,
    });
    crate::kubernetes::scripted_kubernetes_error(kube::Error::Api(status))
}

/// Start a fixture, script a 403 for the first sandbox lookup, and run an
/// ID-less connect expected to fail with that activation error. `Ok(None)`
/// means the test broker was unavailable.
pub(super) async fn connect_failing_activation(
    session_id: &str,
    call_id: &str,
    invoking_session_id: &str,
) -> Result<Option<(ToolsetFixture, ToolInvokeError)>> {
    let Some(fixture) = ToolsetFixture::start(session_id, None).await? else {
        return Ok(None);
    };
    fixture
        .api
        .get_errors
        .lock()
        .push_back(scripted_api_error(403));
    let error = fixture
        .sandbox
        .invoke_with_context(connect_invocation(call_id, invoking_session_id, json!({})))
        .await
        .unwrap_err();
    Ok(Some((fixture, error)))
}

#[tokio::test(flavor = "multi_thread")]
async fn connect_without_id_replaces_a_missing_bound_sandbox() -> Result<()> {
    let Some(fixture) = ToolsetFixture::start("session-gone", Some("claim-gone")).await? else {
        return Ok(());
    };
    fixture
        .api
        .missing_ids
        .lock()
        .push("claim-gone".to_string());

    let result = fixture
        .sandbox
        .invoke_with_context(connect_invocation("call-gone", "session-gone", json!({})))
        .await
        .unwrap();

    let sandbox_id = result["sandbox_id"].as_str().unwrap();
    assert!(
        sandbox_id.starts_with("sandbox-"),
        "expected a fresh claim name, got {sandbox_id}"
    );
    assert_ne!(sandbox_id, "claim-gone");
    assert!(fixture
        .api
        .seen_ids
        .lock()
        .contains(&"claim-gone".to_string()));
    assert_eq!(
        fixture.api.creates.lock().as_slice(),
        [sandbox_id.to_string()]
    );
    assert_eq!(
        fixture.caller.disconnected.lock().as_slice(),
        ["claim-gone"]
    );
    let binding = bound_sandbox(&fixture.metadata, "session-gone").await?;
    assert_eq!(binding.version, 1);
    assert_eq!(binding.sandbox_id, sandbox_id);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn connect_with_an_explicit_id_still_fails_when_the_sandbox_is_missing() -> Result<()> {
    let Some(fixture) =
        ToolsetFixture::start("session-explicit-gone", Some("claim-ambient")).await?
    else {
        return Ok(());
    };
    fixture
        .api
        .missing_ids
        .lock()
        .push("claim-explicit-gone".to_string());

    let error = fixture
        .sandbox
        .invoke_with_context(connect_invocation(
            "call-explicit-gone",
            "session-explicit-gone",
            json!({"sandbox_id": "claim-explicit-gone"}),
        ))
        .await
        .unwrap_err();

    assert_eq!(error.to_string(), "sandbox not found: claim-explicit-gone");
    assert!(matches!(error, ToolInvokeError::Recoverable(_)));
    assert!(fixture.api.creates.lock().is_empty());
    assert!(fixture.caller.disconnected.lock().is_empty());
    let binding = bound_sandbox(&fixture.metadata, "session-explicit-gone").await?;
    assert_eq!(binding.sandbox_id, "claim-ambient");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn connect_without_id_keeps_the_binding_on_an_auth_error() -> Result<()> {
    let Some(fixture) = ToolsetFixture::start("session-forbidden", Some("claim-ambient")).await?
    else {
        return Ok(());
    };
    fixture
        .api
        .get_errors
        .lock()
        .push_back(scripted_api_error(403));

    let error = fixture
        .sandbox
        .invoke_with_context(connect_invocation(
            "call-forbidden",
            "session-forbidden",
            json!({}),
        ))
        .await
        .unwrap_err();

    assert!(matches!(error, ToolInvokeError::Recoverable(_)));
    assert!(fixture.api.creates.lock().is_empty());
    assert!(fixture.caller.disconnected.lock().is_empty());
    let binding = bound_sandbox(&fixture.metadata, "session-forbidden").await?;
    assert_eq!(binding.sandbox_id, "claim-ambient");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn connect_without_id_reuses_the_binding_after_a_transient_error() -> Result<()> {
    let Some(fixture) = ToolsetFixture::start("session-transient", Some("claim-ambient")).await?
    else {
        return Ok(());
    };
    fixture
        .api
        .get_errors
        .lock()
        .push_back(crate::kubernetes::scripted_kubernetes_error(
            kube::Error::Service(
                std::io::Error::new(std::io::ErrorKind::TimedOut, "scripted Kubernetes error")
                    .into(),
            ),
        ));

    let result = fixture
        .sandbox
        .invoke_with_context(connect_invocation(
            "call-transient",
            "session-transient",
            json!({}),
        ))
        .await
        .unwrap();

    assert_eq!(result["sandbox_id"], "claim-ambient");
    assert!(fixture.api.creates.lock().is_empty());
    let binding = bound_sandbox(&fixture.metadata, "session-transient").await?;
    assert_eq!(binding.sandbox_id, "claim-ambient");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn connect_without_id_keeps_the_binding_when_cancelled() -> Result<()> {
    let Some(fixture) = ToolsetFixture::start("session-cancel", Some("claim-ambient")).await?
    else {
        return Ok(());
    };
    let invocation = connect_invocation("call-cancel", "session-cancel", json!({}));
    invocation.cancel.cancel();

    let error = fixture
        .sandbox
        .invoke_with_context(invocation)
        .await
        .unwrap_err();

    assert!(matches!(error, ToolInvokeError::Fatal(_)));
    assert!(fixture.api.seen_ids.lock().is_empty());
    assert!(fixture.api.creates.lock().is_empty());
    assert!(fixture.caller.disconnected.lock().is_empty());
    let binding = bound_sandbox(&fixture.metadata, "session-cancel").await?;
    assert_eq!(binding.sandbox_id, "claim-ambient");
    Ok(())
}

/// A recreate whose claim creation fails must propagate the failure and
/// leave the stale binding in place so the next connect retries recovery.
#[tokio::test(flavor = "multi_thread")]
async fn connect_without_id_keeps_the_binding_when_the_replacement_create_fails() -> Result<()> {
    let Some(fixture) = ToolsetFixture::start("session-create-fail", Some("claim-gone")).await?
    else {
        return Ok(());
    };
    fixture
        .api
        .missing_ids
        .lock()
        .push("claim-gone".to_string());
    fixture
        .api
        .create_errors
        .lock()
        .push_back(scripted_api_error(403));

    let error = fixture
        .sandbox
        .invoke_with_context(connect_invocation(
            "call-create-fail",
            "session-create-fail",
            json!({}),
        ))
        .await
        .unwrap_err();

    assert!(matches!(error, ToolInvokeError::Recoverable(_)));
    assert_eq!(
        fixture.caller.disconnected.lock().as_slice(),
        ["claim-gone"]
    );
    assert_eq!(fixture.api.creates.lock().len(), 1);
    let binding = bound_sandbox(&fixture.metadata, "session-create-fail").await?;
    assert_eq!(binding.sandbox_id, "claim-gone");
    Ok(())
}

/// A replacement whose activation fails is still bound to the session, so the
/// next connect resumes it instead of minting another claim; the failure
/// returns without a second dispatch, so recreate cannot loop.
#[tokio::test(flavor = "multi_thread")]
async fn connect_without_id_binds_and_resumes_a_replacement_that_never_activates() -> Result<()> {
    let Some(fixture) =
        ToolsetFixture::start("session-activation-fail", Some("claim-gone")).await?
    else {
        return Ok(());
    };
    let replacement = crate::lifecycle::claim_name("call-activation-fail");
    fixture
        .api
        .missing_ids
        .lock()
        .push("claim-gone".to_string());
    // First get: the ambient sandbox is gone. Second get: the replacement's
    // activation fails with a non-not-found error, so the claim exists and
    // is worth binding.
    fixture.api.scripted_gets.lock().push_back(None);
    fixture
        .api
        .get_errors
        .lock()
        .push_back(scripted_api_error(403));

    let error = fixture
        .sandbox
        .invoke_with_context(connect_invocation(
            "call-activation-fail",
            "session-activation-fail",
            json!({}),
        ))
        .await
        .unwrap_err();

    assert!(matches!(error, ToolInvokeError::Recoverable(_)));
    assert!(error.to_string().contains("scripted Kubernetes error"));
    assert_eq!(
        fixture.caller.disconnected.lock().as_slice(),
        ["claim-gone"]
    );
    // Exactly one replacement was claimed: the activation error did not
    // trigger a second dispatch inside the first connect.
    assert_eq!(
        fixture.api.creates.lock().as_slice(),
        [replacement.as_str()]
    );
    // The bind applied, so the claim is tracked and must not be destroyed.
    assert!(fixture.api.deletes.lock().is_empty());
    let binding = bound_sandbox(&fixture.metadata, "session-activation-fail").await?;
    assert_eq!(binding.version, 1);
    assert_eq!(binding.sandbox_id, replacement);

    // The next ID-less connect resumes the bound replacement instead of
    // creating yet another claim.
    let result = fixture
        .sandbox
        .invoke_with_context(connect_invocation(
            "call-resume",
            "session-activation-fail",
            json!({}),
        ))
        .await
        .unwrap();

    assert_eq!(result["sandbox_id"], replacement.as_str());
    assert_eq!(fixture.api.creates.lock().len(), 1);
    let binding = bound_sandbox(&fixture.metadata, "session-activation-fail").await?;
    assert_eq!(binding.sandbox_id, replacement);
    Ok(())
}

/// A replacement that is proven absent is not bound: there is nothing to
/// resume, so the stale binding stays and the next connect replaces it
/// again instead of pointing the session at a dead claim.
#[tokio::test(flavor = "multi_thread")]
async fn connect_without_id_skips_binding_when_the_replacement_is_proven_absent() -> Result<()> {
    let Some(fixture) = ToolsetFixture::start("session-proven-absent", Some("claim-gone")).await?
    else {
        return Ok(());
    };
    let first = crate::lifecycle::claim_name("call-proven-absent");
    fixture
        .api
        .missing_ids
        .lock()
        .push("claim-gone".to_string());
    // First get: the ambient sandbox is gone. Second get: the freshly
    // created replacement is gone too, proving the new claim absent.
    fixture.api.scripted_gets.lock().extend([None, None]);

    let error = fixture
        .sandbox
        .invoke_with_context(connect_invocation(
            "call-proven-absent",
            "session-proven-absent",
            json!({}),
        ))
        .await
        .unwrap_err();

    assert_eq!(error.to_string(), format!("sandbox not found: {first}"));
    assert!(matches!(error, ToolInvokeError::Recoverable(_)));
    assert_eq!(
        fixture.caller.disconnected.lock().as_slice(),
        ["claim-gone"]
    );
    assert_eq!(fixture.api.creates.lock().as_slice(), [first.as_str()]);
    // No binding rewrite and no destroy: the proven-absent replacement was
    // neither recorded nor tracked.
    assert!(fixture.api.deletes.lock().is_empty());
    // No binding rewrite: the vanished replacement was not recorded.
    let binding = bound_sandbox(&fixture.metadata, "session-proven-absent").await?;
    assert_eq!(binding.sandbox_id, "claim-gone");

    // The next connect goes through the same recovery and claims a fresh
    // replacement under the new call's name.
    let result = fixture
        .sandbox
        .invoke_with_context(connect_invocation(
            "call-replace-again",
            "session-proven-absent",
            json!({}),
        ))
        .await
        .unwrap();

    let second = crate::lifecycle::claim_name("call-replace-again");
    assert_eq!(result["sandbox_id"], second.as_str());
    assert_eq!(
        fixture.api.creates.lock().as_slice(),
        [first.as_str(), second.as_str()]
    );
    let binding = bound_sandbox(&fixture.metadata, "session-proven-absent").await?;
    assert_eq!(binding.sandbox_id, second);
    Ok(())
}

/// A fresh create whose activation fails must also be bound, so the claim is
/// not leaked and the next connect resumes it.
#[tokio::test(flavor = "multi_thread")]
async fn connect_without_id_binds_a_fresh_claim_when_its_activation_fails() -> Result<()> {
    let Some((fixture, error)) = connect_failing_activation(
        "session-fresh-fail",
        "call-fresh-fail",
        "session-fresh-fail",
    )
    .await?
    else {
        return Ok(());
    };

    assert!(matches!(error, ToolInvokeError::Recoverable(_)));
    assert!(error.to_string().contains("scripted Kubernetes error"));
    let created = fixture.api.creates.lock().clone();
    assert_eq!(created.len(), 1);
    // The bind applied, so the claim is tracked and must not be destroyed.
    assert!(fixture.api.deletes.lock().is_empty());
    let binding = bound_sandbox(&fixture.metadata, "session-fresh-fail").await?;
    assert_eq!(binding.version, 1);
    assert_eq!(binding.sandbox_id, created[0]);

    // The next ID-less connect resumes the bound claim instead of leaking
    // it and creating another.
    let result = fixture
        .sandbox
        .invoke_with_context(connect_invocation(
            "call-fresh-resume",
            "session-fresh-fail",
            json!({}),
        ))
        .await
        .unwrap();

    assert_eq!(result["sandbox_id"], created[0].as_str());
    assert_eq!(fixture.api.creates.lock().len(), 1);
    Ok(())
}
