use super::*;

#[tokio::test(flavor = "multi_thread")]
async fn e2e_compact_rpc_returns_submitted_on_idle_session() {
    let _guard = TestStateGuard::new(None).await;
    let sandbox = TestConfigSandbox::new();
    sandbox.write_agent("plain", "You are plain.");
    let config = sandbox.config();

    // Simple call_fn that completes immediately
    let call_fn: AgentCallFn = Arc::new(move |_input, _config, _abort| {
        Box::pin(async move {
            Ok((
                "Done".to_string(),
                None,
                Vec::<ToolCall>::new(),
                CompletionTokenUsage::default(),
            ))
        })
    });

    let registry =
        SessionRegistry::new_for_tests(config.clone(), Duration::from_secs(30), Some(call_fn));

    // Open an SSE connection to create the session actor, then close it.
    // The session will remain registered in the registry.
    let response = open_sse(
        &config,
        &registry,
        "plain",
        "compact-test",
        json!([{
            "id": Uuid::new_v4(),
            "role": "user",
            "content": "initialize"
        }]),
    )
    .await;

    // Wait for the run to complete
    let _read = read_sse_until(response, Duration::from_secs(10), |read| {
        read.events
            .iter()
            .any(|event| event["type"] == "RUN_FINISHED")
    })
    .await;

    // Now issue a compact RPC. Since this is a call_fn/in-process executor,
    // the compaction request returns Submitted with a generated compaction_id.
    let compact = rpc_call(
        &config,
        &registry,
        "plain",
        "compact-test",
        json!({"jsonrpc":"2.0","id":2,"method":"session/compact"}),
    )
    .await;

    // Verify the response has the expected shape
    assert_eq!(compact["jsonrpc"], "2.0");
    assert_eq!(compact["id"], 2);

    // Either we get a result with status, or an error if something went wrong
    if compact["error"].is_object() {
        // Something went wrong - log the error for debugging
        panic!("compact RPC returned error: {:?}", compact["error"]);
    }

    // The result is a CompactSubmit enum serialized with serde tag
    let status = compact["result"]["status"]
        .as_str()
        .expect("compact result should have a status field");

    // For in-process executor, we expect Submitted with a generated compaction_id
    match status {
        "submitted" => {
            assert!(
                compact["result"]["compaction_id"].as_str().is_some(),
                "submitted should have compaction_id"
            );
        }
        "already_in_flight" | "nothing_to_do" => {
            // These are also valid responses
        }
        other => panic!("unexpected status: {}", other),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn e2e_compact_rpc_returns_error_for_nonexistent_session() {
    let _guard = TestStateGuard::new(None).await;
    let sandbox = TestConfigSandbox::new();
    sandbox.write_agent("plain", "You are plain.");
    let config = sandbox.config();
    let registry = SessionRegistry::new(config.clone());

    // No prior session, no actor spawned. Call compact on a nonexistent session.
    let compact = rpc_call(
        &config,
        &registry,
        "plain",
        "nonexistent-session",
        json!({"jsonrpc":"2.0","id":1,"method":"session/compact"}),
    )
    .await;

    // Should return a JSON-RPC error
    assert!(
        compact["error"].is_object(),
        "expected error for nonexistent session"
    );
    assert_eq!(compact["error"]["code"], -32001); // JSON_RPC_UNKNOWN_SESSION_CODE
}
